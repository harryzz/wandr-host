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
}

fn build(display: &glutin::display::Display) -> Option<Egl> {
    use glutin::display::{AsRawDisplay, GlDisplay, RawDisplay};

    let RawDisplay::Egl(egl_display) = display.raw_display() else {
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
    if !exts.contains("EGL_EXT_image_dma_buf_import") {
        log::info!("video_gl: EGL_EXT_image_dma_buf_import missing — zero-copy import off");
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
    };
    log::info!(
        "video_gl: zero-copy import AVAILABLE (dma_buf_import, modifiers {})",
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
        let Some(Some(egl)) = EGL.get() else { return };
        let ids = [self.y_tex, self.uv_tex];
        // SAFETY: our own texture names, on the thread that created them.
        unsafe { (egl.delete_textures)(2, ids.as_ptr()) };
    }
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
    let Some(Some(egl)) = EGL.get() else { return Err(frame) };
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
            std::os::fd::AsRawFd::as_raw_fd(&p.fd),
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
