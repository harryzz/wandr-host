use anyhow::Result;
use skia_safe::{Canvas, Color, Font, Paint, PaintStyle, Rect, RRect, Surface, Typeface};
use std::collections::HashMap;
use std::sync::Arc;
use winit::window::Window;

use crate::bindings::my::skiko_gfx::canvas::{
    BlendMode, ColorFilterKind, PaintAttrs, PaintStyle as WitPaintStyle, StrokeCap, StrokeJoin,
};

// ─── Rasterized-text cache ───────────────────────────────────────────────────
//
// Without this cache `blit_text_blob` allocates a fresh CPU surface +
// `SkImage` on every call and Skia uploads each as a unique GPU texture that
// is never reused. With ~50 text draws per frame at 60 fps that's ~3000
// texture uploads/sec and ~9 MB/sec leak. Caching by (blob-bounds-hash,
// paint colour) caps the working set at O(distinct labels).

struct CachedTextImage {
    image:    skia_safe::Image,
    offset_x: f32,
    offset_y: f32,
}

const TEXT_IMAGE_CACHE_CAP: usize = 256;

fn rasterize_text_blob(blob: &skia_safe::TextBlob, paint: &Paint) -> Option<CachedTextImage> {
    let bounds = blob.bounds();
    let img_w = (bounds.width().ceil()  as i32 + 4).max(1);
    let img_h = (bounds.height().ceil() as i32 + 4).max(1);
    let mut cpu = skia_safe::surfaces::raster_n32_premul((img_w, img_h))?;
    cpu.canvas().clear(Color::TRANSPARENT);
    cpu.canvas().draw_text_blob(blob, (-bounds.left() + 1.0, -bounds.top() + 1.0), paint);
    Some(CachedTextImage {
        image:    cpu.image_snapshot(),
        offset_x: bounds.left() - 1.0,
        offset_y: bounds.top() - 1.0,
    })
}

fn paint_cache_key(p: &Paint) -> u32 {
    // skia_safe::Color is repr(transparent) wrapping SkColor (u32) —
    // safe to transmute.
    unsafe { std::mem::transmute::<skia_safe::Color, u32>(p.color()) }
}

/// Content-based hash of a text blob: text + font params. Two blobs with the
/// same content hash render identically; two with different content always
/// get different keys (regardless of whether their visual bounds match).
fn text_content_hash(text: &str, family: &str, size: f32, weight: u32, italic: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    family.hash(&mut h);
    size.to_bits().hash(&mut h);
    weight.hash(&mut h);
    italic.hash(&mut h);
    h.finish()
}

// ─── Emoji-capable text shaping ──────────────────────────────────────────────
//
// `TextBlob::from_str` lays out with a SINGLE typeface and no font fallback, so
// any codepoint the primary face lacks (emoji, CJK) is dropped — that's why
// emoji vanished in dioxus-canvas guests while the Compose path (which goes
// through the textlayout FontCollection, with fallback) rendered them fine.
// Shape through SkShaper (harfbuzz + ICU, embedded) with the system FontMgr as
// the fallback chain so missing glyphs land on NotoColorEmoji etc. The N32
// raster surface in `rasterize_text_blob` preserves color-emoji bitmaps.
// Cached thread-local: shaping is stateless and the renderer runs on one thread.
thread_local! {
    static FALLBACK_SHAPER: skia_safe::shaper::Shaper =
        skia_safe::shaper::Shaper::new(skia_safe::FontMgr::new());
}

/// Shape `text` at `font` into a single-line blob with system-font fallback for
/// glyphs the primary face lacks. Falls back to `from_str` if shaping yields
/// nothing, so plain text never regresses.
///
/// Baseline alignment: `SkShaper`'s run handler puts the first line's TOP at the
/// offset, so the baseline lands at `offset.y - ascent` (ascent is negative).
/// `TextBlob::from_str` puts the baseline at y=0, and all guest draw points are
/// baseline-relative — so we pass `offset.y = ascent` to land the baseline back
/// at 0 and keep text where the guest expects it.
fn shape_text_fallback(text: &str, font: &Font) -> Option<skia_safe::TextBlob> {
    if text.is_empty() {
        return None;
    }
    let (_, metrics) = font.metrics();
    let offset_y = metrics.ascent; // negative → shifts the blob up to baseline 0
    FALLBACK_SHAPER
        .with(|sh| sh.shape_text_blob(text, font, true, 1.0e6, (0.0, offset_y)))
        .map(|(blob, _)| blob)
        .or_else(|| skia_safe::TextBlob::from_str(text, font))
}

// ─── WasiDrawable FFI ────────────────────────────────────────────────────────
//
// C++ shim in host/cpp/wasi_drawable.{h,cpp} subclasses SkDrawable with a
// mutable picture handle so parent recordings can capture `drawDrawable(id)`
// ops that resolve to the CURRENT picture at replay time. See
// cpp/wasi_drawable.h for rationale.

mod wasi_drawable_ffi {
    use std::os::raw::c_void;
    extern "C" {
        pub fn wasi_drawable_create() -> *mut c_void;
        pub fn wasi_drawable_set_inner(outer: *mut c_void, inner: *mut c_void);
        pub fn wasi_drawable_set_bounds(d: *mut c_void,
                                        l: f32, t: f32, r: f32, b: f32);
        pub fn wasi_drawable_set_transform(d: *mut c_void,
                                           layer_x: f32, layer_y: f32,
                                           translation_x: f32, translation_y: f32,
                                           scale_x: f32, scale_y: f32,
                                           rotation_z: f32,
                                           pivot_x: f32, pivot_y: f32,
                                           alpha: f32);
        pub fn wasi_drawable_set_clip_rect(d: *mut c_void,
                                           l: f32, t: f32, r: f32, b: f32,
                                           antialias: bool);
        pub fn wasi_drawable_set_clip_rrect(d: *mut c_void,
                                            l: f32, t: f32, r: f32, b: f32,
                                            radii_xy_4_corners: *const f32,
                                            antialias: bool);
        pub fn wasi_drawable_clear_clip(d: *mut c_void);
        pub fn wasi_drawable_set_shadow_elevation(d: *mut c_void, elevation: f32);
        pub fn wasi_drawable_ref(d: *mut c_void);
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
fn handle_to_native_ptr<T>(handle: *const T) -> *mut std::os::raw::c_void {
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

// ─── Multi-run text-blob builder ─────────────────────────────────────────────

struct TextBlobRun {
    text:   String,
    family: String,
    size:   f32,
    weight: u32,
    italic: bool,
    x:      f32,
    y:      f32,
}

// ─── Renderer state ──────────────────────────────────────────────────────────

/// Plain-data copy of `skia_safe::textlayout::LineMetrics` numeric fields.
/// Task 50 — see `SkiaRenderer::para_line_metrics_cache`.
pub struct CachedLineMetrics {
    pub start_index: u32,
    pub end_index: u32,
    pub end_excluding_whitespaces: u32,
    pub end_including_newline: u32,
    pub hard_break: bool,
    pub ascent: f64,
    pub descent: f64,
    pub unscaled_ascent: f64,
    pub height: f64,
    pub width: f64,
    pub left: f64,
    pub baseline: f64,
    pub line_number: u32,
}

pub struct SkiaRenderer {
    // Drop order matters: gr_context + surface must drop before egl so that
    // Skia's GL cleanup happens while the EGL context is still bound.
    #[cfg(target_os = "android")]
    gr_context: skia_safe::gpu::DirectContext,

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
    egl:        crate::egl::android::EglContext,

    // Each blob carries a content hash (text + font params) so the text-image
    // cache key can distinguish "Count: 5" from "Count: 0" — same bounds,
    // different content. Without this the cache returns a stale GPU texture
    // and the displayed text never updates.
    text_blobs:       HashMap<u32, (skia_safe::TextBlob, u64)>,
    multi_blob_cache: HashMap<u32, Vec<(skia_safe::TextBlob, f32, f32, u64)>>,
    text_blob_runs:   Vec<TextBlobRun>,
    images:           HashMap<u32, skia_safe::Image>,
    shader_cache:     HashMap<u32, skia_safe::Shader>,
    next_blob_id:     u32,
    next_shader_id:   u32,
    // Picture recording (Tier A skia shim). recorders are in either
    // "idle" or "recording" state; recording_stack holds the IDs of
    // recorders currently in begin_recording → finish state, with the
    // top redirecting `canvas()` draws into the recorder's canvas.
    recorders:        HashMap<u32, skia_safe::PictureRecorder>,
    pictures:         HashMap<u32, skia_safe::Picture>,
    recording_stack:  Vec<u32>,
    next_recorder_id: u32,
    next_picture_id:  u32,
    // WasiDrawable instances (deferred-replay shim). Each maps id → owned
    // SkDrawable*. Parent recordings hold raw pointers via drawDrawable, so
    // dropping a drawable while a parent picture still references it would
    // dangling. Compose drops them at RenderNode.close() AFTER releasing
    // the parent layer that referenced them, which is correct order.
    drawables:        HashMap<u32, WasiDrawable>,
    next_drawable_id: u32,
    typeface_cache:   HashMap<(String, bool, bool), Typeface>,
    /// Guest-registered typefaces (create-typeface from raw font bytes) for
    /// the guest-shaped glyph path (draw-glyphs). Ids share next_blob_id.
    guest_typefaces:  HashMap<u32, Typeface>,

    text_image_cache: HashMap<(u64, u32), CachedTextImage>,
    text_image_keys:  std::collections::VecDeque<(u64, u32)>,

    pub para_builders:   HashMap<u32, skia_safe::textlayout::ParagraphBuilder>,
    pub paragraphs:      HashMap<u32, skia_safe::textlayout::Paragraph>,
    pub font_collection: skia_safe::textlayout::FontCollection,
    pub next_para_id:    u32,
    // Task 28 Path D: host-side raster surfaces backing Compose's
    // org.jetbrains.skia.Canvas(bitmap) — short-lived raster targets for
    // vector-icon rasterization. Snapshots land in `images` via the same
    // next_blob_id counter as other images. The LRU vec tracks insertion
    // order so a soft cap can evict the oldest surface when Compose
    // abandons it (Compose doesn't call Canvas.close on wasi).
    bitmap_canvases:            HashMap<u32, skia_safe::Surface>,
    bitmap_canvas_lru:          std::collections::VecDeque<u32>,
    next_bitmap_canvas_id:      u32,
    /// Holds the result of the last `prepare-rects-for-range` call so the
    /// guest can pull rect fields out via indexed getters (avoiding the
    /// need for `list<f32>` return marshaling in the WIT bindings). One
    /// renderer-wide slot is sufficient: the guest always reads the cache
    /// in the same WIT call burst, never interleaved with another prepare.
    pub para_rect_cache: Vec<skia_safe::textlayout::TextBox>,
    /// Task 50 — per-line metrics cache. Populated by
    /// `prepare_line_metrics`; read by the 13 `get_cached_line_*` getters.
    /// Fixes the multi-line cursor-render bug: without this, skiko-wasi's
    /// `Paragraph.lineMetrics` returns an empty array, and Compose's
    /// `SkiaParagraph.getCursorRect` falls back to line 0 metrics for any
    /// offset → cursor blinks on line 1 regardless of selection position.
    ///
    /// Copies the numeric fields out of `skia_safe::textlayout::LineMetrics`
    /// so we don't have to thread the source paragraph's lifetime.
    pub para_line_metrics_cache: Vec<CachedLineMetrics>,
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
                text_blobs:       HashMap::new(),
                multi_blob_cache: HashMap::new(),
                text_blob_runs:   Vec::new(),
                images:           HashMap::new(),
                shader_cache:     HashMap::new(),
                next_blob_id:     1,
                next_shader_id:   1,
                recorders:        HashMap::new(),
                pictures:         HashMap::new(),
                recording_stack:  Vec::new(),
                next_recorder_id: 1,
                next_picture_id:  1,
                drawables:        HashMap::new(),
                next_drawable_id: 1,
                typeface_cache:   HashMap::new(),
                guest_typefaces:  HashMap::new(),
                text_image_cache: HashMap::new(),
                text_image_keys:  std::collections::VecDeque::with_capacity(TEXT_IMAGE_CACHE_CAP),
                para_builders:    HashMap::new(),
                paragraphs:       HashMap::new(),
                font_collection:  Self::make_font_collection(),
                next_para_id:     1,
                para_rect_cache:  Vec::new(),
                para_line_metrics_cache: Vec::new(),
                bitmap_canvases:       HashMap::new(),
                bitmap_canvas_lru:     std::collections::VecDeque::with_capacity(128),
                next_bitmap_canvas_id: 1,
            });
        }

        #[cfg(not(target_os = "android"))]
        {
            let surface = skia_safe::surfaces::raster_n32_premul(
                (size.width as i32, size.height as i32)
            ).ok_or_else(|| anyhow::anyhow!("raster surface failed"))?;
            Ok(Self {
                surface, width: size.width, height: size.height,
                logical_width: size.width, logical_height: size.height,
                inset_top: 0, inset_bottom: 0, keyboard_base_px: 0,
                base_matrix: skia_safe::Matrix::new_identity(),
                current_orient: 0,
                text_blobs:       HashMap::new(),
                multi_blob_cache: HashMap::new(),
                text_blob_runs:   Vec::new(),
                images:           HashMap::new(),
                shader_cache:     HashMap::new(),
                next_blob_id:     1,
                next_shader_id:   1,
                recorders:        HashMap::new(),
                pictures:         HashMap::new(),
                recording_stack:  Vec::new(),
                next_recorder_id: 1,
                next_picture_id:  1,
                drawables:        HashMap::new(),
                next_drawable_id: 1,
                typeface_cache:   HashMap::new(),
                guest_typefaces:  HashMap::new(),
                text_image_cache: HashMap::new(),
                text_image_keys:  std::collections::VecDeque::with_capacity(TEXT_IMAGE_CACHE_CAP),
                para_builders:    HashMap::new(),
                paragraphs:       HashMap::new(),
                font_collection:  Self::make_font_collection(),
                next_para_id:     1,
                para_rect_cache:  Vec::new(),
                para_line_metrics_cache: Vec::new(),
                bitmap_canvases:       HashMap::new(),
                bitmap_canvas_lru:     std::collections::VecDeque::with_capacity(128),
                next_bitmap_canvas_id: 1,
            })
        }
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
            text_blobs:       HashMap::new(),
            multi_blob_cache: HashMap::new(),
            text_blob_runs:   Vec::new(),
            images:           HashMap::new(),
            shader_cache:     HashMap::new(),
            next_blob_id:     1,
            next_shader_id:   1,
            recorders:        HashMap::new(),
            pictures:         HashMap::new(),
            recording_stack:  Vec::new(),
            next_recorder_id: 1,
            next_picture_id:  1,
            drawables:        HashMap::new(),
            next_drawable_id: 1,
            typeface_cache:   HashMap::new(),
            guest_typefaces:  HashMap::new(),
            text_image_cache: HashMap::new(),
            text_image_keys:  std::collections::VecDeque::with_capacity(TEXT_IMAGE_CACHE_CAP),
            para_builders:    HashMap::new(),
            paragraphs:       HashMap::new(),
            font_collection:  Self::make_font_collection(),
            next_para_id:     1,
            para_rect_cache:  Vec::new(),
            para_line_metrics_cache: Vec::new(),
            bitmap_canvases:       HashMap::new(),
            bitmap_canvas_lru:     std::collections::VecDeque::with_capacity(128),
            next_bitmap_canvas_id: 1,
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
        self.text_blobs       = std::mem::take(&mut old.text_blobs);
        self.multi_blob_cache = std::mem::take(&mut old.multi_blob_cache);
        self.text_blob_runs   = std::mem::take(&mut old.text_blob_runs);
        self.shader_cache     = std::mem::take(&mut old.shader_cache);
        self.next_blob_id     = old.next_blob_id;
        self.next_shader_id   = old.next_shader_id;
        self.recorders        = std::mem::take(&mut old.recorders);
        self.pictures         = std::mem::take(&mut old.pictures);
        self.recording_stack  = std::mem::take(&mut old.recording_stack);
        self.next_recorder_id = old.next_recorder_id;
        self.next_picture_id  = old.next_picture_id;
        self.drawables        = std::mem::take(&mut old.drawables);
        self.next_drawable_id = old.next_drawable_id;
        self.typeface_cache   = std::mem::take(&mut old.typeface_cache);
        self.para_builders    = std::mem::take(&mut old.para_builders);
        self.paragraphs       = std::mem::take(&mut old.paragraphs);
        self.next_para_id     = old.next_para_id;
        // bitmap_canvases hold a raster Surface (CPU-only), safe to inherit.
        self.bitmap_canvases       = std::mem::take(&mut old.bitmap_canvases);
        self.bitmap_canvas_lru     = std::mem::take(&mut old.bitmap_canvas_lru);
        self.next_bitmap_canvas_id = old.next_bitmap_canvas_id;
        // font_collection holds a default-FontMgr; keep the freshly built
        // one to be safe (cheap to recreate).
    }

    #[cfg(target_os = "android")]
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

    fn make_font_collection() -> skia_safe::textlayout::FontCollection {
        let mut fc = skia_safe::textlayout::FontCollection::new();
        let mgr = skia_safe::FontMgr::new();
        fc.set_default_font_manager(mgr, None);
        fc
    }

    pub fn canvas(&mut self) -> &Canvas {
        // If a picture recording is active, route draw calls into the
        // recorder's canvas instead of the screen surface. The recorder owns
        // an internal Canvas during begin_recording → finish; we look it up
        // by the top-of-stack recorder id.
        if let Some(&rid) = self.recording_stack.last() {
            if let Some(rec) = self.recorders.get_mut(&rid) {
                if let Some(c) = rec.recording_canvas() {
                    // Lifetime extension: skia-safe returns &Canvas borrowed
                    // from `self` through the recorder; that's the same
                    // shape callers expect from `surface.canvas()`. Safe so
                    // long as callers don't hold the borrow across another
                    // `&mut self` call (mirrors the surface.canvas() rules).
                    return unsafe { &*(c as *const skia_safe::Canvas) };
                }
            }
        }
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
            if let Some(s) = skia_safe::surfaces::raster_n32_premul(
                (w as i32, h as i32)) {
                self.surface = s;
            }
        }
    }

    pub fn draw_test_frame(&mut self) {
        #[cfg(target_os = "android")]
        self.egl.make_current();
        {
            let c = self.surface.canvas();
            c.clear(Color::from_argb(255, 10, 20, 60));
            c.draw_rect(
                Rect::from_xywh(50.0, 50.0, 200.0, 100.0),
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

    /// CPU-rasterise the blob then blit to the GPU canvas, caching the
    /// SkImage so identical (blob content, paint colour) draws reuse the
    /// same GPU texture. The content hash is computed in `create_text_blob`
    /// from text + font params — distinct content always gets distinct keys
    /// even when bounds collide.
    fn blit_text_blob_cached(
        &mut self,
        blob: &skia_safe::TextBlob,
        content_hash: u64,
        x: f32, y: f32,
        paint: &Paint,
    ) {
        let key = (content_hash, paint_cache_key(paint));

        if !self.text_image_cache.contains_key(&key) {
            let entry = match rasterize_text_blob(blob, paint) {
                Some(e) => e,
                None    => return,
            };
            if self.text_image_keys.len() >= TEXT_IMAGE_CACHE_CAP {
                if let Some(old) = self.text_image_keys.pop_front() {
                    self.text_image_cache.remove(&old);
                }
            }
            self.text_image_cache.insert(key, entry);
            self.text_image_keys.push_back(key);
        }
        // Use canvas() helper so the image lands on the recording canvas
        // when a Picture is being recorded, not the screen surface.
        let image = self.text_image_cache.get(&key).unwrap().image.clone();
        let ox = self.text_image_cache.get(&key).unwrap().offset_x;
        let oy = self.text_image_cache.get(&key).unwrap().offset_y;
        self.canvas().draw_image(&image, (x + ox, y + oy), None);
    }

    pub fn draw_paragraph(&mut self, id: u32, x: f32, y: f32) {
        // Skia's Paragraph (RefHandle) isn't Clone. We need to paint via
        // self.canvas() which respects the recording stack. Hold the paragraph
        // and the recorder-or-surface canvas as raw pointers briefly so the
        // borrow checker doesn't see overlapping borrows. Safe because we
        // never re-enter `self` during the paint call.
        let para_ptr: *const skia_safe::textlayout::Paragraph =
            match self.paragraphs.get(&id) {
                Some(p) => p as *const _,
                None => return,
            };
        let canvas_ptr: *const Canvas = self.canvas() as *const Canvas;
        unsafe { (&*para_ptr).paint(&*canvas_ptr, (x, y)); }
    }
    #[allow(dead_code)]
    fn _old_draw_paragraph(&mut self, id: u32, x: f32, y: f32) {
        if let Some(p) = self.paragraphs.get(&id) {
            p.paint(self.surface.canvas(), (x, y));
        }
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

impl crate::bindings::my::skiko_gfx::canvas::Host for crate::HostState {

    // Guests lay out against the LOGICAL size — equal to the physical GL
    // surface except in the standalone rotated mode (portrait logical,
    // landscape physical).
    fn surface_width (&mut self) -> u32 { self.renderer.logical_width  }
    fn surface_height(&mut self) -> u32 { self.renderer.logical_height }

    fn begin_frame(&mut self) {
        #[cfg(target_os = "android")]
        {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::info!("begin_frame: logical={}x{} physical={}x{}",
                    self.renderer.logical_width, self.renderer.logical_height,
                    self.renderer.width, self.renderer.height);
            }
            self.renderer.egl.make_current();
        }
        // Clear to opaque black, then re-apply the base transform as this
        // frame's canvas root (identity normally, a 90° rotation in the
        // standalone rotated mode). The clear gives unpainted regions a
        // defined opaque value — the standalone SurfaceControl is marked
        // opaque, so without it those pixels would be garbage rather than
        // black, and on the NativeActivity path it's a cheap redundancy.
        let base = self.renderer.base_matrix;
        let c = self.renderer.canvas();
        c.reset_matrix();
        c.clear(skia_safe::Color::BLACK);
        c.concat(&base);
    }

    fn end_frame(&mut self) {
        self.renderer.flush_and_swap();
    }

    fn save(&mut self)    { self.renderer.canvas().save(); }
    fn restore(&mut self) { self.renderer.canvas().restore(); }

    fn save_layer(&mut self, x: f32, y: f32, w: f32, h: f32, has_bounds: bool, alpha: u8) {
        if has_bounds {
            self.renderer.canvas()
                .save_layer_alpha(Some(Rect::from_xywh(x, y, w, h)), alpha as u32);
        } else {
            self.renderer.canvas().save_layer_alpha(None, alpha as u32);
        }
    }

    fn translate(&mut self, dx: f32, dy: f32) { self.renderer.canvas().translate((dx, dy)); }
    fn scale    (&mut self, sx: f32, sy: f32) { self.renderer.canvas().scale((sx, sy));     }
    fn rotate   (&mut self, deg: f32)         { self.renderer.canvas().rotate(deg, None);   }
    fn skew     (&mut self, sx: f32, sy: f32) { self.renderer.canvas().skew((sx, sy));      }

    fn concat(&mut self, a: f32, b: f32, c: f32,
                         d: f32, e: f32, f: f32,
                         g: f32, h: f32, i: f32) {
        let m = skia_safe::Matrix::new_all(a, b, c, d, e, f, g, h, i);
        self.renderer.canvas().concat(&m);
    }

    fn reset_matrix(&mut self) {
        // Reset to the base transform, not bare identity — otherwise the
        // standalone rotated-mode 90° base would be lost mid-frame.
        let base = self.renderer.base_matrix;
        let c = self.renderer.canvas();
        c.reset_matrix();
        c.concat(&base);
    }

    fn clip_rect(&mut self, x: f32, y: f32, w: f32, h: f32, anti_alias: bool) {
        self.renderer.canvas().clip_rect(
            Rect::from_xywh(x, y, w, h),
            Some(skia_safe::ClipOp::Intersect),
            Some(anti_alias),
        );
    }

    fn clip_rrect(&mut self, x: f32, y: f32, w: f32, h: f32,
                   rx: f32, ry: f32, anti_alias: bool) {
        let rr = RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), rx, ry);
        self.renderer.canvas().clip_rrect(
            rr, Some(skia_safe::ClipOp::Intersect), Some(anti_alias),
        );
    }

    fn clip_path(&mut self, path_data: Vec<u8>, anti_alias: bool) {
        let s = String::from_utf8_lossy(&path_data);
        if let Some(p) = skia_safe::Path::from_svg(&*s) {
            self.renderer.canvas().clip_path(
                &p, Some(skia_safe::ClipOp::Intersect), Some(anti_alias),
            );
        }
    }

    fn clear(&mut self, argb: u32) {
        self.renderer.canvas().clear(Color::new(argb));
    }

    fn draw_paint(&mut self, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_paint(&paint);
    }

    fn draw_rect(&mut self, x: f32, y: f32, w: f32, h: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_rect(Rect::from_xywh(x, y, w, h), &paint);
    }

    fn draw_rrect(&mut self, x: f32, y: f32, w: f32, h: f32,
                   rx: f32, ry: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        let rr    = RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), rx, ry);
        self.renderer.canvas().draw_rrect(rr, &paint);
    }

    fn draw_drrect(&mut self,
        ox: f32, oy: f32, ow: f32, oh: f32, orx: f32, ory: f32,
        ix: f32, iy: f32, iw: f32, ih: f32, irx: f32, iry: f32,
        p: PaintAttrs,
    ) {
        let paint = make_paint_full(&p, &self.renderer);
        let outer = RRect::new_rect_xy(Rect::from_xywh(ox, oy, ow, oh), orx, ory);
        let inner = RRect::new_rect_xy(Rect::from_xywh(ix, iy, iw, ih), irx, iry);
        self.renderer.canvas().draw_drrect(outer, inner, &paint);
    }

    fn draw_oval(&mut self, x: f32, y: f32, w: f32, h: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_oval(Rect::from_xywh(x, y, w, h), &paint);
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_line((x0, y0), (x1, y1), &paint);
    }

    fn draw_arc(&mut self, x: f32, y: f32, w: f32, h: f32,
                start_angle: f32, sweep_angle: f32, include_center: bool,
                p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_arc(
            Rect::from_xywh(x, y, w, h),
            start_angle, sweep_angle, include_center, &paint,
        );
    }

    fn draw_path(&mut self, path_data: Vec<u8>, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        let s = String::from_utf8_lossy(&path_data);
        if let Some(path) = skia_safe::Path::from_svg(&*s) {
            self.renderer.canvas().draw_path(&path, &paint);
        }
    }

    /// Boolean-combine two SVG paths. op: 0=difference 1=intersect
    /// 2=union 3=xor 4=reverse-difference. Returns the result as an
    /// SVG path string; empty on parse failure or empty result.
    fn path_combine(&mut self, path_a: Vec<u8>, path_b: Vec<u8>, op: u32) -> Vec<u8> {
        let sa = String::from_utf8_lossy(&path_a);
        let sb = String::from_utf8_lossy(&path_b);
        let (pa, pb) = match (
            skia_safe::Path::from_svg(&*sa),
            skia_safe::Path::from_svg(&*sb),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return Vec::new(),
        };
        let skop = match op {
            0 => skia_safe::PathOp::Difference,
            1 => skia_safe::PathOp::Intersect,
            2 => skia_safe::PathOp::Union,
            3 => skia_safe::PathOp::XOR,
            4 => skia_safe::PathOp::ReverseDifference,
            _ => skia_safe::PathOp::Difference,
        };
        match pa.op(&pb, skop) {
            Some(result) => {
                // skia path-op results use the EvenOdd fill rule, but an
                // SVG path string carries no fill rule — re-parsing
                // defaults to Winding and would re-fill the hole (e.g.
                // collapsing Modifier.border's ring into a solid fill).
                // Convert to an equivalent Winding-fill path (reverses
                // inner contours) so the SVG round-trip renders right.
                let winding = result.as_winding().unwrap_or(result);
                winding.to_svg().into_bytes()
            }
            None => Vec::new(),
        }
    }

    // ── text blobs ────────────────────────────────────────────────────────

    fn create_text_blob(&mut self, text: Vec<u8>, font_family: Vec<u8>,
                         size: f32, weight: u32, italic: bool) -> u32 {
        let text_str   = String::from_utf8_lossy(&text).into_owned();
        let family_str = String::from_utf8_lossy(&font_family).into_owned();
        let bold       = weight >= 600;
        let tf         = self.renderer.get_typeface(&family_str, bold, italic);
        let mut font   = Font::new(tf, size);
        font.set_edging(skia_safe::font::Edging::AntiAlias);
        font.set_subpixel(false);
        let blob = shape_text_fallback(&text_str, &font);
        let content_hash = text_content_hash(&text_str, &family_str, size, weight, italic);
        static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ONCE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let b = blob.as_ref().map(|b| b.bounds());
            log::info!("create_text_blob first: size={size} blob={} bounds={b:?}",
                blob.is_some());
        }
        let id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = id.wrapping_add(1).max(1);
        if let Some(b) = blob {
            self.renderer.text_blobs.insert(id, (b, content_hash));
        }
        id
    }

    fn draw_text_blob(&mut self, id: u32, x: f32, y: f32, p: PaintAttrs) {
        // Multi-run path: each run has its own (bx, by) offset + content hash.
        if let Some(runs) = self.renderer.multi_blob_cache.get(&id).cloned() {
            let paint = make_paint_full(&p, &self.renderer);
            for (blob, bx, by, h) in &runs {
                self.renderer.blit_text_blob_cached(blob, *h, x + bx, y + by, &paint);
            }
            return;
        }
        // Single-run path.
        let Some((blob, content_hash)) = self.renderer.text_blobs.get(&id).cloned() else { return };
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.blit_text_blob_cached(&blob, content_hash, x, y, &paint);
    }

    fn drop_text_blob(&mut self, id: u32) {
        self.renderer.text_blobs.remove(&id);
        self.renderer.multi_blob_cache.remove(&id);
    }

    fn begin_text_blob(&mut self) {
        self.renderer.text_blob_runs.clear();
    }

    fn add_text_run(&mut self, text: Vec<u8>, font_family: Vec<u8>,
                    size: f32, weight: u32, italic: bool, x: f32, y: f32) {
        self.renderer.text_blob_runs.push(TextBlobRun {
            text:   String::from_utf8_lossy(&text).into_owned(),
            family: String::from_utf8_lossy(&font_family).into_owned(),
            size, weight, italic, x, y,
        });
    }

    fn end_text_blob(&mut self) -> u32 {
        let runs = std::mem::take(&mut self.renderer.text_blob_runs);
        let id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = id.wrapping_add(1).max(1);
        let blobs: Vec<(skia_safe::TextBlob, f32, f32, u64)> = runs.iter().filter_map(|r| {
            let tf = self.renderer.get_typeface(&r.family, r.weight >= 600, r.italic);
            let mut font = Font::new(tf, r.size);
            font.set_edging(skia_safe::font::Edging::AntiAlias);
            font.set_subpixel(false);
            let h = text_content_hash(&r.text, &r.family, r.size, r.weight, r.italic);
            shape_text_fallback(&r.text, &font).map(|b| (b, r.x, r.y, h))
        }).collect();
        self.renderer.multi_blob_cache.insert(id, blobs);
        id
    }

    // ── images ────────────────────────────────────────────────────────────

    fn create_image(&mut self, width: u32, height: u32, pixels: Vec<u8>) -> u32 {
        let info = skia_safe::ImageInfo::new(
            (width as i32, height as i32),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        let data = skia_safe::Data::new_copy(&pixels);
        let id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = id.wrapping_add(1).max(1);
        if let Some(img) = skia_safe::images::raster_from_data(
            &info, data, (width * 4) as usize,
        ) {
            self.renderer.images.insert(id, img);
        }
        id
    }

    fn draw_image(&mut self, id: u32, x: f32, y: f32, alpha: u8) {
        let Some(img) = self.renderer.images.get(&id).cloned() else { return };
        let mut p = Paint::default();
        p.set_alpha(alpha);
        self.renderer.canvas().draw_image(&img, (x, y), Some(&p));
    }

    fn draw_image_rect(&mut self, image_id: u32,
                       src_x: f32, src_y: f32, src_w: f32, src_h: f32,
                       dst_x: f32, dst_y: f32, dst_w: f32, dst_h: f32,
                       p: PaintAttrs) {
        let Some(img) = self.renderer.images.get(&image_id).cloned() else { return };
        let paint = make_paint_full(&p, &self.renderer);
        let src = Rect::from_xywh(src_x, src_y, src_w, src_h);
        let dst = Rect::from_xywh(dst_x, dst_y, dst_w, dst_h);
        self.renderer.canvas().draw_image_rect(
            &img,
            Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
            dst, &paint,
        );
    }

    fn drop_image(&mut self, id: u32) {
        self.renderer.images.remove(&id);
    }

    fn create_image_from_encoded(&mut self, bytes: Vec<u8>) -> u32 {
        let data = skia_safe::Data::new_copy(&bytes);
        let Some(img) = skia_safe::Image::from_encoded(data) else { return 0 };
        let id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = id.wrapping_add(1).max(1);
        self.renderer.images.insert(id, img);
        id
    }

    fn get_image_width(&mut self, image_id: u32) -> u32 {
        self.renderer.images.get(&image_id).map(|i| i.width() as u32).unwrap_or(0)
    }

    fn get_image_height(&mut self, image_id: u32) -> u32 {
        self.renderer.images.get(&image_id).map(|i| i.height() as u32).unwrap_or(0)
    }

    // ── shaders ───────────────────────────────────────────────────────────

    fn create_linear_gradient(&mut self,
        x0: f32, y0: f32, x1: f32, y1: f32,
        colors: Vec<u32>, stops: Vec<f32>, tile_mode: u8,
    ) -> u32 {
        let p0 = skia_safe::Point::new(x0, y0);
        let p1 = skia_safe::Point::new(x1, y1);
        let cols: Vec<skia_safe::Color> = colors.iter().map(|&c| Color::new(c)).collect();
        let stops_opt: Option<&[f32]> = if stops.is_empty() { None } else { Some(&stops) };
        let mode = tile_mode_from_u8(tile_mode);
        let shader = skia_safe::gradient_shader::linear(
            (p0, p1), cols.as_slice(), stops_opt, mode, None, None,
        );
        if let Some(s) = shader {
            let id = self.renderer.next_shader_id;
            self.renderer.next_shader_id = id.wrapping_add(1).max(1);
            self.renderer.shader_cache.insert(id, s);
            id
        } else { 0 }
    }

    fn create_radial_gradient(&mut self,
        cx: f32, cy: f32, radius: f32,
        colors: Vec<u32>, stops: Vec<f32>, tile_mode: u8,
    ) -> u32 {
        let center = skia_safe::Point::new(cx, cy);
        let cols: Vec<skia_safe::Color> = colors.iter().map(|&c| Color::new(c)).collect();
        let stops_opt: Option<&[f32]> = if stops.is_empty() { None } else { Some(&stops) };
        let mode = tile_mode_from_u8(tile_mode);
        let shader = skia_safe::gradient_shader::radial(
            center, radius, cols.as_slice(), stops_opt, mode, None, None,
        );
        if let Some(s) = shader {
            let id = self.renderer.next_shader_id;
            self.renderer.next_shader_id = id.wrapping_add(1).max(1);
            self.renderer.shader_cache.insert(id, s);
            id
        } else { 0 }
    }

    fn create_sweep_gradient(&mut self,
        cx: f32, cy: f32, start_angle: f32, end_angle: f32,
        colors: Vec<u32>, stops: Vec<f32>, tile_mode: u8,
    ) -> u32 {
        let center = skia_safe::Point::new(cx, cy);
        let cols: Vec<skia_safe::Color> = colors.iter().map(|&c| Color::new(c)).collect();
        let stops_opt: Option<&[f32]> = if stops.is_empty() { None } else { Some(&stops) };
        let mode = tile_mode_from_u8(tile_mode);
        let shader = skia_safe::gradient_shader::sweep(
            center, cols.as_slice(), stops_opt, mode,
            Some((start_angle, end_angle)), None, None,
        );
        if let Some(s) = shader {
            let id = self.renderer.next_shader_id;
            self.renderer.next_shader_id = id.wrapping_add(1).max(1);
            self.renderer.shader_cache.insert(id, s);
            id
        } else { 0 }
    }

    fn create_image_shader(&mut self,
        image_id: u32, tile_x: u8, tile_y: u8,
        sampling: crate::bindings::my::skiko_gfx::canvas::SamplingOptions,
        local_matrix: crate::bindings::my::skiko_gfx::canvas::Matrix3x3,
    ) -> u32 {
        let Some(img) = self.renderer.images.get(&image_id).cloned() else { return 0 };
        let tmx = tile_mode_from_u8(tile_x);
        let tmy = tile_mode_from_u8(tile_y);
        let sampling_opts = sampling_options_from_wit(&sampling);
        let matrix = matrix3x3_from_wit(&local_matrix);
        let shader = img.to_shader(Some((tmx, tmy)), sampling_opts, Some(&matrix));
        if let Some(s) = shader {
            let id = self.renderer.next_shader_id;
            self.renderer.next_shader_id = id.wrapping_add(1).max(1);
            self.renderer.shader_cache.insert(id, s);
            id
        } else { 0 }
    }

    fn create_blend_shader(&mut self,
        blend_mode: BlendMode, shader1_id: u32, shader2_id: u32,
    ) -> u32 {
        let Some(s1) = self.renderer.shader_cache.get(&shader1_id).cloned() else { return 0 };
        let Some(s2) = self.renderer.shader_cache.get(&shader2_id).cloned() else { return 0 };
        let mode = blend_mode_from_wit(blend_mode);
        let shader = skia_safe::shaders::blend(mode, s1, s2);
        let id = self.renderer.next_shader_id;
        self.renderer.next_shader_id = id.wrapping_add(1).max(1);
        self.renderer.shader_cache.insert(id, shader);
        id
    }

    fn drop_shader(&mut self, id: u32) {
        self.renderer.shader_cache.remove(&id);
    }

    // ── picture recording (Tier A skia shim) ──────────────────────────────

    fn create_picture_recorder(&mut self) -> u32 {
        let id = self.renderer.next_recorder_id;
        self.renderer.next_recorder_id = id.wrapping_add(1).max(1);
        self.renderer.recorders.insert(id, skia_safe::PictureRecorder::new());
        id
    }

    fn begin_picture_recording(&mut self, recorder_id: u32,
                                left: f32, top: f32, right: f32, bottom: f32,
                                with_rtree: bool) {
        // with_rtree=true asks skia to build a bounding-box hierarchy (RTree)
        // for the picture so partial-replay (drawing the picture clipped to
        // a sub-rect) can skip culled commands. Compose's LegacyRenderNodeLayer
        // / RecordDrawRectRenderDecorator both want this when measuring draw
        // bounds. skia-safe 0.9x exposes the choice as a bool.
        let bounds = Rect::from_ltrb(left, top, right, bottom);
        if let Some(rec) = self.renderer.recorders.get_mut(&recorder_id) {
            let _ = rec.begin_recording(bounds, with_rtree);
            self.renderer.recording_stack.push(recorder_id);
        }
    }

    fn finish_recording_as_picture(&mut self, recorder_id: u32) -> u32 {
        // Pop the stack first so subsequent canvas() lookups don't try to
        // borrow the recorder we're about to consume.
        if let Some(pos) = self.renderer.recording_stack.iter().rposition(|&r| r == recorder_id) {
            self.renderer.recording_stack.remove(pos);
        }
        let pic = self.renderer.recorders.get_mut(&recorder_id)
            .and_then(|r| r.finish_recording_as_picture(None));
        match pic {
            Some(p) => {
                let id = self.renderer.next_picture_id;
                self.renderer.next_picture_id = id.wrapping_add(1).max(1);
                self.renderer.pictures.insert(id, p);
                id
            }
            None => 0,
        }
    }

    fn close_picture_recorder(&mut self, recorder_id: u32) {
        // If still on the recording stack, pop it.
        if let Some(pos) = self.renderer.recording_stack.iter().rposition(|&r| r == recorder_id) {
            self.renderer.recording_stack.remove(pos);
        }
        self.renderer.recorders.remove(&recorder_id);
    }

    fn draw_picture(&mut self, picture_id: u32) {
        let pic = self.renderer.pictures.get(&picture_id).cloned();
        if let Some(pic) = pic {
            self.renderer.canvas().draw_picture(&pic, None, None);
        }
    }

    fn drop_picture(&mut self, picture_id: u32) {
        self.renderer.pictures.remove(&picture_id);
    }

    // ── WasiDrawable (deferred-replay) ─────────────────────────────────────

    fn create_drawable(&mut self) -> u32 {
        let id = self.renderer.next_drawable_id;
        self.renderer.next_drawable_id = id.wrapping_add(1).max(1);
        self.renderer.drawables.insert(id, WasiDrawable::new());
        id
    }

    fn set_drawable_from_recorder(&mut self, drawable_id: u32, recorder_id: u32) {
        // Pop the recorder off the recording stack if present (matches
        // finish_recording_as_picture). This pairs the begin/end bracket
        // so subsequent draw ops route to the screen surface again.
        if let Some(pos) = self.renderer.recording_stack.iter()
            .rposition(|&r| r == recorder_id)
        {
            self.renderer.recording_stack.remove(pos);
        }
        // finish_recording_as_drawable detaches the SkRecord + drawable
        // list (no picture snapshot is taken), so embedded child
        // drawables stay live across parent re-records.
        let inner: Option<skia_safe::Drawable> = self.renderer.recorders
            .get_mut(&recorder_id)
            .and_then(|r| r.finish_recording_as_drawable());
        let inner_ptr: *mut std::os::raw::c_void = match inner.as_ref() {
            Some(d) => handle_to_native_ptr(d as *const skia_safe::Drawable),
            None    => std::ptr::null_mut(),
        };
        if let Some(outer) = self.renderer.drawables.get(&drawable_id) {
            unsafe {
                wasi_drawable_ffi::wasi_drawable_set_inner(outer.as_ptr(), inner_ptr);
            }
        }
        // `inner` drops here, releasing its handle ref; the outer's
        // setInner already bumped the underlying SkDrawable's refcount
        // via sk_ref_sp, so the drawable stays alive.
        drop(inner);
    }

    fn set_drawable_bounds(&mut self, drawable_id: u32,
                           l: f32, t: f32, r: f32, b: f32) {
        if let Some(d) = self.renderer.drawables.get_mut(&drawable_id) {
            d.set_bounds(l, t, r, b);
        }
    }

    fn draw_drawable(&mut self, drawable_id: u32) {
        // We dispatch via our own C FFI (wasi_canvas_draw_drawable) rather
        // than skia_safe::Canvas::draw_drawable because Drawable::from_ptr
        // is pub(crate) in skia-safe. skia_safe::Canvas is
        // `pub struct Canvas(UnsafeCell<SkCanvas>)` — a transparent
        // single-field wrapper around SkCanvas, so its first byte coincides
        // with the SkCanvas* and casting through *mut c_void is sound.
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        if raw_d.is_null() { return; }
        let canvas: &skia_safe::Canvas = self.renderer.canvas();
        let canvas_ptr = canvas as *const skia_safe::Canvas as *mut std::os::raw::c_void;
        unsafe { wasi_drawable_ffi::wasi_canvas_draw_drawable(canvas_ptr, raw_d); }
    }

    fn drop_drawable(&mut self, drawable_id: u32) {
        self.renderer.drawables.remove(&drawable_id);
    }

    fn set_drawable_transform(
        &mut self, drawable_id: u32,
        layer_x: f32, layer_y: f32,
        translation_x: f32, translation_y: f32,
        scale_x: f32, scale_y: f32,
        rotation_z: f32,
        pivot_x: f32, pivot_y: f32,
        alpha: f32,
    ) {
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        unsafe { wasi_drawable_ffi::wasi_drawable_set_transform(
            raw_d, layer_x, layer_y, translation_x, translation_y,
            scale_x, scale_y, rotation_z, pivot_x, pivot_y, alpha,
        ); }
    }

    fn set_drawable_clip_rect(
        &mut self, drawable_id: u32,
        l: f32, t: f32, r: f32, b: f32, antialias: bool,
    ) {
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        unsafe { wasi_drawable_ffi::wasi_drawable_set_clip_rect(
            raw_d, l, t, r, b, antialias,
        ); }
    }

    fn set_drawable_clip_rrect(
        &mut self, drawable_id: u32,
        l: f32, t: f32, r: f32, b: f32, radii: Vec<f32>, antialias: bool,
    ) {
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        // C++ side expects exactly 8 floats (4 corners × (rx, ry)). Skiko's
        // RRect.makeComplexLTRB also stores them in upper-left → upper-right
        // → lower-right → lower-left order matching SkRRect::setRectRadii.
        if radii.len() < 8 { return; }
        unsafe { wasi_drawable_ffi::wasi_drawable_set_clip_rrect(
            raw_d, l, t, r, b, radii.as_ptr(), antialias,
        ); }
    }

    fn clear_drawable_clip(&mut self, drawable_id: u32) {
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        unsafe { wasi_drawable_ffi::wasi_drawable_clear_clip(raw_d); }
    }

    fn set_drawable_shadow_elevation(&mut self, drawable_id: u32, elevation: f32) {
        let raw_d = match self.renderer.drawables.get(&drawable_id) {
            Some(d) => d.as_ptr(),
            None    => return,
        };
        unsafe { wasi_drawable_ffi::wasi_drawable_set_shadow_elevation(raw_d, elevation); }
    }

    // ── bitmap canvases (task 28, Path D) ─────────────────────────────────

    fn create_bitmap_canvas(&mut self, width: u32, height: u32) -> u32 {
        // Compose may briefly construct a 0×0 ImageBitmap as scratch state.
        // Skia rejects zero-sized surfaces; clamp to 1×1 to keep the id
        // pipeline live (snapshot of an empty canvas yields a 1×1 image,
        // which downstream `drawImage` callers tolerate).
        let w = width.max(1) as i32;
        let h = height.max(1) as i32;
        let Some(surface) = skia_safe::surfaces::raster_n32_premul((w, h))
        else {
            return 0;
        };
        // Soft cap: when the LRU is at capacity, evict the oldest surface
        // before inserting. Compose never calls Canvas.close on wasi, so
        // every new Canvas(bitmap) leaks until either we evict it or
        // process exit. Cap chosen to comfortably hold the steady-state
        // set of visible vector icons (Material3 calendar fits in ~40);
        // bump if a future widget set needs more.
        const BITMAP_CANVAS_CAP: usize = 128;
        while self.renderer.bitmap_canvases.len() >= BITMAP_CANVAS_CAP {
            if let Some(old_id) = self.renderer.bitmap_canvas_lru.pop_front() {
                self.renderer.bitmap_canvases.remove(&old_id);
            } else {
                break;
            }
        }
        let id = self.renderer.next_bitmap_canvas_id;
        self.renderer.next_bitmap_canvas_id = id.wrapping_add(1).max(1);
        self.renderer.bitmap_canvases.insert(id, surface);
        self.renderer.bitmap_canvas_lru.push_back(id);
        id
    }

    fn drop_bitmap_canvas(&mut self, id: u32) {
        if self.renderer.bitmap_canvases.remove(&id).is_some() {
            self.renderer.bitmap_canvas_lru.retain(|&i| i != id);
        }
    }

    fn bitmap_canvas_snapshot(&mut self, id: u32) -> u32 {
        let img = match self.renderer.bitmap_canvases.get_mut(&id) {
            Some(s) => s.image_snapshot(),
            None    => return 0,
        };
        let img_id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = img_id.wrapping_add(1).max(1);
        self.renderer.images.insert(img_id, img);
        img_id
    }

    fn bc_save(&mut self, id: u32) -> u32 {
        bc_canvas_mut(&mut self.renderer, id)
            .map(|c| c.save() as u32)
            .unwrap_or(0)
    }

    fn bc_save_layer(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                     has_bounds: bool, paint: PaintAttrs) -> u32 {
        let p = make_paint_full(&paint, &self.renderer);
        let Some(c) = bc_canvas_mut(&mut self.renderer, id) else { return 0 };
        let layer = skia_safe::canvas::SaveLayerRec::default().paint(&p);
        let bounds = Rect::from_xywh(x, y, w, h);
        let layer = if has_bounds { layer.bounds(&bounds) } else { layer };
        c.save_layer(&layer) as u32
    }

    fn bc_restore(&mut self, id: u32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.restore(); }
    }

    fn bc_restore_to_count(&mut self, id: u32, save_count: u32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.restore_to_count(save_count as usize);
        }
    }

    fn bc_translate(&mut self, id: u32, dx: f32, dy: f32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.translate((dx, dy)); }
    }

    fn bc_scale(&mut self, id: u32, sx: f32, sy: f32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.scale((sx, sy)); }
    }

    fn bc_rotate(&mut self, id: u32, degrees: f32, px: f32, py: f32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.rotate(degrees, Some(skia_safe::Point::new(px, py)));
        }
    }

    fn bc_skew(&mut self, id: u32, sx: f32, sy: f32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.skew((sx, sy)); }
    }

    fn bc_concat(&mut self, id: u32, m: crate::bindings::my::skiko_gfx::canvas::Matrix3x3) {
        let mat = matrix3x3_from_wit(&m);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.concat(&mat); }
    }

    fn bc_set_matrix(&mut self, id: u32, m: crate::bindings::my::skiko_gfx::canvas::Matrix3x3) {
        let mat = matrix3x3_from_wit(&m);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.set_matrix(&mat.into()); }
    }

    fn bc_reset_matrix(&mut self, id: u32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.reset_matrix(); }
    }

    fn bc_clip_rect(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                    mode: crate::bindings::my::skiko_gfx::canvas::ClipMode,
                    anti_alias: bool) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.clip_rect(Rect::from_xywh(x, y, w, h),
                Some(clip_op_from_wit(mode)), Some(anti_alias));
        }
    }

    fn bc_clip_rrect(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                     radii: Vec<f32>,
                     mode: crate::bindings::my::skiko_gfx::canvas::ClipMode,
                     anti_alias: bool) {
        let rr = make_rrect_with_radii(Rect::from_xywh(x, y, w, h), &radii);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.clip_rrect(rr, Some(clip_op_from_wit(mode)), Some(anti_alias));
        }
    }

    fn bc_clip_path(&mut self, id: u32, path_data: Vec<u8>,
                    mode: crate::bindings::my::skiko_gfx::canvas::ClipMode,
                    anti_alias: bool) {
        let s = String::from_utf8_lossy(&path_data);
        if let Some(path) = skia_safe::Path::from_svg(&*s) {
            if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
                c.clip_path(&path, Some(clip_op_from_wit(mode)), Some(anti_alias));
            }
        }
    }

    fn bc_clear(&mut self, id: u32, argb: u32) {
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.clear(Color::new(argb));
        }
    }

    fn bc_draw_paint(&mut self, id: u32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.draw_paint(&paint); }
    }

    fn bc_draw_rect(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_rect(Rect::from_xywh(x, y, w, h), &paint);
        }
    }

    fn bc_draw_rrect(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                     radii: Vec<f32>, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        let rr = make_rrect_with_radii(Rect::from_xywh(x, y, w, h), &radii);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) { c.draw_rrect(rr, &paint); }
    }

    fn bc_draw_drrect(&mut self, id: u32,
        ox: f32, oy: f32, ow: f32, oh: f32, o_radii: Vec<f32>,
        ix: f32, iy: f32, iw: f32, ih: f32, i_radii: Vec<f32>,
        p: PaintAttrs,
    ) {
        let paint = make_paint_full(&p, &self.renderer);
        let outer = make_rrect_with_radii(Rect::from_xywh(ox, oy, ow, oh), &o_radii);
        let inner = make_rrect_with_radii(Rect::from_xywh(ix, iy, iw, ih), &i_radii);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_drrect(outer, inner, &paint);
        }
    }

    fn bc_draw_oval(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_oval(Rect::from_xywh(x, y, w, h), &paint);
        }
    }

    fn bc_draw_circle(&mut self, id: u32, x: f32, y: f32, radius: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_circle((x, y), radius, &paint);
        }
    }

    fn bc_draw_line(&mut self, id: u32, x0: f32, y0: f32, x1: f32, y1: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_line((x0, y0), (x1, y1), &paint);
        }
    }

    fn bc_draw_arc(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                   start_angle: f32, sweep_angle: f32, include_center: bool,
                   p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_arc(Rect::from_xywh(x, y, w, h), start_angle, sweep_angle,
                include_center, &paint);
        }
    }

    fn bc_draw_path(&mut self, id: u32, path_data: Vec<u8>, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        let s = String::from_utf8_lossy(&path_data);
        if let Some(path) = skia_safe::Path::from_svg(&*s) {
            if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
                c.draw_path(&path, &paint);
            }
        }
    }

    fn bc_draw_point(&mut self, id: u32, x: f32, y: f32, p: PaintAttrs) {
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_point((x, y), &paint);
        }
    }

    fn bc_draw_points(&mut self, id: u32, coords: Vec<f32>, p: PaintAttrs) {
        let pts = coords_to_points(&coords);
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_points(skia_safe::canvas::PointMode::Points, &pts, &paint);
        }
    }

    fn bc_draw_lines(&mut self, id: u32, coords: Vec<f32>, p: PaintAttrs) {
        let pts = coords_to_points(&coords);
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_points(skia_safe::canvas::PointMode::Lines, &pts, &paint);
        }
    }

    fn bc_draw_polygon(&mut self, id: u32, coords: Vec<f32>, p: PaintAttrs) {
        let pts = coords_to_points(&coords);
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_points(skia_safe::canvas::PointMode::Polygon, &pts, &paint);
        }
    }

    fn bc_draw_image(&mut self, id: u32, image_id: u32, x: f32, y: f32, alpha: u8) {
        let Some(img) = self.renderer.images.get(&image_id).cloned() else { return };
        let mut p = Paint::default();
        p.set_alpha(alpha);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_image(&img, (x, y), Some(&p));
        }
    }

    fn bc_draw_image_rect(&mut self, id: u32, image_id: u32,
                          src_x: f32, src_y: f32, src_w: f32, src_h: f32,
                          dst_x: f32, dst_y: f32, dst_w: f32, dst_h: f32,
                          p: PaintAttrs) {
        let Some(img) = self.renderer.images.get(&image_id).cloned() else { return };
        let paint = make_paint_full(&p, &self.renderer);
        let src = Rect::from_xywh(src_x, src_y, src_w, src_h);
        let dst = Rect::from_xywh(dst_x, dst_y, dst_w, dst_h);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_image_rect(&img,
                Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                dst, &paint);
        }
    }

    fn bc_draw_text_blob(&mut self, id: u32, blob_id: u32, x: f32, y: f32, p: PaintAttrs) {
        let Some((blob, _)) = self.renderer.text_blobs.get(&blob_id).cloned() else { return };
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_text_blob(&blob, (x, y), &paint);
        }
    }

    fn bc_draw_text_line(&mut self, id: u32, blob_id: u32, x: f32, y: f32, p: PaintAttrs) {
        // TextLine maps to TextBlob on wasi.
        self.bc_draw_text_blob(id, blob_id, x, y, p);
    }

    fn bc_draw_picture(&mut self, id: u32, picture_id: u32) {
        let Some(pic) = self.renderer.pictures.get(&picture_id).cloned() else { return };
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_picture(&pic, None, None);
        }
    }

    fn bc_draw_string(&mut self, _id: u32, _text: Vec<u8>, _x: f32, _y: f32, _p: PaintAttrs) {
        // No Font resource on wasi yet — drawString is a no-op until needed.
    }

    fn bc_draw_vertices(&mut self, _id: u32, _vertex_mode: u8,
                        _positions: Vec<f32>, _colors: Vec<u32>,
                        _tex_coords: Vec<f32>, _indices: Vec<u16>,
                        _blend_mode: BlendMode, _paint: PaintAttrs) {
        // drawVertices is not used by the current widget set; no-op stub.
    }

    // ── guest-shaped glyph text (docs/skia-wit-mapping.md) ────────────────

    fn create_typeface(&mut self, data: Vec<u8>, index: u32) -> u32 {
        let mgr = skia_safe::FontMgr::new();
        let Some(tf) = mgr.new_from_data(&data, if index > 0 { Some(index as usize) } else { None })
        else {
            log::warn!("create_typeface: unparseable font data ({} bytes)", data.len());
            return 0;
        };
        let id = self.renderer.next_blob_id;
        self.renderer.next_blob_id = id.wrapping_add(1).max(1);
        self.renderer.guest_typefaces.insert(id, tf);
        id
    }

    fn drop_typeface(&mut self, id: u32) {
        self.renderer.guest_typefaces.remove(&id);
    }

    fn draw_glyphs(&mut self, typeface_id: u32, size: f32,
                   glyph_ids: Vec<u16>, positions: Vec<f32>,
                   origin_x: f32, origin_y: f32, p: PaintAttrs) {
        let Some((glyphs, points, font)) =
            prep_glyph_run(&self.renderer, typeface_id, size, glyph_ids, &positions)
        else { return };
        let paint = make_paint_full(&p, &self.renderer);
        self.renderer.canvas().draw_glyphs_at(
            &glyphs, points.as_slice(), (origin_x, origin_y), &font, &paint);
    }

    fn bc_draw_glyphs(&mut self, id: u32, typeface_id: u32, size: f32,
                      glyph_ids: Vec<u16>, positions: Vec<f32>,
                      origin_x: f32, origin_y: f32, p: PaintAttrs) {
        let Some((glyphs, points, font)) =
            prep_glyph_run(&self.renderer, typeface_id, size, glyph_ids, &positions)
        else { return };
        let paint = make_paint_full(&p, &self.renderer);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_glyphs_at(&glyphs, points.as_slice(), (origin_x, origin_y), &font, &paint);
        }
    }

    fn draw_shadow_rrect(&mut self, x: f32, y: f32, w: f32, h: f32,
                         radii: Vec<f32>, sigma: f32, color: u32) {
        let (rr, paint) = make_shadow(x, y, w, h, &radii, sigma, color);
        self.renderer.canvas().draw_rrect(rr, &paint);
    }

    fn bc_draw_shadow_rrect(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32,
                            radii: Vec<f32>, sigma: f32, color: u32) {
        let (rr, paint) = make_shadow(x, y, w, h, &radii, sigma, color);
        if let Some(c) = bc_canvas_mut(&mut self.renderer, id) {
            c.draw_rrect(rr, &paint);
        }
    }

    // ── debug log ─────────────────────────────────────────────────────────

    fn log_message(&mut self, msg: String) {
        log::info!("[wasm] {}", msg);
    }
}

/// Shared prep for draw-glyphs / bc-draw-glyphs: look up the guest typeface,
/// pair glyph ids with their (x, y) positions (extra ids without a position
/// are dropped), and build the sized Font. Subpixel positioning is on — the
/// guest's shaper (parley) produces fractional advances.
fn prep_glyph_run(r: &SkiaRenderer, typeface_id: u32, size: f32,
                  glyph_ids: Vec<u16>, positions: &[f32])
                  -> Option<(Vec<u16>, Vec<skia_safe::Point>, Font)> {
    let tf = r.guest_typefaces.get(&typeface_id)?.clone();
    let points = coords_to_points(positions);
    let mut glyphs = glyph_ids;
    glyphs.truncate(points.len());
    if glyphs.is_empty() { return None; }
    let mut font = Font::from_typeface(tf, size);
    font.set_edging(skia_safe::font::Edging::AntiAlias);
    font.set_subpixel(true);
    Some((glyphs, points, font))
}

/// Shared prep for draw-shadow-rrect / bc-draw-shadow-rrect: the rrect plus
/// a filled anti-aliased paint carrying the blur mask filter (Normal style,
/// matching the Skia box-shadow idiom). sigma <= 0 degrades to a plain fill.
fn make_shadow(x: f32, y: f32, w: f32, h: f32, radii: &[f32],
               sigma: f32, color: u32) -> (RRect, Paint) {
    let rr = make_rrect_with_radii(Rect::from_xywh(x, y, w, h), radii);
    let mut paint = Paint::default();
    paint.set_argb(
        ((color >> 24) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        ((color >>  8) & 0xFF) as u8,
        ( color        & 0xFF) as u8,
    );
    paint.set_anti_alias(true);
    if sigma > 0.0 {
        paint.set_mask_filter(skia_safe::MaskFilter::blur(
            skia_safe::BlurStyle::Normal, sigma, None));
    }
    (rr, paint)
}

/// Look up the inner Canvas of a bitmap-canvas surface. Returns None for
/// id 0 (sentinel) or any id not currently live.
fn bc_canvas_mut(r: &mut SkiaRenderer, id: u32) -> Option<&Canvas> {
    if id == 0 { return None; }
    let surface = r.bitmap_canvases.get_mut(&id)?;
    Some(surface.canvas())
}

fn clip_op_from_wit(
    m: crate::bindings::my::skiko_gfx::canvas::ClipMode,
) -> skia_safe::ClipOp {
    use crate::bindings::my::skiko_gfx::canvas::ClipMode;
    match m {
        ClipMode::Intersect  => skia_safe::ClipOp::Intersect,
        ClipMode::Difference => skia_safe::ClipOp::Difference,
    }
}

/// Build an RRect from a Rect and a radii list:
///   0 floats → square corners
///   1 float  → uniform radius
///   2 floats → (rx, ry) shared by all corners
///   ≥ 8 floats → per-corner (rx, ry), order top-left, top-right,
///                bottom-right, bottom-left (Kotlin RRect order)
fn make_rrect_with_radii(rect: Rect, radii: &[f32]) -> RRect {
    match radii.len() {
        0 => RRect::new_rect(rect),
        1 => RRect::new_rect_xy(rect, radii[0], radii[0]),
        2 => RRect::new_rect_xy(rect, radii[0], radii[1]),
        4 => {
            // (tl, tr, br, bl) uniform per corner.
            let pts = [
                skia_safe::Point::new(radii[0], radii[0]),
                skia_safe::Point::new(radii[1], radii[1]),
                skia_safe::Point::new(radii[2], radii[2]),
                skia_safe::Point::new(radii[3], radii[3]),
            ];
            RRect::new_rect_radii(rect, &pts)
        }
        n if n >= 8 => {
            let pts = [
                skia_safe::Point::new(radii[0], radii[1]),
                skia_safe::Point::new(radii[2], radii[3]),
                skia_safe::Point::new(radii[4], radii[5]),
                skia_safe::Point::new(radii[6], radii[7]),
            ];
            RRect::new_rect_radii(rect, &pts)
        }
        _ => RRect::new_rect(rect),
    }
}

/// Convert interleaved (x, y) f32 pairs to skia Points. Odd-length lists
/// are truncated (Kotlin shouldn't emit them; defensive).
fn coords_to_points(coords: &[f32]) -> Vec<skia_safe::Point> {
    coords.chunks_exact(2)
        .map(|pair| skia_safe::Point::new(pair[0], pair[1]))
        .collect()
}

// ─── Paint helpers ───────────────────────────────────────────────────────────

fn make_paint(attrs: &PaintAttrs) -> Paint {
    let mut p = Paint::default();
    p.set_argb(
        ((attrs.color >> 24) & 0xFF) as u8,
        ((attrs.color >> 16) & 0xFF) as u8,
        ((attrs.color >>  8) & 0xFF) as u8,
        ( attrs.color        & 0xFF) as u8,
    );
    p.set_style(match attrs.style {
        WitPaintStyle::Fill          => PaintStyle::Fill,
        WitPaintStyle::Stroke        => PaintStyle::Stroke,
        WitPaintStyle::FillAndStroke => PaintStyle::StrokeAndFill,
    });
    p.set_stroke_width(attrs.stroke_width);
    p.set_stroke_miter(attrs.stroke_miter);
    p.set_stroke_cap(match attrs.stroke_cap {
        StrokeCap::Butt   => skia_safe::PaintCap::Butt,
        StrokeCap::Round  => skia_safe::PaintCap::Round,
        StrokeCap::Square => skia_safe::PaintCap::Square,
    });
    p.set_stroke_join(match attrs.stroke_join {
        StrokeJoin::Miter => skia_safe::PaintJoin::Miter,
        StrokeJoin::Round => skia_safe::PaintJoin::Round,
        StrokeJoin::Bevel => skia_safe::PaintJoin::Bevel,
    });
    p.set_anti_alias(attrs.anti_alias);
    p.set_alpha(attrs.alpha);
    p.set_blend_mode(match attrs.blend_mode {
        BlendMode::SrcOver    => skia_safe::BlendMode::SrcOver,
        BlendMode::Src        => skia_safe::BlendMode::Src,
        BlendMode::DstIn      => skia_safe::BlendMode::DstIn,
        BlendMode::DstOut     => skia_safe::BlendMode::DstOut,
        BlendMode::SrcAtop    => skia_safe::BlendMode::SrcATop,
        BlendMode::DstAtop    => skia_safe::BlendMode::DstATop,
        BlendMode::Xor        => skia_safe::BlendMode::Xor,
        BlendMode::Multiply   => skia_safe::BlendMode::Multiply,
        BlendMode::Screen     => skia_safe::BlendMode::Screen,
        BlendMode::Overlay    => skia_safe::BlendMode::Overlay,
        BlendMode::Darken     => skia_safe::BlendMode::Darken,
        BlendMode::Lighten    => skia_safe::BlendMode::Lighten,
        BlendMode::ColorDodge => skia_safe::BlendMode::ColorDodge,
        BlendMode::ColorBurn  => skia_safe::BlendMode::ColorBurn,
        BlendMode::HardLight  => skia_safe::BlendMode::HardLight,
        BlendMode::SoftLight  => skia_safe::BlendMode::SoftLight,
        BlendMode::Difference => skia_safe::BlendMode::Difference,
        BlendMode::Exclusion  => skia_safe::BlendMode::Exclusion,
        BlendMode::Clear      => skia_safe::BlendMode::Clear,
    });
    p
}

fn make_paint_full(attrs: &PaintAttrs, renderer: &SkiaRenderer) -> Paint {
    let mut p = make_paint(attrs);
    // Shader
    if attrs.shader_id != 0 {
        if let Some(s) = renderer.shader_cache.get(&attrs.shader_id) {
            p.set_shader(Some(s.clone()));
        }
    }
    // Color filter
    match attrs.color_filter_kind {
        ColorFilterKind::Blend => {
            let c = attrs.color_filter_color;
            let color = skia_safe::Color::from_argb(
                (c >> 24) as u8, (c >> 16) as u8, (c >> 8) as u8, c as u8);
            if let Some(cf) = skia_safe::color_filters::blend(color, skia_safe::BlendMode::Modulate) {
                p.set_color_filter(cf);
            }
        }
        ColorFilterKind::Invert => {
            let matrix = [
                -1f32,  0f32,  0f32, 0f32, 1f32,
                 0f32, -1f32,  0f32, 0f32, 1f32,
                 0f32,  0f32, -1f32, 0f32, 1f32,
                 0f32,  0f32,  0f32, 1f32, 0f32,
            ];
            let cf = skia_safe::color_filters::matrix_row_major(&matrix, None);
            p.set_color_filter(cf);
        }
        ColorFilterKind::None => {}
    }
    p
}

fn tile_mode_from_u8(m: u8) -> skia_safe::TileMode {
    match m {
        1 => skia_safe::TileMode::Repeat,
        2 => skia_safe::TileMode::Mirror,
        3 => skia_safe::TileMode::Decal,
        _ => skia_safe::TileMode::Clamp,
    }
}

fn sampling_options_from_wit(
    opts: &crate::bindings::my::skiko_gfx::canvas::SamplingOptions,
) -> skia_safe::SamplingOptions {
    if opts.use_cubic {
        skia_safe::SamplingOptions::from(skia_safe::CubicResampler {
            b: opts.cubic_b, c: opts.cubic_c,
        })
    } else {
        let filter = match opts.filter {
            1 => skia_safe::FilterMode::Linear,
            _ => skia_safe::FilterMode::Nearest,
        };
        let mipmap = match opts.mipmap {
            1 => skia_safe::MipmapMode::Nearest,
            2 => skia_safe::MipmapMode::Linear,
            _ => skia_safe::MipmapMode::None,
        };
        skia_safe::SamplingOptions::new(filter, mipmap)
    }
}

fn matrix3x3_from_wit(
    m: &crate::bindings::my::skiko_gfx::canvas::Matrix3x3,
) -> skia_safe::Matrix {
    let mut out = skia_safe::Matrix::new_identity();
    out.set_9(&[
        m.scale_x, m.skew_x,  m.trans_x,
        m.skew_y,  m.scale_y, m.trans_y,
        m.persp_0, m.persp_1, m.persp_2,
    ]);
    out
}

fn blend_mode_from_wit(bm: BlendMode) -> skia_safe::BlendMode {
    match bm {
        BlendMode::SrcOver    => skia_safe::BlendMode::SrcOver,
        BlendMode::Src        => skia_safe::BlendMode::Src,
        BlendMode::DstIn      => skia_safe::BlendMode::DstIn,
        BlendMode::DstOut     => skia_safe::BlendMode::DstOut,
        BlendMode::SrcAtop    => skia_safe::BlendMode::SrcATop,
        BlendMode::DstAtop    => skia_safe::BlendMode::DstATop,
        BlendMode::Xor        => skia_safe::BlendMode::Xor,
        BlendMode::Multiply   => skia_safe::BlendMode::Multiply,
        BlendMode::Screen     => skia_safe::BlendMode::Screen,
        BlendMode::Overlay    => skia_safe::BlendMode::Overlay,
        BlendMode::Darken     => skia_safe::BlendMode::Darken,
        BlendMode::Lighten    => skia_safe::BlendMode::Lighten,
        BlendMode::ColorDodge => skia_safe::BlendMode::ColorDodge,
        BlendMode::ColorBurn  => skia_safe::BlendMode::ColorBurn,
        BlendMode::HardLight  => skia_safe::BlendMode::HardLight,
        BlendMode::SoftLight  => skia_safe::BlendMode::SoftLight,
        BlendMode::Difference => skia_safe::BlendMode::Difference,
        BlendMode::Exclusion  => skia_safe::BlendMode::Exclusion,
        BlendMode::Clear      => skia_safe::BlendMode::Clear,
    }
}
