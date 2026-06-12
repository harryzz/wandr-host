//! Backing types for the wasi:canvas host resources (shared by the
//! 0.0.2 impl in `wasi_canvas_002_impl.rs`). The 0.0.1 trait impls that
//! used to live here died in Phase C (task 105) — 0.0.2 is the only
//! served canvas package.

use wasmtime::component::Resource;

pub struct ShaderRes(pub skia_safe::Shader);
pub struct ImageRes(pub skia_safe::Image);
pub struct PictureRes(pub skia_safe::Picture);
pub struct TypefaceRes(pub skia_safe::Typeface);

/// Host-shaped paragraph (skia textlayout); Rc<RefCell> because layout()
/// mutates while queries borrow.
pub struct ParagraphRes(pub std::rc::Rc<std::cell::RefCell<skia_safe::textlayout::Paragraph>>);

/// The creation capability is the handle itself; no state behind it (an
/// attenuating embedder would interpose, not parameterize this).
pub struct GraphicsRes;

pub enum CanvasRes {
    /// The embedder-presented target: the renderer's current surface.
    Main,
    /// `graphics.new-offscreen` — own raster surface, snapshot-able.
    Offscreen(skia_safe::Surface),
    /// `graphics.start-recording` — captures into a display list.
    Recording(skia_safe::PictureRecorder),
}

// SAFETY: ResourceTable requires Send, but skia Surface/PictureRecorder are
// not Send. All access is serialized through `&mut HostState` on the
// store's single thread (the same justification as the raw NDK pointers in
// video.rs and the renderer's own surfaces living in HostState) — wandr
// guests are single-threaded and the store never crosses threads.
unsafe impl Send for CanvasRes {}

/// The per-surface canvas-context (wasi-gfx graphics-context idiom).
pub struct CanvasContextRes;

// SAFETY: same single-threaded-store argument as CanvasRes.
unsafe impl Send for ShaderRes {}
unsafe impl Send for ImageRes {}
unsafe impl Send for PictureRes {}
unsafe impl Send for TypefaceRes {}
unsafe impl Send for ParagraphRes {}

// Resource is referenced by the bindgen `with:` mappings in lib.rs.
#[allow(unused_imports)]
use Resource as _;
