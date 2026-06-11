//! Host implementation of the `wasi:canvas` DRAFT (proposals/wasi-canvas),
//! behind the `wasi-canvas` cargo feature. Maps ~1:1 onto the same
//! `SkiaRenderer` that serves `my:skiko-gfx` — the coexistence strategy
//! from COMPATIBILITY.md (this is a second linker package, not a
//! migration). The optional `layout` interface is NOT implemented yet
//! (the draft marks it embedder-optional; follow-up with the first
//! managed-runtime consumer).
//!
//! Resource model: every wasi:canvas resource is a `ResourceTable` entry
//! (the wandr:crypto / wandr:video pattern). The `canvas` resource is
//! either the renderer's MAIN surface, an OFFSCREEN raster surface, or a
//! picture RECORDING — drawing helpers split the `HostState` borrow so
//! the table and the renderer can be used together.

use wasmtime::component::{Resource, ResourceTable};

use crate::HostState;
use crate::wasi_canvas_bindings::wasi::canvas::draw as wit_draw;
use crate::wasi_canvas_bindings::wasi::canvas::glyphs as wit_glyphs;
use crate::wasi_canvas_bindings::wasi::canvas::types as wit_types;
// (module path note: bindgen nests generated modules under wasi::canvas::*)

// ─── resource backing types ──────────────────────────────────────────────────

pub struct ShaderRes(pub skia_safe::Shader);
pub struct ImageRes(pub skia_safe::Image);
pub struct PictureRes(pub skia_safe::Picture);
pub struct TypefaceRes(pub skia_safe::Typeface);
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

// ─── conversions ─────────────────────────────────────────────────────────────

fn blend_mode(m: wit_types::BlendMode) -> skia_safe::BlendMode {
    use skia_safe::BlendMode as B;
    use wit_types::BlendMode as W;
    match m {
        W::SrcOver => B::SrcOver,
        W::Src => B::Src,
        W::DstIn => B::DstIn,
        W::DstOut => B::DstOut,
        W::SrcAtop => B::SrcATop,
        W::DstAtop => B::DstATop,
        W::Xor => B::Xor,
        W::Multiply => B::Multiply,
        W::Screen => B::Screen,
        W::Overlay => B::Overlay,
        W::Darken => B::Darken,
        W::Lighten => B::Lighten,
        W::ColorDodge => B::ColorDodge,
        W::ColorBurn => B::ColorBurn,
        W::HardLight => B::HardLight,
        W::SoftLight => B::SoftLight,
        W::Difference => B::Difference,
        W::Exclusion => B::Exclusion,
        W::Clear => B::Clear,
    }
}

fn tile_mode(t: wit_types::TileMode) -> skia_safe::TileMode {
    use skia_safe::TileMode as S;
    use wit_types::TileMode as W;
    match t {
        W::Clamp => S::Clamp,
        W::Repeat => S::Repeat,
        W::Mirror => S::Mirror,
        W::Decal => S::Decal,
    }
}

fn sampling(s: wit_types::Sampling) -> skia_safe::SamplingOptions {
    let filter = match s.filter {
        wit_types::FilterMode::Nearest => skia_safe::FilterMode::Nearest,
        wit_types::FilterMode::Linear => skia_safe::FilterMode::Linear,
    };
    let mipmap = match s.mipmap {
        wit_types::MipmapMode::None => skia_safe::MipmapMode::None,
        wit_types::MipmapMode::Nearest => skia_safe::MipmapMode::Nearest,
        wit_types::MipmapMode::Linear => skia_safe::MipmapMode::Linear,
    };
    skia_safe::SamplingOptions::new(filter, mipmap)
}

fn rect(r: wit_types::Rect) -> skia_safe::Rect {
    skia_safe::Rect::from_xywh(r.x, r.y, r.width, r.height)
}

fn rrect(rr: &wit_types::RoundedRect) -> skia_safe::RRect {
    let radii = [
        skia_safe::Point::new(rr.top_left.x, rr.top_left.y),
        skia_safe::Point::new(rr.top_right.x, rr.top_right.y),
        skia_safe::Point::new(rr.bottom_right.x, rr.bottom_right.y),
        skia_safe::Point::new(rr.bottom_left.x, rr.bottom_left.y),
    ];
    skia_safe::RRect::new_rect_radii(rect(rr.rect), &radii)
}

fn matrix(t: wit_types::Transform) -> skia_safe::Matrix {
    skia_safe::Matrix::new_all(
        t.m00, t.m01, t.m02, t.m10, t.m11, t.m12, t.m20, t.m21, t.m22,
    )
}

fn svg_path(path: &str, rule: wit_types::FillRule) -> Option<skia_safe::Path> {
    let mut p = skia_safe::Path::from_svg(path)?;
    p.set_fill_type(match rule {
        wit_types::FillRule::Nonzero => skia_safe::PathFillType::Winding,
        wit_types::FillRule::Evenodd => skia_safe::PathFillType::EvenOdd,
    });
    Some(p)
}

fn color(argb: u32) -> skia_safe::Color {
    skia_safe::Color::new(argb)
}

/// wit paint → skia Paint (shader looked up from the table; mask blur
/// applied; `alpha` multiplied via setAlpha after color/shader).
fn paint(table: &ResourceTable, p: &wit_types::Paint) -> skia_safe::Paint {
    let mut sp = skia_safe::Paint::default();
    sp.set_color(color(p.color));
    sp.set_style(match p.style {
        wit_types::PaintStyle::Fill => skia_safe::PaintStyle::Fill,
        wit_types::PaintStyle::Stroke => skia_safe::PaintStyle::Stroke,
        wit_types::PaintStyle::FillAndStroke => skia_safe::PaintStyle::StrokeAndFill,
    });
    sp.set_blend_mode(blend_mode(p.blend));
    sp.set_anti_alias(p.anti_alias);
    sp.set_stroke_width(p.stroke_width);
    sp.set_stroke_miter(p.stroke_miter);
    sp.set_stroke_cap(match p.stroke_cap {
        wit_types::StrokeCap::Butt => skia_safe::PaintCap::Butt,
        wit_types::StrokeCap::Round => skia_safe::PaintCap::Round,
        wit_types::StrokeCap::Square => skia_safe::PaintCap::Square,
    });
    sp.set_stroke_join(match p.stroke_join {
        wit_types::StrokeJoin::Miter => skia_safe::PaintJoin::Miter,
        wit_types::StrokeJoin::Round => skia_safe::PaintJoin::Round,
        wit_types::StrokeJoin::Bevel => skia_safe::PaintJoin::Bevel,
    });
    if let Some(sh) = &p.shader {
        if let Ok(s) = table.get(sh) {
            sp.set_shader(Some(s.0.clone()));
        }
    }
    if let Some(b) = &p.blur {
        let style = match b.style {
            wit_types::BlurStyle::Normal => skia_safe::BlurStyle::Normal,
            wit_types::BlurStyle::Solid => skia_safe::BlurStyle::Solid,
            wit_types::BlurStyle::Outer => skia_safe::BlurStyle::Outer,
            wit_types::BlurStyle::Inner => skia_safe::BlurStyle::Inner,
        };
        sp.set_mask_filter(skia_safe::MaskFilter::blur(style, b.sigma, None));
    }
    // Alpha multiplies AFTER color/shader, like paint-attrs in skiko-gfx.
    let a = ((sp.alpha() as u32 * p.alpha as u32) / 255) as u8;
    sp.set_alpha(a);
    sp
}

fn stops_to_arrays(stops: &[(f32, u32)]) -> (Vec<skia_safe::Color>, Vec<f32>) {
    let mut colors = Vec::with_capacity(stops.len());
    let mut positions = Vec::with_capacity(stops.len());
    for (off, c) in stops {
        positions.push(*off);
        colors.push(color(*c));
    }
    (colors, positions)
}

// ─── canvas access (split-borrow helper) ─────────────────────────────────────

/// Run `f` against the skia canvas behind a `canvas` resource. Splits the
/// HostState borrow so Main can reach the renderer while the table entry
/// is held.
fn with_canvas<R>(
    state: &mut HostState,
    c: &Resource<CanvasRes>,
    f: impl FnOnce(&skia_safe::Canvas) -> R,
) -> wasmtime::Result<R> {
    let HostState { table, renderer, .. } = state;
    let entry = table.get_mut(c)?;
    Ok(match entry {
        CanvasRes::Main => f(renderer.canvas()),
        CanvasRes::Offscreen(surface) => f(surface.canvas()),
        CanvasRes::Recording(recorder) => match recorder.recording_canvas() {
            Some(canvas) => f(canvas),
            None => return Err(wasmtime::Error::msg("wasi:canvas — recording already finished")),
        },
    })
}

// ─── types interface ─────────────────────────────────────────────────────────

impl wit_types::Host for HostState {}

impl wit_types::HostShader for HostState {
    fn drop(&mut self, rep: Resource<ShaderRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit_types::HostImage for HostState {
    fn width(&mut self, self_: Resource<ImageRes>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&self_)?.0.width() as u32)
    }
    fn height(&mut self, self_: Resource<ImageRes>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&self_)?.0.height() as u32)
    }
    fn drop(&mut self, rep: Resource<ImageRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ─── draw interface ──────────────────────────────────────────────────────────

impl wit_draw::Host for HostState {}

impl wit_draw::HostPicture for HostState {
    fn drop(&mut self, rep: Resource<PictureRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit_draw::HostGraphics for HostState {
    fn linear_gradient(
        &mut self,
        _self_: Resource<GraphicsRes>,
        start: wit_types::Point,
        end: wit_types::Point,
        stops: Vec<(f32, u32)>,
        tile: wit_types::TileMode,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let shader = skia_safe::gradient_shader::linear(
            (
                skia_safe::Point::new(start.x, start.y),
                skia_safe::Point::new(end.x, end.y),
            ),
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            None,
            None,
        )
        .ok_or_else(|| wasmtime::Error::msg("linear-gradient failed"))?;
        Ok(self.table.push(ShaderRes(shader))?)
    }

    fn radial_gradient(
        &mut self,
        _self_: Resource<GraphicsRes>,
        center: wit_types::Point,
        radius: f32,
        stops: Vec<(f32, u32)>,
        tile: wit_types::TileMode,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let shader = skia_safe::gradient_shader::radial(
            skia_safe::Point::new(center.x, center.y),
            radius,
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            None,
            None,
        )
        .ok_or_else(|| wasmtime::Error::msg("radial-gradient failed"))?;
        Ok(self.table.push(ShaderRes(shader))?)
    }

    fn sweep_gradient(
        &mut self,
        _self_: Resource<GraphicsRes>,
        center: wit_types::Point,
        start_angle: f32,
        end_angle: f32,
        stops: Vec<(f32, u32)>,
        tile: wit_types::TileMode,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let shader = skia_safe::gradient_shader::sweep(
            skia_safe::Point::new(center.x, center.y),
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            (start_angle, end_angle),
            None,
            None,
        )
        .ok_or_else(|| wasmtime::Error::msg("sweep-gradient failed"))?;
        Ok(self.table.push(ShaderRes(shader))?)
    }

    fn shader_blend(
        &mut self,
        _self_: Resource<GraphicsRes>,
        mode: wit_types::BlendMode,
        dst: Resource<ShaderRes>,
        src: Resource<ShaderRes>,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let d = self.table.get(&dst)?.0.clone();
        let s = self.table.get(&src)?.0.clone();
        let blended = skia_safe::shaders::blend(blend_mode(mode), d, s);
        Ok(self.table.push(ShaderRes(blended))?)
    }

    fn image_pattern(
        &mut self,
        _self_: Resource<GraphicsRes>,
        image: Resource<ImageRes>,
        tile_x: wit_types::TileMode,
        tile_y: wit_types::TileMode,
        sampling_: wit_types::Sampling,
        local: wit_types::Transform,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let img = self.table.get(&image)?.0.clone();
        let shader = img
            .to_shader(
                Some((tile_mode(tile_x), tile_mode(tile_y))),
                sampling(sampling_),
                Some(&matrix(local)),
            )
            .ok_or_else(|| wasmtime::Error::msg("image-pattern failed"))?;
        Ok(self.table.push(ShaderRes(shader))?)
    }

    fn decode_image(
        &mut self,
        _self_: Resource<GraphicsRes>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<ImageRes>, ()>> {
        let data = skia_safe::Data::new_copy(&bytes);
        match skia_safe::Image::from_encoded(data) {
            Some(img) => Ok(Ok(self.table.push(ImageRes(img))?)),
            None => Ok(Err(())),
        }
    }

    fn image_from_rgba8(
        &mut self,
        _self_: Resource<GraphicsRes>,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<ImageRes>, ()>> {
        if pixels.len() != (width as usize) * (height as usize) * 4 {
            return Ok(Err(()));
        }
        let info = skia_safe::ImageInfo::new(
            (width as i32, height as i32),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        let data = skia_safe::Data::new_copy(&pixels);
        match skia_safe::images::raster_from_data(&info, data, (width * 4) as usize) {
            Some(img) => Ok(Ok(self.table.push(ImageRes(img))?)),
            None => Ok(Err(())),
        }
    }

    fn new_offscreen(
        &mut self,
        _self_: Resource<GraphicsRes>,
        width: u32,
        height: u32,
    ) -> wasmtime::Result<Resource<CanvasRes>> {
        let surface =
            skia_safe::surfaces::raster_n32_premul((width.max(1) as i32, height.max(1) as i32))
                .ok_or_else(|| wasmtime::Error::msg("offscreen surface failed"))?;
        Ok(self.table.push(CanvasRes::Offscreen(surface))?)
    }

    fn start_recording(
        &mut self,
        _self_: Resource<GraphicsRes>,
        bounds: wit_types::Rect,
    ) -> wasmtime::Result<Resource<CanvasRes>> {
        let mut recorder = skia_safe::PictureRecorder::new();
        let _ = recorder.begin_recording(rect(bounds), false);
        Ok(self.table.push(CanvasRes::Recording(recorder))?)
    }

    fn drop(&mut self, rep: Resource<GraphicsRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit_draw::HostCanvas for HostState {
    fn width(&mut self, self_: Resource<CanvasRes>) -> wasmtime::Result<f32> {
        let HostState { table, renderer, .. } = self;
        Ok(match table.get(&self_)? {
            CanvasRes::Main => renderer.logical_width as f32,
            CanvasRes::Offscreen(s) => s.width() as f32,
            CanvasRes::Recording(_) => 0.0, // bounds not retained; fine for v1
        })
    }
    fn height(&mut self, self_: Resource<CanvasRes>) -> wasmtime::Result<f32> {
        let HostState { table, renderer, .. } = self;
        Ok(match table.get(&self_)? {
            CanvasRes::Main => renderer.logical_height as f32,
            CanvasRes::Offscreen(s) => s.height() as f32,
            CanvasRes::Recording(_) => 0.0,
        })
    }

    fn save(&mut self, self_: Resource<CanvasRes>) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.save();
        })
    }
    fn save_layer(
        &mut self,
        self_: Resource<CanvasRes>,
        bounds: Option<wit_types::Rect>,
        alpha: u8,
    ) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.save_layer_alpha(bounds.map(rect), alpha as u32);
        })
    }
    fn restore(&mut self, self_: Resource<CanvasRes>) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.restore();
        })
    }

    fn translate(&mut self, self_: Resource<CanvasRes>, dx: f32, dy: f32) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.translate((dx, dy));
        })
    }
    fn scale(&mut self, self_: Resource<CanvasRes>, sx: f32, sy: f32) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.scale((sx, sy));
        })
    }
    fn rotate(&mut self, self_: Resource<CanvasRes>, degrees: f32) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.rotate(degrees, None);
        })
    }
    fn concat(
        &mut self,
        self_: Resource<CanvasRes>,
        t: wit_types::Transform,
    ) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.concat(&matrix(t));
        })
    }

    fn clip_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        r: wit_types::Rect,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.clip_rect(rect(r), Some(skia_safe::ClipOp::Intersect), Some(anti_alias));
        })
    }
    fn clip_rounded_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        rr: wit_types::RoundedRect,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.clip_rrect(rrect(&rr), Some(skia_safe::ClipOp::Intersect), Some(anti_alias));
        })
    }
    fn clip_path(
        &mut self,
        self_: Resource<CanvasRes>,
        path: String,
        rule: wit_types::FillRule,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            if let Some(p) = svg_path(&path, rule) {
                c.clip_path(&p, Some(skia_safe::ClipOp::Intersect), Some(anti_alias));
            }
        })
    }

    fn clear(&mut self, self_: Resource<CanvasRes>, color_: u32) -> wasmtime::Result<()> {
        with_canvas(self, &self_, |c| {
            c.clear(color(color_));
        })
    }
    fn draw_paint(
        &mut self,
        self_: Resource<CanvasRes>,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_paint(&sp);
        })
    }
    fn draw_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        r: wit_types::Rect,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_rect(rect(r), &sp);
        })
    }
    fn draw_rounded_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        rr: wit_types::RoundedRect,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_rrect(rrect(&rr), &sp);
        })
    }
    fn draw_double_rounded_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        outer: wit_types::RoundedRect,
        inner: wit_types::RoundedRect,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_drrect(rrect(&outer), rrect(&inner), &sp);
        })
    }
    fn draw_oval(
        &mut self,
        self_: Resource<CanvasRes>,
        bounds: wit_types::Rect,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_oval(rect(bounds), &sp);
        })
    }
    fn draw_line(
        &mut self,
        self_: Resource<CanvasRes>,
        start: wit_types::Point,
        end: wit_types::Point,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_line((start.x, start.y), (end.x, end.y), &sp);
        })
    }
    fn draw_arc(
        &mut self,
        self_: Resource<CanvasRes>,
        bounds: wit_types::Rect,
        start_angle: f32,
        sweep_angle: f32,
        include_center: bool,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_arc(rect(bounds), start_angle, sweep_angle, include_center, &sp);
        })
    }
    fn draw_path(
        &mut self,
        self_: Resource<CanvasRes>,
        path: String,
        rule: wit_types::FillRule,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            if let Some(pp) = svg_path(&path, rule) {
                c.draw_path(&pp, &sp);
            }
        })
    }

    fn draw_image(
        &mut self,
        self_: Resource<CanvasRes>,
        image: Resource<ImageRes>,
        at: wit_types::Point,
        sampling_: wit_types::Sampling,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let img = self.table.get(&image)?.0.clone();
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_image_with_sampling_options(&img, (at.x, at.y), sampling(sampling_), Some(&sp));
        })
    }
    fn draw_image_rect(
        &mut self,
        self_: Resource<CanvasRes>,
        image: Resource<ImageRes>,
        src: wit_types::Rect,
        dst: wit_types::Rect,
        sampling_: wit_types::Sampling,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let img = self.table.get(&image)?.0.clone();
        let sp = paint(&self.table, &p);
        with_canvas(self, &self_, |c| {
            c.draw_image_rect_with_sampling_options(
                &img,
                Some((&rect(src), skia_safe::canvas::SrcRectConstraint::Fast)),
                rect(dst),
                sampling(sampling_),
                &sp,
            );
        })
    }

    fn finish_recording(
        &mut self,
        c: Resource<CanvasRes>,
    ) -> wasmtime::Result<Resource<PictureRes>> {
        let entry = self.table.delete(c)?;
        match entry {
            CanvasRes::Recording(mut recorder) => {
                let pic = recorder
                    .finish_recording_as_picture(None)
                    .ok_or_else(|| wasmtime::Error::msg("finish-recording: empty recording"))?;
                Ok(self.table.push(PictureRes(pic))?)
            }
            _ => Err(wasmtime::Error::msg("finish-recording on a non-recording canvas")),
        }
    }
    fn draw_picture(
        &mut self,
        self_: Resource<CanvasRes>,
        p: Resource<PictureRes>,
    ) -> wasmtime::Result<()> {
        let pic = self.table.get(&p)?.0.clone();
        with_canvas(self, &self_, |c| {
            c.draw_picture(&pic, None, None);
        })
    }

    fn snapshot(
        &mut self,
        self_: Resource<CanvasRes>,
    ) -> wasmtime::Result<Result<Resource<ImageRes>, ()>> {
        let img = match self.table.get_mut(&self_)? {
            CanvasRes::Offscreen(surface) => Some(surface.image_snapshot()),
            // The embedder-presented canvas keeps its pixels private;
            // recordings have no pixels.
            CanvasRes::Main | CanvasRes::Recording(_) => None,
        };
        match img {
            Some(i) => Ok(Ok(self.table.push(ImageRes(i))?)),
            None => Ok(Err(())),
        }
    }

    fn drop(&mut self, rep: Resource<CanvasRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ─── glyphs interface ────────────────────────────────────────────────────────

impl wit_glyphs::HostTypeface for HostState {
    fn from_bytes(
        &mut self,
        bytes: Vec<u8>,
        index: u32,
    ) -> wasmtime::Result<Result<Resource<TypefaceRes>, ()>> {
        let mgr = skia_safe::FontMgr::new();
        match mgr.new_from_data(&bytes, if index > 0 { Some(index as usize) } else { None }) {
            Some(tf) => Ok(Ok(self.table.push(TypefaceRes(tf))?)),
            None => Ok(Err(())),
        }
    }
    fn drop(&mut self, rep: Resource<TypefaceRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit_glyphs::Host for HostState {
    fn draw_glyphs(
        &mut self,
        canvas: Resource<CanvasRes>,
        typeface: Resource<TypefaceRes>,
        size: f32,
        glyphs: Vec<wit_glyphs::PositionedGlyph>,
        origin: wit_types::Point,
        p: wit_types::Paint,
    ) -> wasmtime::Result<()> {
        let tf = self.table.get(&typeface)?.0.clone();
        let sp = paint(&self.table, &p);
        let mut font = skia_safe::Font::from_typeface(tf, size);
        font.set_edging(skia_safe::font::Edging::AntiAlias);
        font.set_subpixel(true);
        let ids: Vec<u16> = glyphs.iter().map(|g| g.id as u16).collect();
        let points: Vec<skia_safe::Point> =
            glyphs.iter().map(|g| skia_safe::Point::new(g.at.x, g.at.y)).collect();
        with_canvas(self, &canvas, |c| {
            if !ids.is_empty() {
                c.draw_glyphs_at(&ids, points.as_slice(), (origin.x, origin.y), &font, &sp);
            }
        })
    }
}

// ─── linker registration ─────────────────────────────────────────────────────

/// Register the wasi:canvas draft onto a guest linker (feature-gated call
/// sites in app_loader).
pub fn add_to_linker(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> wasmtime::Result<()> {
    crate::wasi_canvas_bindings::CanvasHost::add_to_linker::<_, wasmtime::component::HasSelf<HostState>>(
        linker,
        |s| s,
    )
}

// ─── tests (offscreen path; no renderer needed) ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the skia mapping helpers directly against an offscreen
    /// surface — the table/HostState plumbing is exercised on device, the
    /// pixel-producing conversions are what unit tests can lock down.
    #[test]
    fn offscreen_draw_and_snapshot() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((10, 10)).unwrap();
        let c = surface.canvas();
        c.clear(color(0xFF000000));
        let table = ResourceTable::new();
        let p = wit_types::Paint {
            style: wit_types::PaintStyle::Fill,
            color: 0xFFFF0000,
            alpha: 255,
            blend: wit_types::BlendMode::SrcOver,
            anti_alias: false,
            shader: None,
            stroke_width: 0.0,
            stroke_cap: wit_types::StrokeCap::Butt,
            stroke_join: wit_types::StrokeJoin::Miter,
            stroke_miter: 4.0,
            blur: None,
        };
        c.draw_rect(rect(wit_types::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 }), &paint(&table, &p));
        let img = surface.image_snapshot();
        let pm = img.peek_pixels().unwrap();
        let px = pm.get_color((5, 5));
        assert_eq!(px, skia_safe::Color::RED);
    }

    #[test]
    fn rrect_and_path_mapping() {
        let rr = wit_types::RoundedRect {
            rect: wit_types::Rect { x: 0.0, y: 0.0, width: 20.0, height: 10.0 },
            top_left: wit_types::Point { x: 2.0, y: 2.0 },
            top_right: wit_types::Point { x: 3.0, y: 3.0 },
            bottom_right: wit_types::Point { x: 0.0, y: 0.0 },
            bottom_left: wit_types::Point { x: 1.0, y: 1.0 },
        };
        let s = rrect(&rr);
        assert_eq!(s.bounds().width(), 20.0);
        assert!(svg_path("M 0 0 L 10 0 L 10 10 Z", wit_types::FillRule::Evenodd).is_some());
        assert!(svg_path("not a path", wit_types::FillRule::Nonzero).is_none());
    }

    #[test]
    fn gradient_stops_mapping() {
        let (colors, positions) = stops_to_arrays(&[(0.0, 0xFF000000), (1.0, 0xFFFFFFFF)]);
        assert_eq!(colors.len(), 2);
        assert_eq!(positions, vec![0.0, 1.0]);
    }
}
