use anyhow::Result;
use skia_safe::{Canvas, Color, Font, Paint, PaintStyle, Rect, RRect, Surface, Typeface};
use std::collections::HashMap;
use std::sync::Arc;
use winit::window::Window;

// ─── WasiDrawable FFI ────────────────────────────────────────────────────────
//
// C++ shim in host/cpp/wasi_drawable.{h,cpp} subclasses SkDrawable with a
// mutable picture handle so parent recordings can capture `drawDrawable(id)`
// ops that resolve to the CURRENT picture at replay time. See
// cpp/wasi_drawable.h for rationale.

pub(crate) mod wasi_drawable_ffi {
    use std::os::raw::c_void;
    extern "C" {
        pub fn wasi_drawable_create() -> *mut c_void;
        pub fn wasi_drawable_set_inner(outer: *mut c_void, inner: *mut c_void);
        pub fn wasi_drawable_set_bounds(d: *mut c_void,
                                        l: f32, t: f32, r: f32, b: f32);
        pub fn wasi_drawable_set_clip_rect(d: *mut c_void,
                                           l: f32, t: f32, r: f32, b: f32,
                                           antialias: bool);
        pub fn wasi_drawable_set_clip_rrect(d: *mut c_void,
                                            l: f32, t: f32, r: f32, b: f32,
                                            radii_xy_4_corners: *const f32,
                                            antialias: bool);
        pub fn wasi_drawable_clear_clip(d: *mut c_void);
        pub fn wasi_drawable_set_shadow_elevation(d: *mut c_void, elevation: f32);
        // scene 0.0.2 entries (wasi_canvas_002_impl).
        pub fn wasi_drawable_set_matrix(d: *mut c_void,
                                        m00: f32, m01: f32, m02: f32,
                                        m10: f32, m11: f32, m12: f32,
                                        m20: f32, m21: f32, m22: f32);
        pub fn wasi_drawable_set_alpha(d: *mut c_void, alpha: f32);
        pub fn wasi_drawable_set_clip_path(d: *mut c_void, path: *const c_void,
                                           antialias: bool);
        pub fn wasi_drawable_unref(d: *mut c_void);
        pub fn wasi_canvas_draw_drawable(canvas: *mut c_void, d: *mut c_void);
    }
}

/// Read the underlying raw `SkPicture*` (or `SkCanvas*`, `SkDrawable*`, …)
/// out of a skia-safe handle. `RCHandle<N>` and `RefHandle<N>` are both
/// single-field tuple structs over `ptr::NonNull<N>`, so the first 8 bytes
/// of the struct are the native pointer. `NonNull` is `#[repr(transparent)]`
/// over `*const N`, and a single-field tuple struct over a transparent
/// type has the same starting layout. We use this to bridge skia-safe ↔
/// our C FFI without going through `pub(crate)` `NativeAccess`/`from_ptr`.
#[inline]
pub(crate) fn handle_to_native_ptr<T>(handle: *const T) -> *mut std::os::raw::c_void {
    unsafe { *(handle as *const *mut std::os::raw::c_void) }
}

/// Owned handle to a WasiDrawable. Holds one ref; Drop releases it.
pub struct WasiDrawable {
    ptr: *mut std::os::raw::c_void,
}

impl WasiDrawable {
    pub fn new() -> Self {
        Self { ptr: unsafe { wasi_drawable_ffi::wasi_drawable_create() } }
    }

    /// Swap the inner SkDrawable this wrapper delegates to. `None` clears it.
    pub fn set_inner(&mut self, inner: Option<&skia_safe::Drawable>) {
        let inner_ptr = match inner {
            Some(d) => handle_to_native_ptr(d as *const skia_safe::Drawable),
            None    => std::ptr::null_mut(),
        };
        unsafe { wasi_drawable_ffi::wasi_drawable_set_inner(self.ptr, inner_ptr); }
    }

    pub fn set_bounds(&mut self, l: f32, t: f32, r: f32, b: f32) {
        unsafe { wasi_drawable_ffi::wasi_drawable_set_bounds(self.ptr, l, t, r, b); }
    }

    pub fn as_ptr(&self) -> *mut std::os::raw::c_void { self.ptr }
}

impl Drop for WasiDrawable {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { wasi_drawable_ffi::wasi_drawable_unref(self.ptr); }
        }
    }
}

// SkDrawable refcount is non-atomic but the renderer is never shared across
// threads (winit event-loop only), so this is sound — matches the unsafe
// impl Send for SkiaRenderer below.
unsafe impl Send for WasiDrawable {}

// ─── Renderer state ──────────────────────────────────────────────────────────

/// Desktop GPU present state: a glutin GL context + window surface whose default
/// framebuffer (FBO 0) is wrapped by skia as the render target. Presenting via
/// `swap_buffers` hands the compositor a GPU buffer (dmabuf under WSLg), so the
/// window can keep client-side decorations without tripping weston's RDP pixman
/// scaler on fractional-scaled SHM surfaces.
#[cfg(not(target_os = "android"))]
struct DesktopGl {
    gl_context: glutin::context::PossiblyCurrentContext,
    gl_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    gr_context: skia_safe::gpu::DirectContext,
}

pub struct SkiaRenderer {
    // Drop order matters: gr_context + surface must drop before egl so that
    // Skia's GL cleanup happens while the EGL context is still bound.
    #[cfg(target_os = "android")]
    gr_context: skia_safe::gpu::DirectContext,

    /// Desktop present path: blits the CPU raster `surface` to the winit
    /// window each flush_and_swap (no GL — see Cargo.toml softbuffer note).
    /// None in window-less constructions (tests / --run-once).
    #[cfg(not(target_os = "android"))]
    sb_surface: Option<softbuffer::Surface<
        std::sync::Arc<winit::window::Window>,
        std::sync::Arc<winit::window::Window>,
    >>,

    /// The winit window (desktop only) — kept so `present_softbuffer` can
    /// reconcile the CPU buffer to the window's LIVE physical size before
    /// attaching it. Under fractional HiDPI (e.g. a 4K monitor at 150%) the
    /// real surface size is reported asynchronously after map, so a buffer
    /// sized to a stale `width/height` would disagree with the compositor's
    /// scaled surface and crash weston's RDP pixman scaler (OOB read).
    #[cfg(not(target_os = "android"))]
    window: Option<std::sync::Arc<winit::window::Window>>,

    /// Desktop GPU present (preferred): a glutin GL context whose default
    /// framebuffer backs `surface`. `Some` = present via `swap_buffers` (GPU
    /// path, frame-safe at fractional scale); `None` = softbuffer CPU fallback.
    #[cfg(not(target_os = "android"))]
    gl: Option<DesktopGl>,

    surface:    Surface,
    /// Physical GL surface size (the EGL surface dimensions).
    pub width:  u32,
    pub height: u32,
    /// Logical canvas size reported to the guest via surface-width/height.
    /// Equals width/height except in the task-33 standalone rotated mode,
    /// where the guest authors a portrait UI into a landscape GL surface.
    pub logical_width:  u32,
    pub logical_height: u32,
    /// Chrome content insets in PHYSICAL px (task 56): the status-bar strip
    /// at the physical top + the taskbar strip at the physical bottom. The
    /// fullscreen app's logical frame is shrunk to the gap between them and
    /// its content is translated down by `inset_top`, so it never draws
    /// under the chrome — in any orientation (the rotation is applied to the
    /// already-inset available rect). Overlays leave these 0.
    pub inset_top:    u32,
    pub inset_bottom: u32,
    /// Task 68/71 — soft-keyboard reservation in physical px for the CURRENT
    /// orientation. The IME is the source of truth: it reports the actual px it
    /// wants per orientation (re-reporting on rotation), so the host applies this
    /// value verbatim (subtracted from `logical_height` AFTER the dihedral
    /// rotation so it eats the USER bottom). No host-side orientation scaling.
    /// 0 = keyboard hidden.
    pub keyboard_base_px: u32,
    /// Base canvas transform re-applied at every begin_frame — identity
    /// normally, a 90° rotation in the standalone rotated mode so the
    /// guest's portrait drawing maps into the landscape GL surface.
    pub base_matrix: skia_safe::Matrix,
    /// The dihedral orientation code (0..7) currently applied. 0 =
    /// identity. Recomputed live by `set_orientation` (task 43 runtime
    /// rotation). Used to skip no-op updates and to inverse-map pointer
    /// coordinates from physical-buffer space back to logical space.
    pub current_orient: u32,

    #[cfg(target_os = "android")]
    pub(crate) egl: crate::egl::android::EglContext,

    typeface_cache:   HashMap<(String, bool, bool), Typeface>,
    pub font_collection: skia_safe::textlayout::FontCollection,
}

// Skia's RCHandle uses non-atomic refcounts so its types aren't auto-Send.
// We hold the renderer in a wasmtime Store whose `T: WasiView: Send` bound
// forces HostState to be Send. The renderer is never shared across threads —
// the entire host runs on the winit event-loop thread — so this is sound.
unsafe impl Send for SkiaRenderer {}

/// Map a dihedral orientation code (0..7) to the
/// `(base_matrix, logical_width, logical_height)` triple.
///
/// Factored out of the renderer constructor (task 33 standalone
/// orientation) so the task-43 runtime-rotation path can recompute it
/// live without duplicating the matrix math. `width`/`height` are the
/// PHYSICAL GL buffer dimensions; the returned matrix maps a logical
/// point into that buffer. Bitmask: FLIP_H=1, FLIP_V=2, ROT_90=4 — the
/// ROT_90/270 codes swap the logical axes (a portrait UI authored into a
/// landscape buffer), so they return swapped `(height, width)` logical
/// dims.
/// Task 71 step 3 — the real panel's native (portrait) dimensions for THIS
/// process, so an overlay guest can learn the actual screen size via
/// `display.display-size` (its own surface is just a strip and can't tell it).
/// 0 = unset → callers fall back to the renderer's own surface size (correct for
/// fullscreen apps, where the surface *is* the panel). Per-process global: each
/// app/overlay is its own zygote child, so there's exactly one panel here. Set
/// once at loop start from `SfSurface::panel_w/panel_h` (the `sf_panel_dims`
/// shim read). No literal — the value is the measured panel.
static PANEL_W: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static PANEL_H: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Record the measured native panel size for this process (see `PANEL_W/H`).
pub fn set_panel_dims(w: u32, h: u32) {
    use std::sync::atomic::Ordering;
    PANEL_W.store(w, Ordering::Relaxed);
    PANEL_H.store(h, Ordering::Relaxed);
}

pub(crate) fn dihedral_transform(orient: u32, width: u32, height: u32)
    -> (skia_safe::Matrix, u32, u32)
{
    let w = width as f32;
    let h = height as f32;
    match orient & 7 {
        // ── ROT_90 clear — logical == buffer (w × h) ──
        0 => (skia_safe::Matrix::new_identity(), width, height),
        // FLIP_H: (lx,ly) -> (w - lx, ly)
        1 => (skia_safe::Matrix::new_all(-1.0, 0.0, w,
                                          0.0, 1.0, 0.0, 0.0, 0.0, 1.0),
              width, height),
        // FLIP_V: (lx,ly) -> (lx, h - ly)
        2 => (skia_safe::Matrix::new_all(1.0, 0.0, 0.0,
                                          0.0, -1.0, h, 0.0, 0.0, 1.0),
              width, height),
        // ROT_180 (FLIP_H|FLIP_V): (lx,ly) -> (w - lx, h - ly)
        3 => (skia_safe::Matrix::new_all(-1.0, 0.0, w,
                                          0.0, -1.0, h, 0.0, 0.0, 1.0),
              width, height),
        // ── ROT_90 set — logical axes swapped (h × w) ──
        // ROT_90 — 90° CW: (lx,ly) -> (w - ly, lx)
        4 => (skia_safe::Matrix::new_all(0.0, -1.0, w,
                                          1.0, 0.0, 0.0, 0.0, 0.0, 1.0),
              height, width),
        // ROT_90|FLIP_H — transpose: (lx,ly) -> (ly, lx)
        5 => (skia_safe::Matrix::new_all(0.0, 1.0, 0.0,
                                          1.0, 0.0, 0.0, 0.0, 0.0, 1.0),
              height, width),
        // ROT_90|FLIP_V — anti-transpose: (lx,ly) -> (w - ly, h - lx)
        6 => (skia_safe::Matrix::new_all(0.0, -1.0, w,
                                          -1.0, 0.0, h, 0.0, 0.0, 1.0),
              height, width),
        // ROT_270 (ROT_90|FLIP_H|FLIP_V) — 90° CCW: (lx,ly) -> (ly, h - lx)
        _ => (skia_safe::Matrix::new_all(0.0, 1.0, 0.0,
                                          -1.0, 0.0, h, 0.0, 0.0, 1.0),
              height, width),
    }
}

impl SkiaRenderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();

        #[cfg(target_os = "android")]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let raw = window
                .window_handle()
                .map_err(|e| anyhow::anyhow!("window_handle failed: {e:?}"))?
                .as_raw();
            let native_window = match raw {
                RawWindowHandle::AndroidNdk(h) => h.a_native_window.as_ptr(),
                other => return Err(anyhow::anyhow!(
                    "expected AndroidNdk window handle, got {other:?}"
                )),
            };
            let egl = crate::egl::android::EglContext::new(native_window)?;

            let gl_interface = skia_safe::gpu::gl::Interface::new_load_with(
                crate::egl::android::EglContext::proc_resolver()
            ).ok_or_else(|| anyhow::anyhow!("GL interface failed"))?;

            let mut gr_context = skia_safe::gpu::direct_contexts::make_gl(
                gl_interface, None
            ).ok_or_else(|| anyhow::anyhow!("GrContext failed"))?;

            let surface = Self::make_gl_surface(
                &mut gr_context, egl.width, egl.height)?;

            return Ok(Self {
                egl, gr_context, surface,
                width: size.width, height: size.height,
                logical_width: size.width, logical_height: size.height,
                inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
                base_matrix: skia_safe::Matrix::new_identity(),
                current_orient: 0,
                typeface_cache:   HashMap::new(),
                font_collection:  Self::make_font_collection(),
            });
        }

        #[cfg(not(target_os = "android"))]
        {
            // GPU-first: present via a skia GL surface (glutin) so frames reach
            // the compositor as GPU buffers, dodging weston's crashing SHM
            // pixman scaler at fractional scale. Fall back to the softbuffer CPU
            // blit if GL init fails (older WSLg / no GL available).
            match Self::try_init_gl(&window, size) {
                Ok((gl, surface)) => {
                    log::info!("desktop present: GL via glutin ({}x{}) scale={}", size.width, size.height, window.scale_factor());
                    return Ok(Self {
                        gl: Some(gl),
                        sb_surface: None,
                        window: Some(window),
                        surface, width: size.width, height: size.height,
                        logical_width: size.width, logical_height: size.height,
                        inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
                        base_matrix: skia_safe::Matrix::new_identity(),
                        current_orient: 0,
                        typeface_cache:   HashMap::new(),
                        font_collection:  Self::make_font_collection(),
                    });
                }
                Err(e) => log::warn!("desktop GL init failed ({e:#}) — softbuffer fallback"),
            }
            let surface = skia_safe::surfaces::raster_n32_premul(
                (size.width as i32, size.height as i32)
            ).ok_or_else(|| anyhow::anyhow!("raster surface failed"))?;
            let sb_surface = match softbuffer::Context::new(window.clone()) {
                Ok(ctx) => match softbuffer::Surface::new(&ctx, window.clone()) {
                    Ok(s) => Some(s),
                    Err(e) => { log::warn!("softbuffer surface failed: {e}"); None }
                },
                Err(e) => { log::warn!("softbuffer context failed: {e}"); None }
            };
            Ok(Self {
                gl: None,
                sb_surface,
                window: Some(window),
                surface, width: size.width, height: size.height,
                logical_width: size.width, logical_height: size.height,
                inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
                base_matrix: skia_safe::Matrix::new_identity(),
                current_orient: 0,
                typeface_cache:   HashMap::new(),
                font_collection:  Self::make_font_collection(),
            })
        }
    }

    /// Headless renderer (no window, no GL) backed by a CPU raster surface.
    /// Used by desktop `--run-once` for `wasi:cli/command` guests (e.g.
    /// wandr.video.test) that never draw but must satisfy `HostState`'s
    /// non-Option renderer. `present()` no-ops with window/sb_surface = None.
    #[cfg(not(target_os = "android"))]
    pub fn new_headless(width: u32, height: u32) -> Result<Self> {
        let surface = skia_safe::surfaces::raster_n32_premul(
            (width.max(1) as i32, height.max(1) as i32),
        ).ok_or_else(|| anyhow::anyhow!("headless raster surface failed"))?;
        Ok(Self {
            gl: None,
            sb_surface: None,
            window: None,
            surface, width, height,
            logical_width: width, logical_height: height,
            inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
            base_matrix: skia_safe::Matrix::new_identity(),
            current_orient: 0,
            typeface_cache:   HashMap::new(),
            font_collection:  Self::make_font_collection(),
        })
    }

    /// The window's display scale factor (1.0 on a 1:1 desktop; true HiDPI on a
    /// retina display). Reported to guests as the UI density via
    /// `wandr:ui-shell/metrics.get-density`, so a dioxus/Slint app scales to the
    /// real display instead of the old hardcoded 2.0. Headless (no window) = 1.0.
    #[cfg(not(target_os = "android"))]
    pub fn scale_factor(&self) -> f64 {
        self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0)
    }

    /// Snapshot the current surface to PNG bytes (desktop diagnostics — e.g. the
    /// `--camera-shot` PiP-compositing check). Raster surface = a plain readback.
    #[cfg(not(target_os = "android"))]
    pub fn snapshot_png(&mut self) -> Option<Vec<u8>> {
        let image = self.surface.image_snapshot();
        let data = image.encode_to_data(skia_safe::EncodedImageFormat::PNG)?;
        Some(data.as_bytes().to_vec())
    }

    /// Build a renderer directly on a raw `ANativeWindow*`, bypassing winit.
    /// Used by the task-33 standalone (no-`NativeActivity`) mode, where the
    /// window comes from SurfaceFlinger via the `libsf_surface` shim. The
    /// EGL/GrContext/Skia path below is identical to `new()`'s Android branch.
    ///
    /// `query_hint` is invoked *after* EGL connects to read the Android
    /// producer transform hint (`NATIVE_WINDOW_TRANSFORM_HINT`) — that hint
    /// is not populated until the producer connects, so it cannot be passed
    /// in pre-connect.
    #[cfg(target_os = "android")]
    pub fn from_native_window(
        native_window: *mut std::ffi::c_void,
        intended_w: u32,
        intended_h: u32,
        query_hint: impl FnOnce() -> u32,
    ) -> Result<Self> {
        let egl = crate::egl::android::EglContext::new(native_window)?;
        // EGL producer is connected now — the transform hint is populated.
        let hint = query_hint() & 7;

        let gl_interface = skia_safe::gpu::gl::Interface::new_load_with(
            crate::egl::android::EglContext::proc_resolver()
        ).ok_or_else(|| anyhow::anyhow!("GL interface failed"))?;

        let mut gr_context = skia_safe::gpu::direct_contexts::make_gl(
            gl_interface, None
        ).ok_or_else(|| anyhow::anyhow!("GrContext failed"))?;

        let surface = Self::make_gl_surface(
            &mut gr_context, egl.width, egl.height)?;
        // egl.{width,height} are the real GL buffer dimensions — taken from
        // the ANativeWindow, not eglQuerySurface (which lies with a transposed
        // size on the taimen Adreno driver; see egl.rs). They match the
        // SurfaceFlinger buffer, so the guest renders 1:1, upright.
        let (width, height) = (egl.width as u32, egl.height as u32);

        // Standalone orientation (task 33). With the real buffer dimensions
        // above, the guest's portrait logical canvas maps 1:1 into the EGL
        // buffer and `base_matrix` is identity — no rotation needed.
        //
        // The `WANDR_ORIENT` env var / queried transform hint remain a manual
        // override: a 0..7 bitmask (FLIP_H=1, FLIP_V=2, ROT_90=4; ROT_180=3,
        // ROT_270=7) selecting any of the 8 dihedral placements, for a device
        // whose panel genuinely needs a rotation. Unset + a correctly-sized
        // buffer ⇒ orient 0 (identity), the normal path.
        let dims_swapped = intended_w != 0 && intended_h != 0
            && (intended_w < intended_h) != (width < height);
        // Effective transform: WANDR_ORIENT override, else the queried hint;
        // if neither is informative but the buffer came up axis-swapped,
        // fall back to a plain 90° rotation (ROT_90).
        let orient: u32 = std::env::var("WANDR_ORIENT").ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if hint != 0 { hint }
                       else if dims_swapped { 4 /* ROT_90 */ }
                       else { 0 })
            & 7;
        // base_matrix maps a logical point into the physical EGL buffer.
        // new_all(scaleX, skewX, transX, skewY, scaleY, transY, 0,0,1).
        // The ROT_90 bit swaps the logical axes (portrait UI into a landscape
        // buffer); the FLIP bits select the mirrored variants.
        // Factored into `dihedral_transform` (below the impl) so the
        // task-43 runtime rotation path can recompute it live via
        // `set_orientation`.
        let (base_matrix, logical_width, logical_height) =
            dihedral_transform(orient, width, height);
        log::info!(
            "renderer: orientation — transform hint {hint}, effective \
             orient {orient} — logical {logical_width}x{logical_height}, \
             physical {width}x{height}",
        );

        Ok(Self {
            egl, gr_context, surface, width, height,
            logical_width, logical_height, base_matrix,
            inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
            current_orient: orient,
            typeface_cache:   HashMap::new(),
            font_collection:  Self::make_font_collection(),
        })
    }

    /// Task 43 — apply a new dihedral orientation at runtime. Recomputes
    /// `base_matrix` + `logical_width/height` from the physical buffer
    /// dims (which never change — the content is pre-rotated into the
    /// fixed portrait GL buffer, so there is no EGL resize and no
    /// SurfaceFlinger buffer-transform call). Returns `true` if the
    /// orientation actually changed, in which case the caller must
    /// re-issue `on_resize(logical_width, logical_height)` to the guest
    /// so Compose re-lays out to the (possibly swapped) logical size.
    pub fn set_orientation(&mut self, orient: u32) -> bool {
        let orient = orient & 7;
        if orient == self.current_orient {
            return false;
        }
        self.current_orient = orient;
        self.recompute_transform();
        log::info!(
            "renderer: orientation → orient {orient}, logical {}x{} \
             (physical {}x{} unchanged)",
            self.logical_width, self.logical_height, self.width, self.height,
        );
        true
    }

    /// Task 56 — set the chrome content insets (physical px) for a
    /// fullscreen app and recompute the transform. `top`/`bottom` are the
    /// status-bar / taskbar strip heights; 0/0 (the default) means a true
    /// fullscreen / immersive app with no chrome, whose logical size equals
    /// the native display size. Safe to call with 0/0 to clear.
    /// Task 68 — set the soft-keyboard reservation as a portrait-reference height
    /// (px); 0 clears it. Re-derives the logical area (the keyboard eats the user
    /// bottom, orientation-scaled). The caller re-issues `on_resize`.
    pub fn set_keyboard_base(&mut self, px: u32) {
        self.keyboard_base_px = px;
        self.recompute_transform();
    }

    pub fn set_insets(&mut self, top: u32, bottom: u32) {
        self.inset_top = top;
        self.inset_bottom = bottom;
        self.recompute_transform();
        log::info!(
            "renderer: content insets top={top} bottom={bottom} → logical {}x{} \
             (physical {}x{})",
            self.logical_width, self.logical_height, self.width, self.height,
        );
    }

    /// Recompute `base_matrix` + `logical_width/height` from the current
    /// orientation + insets.
    ///
    /// Model: rotate the FULL panel, then reserve insets in USER space — the
    /// status bar at the user-top, the taskbar + soft-keyboard at the user-bottom.
    /// Doing it post-rotation means each inset always lands on the user's
    /// top/bottom in any orientation (subtracting from the physical height
    /// *before* the rotation would eat logical WIDTH in landscape). With 0 insets
    /// this is exactly `dihedral_transform(orient, width, height)` (native size).
    fn recompute_transform(&mut self) {
        let (m, lw, lh) = dihedral_transform(self.current_orient, self.width, self.height);
        let user_top = self.inset_top; // status bar

        // Task 71 — the host is a pure applier: reserve EXACTLY the keyboard px
        // the IME reported (it owns its size; no host-side scaling, no magic
        // floor). Guarding against a collapsed content area is the GUEST's job
        // (its own clamp/min must not invert) — not a fabricated host minimum.
        let kb = self.keyboard_base_px;
        let user_bottom = self.inset_bottom + kb; // taskbar + keyboard

        self.logical_width = lw;
        self.logical_height = lh.saturating_sub(user_top + user_bottom).max(1);
        // Shift content down past the user-top inset, in USER space (pre-rotation).
        let mut base = m;
        base.pre_concat(&skia_safe::Matrix::translate((0.0, user_top as f32)));
        self.base_matrix = base;
    }

    // ── Task 71 — unified display geometry (read-only) ────────────────────
    // The three nested rectangles, in user/logical space at the current
    // orientation. `recompute_transform` already owns the inputs (orientation,
    // chrome insets, keyboard reservation); these just re-project them without
    // mutating any state. See `my:skiko-gfx/display`.

    /// The whole panel at the current orientation, no insets. Uses the real
    /// measured panel size (task 71 step 3) so an OVERLAY guest gets the true
    /// screen, not its own strip surface; falls back to the surface size when
    /// the panel isn't known (fullscreen apps, where they're equal).
    pub fn display_size(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering;
        let pw = PANEL_W.load(Ordering::Relaxed);
        let ph = PANEL_H.load(Ordering::Relaxed);
        let (w, h) = if pw > 0 && ph > 0 { (pw, ph) } else { (self.width, self.height) };
        let (_, lw, lh) = dihedral_transform(self.current_orient, w, h);
        (lw, lh)
    }

    /// `display` minus the chrome strips (status bar + task bar). The soft
    /// keyboard is NOT removed here — that's `safe_size`.
    pub fn content_size(&self) -> (u32, u32) {
        let (_, lw, lh) = dihedral_transform(self.current_orient, self.width, self.height);
        let chrome = self.inset_top + self.inset_bottom;
        (lw, lh.saturating_sub(chrome).max(1))
    }

    /// `content` minus the soft keyboard — equals the live logical (surface)
    /// size `recompute_transform` already maintains (what `canvas.surface-*`
    /// reports). `safe == content` when the keyboard is down.
    pub fn safe_size(&self) -> (u32, u32) {
        (self.logical_width, self.logical_height)
    }

    /// 0 = portrait, 1 = landscape. Landscape is exactly the set of dihedral
    /// codes that swap the logical axes (4..=7), matching `dihedral_transform`.
    pub fn orientation_code(&self) -> u32 {
        match self.current_orient & 7 {
            4..=7 => 1,
            _ => 0,
        }
    }

    /// Move CPU-side caches from `old` into `self` so warm-resume preserves
    /// wasm-allocated handle IDs (pictures, recorders, text blobs, shaders,
    /// paragraphs, ...). The next_*_id counters carry over so the next ID
    /// the guest mints doesn't collide with one already in the inherited
    /// tables. GPU-resident caches (`text_image_cache`, `images`) are NOT
    /// inherited because their textures live in the dying gr_context.
    pub fn inherit_caches_from(&mut self, old: &mut Self) {
        self.typeface_cache   = std::mem::take(&mut old.typeface_cache);
        // bitmap_canvases hold a raster Surface (CPU-only), safe to inherit.
        // font_collection holds a default-FontMgr; keep the freshly built
        // one to be safe (cheap to recreate).
    }

    /// Wrap the current GL context's default framebuffer (FBO 0) as a skia
    /// render-target surface. Shared by the Android EGL path and the desktop
    /// glutin path (pure skia — no platform specifics).
    fn make_gl_surface(
        gr: &mut skia_safe::gpu::DirectContext,
        w: i32, h: i32,
    ) -> Result<Surface> {
        let fb_info = skia_safe::gpu::gl::FramebufferInfo {
            fboid:     0,
            format:    skia_safe::gpu::gl::Format::RGBA8.into(),
            protected: skia_safe::gpu::Protected::No,
        };
        let target = skia_safe::gpu::backend_render_targets::make_gl(
            (w, h), Some(0), 8, fb_info);
        skia_safe::gpu::surfaces::wrap_backend_render_target(
            gr, &target,
            skia_safe::gpu::SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            None, None,
        ).ok_or_else(|| anyhow::anyhow!("wrap_backend_render_target failed"))
    }

    /// Build the desktop GPU present path: a glutin EGL/GLX context + window
    /// surface on `window`, with skia bound to its default framebuffer. Returns
    /// the GL state and the skia surface, or an error (caller falls back to
    /// softbuffer). All `unsafe` calls are FFI into the platform GL loader on
    /// the single event-loop thread.
    #[cfg(not(target_os = "android"))]
    fn try_init_gl(
        window: &std::sync::Arc<winit::window::Window>,
        size: winit::dpi::PhysicalSize<u32>,
    ) -> Result<(DesktopGl, Surface)> {
        use glutin::config::ConfigTemplateBuilder;
        use glutin::context::{ContextApi, ContextAttributesBuilder};
        use glutin::prelude::*;
        use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
        use std::num::NonZeroU32;

        let (nw, nh) = (
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        let raw_window = window.window_handle()?.as_raw();
        let raw_display = window.display_handle()?.as_raw();

        // EGL display (WSLg/Wayland + Xwayland both expose EGL).
        let gl_display = unsafe {
            glutin::display::Display::new(raw_display, glutin::display::DisplayApiPreference::Egl)?
        };
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .compatible_with_native_window(raw_window)
            .build();
        let config = unsafe { gl_display.find_configs(template)? }
            .reduce(|best, c| {
                use glutin::config::GlConfig;
                if c.num_samples() < best.num_samples() { c } else { best }
            })
            .ok_or_else(|| anyhow::anyhow!("no GL config"))?;

        // Prefer a GLES2 context (matches the skia GLES backend used on device).
        let ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(None))
            .build(Some(raw_window));
        let not_current = unsafe { gl_display.create_context(&config, &ctx_attrs)? };

        let surf_attrs =
            SurfaceAttributesBuilder::<WindowSurface>::new().build(raw_window, nw, nh);
        let gl_surface = unsafe { gl_display.create_window_surface(&config, &surf_attrs)? };
        let gl_context = not_current.make_current(&gl_surface)?;
        // Present is paced by the guest's frame-pacing; don't double-block on vsync.
        let _ = gl_surface.set_swap_interval(
            &gl_context,
            glutin::surface::SwapInterval::DontWait,
        );

        let interface = skia_safe::gpu::gl::Interface::new_load_with(|name| {
            let Ok(cname) = std::ffi::CString::new(name) else { return std::ptr::null() };
            gl_display.get_proc_address(cname.as_c_str())
        })
        .ok_or_else(|| anyhow::anyhow!("skia GL interface load failed"))?;
        let mut gr_context = skia_safe::gpu::direct_contexts::make_gl(interface, None)
            .ok_or_else(|| anyhow::anyhow!("GrContext (desktop GL) failed"))?;
        let surface = Self::make_gl_surface(&mut gr_context, size.width as i32, size.height as i32)?;

        Ok((DesktopGl { gl_context, gl_surface, gr_context }, surface))
    }

    fn make_font_collection() -> skia_safe::textlayout::FontCollection {
        let mut fc = skia_safe::textlayout::FontCollection::new();
        let mgr = skia_safe::FontMgr::new();
        fc.set_default_font_manager(mgr, None);
        fc
    }

    pub fn canvas(&mut self) -> &Canvas {
        // The embedder-presented surface. (The legacy host-side recording
        // stack is gone — 0.0.2 recordings are first-class table resources
        // with their own canvases.)
        self.surface.canvas()
    }

    pub fn flush_and_swap(&mut self) {
        #[cfg(target_os = "android")]
        {
            self.egl.make_current();
            self.gr_context.flush_and_submit();
            self.egl.swap();
            // Each `blit_text_blob_cached` miss uploads a CPU raster to a GPU
            // texture. The cached SkImage holds a reference to that texture
            // for next-frame reuse. Without this purge, Skia's resource cache
            // ALSO retains scratch/throwaway resources from path tessellation,
            // gradient shaders, etc. — capping ~9 MB/sec growth on the showcase.
            self.gr_context.purge_unlocked_resources(
                skia_safe::gpu::PurgeResourceOptions::AllResources,
            );
        }
        #[cfg(not(target_os = "android"))]
        {
            use glutin::prelude::GlSurface;
            if let Some(gl) = self.gl.as_mut() {
                gl.gr_context.flush_and_submit();
                let _ = gl.gl_surface.swap_buffers(&gl.gl_context);
                gl.gr_context.purge_unlocked_resources(
                    skia_safe::gpu::PurgeResourceOptions::AllResources,
                );
            } else {
                self.present_softbuffer();
            }
        }
    }

    /// Desktop present: copy the skia raster surface into the softbuffer
    /// window buffer. Skia N32-premul on little-endian is BGRA bytes;
    /// softbuffer wants 0RGB u32 — `from_le_bytes([b,g,r,a])` lands each
    /// pixel as 0xAARRGGBB with the alpha byte ignored by softbuffer.
    #[cfg(not(target_os = "android"))]
    fn present_softbuffer(&mut self) {
        use std::num::NonZeroU32;
        // Reconcile the buffer to the window's LIVE physical size BEFORE
        // attaching it. winit reports the real surface size asynchronously
        // (ScaleFactorChanged/Resized after map), so under fractional HiDPI a
        // frame drawn at the stale size must not be presented: a buffer whose
        // dimensions disagree with the compositor's scaled surface makes
        // weston's RDP pixman scaler read out of bounds and SIGSEGV. On a
        // mismatch, resize to match and re-render next frame instead.
        if let Some(win) = self.window.clone() {
            let phys = win.inner_size();
            if phys.width == 0 || phys.height == 0 {
                return; // not configured yet — nothing safe to present
            }
            if phys.width != self.width || phys.height != self.height {
                self.resize(phys.width, phys.height);
                win.request_redraw();
                return;
            }
        }
        let (w, h) = (self.width, self.height);
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else { return };
        let Some(sb) = self.sb_surface.as_mut() else { return };
        if sb.resize(nw, nh).is_err() {
            return;
        }
        let Some(pixmap) = self.surface.peek_pixels() else { return };
        let Some(bytes) = pixmap.bytes() else { return };
        let row_bytes = pixmap.row_bytes();
        let Ok(mut buf) = sb.buffer_mut() else { return };
        for y in 0..h as usize {
            let row = &bytes[y * row_bytes..y * row_bytes + (w as usize) * 4];
            let out = &mut buf[y * w as usize..(y + 1) * w as usize];
            for (dst, px) in out.iter_mut().zip(row.chunks_exact(4)) {
                *dst = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            }
        }
        let _ = buf.present();
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.width  = w;
        self.height = h;
        // Task 62 — keep logical dims (what the guest lays out to, via
        // surface_width/height) in sync with the new buffer. The overlay
        // rotation path resizes the buffer when the device turns; without
        // this, logical_width/height would lag a buffer change that isn't
        // accompanied by an orientation change (e.g. a portrait
        // request-overlay-height re-anchor above the taskbar). For the
        // rotation path itself, set_orientation recomputes again — cheap.
        self.recompute_transform();
        #[cfg(target_os = "android")]
        {
            if let Ok(s) = Self::make_gl_surface(
                &mut self.gr_context, w as i32, h as i32)
            {
                self.surface = s;
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            if self.gl.is_some() {
                // GL path: resize the glutin surface, then re-wrap FBO 0.
                use glutin::prelude::GlSurface;
                use std::num::NonZeroU32;
                let new_surface = if let (Some(nw), Some(nh)) =
                    (NonZeroU32::new(w), NonZeroU32::new(h))
                {
                    let gl = self.gl.as_mut().unwrap();
                    gl.gl_surface.resize(&gl.gl_context, nw, nh);
                    Self::make_gl_surface(&mut gl.gr_context, w as i32, h as i32).ok()
                } else {
                    None
                };
                if let Some(s) = new_surface {
                    self.surface = s;
                }
            } else if let Some(s) =
                skia_safe::surfaces::raster_n32_premul((w as i32, h as i32))
            {
                self.surface = s;
            }
        }
    }

    pub fn draw_test_frame(&mut self) {
        #[cfg(target_os = "android")]
        self.egl.make_current();
        {
            let c = self.surface.canvas();
            // Don't paint a fullscreen black backdrop: clear transparent (shows whatever
            // is composited behind this layer if the buffer has alpha) and draw only a
            // small indicator badge in the top-left corner instead of a big rect.
            c.clear(Color::TRANSPARENT);
            c.draw_rect(
                Rect::from_xywh(8.0, 8.0, 40.0, 40.0),
                &Paint::new(skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0), None),
            );
        }
        self.flush_and_swap();
    }

    /// Returns a Typeface for the requested (family, bold, italic), reading
    /// from /system/fonts and caching the result.
    pub fn get_typeface(&mut self, family: &str, bold: bool, italic: bool) -> Typeface {
        let key = (family.to_string(), bold, italic);
        if let Some(tf) = self.typeface_cache.get(&key) {
            return tf.clone();
        }
        // If the family is an absolute path, try that first.
        let mut candidates: Vec<String> = Vec::new();
        if family.starts_with('/') {
            candidates.push(family.to_string());
        }
        // Task 41 — recognize Compose Multiplatform's standard family
        // aliases (Noto Serif / Noto Sans Mono / etc.) so
        // FontFamily.Serif and FontFamily.Monospace actually pick up
        // their proper /system/fonts/ files. Without these mappings,
        // both fall through to font_candidate_paths's Roboto fallback.
        candidates.extend(family_alias_paths(family, bold, italic).iter().map(|s| s.to_string()));
        // Match-family-style on Skia's default FontMgr gives zero-metrics
        // typefaces on this device, so we always load from a TTF file.
        candidates.extend(font_candidate_paths(bold, italic).iter().map(|s| s.to_string()));
        let mgr = skia_safe::FontMgr::new();
        for path in &candidates {
            if let Ok(bytes) = std::fs::read(path) {
                if let Some(tf) = mgr.new_from_data(&bytes, None) {
                    self.typeface_cache.insert(key.clone(), tf.clone());
                    log::info!("get_typeface: loaded {path} (bold={bold} italic={italic})");
                    return tf;
                }
            }
        }
        // Last-ditch fallback — Skia's default empty typeface.
        let mgr = skia_safe::FontMgr::new();
        let tf = mgr.legacy_make_typeface(None, skia_safe::FontStyle::normal())
            .expect("no fallback typeface available from FontMgr");
        self.typeface_cache.insert(key, tf.clone());
        tf
    }

}

/// Task 41 — Compose Multiplatform's GenericFontFamiliesMapping for
/// Linux (the platform the wasi stub reports as) lowers Serif/Monospace
/// to these names. Map them to real Android /system/fonts/ paths
/// before falling through to the default Roboto candidates.
///
/// Empty slice for unrecognized families — caller falls through.
fn family_alias_paths(family: &str, bold: bool, italic: bool) -> &'static [&'static str] {
    match family {
        // Compose FontFamily.Serif → "Noto Serif" / "DejaVu Serif" / ...
        "Noto Serif" | "NotoSerif" | "DejaVu Serif" | "Times New Roman" => {
            match (bold, italic) {
                (false, false) => &["/system/fonts/NotoSerif-Regular.ttf"],
                (true,  false) => &["/system/fonts/NotoSerif-Bold.ttf"],
                (false, true ) => &["/system/fonts/NotoSerif-Italic.ttf"],
                (true,  true ) => &["/system/fonts/NotoSerif-BoldItalic.ttf"],
            }
        }
        // Compose FontFamily.Monospace → "Noto Sans Mono" / "DejaVu Sans Mono" / ...
        // No NotoSansMono on this build of Pixel 2 XL — fall back to
        // DroidSansMono (the closest pre-installed monospace).
        "Noto Sans Mono" | "NotoSansMono" | "DejaVu Sans Mono" | "Consolas"
        | "Roboto Mono" | "RobotoMono" => &[
            "/system/fonts/DroidSansMono.ttf",
            "/system/fonts/CutiveMono.ttf",
        ],
        _ => &[],
    }
}

fn font_candidate_paths(bold: bool, italic: bool) -> &'static [&'static str] {
    match (bold, italic) {
        (true,  true ) => &[
            "/system/fonts/Roboto-BoldItalic.ttf",
            "/system/fonts/SourceSansPro-BoldItalic.ttf",
            "/system/fonts/DroidSans-Bold.ttf",
        ],
        (true,  false) => &[
            "/system/fonts/Roboto-Bold.ttf",
            "/system/fonts/SourceSansPro-Bold.ttf",
            "/system/fonts/DroidSans-Bold.ttf",
        ],
        (false, true ) => &[
            "/system/fonts/Roboto-Italic.ttf",
            "/system/fonts/SourceSansPro-Italic.ttf",
            "/system/fonts/DroidSans.ttf",
        ],
        (false, false) => &[
            "/system/fonts/Roboto-Regular.ttf",
            "/system/fonts/SourceSansPro-Regular.ttf",
            "/system/fonts/DroidSans.ttf",
        ],
    }
}

// ─── WIT canvas trait implementation ─────────────────────────────────────────

