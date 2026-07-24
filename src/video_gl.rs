//! Zero-copy import of a hardware-decoded frame: DMA-buf -> EGLImage -> GL
//! textures, so decoded pixels never touch the CPU (task 117 M2, zero-copy).
//!
//! LINUX DESKTOP ONLY. Android has no equivalent problem — MediaCodec decodes
//! straight into a SurfaceFlinger surface and the host never sees pixels.
//!
//! ‼️ SHAPE COPIED FROM SHIPPING PLAYERS, not invented — see
//! `.claude/memory/reference_vaapi_zerocopy_real_players.md` for the sources.
//! Three decisions come straight from them:
//!
//!   * NV12 is imported as TWO textures, `R8` (luma) + `GR88` (chroma), NOT as a
//!     single `GL_TEXTURE_EXTERNAL_OES`. mpv, VLC, Firefox and GStreamer's
//!     per-plane path all do this, because the external path can only pass
//!     colour as a HINT (`EGL_YUV_COLOR_SPACE_HINT_EXT`) that drivers are free
//!     to ignore — and they ignore it, defaulting to BT.601 limited whatever the
//!     content is. Two planes means WE apply the matrix, via Skia's `YUVAInfo`.
//!
//!   * The MODIFIER is passed when the driver can describe one. On this
//!     project's target hardware (Intel Ivybridge / Gen7, i965) NV12 decode
//!     surfaces come back Y-TILED — `i965_surface_native_memory` allocates tiled
//!     whenever `HAS_TILED_SURFACE`, which is true on Gen7. Importing a tiled
//!     buffer as linear renders silent garbage, so when the modifier cannot be
//!     described the attributes are OMITTED ENTIRELY (letting the kernel's GEM
//!     tiling carry it, which is how this worked before modifiers existed).
//!     Substituting `LINEAR` is never correct.
//!
//!   * Import happens PER FRAME. Caching per surface would be better and is what
//!     GStreamer and Chromium do — but they own their surface pools. Ours is
//!     owned by cros-codecs, whose `VaapiPicture` holds `Surface` BY VALUE and
//!     destroys it with the picture, so `VASurfaceID`s are not stable and there
//!     is nothing to key a cache on. mpv/VLC/Firefox are per-frame for the same
//!     reason (they sit on ffmpeg's dynamic pool). Recorded as a follow-up.
//!
//! ‼️ SKIA GL STATE. Skia caches its GL bindings, so every raw GL sequence here
//! is followed by `DirectContext::reset_context`. Skipping it makes the NEXT
//! Skia draw sample whatever we left bound — intermittent, and invisible to
//! every frame counter we have.

#![cfg(not(target_os = "android"))]

use std::ffi::{c_void, CString};
use std::sync::OnceLock;

use wandr_video::GpuFrame;

// ── EGL/GL entry points, resolved once ───────────────────────────────────────

type EglDisplay = *const c_void;
type EglImage = *const c_void;
/// ‼️ `const EGLint *` — attributes are 32-bit, NOT pointer-sized. The EGL 1.5
/// `eglCreateImage` takes 64-bit `EGLAttrib`, the KHR extension does not, and
/// passing 64-bit values here makes the driver read every other word as garbage.
/// It fails with no diagnostic beyond a null image.
type EglCreateImageKhr =
    unsafe extern "C" fn(EglDisplay, *const c_void, u32, *const c_void, *const i32) -> EglImage;
type EglDestroyImageKhr = unsafe extern "C" fn(EglDisplay, EglImage) -> u32;
type GlEglImageTargetTexture2dOes = unsafe extern "C" fn(u32, EglImage);
type GlGenTextures = unsafe extern "C" fn(i32, *mut u32);
type GlDeleteTextures = unsafe extern "C" fn(i32, *const u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, i32);

const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_NONE: i32 = 0x3038;
const EGL_NO_IMAGE: EglImage = std::ptr::null();

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2802;
const GL_TEXTURE_WRAP_S: u32 = 0x2802 + 1;
const GL_TEXTURE_WRAP_T: u32 = 0x2802 + 2;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;

/// DRM fourccs for the two NV12 planes. `R8` carries luma, `GR88` the
/// interleaved chroma pair — the split every player uses.
const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

// EGL_ANGLE_image_d3d11_texture — import a D3D11 NV12 texture into GL on Windows
// (ANGLE), the analog of dma-buf import. Per plane: create an EGLImage over the
// texture with a plane index + GL internal format, then bind it to a GL texture.
// Verified in repros/d3d11-angle-import-spike.
#[allow(dead_code)]
const EGL_D3D11_TEXTURE_ANGLE: u32 = 0x3484;
#[allow(dead_code)]
const EGL_D3D11_TEXTURE_PLANE_ANGLE: i32 = 0x3492;
#[allow(dead_code)]
const EGL_TEXTURE_INTERNAL_FORMAT_ANGLE: i32 = 0x345D;
#[allow(dead_code)]
const GL_R8: i32 = 0x8229; // Y plane
#[allow(dead_code)]
const GL_RG8: i32 = 0x822B; // interleaved UV plane

/// The plane's dma-buf as an EGL attribute (a raw fd). Unix-only: dma-buf import
/// is a Linux concept, and on Windows `import_nv12` returns `Err` at its EGL
/// guard before this is ever reached (there is no dma-buf import extension), so
/// the non-unix arm is genuinely unreachable — it exists only to compile.
#[cfg(unix)]
fn plane_raw_fd(f: &std::fs::File) -> i32 {
    std::os::unix::io::AsRawFd::as_raw_fd(f)
}
#[cfg(not(unix))]
fn plane_raw_fd(_f: &std::fs::File) -> i32 {
    unreachable!("dma-buf import is unix-only; import_nv12 returns Err before this on Windows")
}

struct Egl {
    display: EglDisplay,
    create_image: EglCreateImageKhr,
    destroy_image: EglDestroyImageKhr,
    image_target_texture: GlEglImageTargetTexture2dOes,
    gen_textures: GlGenTextures,
    delete_textures: GlDeleteTextures,
    bind_texture: GlBindTexture,
    tex_parameteri: GlTexParameteri,
    /// `EGL_EXT_image_dma_buf_import_modifiers`. Without it we cannot DESCRIBE a
    /// modifier, which on tiled hardware means we must not claim one.
    modifiers: bool,
    /// `EGL_ANGLE_image_d3d11_texture` — the Windows/ANGLE zero-copy import path.
    d3d11_image: bool,
}

// SAFETY: these are process-wide function pointers and one EGLDisplay handle,
// only ever used from the winit event-loop thread (the only thread that makes
// the GL context current — see canvas_impl::try_init_gl).
unsafe impl Send for Egl {}
unsafe impl Sync for Egl {}

static EGL: OnceLock<Option<Egl>> = OnceLock::new();

/// Register the GL display at context creation. Called from
/// `canvas_impl::try_init_gl`, which is the only place that has the glutin
/// display and the only thread where the context is ever current.
pub fn register(display: &glutin::display::Display) {
    let _ = EGL.set(build(display));
    // Windows: point the d3d11 decoder at ANGLE's D3D11 device, so its output
    // texture imports here as a same-device alias (zero-copy). Harmless if the
    // query fails — the decoder then uses its own device and we read back.
    #[cfg(all(feature = "d3d11", target_os = "windows"))]
    register_angle_d3d11_device(display);
}

/// Extract ANGLE's `ID3D11Device` from the EGL display and hand it to wandr-video
/// (`set_angle_d3d11_device`), so decode lands on the same device this GL context
/// samples from. On x86_64 Windows `extern "C"` == the platform ABI, matching the
/// rest of this module's EGL entry points.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
fn register_angle_d3d11_device(display: &glutin::display::Display) {
    use glutin::display::GlDisplay;
    const EGL_DEVICE_EXT: i32 = 0x322C;
    const EGL_D3D11_DEVICE_ANGLE: i32 = 0x33A1;
    type QueryDisplayAttrib = unsafe extern "C" fn(EglDisplay, i32, *mut isize) -> u32;
    type QueryDeviceAttrib = unsafe extern "C" fn(*const c_void, i32, *mut isize) -> u32;

    let Some(egl_display) = raw_egl_display(display) else { return };
    let sym = |name: &str| -> *const c_void {
        CString::new(name).ok().map(|c| display.get_proc_address(c.as_c_str())).unwrap_or(std::ptr::null())
    };
    let (qd, qdev) = (sym("eglQueryDisplayAttribEXT"), sym("eglQueryDeviceAttribEXT"));
    if qd.is_null() || qdev.is_null() {
        log::info!("video_gl: eglQueryD*AttribEXT missing — d3d11 decode stays own-device (readback)");
        return;
    }
    // SAFETY: non-null pointers for these exact names; queries write one isize each.
    unsafe {
        let query_display: QueryDisplayAttrib = std::mem::transmute(qd);
        let query_device: QueryDeviceAttrib = std::mem::transmute(qdev);
        let mut dev: isize = 0;
        if query_display(egl_display, EGL_DEVICE_EXT, &mut dev) != 1 {
            return;
        }
        let mut d3d: isize = 0;
        if query_device(dev as *const c_void, EGL_D3D11_DEVICE_ANGLE, &mut d3d) != 1 || d3d == 0 {
            return;
        }
        wandr_video::set_angle_d3d11_device(d3d as *mut c_void);
        log::info!("video_gl: d3d11 decode will use ANGLE's device (zero-copy import path)");
    }
}

/// The raw `EGLDisplay`, or `None` if this platform's glutin display is not EGL.
/// ‼️ macOS glutin has NO `RawDisplay::Egl` variant (it is CGL), so matching it
/// there is a COMPILE error — hence the cfg split: the macOS arm never names the
/// variant. Windows has EGL via ANGLE (so it reaches the extension probe, which
/// finds no dma-buf import and returns None); Linux is the real path.
#[cfg(not(target_os = "macos"))]
fn raw_egl_display(display: &glutin::display::Display) -> Option<EglDisplay> {
    use glutin::display::{AsRawDisplay, RawDisplay};
    match display.raw_display() {
        RawDisplay::Egl(p) => Some(p),
        _ => None,
    }
}
#[cfg(target_os = "macos")]
fn raw_egl_display(_display: &glutin::display::Display) -> Option<EglDisplay> {
    None // macOS is CGL, not EGL — no dma-buf zero-copy here
}

fn build(display: &glutin::display::Display) -> Option<Egl> {
    use glutin::display::GlDisplay;

    let Some(egl_display) = raw_egl_display(display) else {
        log::info!("video_gl: not an EGL display — zero-copy import unavailable");
        return None;
    };

    let sym = |name: &str| -> *const c_void {
        CString::new(name).ok().map(|c| display.get_proc_address(c.as_c_str())).unwrap_or(std::ptr::null())
    };
    macro_rules! load {
        ($name:literal, $ty:ty) => {{
            let p = sym($name);
            if p.is_null() {
                log::info!("video_gl: {} unavailable — zero-copy import off", $name);
                return None;
            }
            // SAFETY: the loader returned a non-null pointer for this exact name.
            unsafe { std::mem::transmute::<*const c_void, $ty>(p) }
        }};
    }

    // Extension strings come from the display, not the context.
    let exts = {
        type EglQueryString = unsafe extern "C" fn(EglDisplay, i32) -> *const std::os::raw::c_char;
        let p = sym("eglQueryString");
        if p.is_null() {
            return None;
        }
        let f: EglQueryString = unsafe { std::mem::transmute(p) };
        // 0x3055 = EGL_EXTENSIONS
        let s = unsafe { f(egl_display, 0x3055) };
        if s.is_null() {
            String::new()
        } else {
            unsafe { std::ffi::CStr::from_ptr(s) }.to_string_lossy().into_owned()
        }
    };
    // Two import capabilities: dma-buf (Linux VA-API) and D3D11 texture (Windows
    // ANGLE). Either is enough to be useful; both share eglCreateImageKHR +
    // glEGLImageTargetTexture2DOES, differing only in the image source.
    let dma_buf = exts.contains("EGL_EXT_image_dma_buf_import");
    let d3d11_image = exts.contains("EGL_ANGLE_image_d3d11_texture");
    if !dma_buf && !d3d11_image {
        log::info!("video_gl: no dma-buf or D3D11-texture import — zero-copy import off");
        return None;
    }
    let modifiers = exts.contains("EGL_EXT_image_dma_buf_import_modifiers");

    let egl = Egl {
        display: egl_display,
        create_image: load!("eglCreateImageKHR", EglCreateImageKhr),
        destroy_image: load!("eglDestroyImageKHR", EglDestroyImageKhr),
        image_target_texture: load!("glEGLImageTargetTexture2DOES", GlEglImageTargetTexture2dOes),
        gen_textures: load!("glGenTextures", GlGenTextures),
        delete_textures: load!("glDeleteTextures", GlDeleteTextures),
        bind_texture: load!("glBindTexture", GlBindTexture),
        tex_parameteri: load!("glTexParameteri", GlTexParameteri),
        modifiers,
        d3d11_image,
    };
    log::info!(
        "video_gl: zero-copy import AVAILABLE (dma_buf {}, d3d11_image {}, modifiers {})",
        dma_buf, d3d11_image,
        if modifiers { "yes" } else { "NO — tiled buffers will be refused" }
    );
    Some(egl)
}

pub fn available() -> bool {
    EGL.get().map(|e| e.is_some()).unwrap_or(false)
}

// ── an imported frame ────────────────────────────────────────────────────────

/// Two GL textures over a decoded frame's DMA-buf: luma and interleaved chroma.
///
/// Owns the textures AND the `GpuFrame`, because the frame owns the VA surface
/// the textures point INTO. Dropping this releases the textures first and the
/// surface after — the ordering matters, and tying them to one lifetime is what
/// makes it impossible to get wrong from the outside.
pub struct TextureFrame {
    pub y_tex: u32,
    pub uv_tex: u32,
    pub width: u32,
    pub height: u32,
    /// Held so the VA surface outlives the textures sampling it.
    _frame: GpuFrame,
}

impl Drop for TextureFrame {
    fn drop(&mut self) {
        let ids = [self.y_tex, self.uv_tex];
        // SAFETY: our own texture names, on the thread that created them.
        #[cfg(target_os = "macos")]
        unsafe {
            gl_delete_textures(2, ids.as_ptr())
        };
        #[cfg(not(target_os = "macos"))]
        {
            let Some(Some(egl)) = EGL.get() else { return };
            unsafe { (egl.delete_textures)(2, ids.as_ptr()) };
        }
    }
}

// macOS CGL/IOSurface + GL entry points for the zero-copy import. The GL calls use
// #[link_name] so they don't collide with the EGL-loaded pointers elsewhere; CGL
// and the GL core live in the OpenGL framework, the surface accessors in their own.
#[cfg(target_os = "macos")]
#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn CGLGetCurrentContext() -> *mut std::ffi::c_void;
    fn CGLTexImageIOSurface2D(
        ctx: *mut std::ffi::c_void,
        target: u32,
        internal_format: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        io_surface: *mut std::ffi::c_void,
        plane: u32,
    ) -> i32;
    #[link_name = "glGenTextures"]
    fn gl_gen_textures(n: i32, textures: *mut u32);
    #[link_name = "glBindTexture"]
    fn gl_bind_texture(target: u32, texture: u32);
    #[link_name = "glDeleteTextures"]
    fn gl_delete_textures(n: i32, textures: *const u32);
}
#[cfg(target_os = "macos")]
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferGetIOSurface(pb: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}
#[cfg(target_os = "macos")]
#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceGetWidthOfPlane(surface: *mut std::ffi::c_void, plane: usize) -> usize;
    fn IOSurfaceGetHeightOfPlane(surface: *mut std::ffi::c_void, plane: usize) -> usize;
}

/// Import a decoded NV12 frame as two GL textures.
///
/// ‼️ HANDS THE FRAME BACK on failure (`Err`) rather than consuming it. The first
/// cut returned `Option`, which swallowed the frame — and on the headless raster
/// path (every video diagnostic, `--run-once`) there is no GL at all, so EVERY
/// frame was dropped: `--video-decode-file` reported "300 decoded, 0 presented"
/// and still printed "ok", because its pass criterion never looks at pixels.
/// Returning the frame is what lets the caller read back instead.
pub fn import_nv12(frame: GpuFrame) -> Result<TextureFrame, GpuFrame> {
    // macOS: no EGL — the VideoToolbox CVPixelBuffer's IOSurface maps straight to
    // GL_TEXTURE_RECTANGLE planes via CGLTexImageIOSurface2D (mpv's hwdec_mac_gl).
    #[cfg(target_os = "macos")]
    return import_iosurface(frame);

    #[cfg(not(target_os = "macos"))]
    {
    let Some(Some(egl)) = EGL.get() else { return Err(frame) };

    // Windows/ANGLE: a GPU frame is a D3D11 texture, imported per-plane via
    // EGL_ANGLE_image_d3d11_texture. On Err the frame comes back and the dma-buf
    // path below reads it back (which is also what happens for a non-D3D11 frame).
    #[cfg(all(feature = "d3d11", target_os = "windows"))]
    let frame = match import_d3d11(egl, frame) {
        Ok(tf) => return Ok(tf),
        Err(f) => f,
    };

    if frame.fourcc != DRM_FORMAT_NV12 || frame.planes.len() < 2 {
        log::warn!("video_gl: not 2-plane NV12 (fourcc {:#x}) — reading back", frame.fourcc);
        return Err(frame);
    }
    // ‼️ A tiled buffer imported as linear renders silent garbage, so if we
    // cannot describe the modifier we must not pretend it is linear. Omitting
    // the attributes entirely is the documented fallback (the kernel's GEM
    // tiling then carries it); claiming LINEAR is never right.
    if !egl.modifiers && frame.modifier != DRM_FORMAT_MOD_INVALID && frame.modifier != 0 {
        log::warn!(
            "video_gl: modifier {:#x} but no modifiers extension — refusing import (a tiled \
             buffer read as linear is garbage); reading back",
            frame.modifier
        );
        return Err(frame);
    }

    let (w, h) = (frame.width, frame.height);
    // NV12: luma full size as R8, chroma half size as GR88 (two bytes/sample).
    let planes = [
        (DRM_FORMAT_R8, w, h, 0usize),
        (DRM_FORMAT_GR88, w.div_ceil(2), h.div_ceil(2), 1usize),
    ];

    let mut texes = [0u32; 2];
    // SAFETY: our own names; the context is current on this thread.
    unsafe { (egl.gen_textures)(2, texes.as_mut_ptr()) };

    for (i, (fourcc, pw, ph, plane_idx)) in planes.into_iter().enumerate() {
        let p = &frame.planes[plane_idx];
        let mut attrs: Vec<i32> = vec![
            EGL_WIDTH,
            pw as i32,
            EGL_HEIGHT,
            ph as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane_raw_fd(&p.fd),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            p.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            p.pitch as i32,
        ];
        if egl.modifiers && frame.modifier != DRM_FORMAT_MOD_INVALID {
            attrs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (frame.modifier & 0xffff_ffff) as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (frame.modifier >> 32) as u32 as i32,
            ]);
        }
        attrs.push(EGL_NONE);

        // SAFETY: attrs is a valid EGL_NONE-terminated attribute list; the fd
        // outlives this call because `frame` owns it.
        let image = unsafe {
            (egl.create_image)(
                egl.display,
                std::ptr::null(), // EGL_NO_CONTEXT — dma-buf import is contextless
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null(),
                attrs.as_ptr(),
            )
        };
        if image == EGL_NO_IMAGE {
            log::warn!("video_gl: eglCreateImageKHR failed for plane {i} — reading back");
            // SAFETY: our own names.
            unsafe { (egl.delete_textures)(2, texes.as_ptr()) };
            return Err(frame);
        }
        // SAFETY: valid image; texture name we just generated.
        unsafe {
            (egl.bind_texture)(GL_TEXTURE_2D, texes[i]);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            (egl.image_target_texture)(GL_TEXTURE_2D, image);
            // The texture keeps the storage alive (EGL_KHR_image_base), so the
            // image itself is no longer needed — mpv and rusty-codecs both
            // destroy it right here rather than tracking it.
            (egl.destroy_image)(egl.display, image);
            (egl.bind_texture)(GL_TEXTURE_2D, 0);
        }
    }

    Ok(TextureFrame { y_tex: texes[0], uv_tex: texes[1], width: w, height: h, _frame: frame })
    }
}

/// macOS zero-copy: bind the CVPixelBuffer's IOSurface planes as GL_TEXTURE_RECTANGLE
/// textures (R8 luma + RG8 chroma) with CGLTexImageIOSurface2D. No EGL, no copy —
/// the exact mechanism mpv's hwdec_mac_gl.c uses. Hands the frame back on any failure
/// so the caller reads it back (the headless raster path has no GL context).
#[cfg(target_os = "macos")]
fn import_iosurface(frame: GpuFrame) -> Result<TextureFrame, GpuFrame> {
    const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
    const GL_R8: i32 = 0x8229;
    const GL_RG8: i32 = 0x822B;
    const GL_RED: u32 = 0x1903;
    const GL_RG: u32 = 0x8227;
    const GL_UNSIGNED_BYTE: u32 = 0x1401;

    let Some(view) = frame.iosurface() else { return Err(frame) };
    let pb = view.pixel_buffer;
    unsafe {
        let ctx = CGLGetCurrentContext();
        let surface = CVPixelBufferGetIOSurface(pb);
        if ctx.is_null() || surface.is_null() {
            log::warn!("video_gl: no CGL context / IOSurface — reading back");
            return Err(frame);
        }
        let (w, h) = (frame.width, frame.height);
        let mut texes = [0u32; 2];
        gl_gen_textures(2, texes.as_mut_ptr());
        // plane 0 = luma R8, plane 1 = interleaved chroma RG8.
        let planes = [(0usize, GL_R8, GL_RED), (1usize, GL_RG8, GL_RG)];
        for (i, (plane, internal, format)) in planes.into_iter().enumerate() {
            gl_bind_texture(GL_TEXTURE_RECTANGLE, texes[i]);
            let err = CGLTexImageIOSurface2D(
                ctx,
                GL_TEXTURE_RECTANGLE,
                internal,
                IOSurfaceGetWidthOfPlane(surface, plane) as i32,
                IOSurfaceGetHeightOfPlane(surface, plane) as i32,
                format,
                GL_UNSIGNED_BYTE,
                surface,
                plane as u32,
            );
            gl_bind_texture(GL_TEXTURE_RECTANGLE, 0);
            if err != 0 {
                log::warn!("video_gl: CGLTexImageIOSurface2D plane {i} err {err} — reading back");
                gl_delete_textures(2, texes.as_ptr());
                return Err(frame);
            }
        }
        Ok(TextureFrame { y_tex: texes[0], uv_tex: texes[1], width: w, height: h, _frame: frame })
    }
}

/// Import a decoded NV12 frame that lives in a D3D11 texture (Windows/DXVA2) as
/// two GL textures, via ANGLE's `EGL_ANGLE_image_d3d11_texture`. Same two-texture
/// (R8 luma + GR88 chroma) shape as the dma-buf path — Skia applies the matrix.
///
/// The texture is on ANGLE's own D3D11 device (the decoder was pointed at it via
/// `wandr_video::set_angle_d3d11_device`), so this is a same-device alias — no
/// shared handle, no copy. Hands the frame back on any failure, like `import_nv12`.
#[cfg(all(feature = "d3d11", target_os = "windows"))]
fn import_d3d11(egl: &Egl, frame: GpuFrame) -> Result<TextureFrame, GpuFrame> {
    let Some(view) = frame.d3d11() else { return Err(frame) };
    if !egl.d3d11_image {
        log::warn!("video_gl: D3D11 frame but no EGL_ANGLE_image_d3d11_texture — reading back");
        return Err(frame);
    }
    let (w, h) = (frame.width, frame.height);
    let tex_ptr = view.texture_ptr();

    let mut texes = [0u32; 2];
    // SAFETY: our own names; the context is current on this thread.
    unsafe { (egl.gen_textures)(2, texes.as_mut_ptr()) };

    // Plane 0 = luma (R8), plane 1 = interleaved chroma (RG8).
    for (i, (plane, internal_fmt)) in [(0i32, GL_R8), (1i32, GL_RG8)].into_iter().enumerate() {
        let attrs: [i32; 5] = [
            EGL_D3D11_TEXTURE_PLANE_ANGLE, plane,
            EGL_TEXTURE_INTERNAL_FORMAT_ANGLE, internal_fmt,
            EGL_NONE,
        ];
        // SAFETY: attrs is EGL_NONE-terminated; the texture outlives this call
        // because `frame` (which owns it) is held to the end of the function.
        let image = unsafe {
            (egl.create_image)(
                egl.display,
                std::ptr::null(), // EGL_NO_CONTEXT
                EGL_D3D11_TEXTURE_ANGLE,
                tex_ptr,
                attrs.as_ptr(),
            )
        };
        if image == EGL_NO_IMAGE {
            log::warn!("video_gl: eglCreateImageKHR(D3D11) failed for plane {i} — reading back");
            unsafe { (egl.delete_textures)(2, texes.as_ptr()) };
            return Err(frame);
        }
        // SAFETY: valid image; texture name we just generated.
        unsafe {
            (egl.bind_texture)(GL_TEXTURE_2D, texes[i]);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (egl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            (egl.image_target_texture)(GL_TEXTURE_2D, image);
            (egl.destroy_image)(egl.display, image);
            (egl.bind_texture)(GL_TEXTURE_2D, 0);
        }
    }
    Ok(TextureFrame { y_tex: texes[0], uv_tex: texes[1], width: w, height: h, _frame: frame })
}
