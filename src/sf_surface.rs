//! `dlopen` wrapper for `libsf_surface.so` — the task-33 libgui surface shim.
//!
//! wandr-host's cargo/NDK cross-compile cannot link `libgui` (Android's
//! private platform C++ library). Instead the shim is built in-tree as a
//! soong `cc_library_shared` (see `cpp/sf_surface.{cpp,bp}`) and loaded here
//! at runtime via `dlopen` — its `libgui`/`libui`/… dependencies resolve
//! from `/system/lib64` on the device. See memory
//! `project-boot-model-libgui-build` and `tasks/33-boot-model-bringup.md`.

use anyhow::{ensure, Result};
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicI32, Ordering};

/// Task 47 step 3c — bridge between the `my:skiko-gfx/keyboard`
/// `request-overlay-height` Host impl and the standalone render
/// loop. The Host impl runs inside a wasm-import call (same thread
/// as the render loop, but doesn't have a reference to the
/// `SfSurface`). It writes the requested height here; the render
/// loop drains it per-frame and applies via `sf.resize_overlay`.
///
/// `0` = no pending request. Last writer wins (height requests
/// supersede earlier ones).
static PENDING_OVERLAY_HEIGHT: AtomicI32 = AtomicI32::new(0);

/// Called from the `request-overlay-height` Host impl. Stores the
/// requested height for the render loop to pick up next frame.
pub fn request_overlay_resize(height_px: i32) {
    if height_px <= 0 {
        return;
    }
    PENDING_OVERLAY_HEIGHT.store(height_px, Ordering::SeqCst);
}

/// Called from the standalone render loop per frame. Returns the
/// most recent requested height (and clears it) if any; `None`
/// otherwise. The caller is expected to call `SfSurface::resize_overlay`
/// with the returned value.
pub fn take_pending_overlay_resize() -> Option<i32> {
    let h = PENDING_OVERLAY_HEIGHT.swap(0, Ordering::SeqCst);
    if h > 0 { Some(h) } else { None }
}

/// `ANativeWindow* sf_create_fullscreen_surface(int32_t*, int32_t*, uint32_t*)`.
type CreateFn = unsafe extern "C" fn(*mut i32, *mut i32, *mut u32) -> *mut c_void;
/// `ANativeWindow* sf_create_overlay_surface(int32_t x, int32_t y, int32_t w,
/// int32_t h, int32_t*, int32_t*, uint32_t*)` — geometry-parameterized
/// (task 55). w/h<=0 → full panel dim; y<0 → bottom-anchored.
type CreateOverlayFn =
    unsafe extern "C" fn(i32, i32, i32, i32, *mut i32, *mut i32, *mut u32) -> *mut c_void;
/// `int32_t sf_resize_overlay(int32_t new_height_px)` — task 47 step 3c.
type ResizeOverlayFn = unsafe extern "C" fn(i32) -> i32;
/// `int32_t sf_set_overlay_geometry(int32_t x, int32_t y, int32_t w, int32_t h)` —
/// task 62. Generic overlay move+resize (superset of `sf_resize_overlay`).
type SetOverlayGeometryFn = unsafe extern "C" fn(i32, i32, i32, i32) -> i32;
/// `void sf_panel_dims(int32_t* out_w, int32_t* out_h)` — task 62.
type PanelDimsFn = unsafe extern "C" fn(*mut i32, *mut i32);
/// `void sf_set_input_rect(int32_t x, int32_t y, int32_t w, int32_t h)` — task 80
/// Step 2. Sets this host's input region (global coords) for the ART-less
/// InputReader path; non-positive w/h clears the filter.
type SetInputRectFn = unsafe extern "C" fn(i32, i32, i32, i32);
/// `int32_t sf_input_poll(SfInputEvent*, int32_t)`.
type InputPollFn = unsafe extern "C" fn(*mut SfInputEvent, i32) -> i32;
/// `uint32_t sf_query_transform_hint(void)`.
type QueryHintFn = unsafe extern "C" fn() -> u32;
/// `int32_t sf_request_focus(void)`.
type RequestFocusFn = unsafe extern "C" fn() -> i32;
/// `int32_t sf_set_layer(int32_t z)` — task 46 step 4/5.
type SetLayerFn = unsafe extern "C" fn(i32) -> i32;
/// `int32_t sf_set_visible(int32_t visible)` — task 46 step 4/5.
type SetVisibleFn = unsafe extern "C" fn(i32) -> i32;

extern "C" {
    /// NDK — flushes the ANativeWindow's notion of buffer geometry after the
    /// SurfaceControl has been resized by SurfaceFlinger. Without this, EGL
    /// keeps drawing at the old dimensions even though SF's layer is smaller.
    /// `libandroid.so` is linked into the wandr-host shared object.
    fn ANativeWindow_setBuffersGeometry(
        window: *mut c_void,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
}

/// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM` — also the value of `PIXEL_FORMAT_RGBA_8888`
/// used by the shim's `createSurface` call. Constant from `android/hardware_buffer.h`.
const ANW_FORMAT_RGBA_8888: i32 = 1;

/// Task 62 — resolve the two new optional geometry symbols
/// (`sf_set_overlay_geometry`, `sf_panel_dims`) from an already-`dlopen`'d
/// shim handle. Both are `None` on a pre-task-62 shim, degrading the
/// overlay-rotation path to a no-op (it's gated on `set_overlay_geometry`).
unsafe fn resolve_geometry_syms(
    handle: *mut c_void,
) -> (Option<SetOverlayGeometryFn>, Option<PanelDimsFn>, Option<SetInputRectFn>) {
    let geom_name = CString::new("sf_set_overlay_geometry").unwrap();
    let geom_sym = libc::dlsym(handle, geom_name.as_ptr());
    let set_overlay_geometry: Option<SetOverlayGeometryFn> =
        if geom_sym.is_null() { None } else { Some(std::mem::transmute(geom_sym)) };

    let pd_name = CString::new("sf_panel_dims").unwrap();
    let pd_sym = libc::dlsym(handle, pd_name.as_ptr());
    let panel_dims: Option<PanelDimsFn> =
        if pd_sym.is_null() { None } else { Some(std::mem::transmute(pd_sym)) };

    // Task 80 Step 2 — optional input-region setter (None on a pre-task-80 shim).
    let ir_name = CString::new("sf_set_input_rect").unwrap();
    let ir_sym = libc::dlsym(handle, ir_name.as_ptr());
    let set_input_rect: Option<SetInputRectFn> =
        if ir_sym.is_null() { None } else { Some(std::mem::transmute(ir_sym)) };

    (set_overlay_geometry, panel_dims, set_input_rect)
}

/// Task 62 — read the panel's native dimensions via `sf_panel_dims`,
/// falling back to `(fallback_w, fallback_h)` when the shim predates it
/// or returns nonsense.
fn query_panel_dims(panel_dims: Option<PanelDimsFn>, fallback_w: i32, fallback_h: i32) -> (i32, i32) {
    if let Some(f) = panel_dims {
        let mut pw: i32 = 0;
        let mut ph: i32 = 0;
        unsafe { f(&mut pw, &mut ph) };
        if pw > 0 && ph > 0 {
            return (pw, ph);
        }
    }
    (fallback_w, fallback_h)
}

/// POD input event drained from the shim's InputFlinger channel. Mirrors
/// `struct SfInputEvent` in `cpp/sf_surface.{cpp,h}` — keep all three in sync.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SfInputEvent {
    /// 0=down 1=up 2=move 3=scroll  10=key-down 11=key-up.
    pub kind: i32,
    /// Multi-touch pointer id (0..N); 0 for key events.
    pub pointer_id: i32,
    pub x: f32,
    pub y: f32,
    /// Normalized pressure 0.0..1.0; 0 for key events.
    pub pressure: f32,
    /// `AKEYCODE_*` for key events; 0 otherwise.
    pub key_code: i32,
    /// `AMETA_*` shift/alt/ctrl bitmask for key events; 0 otherwise.
    pub meta_state: i32,
}

/// A fullscreen surface allocated from SurfaceFlinger by `libsf_surface.so`.
/// Keeps the `dlopen` handle for the process lifetime — unloading the shim
/// would invalidate `native_window`.
pub struct SfSurface {
    _handle: *mut c_void,
    /// `ANativeWindow*` — hand straight to `EglContext::new`.
    pub native_window: *mut c_void,
    pub width: i32,
    pub height: i32,
    /// SurfaceFlinger display rotation the shim applied (ui::Rotation 0..3);
    /// the renderer pairs its base transform with this. See task 33.
    pub transform: u32,
    /// `sf_input_poll` — `None` if the shim predates task-33 Step 3.
    input_poll: Option<InputPollFn>,
    /// `sf_query_transform_hint` — `None` if the shim predates the task-33
    /// orientation fix; query it only *after* EGL has connected the producer.
    query_hint: Option<QueryHintFn>,
    /// `sf_request_focus` — `None` if the shim predates standalone key
    /// support; the host calls this periodically to keep wandr focused.
    request_focus: Option<RequestFocusFn>,
    /// `sf_set_layer` — `None` if the shim predates task 46. When the
    /// arbiter (step 4) demotes an app to background it pushes z to 0;
    /// promotion to foreground pulls z back to `i32::MAX`. Until the
    /// .so is rebuilt on the AOSP a-03 host, this stays `None` and
    /// callers fall back to "no z-order control" semantics.
    set_layer: Option<SetLayerFn>,
    /// `sf_set_visible` — `None` if the shim predates task 46. Backs
    /// the cheap "hide while background / show on foreground" path
    /// (the layer stays allocated, BBQ keeps the last frame).
    set_visible: Option<SetVisibleFn>,
    /// `sf_resize_overlay` — `None` if the shim predates task 47 step 3c.
    /// When present, lets the IME guest declare its preferred panel
    /// height via the `request-overlay-height` WIT verb.
    resize_overlay: Option<ResizeOverlayFn>,
    /// `sf_set_overlay_geometry` — `None` if the shim predates task 62.
    /// Generic move+resize used by the overlay-rotation path to flip a
    /// bottom strip into a vertical side strip on landscape.
    set_overlay_geometry: Option<SetOverlayGeometryFn>,
    /// `sf_set_input_rect` — `None` if the shim predates task 80 Step 2. Sets
    /// the fullscreen app's input region (panel minus chrome insets) so taps on
    /// chrome strips don't leak to the app under the ART-less InputReader path.
    set_input_rect: Option<SetInputRectFn>,
    /// Panel's native (portrait) dimensions, read once via `sf_panel_dims`
    /// at create time (falls back to the surface dims when the shim
    /// predates task 62). The overlay-rotation path needs `panel_h` to
    /// size a full-height vertical side strip.
    pub panel_w: i32,
    pub panel_h: i32,
    /// True when this surface was created via `sf_create_overlay_surface`
    /// rather than `sf_create_fullscreen_surface`. Used to gate the
    /// overlay-only `resize_overlay` path.
    is_overlay: bool,
}

impl SfSurface {
    /// `dlopen` the shim at `so_path` and create a fullscreen surface.
    pub fn create(so_path: &str) -> Result<Self> {
        unsafe {
            let path = CString::new(so_path)?;
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
            ensure!(!handle.is_null(), "dlopen({so_path}) failed");

            let name = CString::new("sf_create_fullscreen_surface").unwrap();
            let sym = libc::dlsym(handle, name.as_ptr());
            ensure!(
                !sym.is_null(),
                "dlsym sf_create_fullscreen_surface failed in {so_path}"
            );
            let create: CreateFn = std::mem::transmute(sym);

            // sf_input_poll is optional — a shim without it just yields no input.
            let poll_name = CString::new("sf_input_poll").unwrap();
            let poll_sym = libc::dlsym(handle, poll_name.as_ptr());
            let input_poll: Option<InputPollFn> = if poll_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(poll_sym))
            };

            // sf_query_transform_hint is optional too — an older shim leaves
            // the renderer to fall back on its dims-swapped heuristic.
            let hint_name = CString::new("sf_query_transform_hint").unwrap();
            let hint_sym = libc::dlsym(handle, hint_name.as_ptr());
            let query_hint: Option<QueryHintFn> = if hint_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(hint_sym))
            };

            let focus_name = CString::new("sf_request_focus").unwrap();
            let focus_sym = libc::dlsym(handle, focus_name.as_ptr());
            let request_focus: Option<RequestFocusFn> = if focus_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(focus_sym))
            };

            // Task 46 step 4/5 — z-order + visibility toggles. Optional
            // (older shim builds lack them); the arbiter degrades to
            // "no visual z-order, just lifecycle + OOM" when missing.
            let layer_name = CString::new("sf_set_layer").unwrap();
            let layer_sym = libc::dlsym(handle, layer_name.as_ptr());
            let set_layer: Option<SetLayerFn> = if layer_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(layer_sym))
            };

            let visible_name = CString::new("sf_set_visible").unwrap();
            let visible_sym = libc::dlsym(handle, visible_name.as_ptr());
            let set_visible: Option<SetVisibleFn> = if visible_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(visible_sym))
            };

            let resize_name = CString::new("sf_resize_overlay").unwrap();
            let resize_sym = libc::dlsym(handle, resize_name.as_ptr());
            let resize_overlay: Option<ResizeOverlayFn> = if resize_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(resize_sym))
            };

            let (set_overlay_geometry, panel_dims, set_input_rect) = resolve_geometry_syms(handle);

            // Summarize which optional symbols were resolved — handy when
            // the shim .so is older than the wandr-host binary expects.
            log::info!(
                "sf_surface: dlsym summary — input_poll={} query_hint={} request_focus={} set_layer={} set_visible={} resize_overlay={} set_overlay_geometry={} panel_dims={}",
                input_poll.is_some(),
                query_hint.is_some(),
                request_focus.is_some(),
                set_layer.is_some(),
                set_visible.is_some(),
                resize_overlay.is_some(),
                set_overlay_geometry.is_some(),
                panel_dims.is_some(),
            );

            let mut w: i32 = 0;
            let mut h: i32 = 0;
            let mut t: u32 = 0;
            let nw = create(&mut w, &mut h, &mut t);
            ensure!(!nw.is_null(), "sf_create_fullscreen_surface returned null");

            // Fullscreen surface dims ARE the panel dims; sf_panel_dims (if
            // present) is authoritative either way.
            let (panel_w, panel_h) = query_panel_dims(panel_dims, w, h);

            Ok(SfSurface {
                _handle: handle,
                native_window: nw,
                width: w,
                height: h,
                transform: t,
                input_poll,
                query_hint,
                request_focus,
                set_layer,
                set_visible,
                resize_overlay,
                set_overlay_geometry,
                set_input_rect,
                panel_w,
                panel_h,
                is_overlay: false,
            })
        }
    }

    /// Task 47 step 3c — `dlopen` the shim at `so_path` and create a
    /// bottom-strip overlay surface of `height_px` pixels (positioned at
    /// `(0, PH - height_px)`). Same flags + BBQ attach + input-window
    /// register as the fullscreen path, but at the smaller rect. The
    /// IME guest can later resize via `request-overlay-height`.
    ///
    /// Errors if the shim predates task 47 step 3c (no
    /// `sf_create_overlay_surface` export). Callers can fall back to
    /// `create()` for a fullscreen surface in that case.
    /// Geometry-parameterized overlay (task 55). `(x, y, w, h)` in display
    /// pixels: `w<=0`/`h<=0` → full panel dim; `y<0` → bottom-anchored.
    /// Status bar = `(0, 0, 0, 88)`; IME = `(0, -1, 0, 1200)`. The runtime
    /// owns what each overlay *is*; the shim is geometry-generic.
    pub fn create_overlay(so_path: &str, x: i32, y: i32, w: i32, h: i32) -> Result<Self> {
        unsafe {
            let path = CString::new(so_path)?;
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
            ensure!(!handle.is_null(), "dlopen({so_path}) failed");

            let name = CString::new("sf_create_overlay_surface").unwrap();
            let sym = libc::dlsym(handle, name.as_ptr());
            ensure!(
                !sym.is_null(),
                "dlsym sf_create_overlay_surface failed in {so_path} \
                 — rebuild libsf_surface.so on the a-03 host"
            );
            let create_overlay: CreateOverlayFn = std::mem::transmute(sym);

            // Optional sym table — same as `create()`. The fullscreen
            // path needs all of these too, so once the overlay export
            // ships everything else is already present.
            let poll_name = CString::new("sf_input_poll").unwrap();
            let poll_sym = libc::dlsym(handle, poll_name.as_ptr());
            let input_poll: Option<InputPollFn> = if poll_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(poll_sym))
            };

            let hint_name = CString::new("sf_query_transform_hint").unwrap();
            let hint_sym = libc::dlsym(handle, hint_name.as_ptr());
            let query_hint: Option<QueryHintFn> = if hint_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(hint_sym))
            };

            let focus_name = CString::new("sf_request_focus").unwrap();
            let focus_sym = libc::dlsym(handle, focus_name.as_ptr());
            let request_focus: Option<RequestFocusFn> = if focus_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(focus_sym))
            };

            let layer_name = CString::new("sf_set_layer").unwrap();
            let layer_sym = libc::dlsym(handle, layer_name.as_ptr());
            let set_layer: Option<SetLayerFn> = if layer_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(layer_sym))
            };

            let visible_name = CString::new("sf_set_visible").unwrap();
            let visible_sym = libc::dlsym(handle, visible_name.as_ptr());
            let set_visible: Option<SetVisibleFn> = if visible_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(visible_sym))
            };

            let resize_name = CString::new("sf_resize_overlay").unwrap();
            let resize_sym = libc::dlsym(handle, resize_name.as_ptr());
            let resize_overlay: Option<ResizeOverlayFn> = if resize_sym.is_null() {
                None
            } else {
                Some(std::mem::transmute(resize_sym))
            };

            let (set_overlay_geometry, panel_dims, set_input_rect) = resolve_geometry_syms(handle);

            log::info!(
                "sf_surface: overlay dlsym summary — input_poll={} query_hint={} request_focus={} set_layer={} set_visible={} resize_overlay={} set_overlay_geometry={} panel_dims={}",
                input_poll.is_some(),
                query_hint.is_some(),
                request_focus.is_some(),
                set_layer.is_some(),
                set_visible.is_some(),
                resize_overlay.is_some(),
                set_overlay_geometry.is_some(),
                panel_dims.is_some(),
            );

            let mut out_w: i32 = 0;
            let mut out_h: i32 = 0;
            let mut t: u32 = 0;
            let nw = create_overlay(x, y, w, h, &mut out_w, &mut out_h, &mut t);
            ensure!(
                !nw.is_null(),
                "sf_create_overlay_surface(x={x},y={y},w={w},h={h}) returned null"
            );

            // out_w/out_h are the STRIP dims (e.g. full width × keyboard
            // height), NOT the panel — so the (out_w, out_h) fallback is
            // only a degraded last resort for a pre-task-62 shim, which
            // also lacks set_overlay_geometry so rotation is off anyway.
            let (panel_w, panel_h) = query_panel_dims(panel_dims, out_w, out_h);

            Ok(SfSurface {
                _handle: handle,
                native_window: nw,
                width: out_w,
                height: out_h,
                transform: t,
                input_poll,
                query_hint,
                request_focus,
                set_layer,
                set_visible,
                resize_overlay,
                set_overlay_geometry,
                set_input_rect,
                panel_w,
                panel_h,
                is_overlay: true,
            })
        }
    }

    /// Task 47 step 3c — resize this overlay surface to `new_height_px`
    /// pixels tall (panel width stays at the display width). Re-positions
    /// to `(0, PH - new_height_px)` and flushes the ANativeWindow's
    /// buffer geometry so EGL/Skia notice the new dimensions on the next
    /// frame. No-op (+ warn) when called on a fullscreen surface or when
    /// the shim is too old to expose `sf_resize_overlay`.
    pub fn resize_overlay(&self, new_height_px: i32) -> bool {
        if !self.is_overlay {
            log::warn!(
                "sf_surface: resize_overlay({new_height_px}) ignored — \
                 surface is fullscreen, not overlay"
            );
            return false;
        }
        let Some(resize) = self.resize_overlay else {
            log::warn!(
                "sf_surface: resize_overlay({new_height_px}) ignored — \
                 shim does not export sf_resize_overlay"
            );
            return false;
        };
        let rc = unsafe { resize(new_height_px) };
        if rc != 0 {
            log::warn!("sf_surface: sf_resize_overlay({new_height_px}) returned {rc}");
            return false;
        }
        // Flush the producer-side notion of buffer geometry. Without
        // this, EGL keeps presenting at the surface's previous size
        // even though the SF layer has shrunk/grown.
        let nw_rc = unsafe {
            ANativeWindow_setBuffersGeometry(
                self.native_window,
                self.width,
                new_height_px,
                ANW_FORMAT_RGBA_8888,
            )
        };
        if nw_rc != 0 {
            log::warn!(
                "sf_surface: ANativeWindow_setBuffersGeometry({}, {}) returned {nw_rc}",
                self.width, new_height_px
            );
        }
        true
    }

    /// Task 62 — move + resize this overlay to a panel-space rect, then
    /// flush the ANativeWindow buffer geometry. `(x, y, w, h)` carries the
    /// shim's resolution sentinels (`w<=0/h<=0` → full panel dim; `y<0` →
    /// bottom-anchored; `x<=0` → 0); `(buf_w, buf_h)` are the CONCRETE
    /// resolved buffer dims the caller computed (never sentinels) — the
    /// host resolves the rect itself so `setBuffersGeometry` and the shim
    /// agree. Used by the overlay-rotation path to flip a bottom strip
    /// into a vertical side strip. No-op (+ warn) on a fullscreen surface
    /// or a shim too old to export `sf_set_overlay_geometry`.
    pub fn set_overlay_geometry(&self, x: i32, y: i32, w: i32, h: i32, buf_w: i32, buf_h: i32) -> bool {
        if !self.is_overlay {
            log::warn!("sf_surface: set_overlay_geometry ignored — surface is fullscreen");
            return false;
        }
        let Some(set_geom) = self.set_overlay_geometry else {
            log::warn!(
                "sf_surface: set_overlay_geometry ignored — shim does not export \
                 sf_set_overlay_geometry (rebuild libsf_surface.so on a-03)"
            );
            return false;
        };
        let rc = unsafe { set_geom(x, y, w, h) };
        if rc != 0 {
            log::warn!("sf_surface: sf_set_overlay_geometry({x},{y},{w},{h}) returned {rc}");
            return false;
        }
        let nw_rc = unsafe {
            ANativeWindow_setBuffersGeometry(self.native_window, buf_w, buf_h, ANW_FORMAT_RGBA_8888)
        };
        if nw_rc != 0 {
            log::warn!(
                "sf_surface: ANativeWindow_setBuffersGeometry({buf_w}, {buf_h}) returned {nw_rc}"
            );
        }
        true
    }

    /// True when this surface was created via `create_overlay` rather than
    /// `create()`. Lets the render loop choose between the SIGUSR1/SIGUSR2
    /// foreground/background lifecycle and the SIGRTMIN+1 overlay-behind
    /// add-on.
    pub fn is_overlay(&self) -> bool {
        self.is_overlay
    }

    /// Task 46 step 4/5 — reposition the wandr layer on SurfaceFlinger's
    /// z-axis. Higher z is on top; `i32::MAX` is the default. Backgrounded
    /// apps should `set_layer(0)`. Returns `false` if the shim is too old
    /// to expose this (the arbiter then falls back to lifecycle + OOM
    /// without visual z-order).
    pub fn set_layer(&self, z: i32) -> bool {
        match self.set_layer {
            Some(f) => unsafe { f(z) == 0 },
            None    => false,
        }
    }

    /// Task 46 step 4/5 — toggle wandr-layer visibility. Cheaper than
    /// re-allocating the surface for "background" — the layer stays
    /// alive, BBQ keeps the last frame, re-showing is one round-trip.
    pub fn set_visible(&self, visible: bool) -> bool {
        match self.set_visible {
            Some(f) => unsafe { f(if visible { 1 } else { 0 }) == 0 },
            None    => false,
        }
    }

    /// Task 80 Step 2 — set this host's input region (global display coords) for
    /// the ART-less InputReader path; touches outside are dropped. The fullscreen
    /// app passes its content rect (panel minus chrome insets) so chrome-strip taps
    /// don't leak to it. Non-positive w/h clears (accept all). No-op on a pre-task-80
    /// shim or the inputflinger path.
    pub fn set_input_rect(&self, x: i32, y: i32, w: i32, h: i32) {
        if let Some(f) = self.set_input_rect {
            unsafe { f(x, y, w, h) };
        }
    }

    /// Query the live Android producer transform hint
    /// (`NATIVE_WINDOW_TRANSFORM_HINT`, a 0..7 bitmask). Call this only
    /// *after* EGL has connected the producer — the hint is not populated
    /// before that. Returns 0 if the shim predates this export.
    pub fn query_transform_hint(&self) -> u32 {
        self.query_hint.map(|f| unsafe { f() }).unwrap_or(0)
    }

    /// Re-request input focus for the wandr window. Standalone has no Activity
    /// so activity-backed windows (launcher, last-resumed app) keep stealing
    /// focus from InputDispatcher's view — call this periodically (e.g. once
    /// per second) to keep keys flowing.
    pub fn request_focus(&self) {
        if let Some(f) = self.request_focus {
            unsafe { let _ = f(); }
        }
    }

    /// Drain pending input events from the shim into `buf`; returns the slice
    /// actually filled. Non-blocking — call once per frame.
    pub fn poll_input<'b>(&self, buf: &'b mut [SfInputEvent]) -> &'b [SfInputEvent] {
        let Some(poll) = self.input_poll else { return &[]; };
        let n = unsafe { poll(buf.as_mut_ptr(), buf.len() as i32) };
        let n = (n.max(0) as usize).min(buf.len());
        &buf[..n]
    }
}
