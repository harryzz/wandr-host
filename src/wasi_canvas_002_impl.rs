//! Host implementation of `wasi:canvas@0.0.2` (proposals/wasi-canvas/
//! wit-0.0.2) — served SIDE-BY-SIDE with 0.0.1 over the same SkiaRenderer
//! (the R3 version-coexistence rule, REDESIGN-0.0.2.md §2). Heavy
//! resources share 0.0.1's backing types; the 0.0.2-only pieces are the
//! 29-mode blend enum, gradient local matrices, the union text-style
//! (decoration/shadows/background/baseline-shift), the BUFFERED
//! setter-form paragraph-builder, and the `scene` interface — host-
//! retained layers re-skinning the WasiDrawable machinery the legacy
//! RenderNode path already ships (cpp/wasi_drawable.{h,cpp}).

use wasmtime::component::{Resource, ResourceTable};

use crate::HostState;
use crate::canvas_impl::{WasiDrawable, wasi_drawable_ffi};
use crate::wasi_canvas_impl::{
    CanvasContextRes, CanvasRes, GraphicsRes, ImageRes, ParagraphRes, PictureRes, ShaderRes,
    TypefaceRes,
};
use crate::wasi_canvas_002_bindings::wasi::canvas::draw as wit_draw;
use crate::wasi_canvas_002_bindings::wasi::canvas::embedding as wit_embedding;
use crate::wasi_canvas_002_bindings::wasi::canvas::glyphs as wit_glyphs;
use crate::wasi_canvas_002_bindings::wasi::canvas::layout as wit_layout;
use crate::wasi_canvas_002_bindings::wasi::canvas::scene as wit_scene;
use crate::wasi_canvas_002_bindings::wasi::canvas::types as wit_types;

// ─── 0.0.2-only resource backing types ───────────────────────────────────────

/// Host-retained layer: the WasiDrawable (swappable inner + live
/// matrix/clip/alpha/shadow, applied at replay). Captured recordings keep
/// the underlying SkDrawable alive via skia's refcount, so dropping the
/// guest handle never invalidates captures (the contract's lifetime rule).
pub struct LayerRes(pub WasiDrawable);
// SAFETY: single-threaded store; same justification as CanvasRes.
unsafe impl Send for LayerRes {}

/// Setter-form builder, BUFFERED: skparagraph wants the paragraph style
/// at construction, but 0.0.2 lets setters arrive any time before build —
/// so ops accumulate and the skia builder is constructed at build().
#[derive(Default)]
pub struct ParagraphBuilder002Res {
    default_style: Option<wit_layout::TextStyle>,
    align: Option<wit_layout::Align>,
    direction: Option<wit_layout::TextDirection>,
    max_lines: u32,
    ellipsis: String,
    ops: Vec<BuilderOp>,
}

pub enum BuilderOp {
    PushStyle(wit_layout::TextStyle),
    PopStyle,
    AddText(String),
}

// ─── conversions ─────────────────────────────────────────────────────────────

fn blend_mode(m: wit_types::BlendMode) -> skia_safe::BlendMode {
    use skia_safe::BlendMode as B;
    use wit_types::BlendMode as W;
    match m {
        W::SrcOver => B::SrcOver,
        W::Src => B::Src,
        W::Dst => B::Dst,
        W::DstOver => B::DstOver,
        W::SrcIn => B::SrcIn,
        W::DstIn => B::DstIn,
        W::SrcOut => B::SrcOut,
        W::DstOut => B::DstOut,
        W::SrcAtop => B::SrcATop,
        W::DstAtop => B::DstATop,
        W::Xor => B::Xor,
        W::Plus => B::Plus,
        W::Modulate => B::Modulate,
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
        W::Hue => B::Hue,
        W::Saturation => B::Saturation,
        W::Color => B::Color,
        W::Luminosity => B::Luminosity,
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
    match &p.filter {
        Some(wit_types::ColorFilter::Blend(cb)) => {
            if let Some(cf) =
                skia_safe::color_filters::blend(color(cb.color), blend_mode(cb.mode))
            {
                sp.set_color_filter(cf);
            }
        }
        Some(wit_types::ColorFilter::Invert) => {
            let matrix = [
                -1f32,  0f32,  0f32, 0f32, 1f32,
                 0f32, -1f32,  0f32, 0f32, 1f32,
                 0f32,  0f32, -1f32, 0f32, 1f32,
                 0f32,  0f32,  0f32, 1f32, 0f32,
            ];
            sp.set_color_filter(skia_safe::color_filters::matrix_row_major(&matrix, None));
        }
        None => {}
    }
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

// ─── types ───────────────────────────────────────────────────────────────────

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

// ─── draw ────────────────────────────────────────────────────────────────────

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
        local: Option<wit_types::Transform>,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let lm = local.map(matrix);
        let shader = skia_safe::gradient_shader::linear(
            (
                skia_safe::Point::new(start.x, start.y),
                skia_safe::Point::new(end.x, end.y),
            ),
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            None,
            lm.as_ref(),
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
        local: Option<wit_types::Transform>,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let lm = local.map(matrix);
        let shader = skia_safe::gradient_shader::radial(
            skia_safe::Point::new(center.x, center.y),
            radius,
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            None,
            lm.as_ref(),
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
        local: Option<wit_types::Transform>,
    ) -> wasmtime::Result<Resource<ShaderRes>> {
        let (colors, positions) = stops_to_arrays(&stops);
        let lm = local.map(matrix);
        let shader = skia_safe::gradient_shader::sweep(
            skia_safe::Point::new(center.x, center.y),
            colors.as_slice(),
            Some(positions.as_slice()),
            tile_mode(tile),
            (start_angle, end_angle),
            None,
            lm.as_ref(),
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
            CanvasRes::Recording(_) => 0.0,
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

// ─── glyphs ──────────────────────────────────────────────────────────────────

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

// ─── layout (union text-style + buffered setter-form builder) ────────────────

fn layout_text_style(
    renderer: &mut crate::canvas_impl::SkiaRenderer,
    s: &wit_layout::TextStyle,
) -> skia_safe::textlayout::TextStyle {
    let mut ts = skia_safe::textlayout::TextStyle::new();
    ts.set_font_size(s.size);
    ts.set_color(color(s.color));
    let weight = skia_safe::font_style::Weight::from(s.weight.clamp(1, 1000) as i32);
    let slant = if s.italic {
        skia_safe::font_style::Slant::Italic
    } else {
        skia_safe::font_style::Slant::Upright
    };
    ts.set_font_style(skia_safe::FontStyle::new(
        weight,
        skia_safe::font_style::Width::NORMAL,
        slant,
    ));
    if s.letter_spacing != 0.0 {
        ts.set_letter_spacing(s.letter_spacing);
    }
    if s.line_height > 0.0 {
        ts.set_height(s.line_height);
        ts.set_height_override(true);
    }
    if s.baseline_shift != 0.0 {
        ts.set_baseline_shift(s.baseline_shift);
    }
    if let Some(d) = &s.decoration {
        use skia_safe::textlayout as tl;
        let mut ty = tl::TextDecoration::NO_DECORATION;
        if d.underline {
            ty |= tl::TextDecoration::UNDERLINE;
        }
        if d.overline {
            ty |= tl::TextDecoration::OVERLINE;
        }
        if d.line_through {
            ty |= tl::TextDecoration::LINE_THROUGH;
        }
        ts.set_decoration_type(ty);
        ts.set_decoration_color(if d.color != 0 {
            color(d.color)
        } else {
            color(s.color)
        });
        ts.set_decoration_style(match d.style {
            wit_layout::DecorationLineStyle::Solid => tl::TextDecorationStyle::Solid,
            wit_layout::DecorationLineStyle::Double => tl::TextDecorationStyle::Double,
            wit_layout::DecorationLineStyle::Dotted => tl::TextDecorationStyle::Dotted,
            wit_layout::DecorationLineStyle::Dashed => tl::TextDecorationStyle::Dashed,
            wit_layout::DecorationLineStyle::Wavy => tl::TextDecorationStyle::Wavy,
        });
        ts.set_decoration_thickness_multiplier(if d.thickness > 0.0 { d.thickness } else { 1.0 });
    }
    for sh in &s.shadows {
        ts.add_shadow(skia_safe::textlayout::TextShadow::new(
            color(sh.color),
            skia_safe::Point::new(sh.offset.x, sh.offset.y),
            sh.sigma as f64,
        ));
    }
    if let Some(bg) = s.background {
        let mut bp = skia_safe::Paint::default();
        bp.set_color(color(bg));
        ts.set_background_paint(&bp);
    }
    if !s.family.is_empty() {
        ts.set_font_families(&[s.family.as_str()]);
        if s.family.starts_with('/')
            || matches!(
                s.family.as_str(),
                "Noto Serif" | "NotoSerif" | "DejaVu Serif" | "Times New Roman"
                    | "Noto Sans Mono" | "NotoSansMono" | "DejaVu Sans Mono"
                    | "Consolas" | "Roboto Mono" | "RobotoMono"
            )
        {
            let tf = renderer.get_typeface(&s.family, s.weight >= 600, s.italic);
            ts.set_typeface(Some(tf));
        }
    }
    ts
}

impl wit_layout::Host for HostState {}

impl wit_layout::HostParagraphBuilder for HostState {
    fn new(
        &mut self,
        default_style: wit_layout::TextStyle,
    ) -> wasmtime::Result<Resource<ParagraphBuilder002Res>> {
        Ok(self.table.push(ParagraphBuilder002Res {
            default_style: Some(default_style),
            ..Default::default()
        })?)
    }

    fn set_align(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        a: wit_layout::Align,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.align = Some(a);
        Ok(())
    }
    fn set_direction(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        d: wit_layout::TextDirection,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.direction = Some(d);
        Ok(())
    }
    fn set_max_lines(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        n: u32,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.max_lines = n;
        Ok(())
    }
    fn set_ellipsis(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        e: String,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.ellipsis = e;
        Ok(())
    }

    fn push_style(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        style: wit_layout::TextStyle,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.ops.push(BuilderOp::PushStyle(style));
        Ok(())
    }
    fn pop_style(&mut self, self_: Resource<ParagraphBuilder002Res>) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.ops.push(BuilderOp::PopStyle);
        Ok(())
    }
    fn add_text(
        &mut self,
        self_: Resource<ParagraphBuilder002Res>,
        text: String,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.ops.push(BuilderOp::AddText(text));
        Ok(())
    }

    fn build(
        &mut self,
        b: Resource<ParagraphBuilder002Res>,
    ) -> wasmtime::Result<Resource<ParagraphRes>> {
        let state = self.table.delete(b)?;
        let default_style = state
            .default_style
            .ok_or_else(|| wasmtime::Error::msg("paragraph-builder: missing default style"))?;
        let ts = layout_text_style(&mut self.renderer, &default_style);
        let mut style = skia_safe::textlayout::ParagraphStyle::new();
        style.set_text_style(&ts);
        if let Some(a) = state.align {
            style.set_text_align(match a {
                wit_layout::Align::Start => skia_safe::textlayout::TextAlign::Start,
                wit_layout::Align::Center => skia_safe::textlayout::TextAlign::Center,
                wit_layout::Align::End => skia_safe::textlayout::TextAlign::End,
                wit_layout::Align::Justify => skia_safe::textlayout::TextAlign::Justify,
            });
        }
        if let Some(d) = state.direction {
            style.set_text_direction(match d {
                wit_layout::TextDirection::Ltr => skia_safe::textlayout::TextDirection::LTR,
                wit_layout::TextDirection::Rtl => skia_safe::textlayout::TextDirection::RTL,
            });
        }
        if state.max_lines > 0 {
            style.set_max_lines(Some(state.max_lines as usize));
        }
        if !state.ellipsis.is_empty() {
            style.set_ellipsis(state.ellipsis.as_str());
        }
        let fc = self.renderer.font_collection.clone();
        let mut builder = skia_safe::textlayout::ParagraphBuilder::new(&style, fc);
        for op in &state.ops {
            match op {
                BuilderOp::PushStyle(s) => {
                    let ts = layout_text_style(&mut self.renderer, s);
                    builder.push_style(&ts);
                }
                BuilderOp::PopStyle => {
                    builder.pop();
                }
                BuilderOp::AddText(t) => {
                    builder.add_text(t);
                }
            }
        }
        let paragraph = builder.build();
        Ok(self
            .table
            .push(ParagraphRes(std::rc::Rc::new(std::cell::RefCell::new(paragraph))))?)
    }

    fn drop(&mut self, rep: Resource<ParagraphBuilder002Res>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit_layout::HostParagraph for HostState {
    fn layout(&mut self, self_: Resource<ParagraphRes>, width: f32) -> wasmtime::Result<()> {
        self.table.get(&self_)?.0.borrow_mut().layout(width);
        Ok(())
    }

    fn paint(
        &mut self,
        self_: Resource<ParagraphRes>,
        canvas: Resource<CanvasRes>,
        at: wit_types::Point,
    ) -> wasmtime::Result<()> {
        let para = self.table.get(&self_)?.0.clone();
        with_canvas(self, &canvas, |c| {
            para.borrow().paint(c, (at.x, at.y));
        })
    }

    fn height(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<f32> {
        Ok(self.table.get(&self_)?.0.borrow().height())
    }
    fn max_intrinsic_width(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<f32> {
        Ok(self.table.get(&self_)?.0.borrow().max_intrinsic_width())
    }
    fn min_intrinsic_width(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<f32> {
        Ok(self.table.get(&self_)?.0.borrow().min_intrinsic_width())
    }
    fn alphabetic_baseline(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<f32> {
        Ok(self.table.get(&self_)?.0.borrow().alphabetic_baseline())
    }
    fn ideographic_baseline(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<f32> {
        Ok(self.table.get(&self_)?.0.borrow().ideographic_baseline())
    }
    fn line_count(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&self_)?.0.borrow().line_number() as u32)
    }
    fn did_exceed_max_lines(&mut self, self_: Resource<ParagraphRes>) -> wasmtime::Result<bool> {
        Ok(self.table.get(&self_)?.0.borrow().did_exceed_max_lines())
    }

    fn lines(
        &mut self,
        self_: Resource<ParagraphRes>,
    ) -> wasmtime::Result<Vec<wit_layout::LineMetrics>> {
        let para = self.table.get(&self_)?.0.clone();
        let para = para.borrow();
        Ok(para
            .get_line_metrics()
            .iter()
            .map(|lm| wit_layout::LineMetrics {
                start_offset: lm.start_index as u32,
                end_offset: lm.end_index as u32,
                end_excluding_whitespace: lm.end_excluding_whitespaces as u32,
                end_including_newline: lm.end_including_newline as u32,
                hard_break: lm.hard_break,
                ascent: lm.ascent as f32,
                descent: lm.descent as f32,
                unscaled_ascent: lm.unscaled_ascent as f32,
                height: lm.height as f32,
                width: lm.width as f32,
                left: lm.left as f32,
                baseline: lm.baseline as f32,
                line_number: lm.line_number as u32,
            })
            .collect())
    }

    fn selection_boxes(
        &mut self,
        self_: Resource<ParagraphRes>,
        start: u32,
        end: u32,
        height: wit_layout::RectHeightStyle,
        width: wit_layout::RectWidthStyle,
    ) -> wasmtime::Result<Vec<wit_layout::TextBox>> {
        use skia_safe::textlayout::{RectHeightStyle, RectWidthStyle, TextDirection};
        let height_style = match height {
            wit_layout::RectHeightStyle::Tight => RectHeightStyle::Tight,
            wit_layout::RectHeightStyle::Max => RectHeightStyle::Max,
            wit_layout::RectHeightStyle::IncludeLineSpacingMiddle => {
                RectHeightStyle::IncludeLineSpacingMiddle
            }
            wit_layout::RectHeightStyle::IncludeLineSpacingTop => {
                RectHeightStyle::IncludeLineSpacingTop
            }
            wit_layout::RectHeightStyle::IncludeLineSpacingBottom => {
                RectHeightStyle::IncludeLineSpacingBottom
            }
            wit_layout::RectHeightStyle::Strut => RectHeightStyle::Strut,
        };
        let width_style = match width {
            wit_layout::RectWidthStyle::Tight => RectWidthStyle::Tight,
            wit_layout::RectWidthStyle::Max => RectWidthStyle::Max,
        };
        let para = self.table.get(&self_)?.0.clone();
        let para = para.borrow();
        Ok(para
            .get_rects_for_range(start as usize..end as usize, height_style, width_style)
            .iter()
            .map(|tb| wit_layout::TextBox {
                rect: wit_types::Rect {
                    x: tb.rect.left,
                    y: tb.rect.top,
                    width: tb.rect.width(),
                    height: tb.rect.height(),
                },
                direction: match tb.direct {
                    TextDirection::LTR => wit_layout::TextDirection::Ltr,
                    TextDirection::RTL => wit_layout::TextDirection::Rtl,
                },
            })
            .collect())
    }

    fn offset_at(
        &mut self,
        self_: Resource<ParagraphRes>,
        at: wit_types::Point,
    ) -> wasmtime::Result<u32> {
        let para = self.table.get(&self_)?.0.clone();
        let pos = para.borrow().get_glyph_position_at_coordinate((at.x, at.y));
        Ok(pos.position.max(0) as u32)
    }

    fn word_boundary(
        &mut self,
        self_: Resource<ParagraphRes>,
        offset: u32,
    ) -> wasmtime::Result<(u32, u32)> {
        let para = self.table.get(&self_)?.0.clone();
        let range = para.borrow().get_word_boundary(offset);
        Ok((range.start as u32, range.end as u32))
    }

    fn drop(&mut self, rep: Resource<ParagraphRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ─── scene (host-retained layers over the WasiDrawable machinery) ────────────

impl wit_scene::Host for HostState {
    fn draw_layer(
        &mut self,
        canvas: Resource<CanvasRes>,
        l: Resource<LayerRes>,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&l)?.0.as_ptr();
        if raw_d.is_null() {
            return Ok(());
        }
        with_canvas(self, &canvas, |c| {
            // Same transparent-wrapper cast as the legacy draw-drawable —
            // skia recordings sk_ref the drawable, which IS the contract's
            // "host keeps the layer alive until the last capture drops".
            let canvas_ptr = c as *const skia_safe::Canvas as *mut std::os::raw::c_void;
            unsafe { wasi_drawable_ffi::wasi_canvas_draw_drawable(canvas_ptr, raw_d) };
        })
    }
}

impl wit_scene::HostLayer for HostState {
    fn new(&mut self, _g: Resource<GraphicsRes>) -> wasmtime::Result<Resource<LayerRes>> {
        Ok(self.table.push(LayerRes(WasiDrawable::new()))?)
    }

    fn set_content(
        &mut self,
        self_: Resource<LayerRes>,
        recording: Resource<CanvasRes>,
    ) -> wasmtime::Result<()> {
        let entry = self.table.delete(recording)?;
        match entry {
            CanvasRes::Recording(mut recorder) => {
                // finish_recording_as_drawable keeps captured child layers
                // LIVE (a picture snapshot would freeze them) — the
                // contract's nested-layer rule.
                let inner = recorder.finish_recording_as_drawable();
                self.table.get_mut(&self_)?.0.set_inner(inner.as_ref());
                Ok(())
            }
            _ => Err(wasmtime::Error::msg("scene.set-content on a non-recording canvas")),
        }
    }

    fn set_bounds(
        &mut self,
        self_: Resource<LayerRes>,
        bounds: wit_types::Rect,
    ) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.0.set_bounds(
            bounds.x,
            bounds.y,
            bounds.x + bounds.width,
            bounds.y + bounds.height,
        );
        Ok(())
    }

    fn set_transform(
        &mut self,
        self_: Resource<LayerRes>,
        t: wit_types::Transform,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        unsafe {
            wasi_drawable_ffi::wasi_drawable_set_matrix(
                raw_d, t.m00, t.m01, t.m02, t.m10, t.m11, t.m12, t.m20, t.m21, t.m22,
            );
        }
        Ok(())
    }

    fn set_alpha(&mut self, self_: Resource<LayerRes>, alpha: u8) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        unsafe { wasi_drawable_ffi::wasi_drawable_set_alpha(raw_d, alpha as f32 / 255.0) };
        Ok(())
    }

    fn set_clip_rect(
        &mut self,
        self_: Resource<LayerRes>,
        r: wit_types::Rect,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        unsafe {
            wasi_drawable_ffi::wasi_drawable_set_clip_rect(
                raw_d, r.x, r.y, r.x + r.width, r.y + r.height, anti_alias,
            );
        }
        Ok(())
    }

    fn set_clip_rounded_rect(
        &mut self,
        self_: Resource<LayerRes>,
        rr: wit_types::RoundedRect,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        let radii = [
            rr.top_left.x, rr.top_left.y,
            rr.top_right.x, rr.top_right.y,
            rr.bottom_right.x, rr.bottom_right.y,
            rr.bottom_left.x, rr.bottom_left.y,
        ];
        unsafe {
            wasi_drawable_ffi::wasi_drawable_set_clip_rrect(
                raw_d,
                rr.rect.x,
                rr.rect.y,
                rr.rect.x + rr.rect.width,
                rr.rect.y + rr.rect.height,
                radii.as_ptr(),
                anti_alias,
            );
        }
        Ok(())
    }

    fn set_clip_path(
        &mut self,
        self_: Resource<LayerRes>,
        path: String,
        anti_alias: bool,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        if let Some(p) = svg_path(&path, wit_types::FillRule::Nonzero) {
            // &Path → *const SkPath: Handle<SkPath> is a transparent
            // single-field wrapper (the documented draw-drawable cast rule).
            let path_ptr = &p as *const skia_safe::Path as *const std::os::raw::c_void;
            unsafe { wasi_drawable_ffi::wasi_drawable_set_clip_path(raw_d, path_ptr, anti_alias) };
        }
        Ok(())
    }

    fn clear_clip(&mut self, self_: Resource<LayerRes>) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        unsafe { wasi_drawable_ffi::wasi_drawable_clear_clip(raw_d) };
        Ok(())
    }

    fn set_shadow_elevation(
        &mut self,
        self_: Resource<LayerRes>,
        elevation: f32,
    ) -> wasmtime::Result<()> {
        let raw_d = self.table.get(&self_)?.0.as_ptr();
        unsafe { wasi_drawable_ffi::wasi_drawable_set_shadow_elevation(raw_d, elevation) };
        Ok(())
    }

    fn drop(&mut self, rep: Resource<LayerRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ─── embedding ───────────────────────────────────────────────────────────────

impl wit_embedding::Host for HostState {
    fn get_context(&mut self) -> wasmtime::Result<Resource<CanvasContextRes>> {
        Ok(self.table.push(CanvasContextRes)?)
    }
}

impl wit_embedding::HostCanvasContext for HostState {
    fn graphics(
        &mut self,
        _self_: Resource<CanvasContextRes>,
    ) -> wasmtime::Result<Resource<GraphicsRes>> {
        Ok(self.table.push(GraphicsRes)?)
    }

    fn get_current_buffer(
        &mut self,
        _self_: Resource<CanvasContextRes>,
    ) -> wasmtime::Result<Resource<CanvasRes>> {
        #[cfg(target_os = "android")]
        self.renderer.egl.make_current();
        let base = self.renderer.base_matrix;
        let c = self.renderer.canvas();
        c.reset_matrix();
        c.clear(skia_safe::Color::BLACK);
        c.concat(&base);
        Ok(self.table.push(CanvasRes::Main)?)
    }

    fn present(&mut self, _self_: Resource<CanvasContextRes>) -> wasmtime::Result<()> {
        self.renderer.flush_and_swap();
        Ok(())
    }

    fn drop(&mut self, rep: Resource<CanvasContextRes>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

// ─── linker registration ─────────────────────────────────────────────────────

pub fn add_to_linker(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> wasmtime::Result<()> {
    crate::wasi_canvas_002_bindings::CanvasHost::add_to_linker::<
        _,
        wasmtime::component::HasSelf<HostState>,
    >(linker, |s| s)
}
