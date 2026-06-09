use crate::bindings::my::skiko_gfx::paragraph::{Host, TextStyle as WitTextStyle};

impl Host for crate::HostState {
    fn create_paragraph_builder(&mut self, _width: f32) -> u32 {
        let style = skia_safe::textlayout::ParagraphStyle::new();
        let fc    = self.renderer.font_collection.clone();
        let builder = skia_safe::textlayout::ParagraphBuilder::new(&style, fc);
        let id = self.renderer.next_para_id;
        self.renderer.next_para_id += 1;
        self.renderer.para_builders.insert(id, builder);
        id
    }

    fn push_text_style(&mut self, id: u32, wit_style: WitTextStyle) {
        // Build the base TextStyle.
        let mut ts = wit_text_style_to_skia(&wit_style);

        // Task 41 — for the family aliases the host knows how to load
        // from /system/fonts/, fetch the SkTypeface directly via
        // SkiaRenderer's typeface cache + set it on the style. Skia's
        // FontCollection alone can't resolve these names on this
        // device (FontMgr returns zero-metrics typefaces), so without
        // this every FontFamily.Serif / .Monospace text falls back to
        // the default sans-serif.
        let family = String::from_utf8_lossy(&wit_style.font_family).into_owned();
        if !family.is_empty()
            && (family.starts_with('/')
                || matches!(
                    family.as_str(),
                    "Noto Serif" | "NotoSerif" | "DejaVu Serif" | "Times New Roman"
                    | "Noto Sans Mono" | "NotoSansMono" | "DejaVu Sans Mono"
                    | "Consolas" | "Roboto Mono" | "RobotoMono"
                ))
        {
            let bold = wit_style.font_weight >= 600;
            let italic = wit_style.italic;
            let tf = self.renderer.get_typeface(&family, bold, italic);
            ts.set_typeface(Some(tf));
        }

        let Some(builder) = self.renderer.para_builders.get_mut(&id) else { return };
        builder.push_style(&ts);
    }

    fn add_text(&mut self, id: u32, text: Vec<u8>) {
        let Some(builder) = self.renderer.para_builders.get_mut(&id) else { return };
        let s = String::from_utf8_lossy(&text);
        builder.add_text(s.as_ref());
    }

    fn pop_text_style(&mut self, id: u32) {
        let Some(builder) = self.renderer.para_builders.get_mut(&id) else { return };
        builder.pop();
    }

    fn build_paragraph(&mut self, id: u32) -> u32 {
        let Some(builder) = self.renderer.para_builders.get_mut(&id) else { return 0 };
        let para = builder.build();
        let para_id = self.renderer.next_para_id;
        self.renderer.next_para_id += 1;
        self.renderer.paragraphs.insert(para_id, para);
        para_id
    }

    fn drop_paragraph_builder(&mut self, id: u32) {
        self.renderer.para_builders.remove(&id);
    }

    fn layout(&mut self, id: u32, width: f32) {
        if let Some(para) = self.renderer.paragraphs.get_mut(&id) {
            para.layout(width);
        }
    }

    fn paint_paragraph(&mut self, id: u32, x: f32, y: f32) {
        self.renderer.draw_paragraph(id, x, y);
    }

    fn get_height(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id)
            .map(|p| p.height())
            .unwrap_or(0.0)
    }

    fn get_line_count(&mut self, id: u32) -> u32 {
        self.renderer.paragraphs.get(&id)
            .map(|p| p.line_number() as u32)
            .unwrap_or(0)
    }

    fn get_max_width(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id).map(|p| p.max_width()).unwrap_or(0.0)
    }

    fn get_max_intrinsic_width(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id).map(|p| p.max_intrinsic_width()).unwrap_or(0.0)
    }

    fn get_min_intrinsic_width(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id).map(|p| p.min_intrinsic_width()).unwrap_or(0.0)
    }

    fn get_alphabetic_baseline(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id).map(|p| p.alphabetic_baseline()).unwrap_or(0.0)
    }

    fn get_ideographic_baseline(&mut self, id: u32) -> f32 {
        self.renderer.paragraphs.get(&id).map(|p| p.ideographic_baseline()).unwrap_or(0.0)
    }

    fn prepare_rects_for_range(
        &mut self, id: u32, start: u32, end: u32,
        height_mode: u32, width_mode: u32,
    ) -> u32 {
        use skia_safe::textlayout::{RectHeightStyle, RectWidthStyle};
        let height_style = match height_mode {
            0 => RectHeightStyle::Tight,
            1 => RectHeightStyle::Max,
            2 => RectHeightStyle::IncludeLineSpacingMiddle,
            3 => RectHeightStyle::IncludeLineSpacingTop,
            4 => RectHeightStyle::IncludeLineSpacingBottom,
            _ => RectHeightStyle::Strut,
        };
        let width_style = if width_mode == 1 {
            RectWidthStyle::Max
        } else {
            RectWidthStyle::Tight
        };
        let s = start as usize;
        let e = end as usize;
        let boxes = self.renderer.paragraphs.get(&id)
            .map(|p| p.get_rects_for_range(s..e, height_style, width_style))
            .unwrap_or_default();
        self.renderer.para_rect_cache = boxes;
        self.renderer.para_rect_cache.len() as u32
    }

    fn get_cached_rect_left(&mut self, index: u32) -> f32 {
        self.renderer.para_rect_cache.get(index as usize)
            .map(|b| b.rect.left).unwrap_or(0.0)
    }
    fn get_cached_rect_top(&mut self, index: u32) -> f32 {
        self.renderer.para_rect_cache.get(index as usize)
            .map(|b| b.rect.top).unwrap_or(0.0)
    }
    fn get_cached_rect_right(&mut self, index: u32) -> f32 {
        self.renderer.para_rect_cache.get(index as usize)
            .map(|b| b.rect.right).unwrap_or(0.0)
    }
    fn get_cached_rect_bottom(&mut self, index: u32) -> f32 {
        self.renderer.para_rect_cache.get(index as usize)
            .map(|b| b.rect.bottom).unwrap_or(0.0)
    }
    fn get_cached_rect_direction(&mut self, index: u32) -> u32 {
        use skia_safe::textlayout::TextDirection;
        self.renderer.para_rect_cache.get(index as usize)
            .map(|b| match b.direct { TextDirection::LTR => 0u32, TextDirection::RTL => 1u32 })
            .unwrap_or(0)
    }

    fn get_glyph_position_at_coordinate(&mut self, id: u32, x: f32, y: f32) -> u32 {
        self.renderer.paragraphs.get(&id)
            .map(|p| {
                let pa = p.get_glyph_position_at_coordinate((x, y));
                if pa.position < 0 { 0 } else { pa.position as u32 }
            })
            .unwrap_or(0)
    }

    fn get_word_boundary_start(&mut self, id: u32, offset: u32) -> u32 {
        self.renderer.paragraphs.get(&id)
            .map(|p| p.get_word_boundary(offset).start as u32)
            .unwrap_or(offset)
    }

    fn get_word_boundary_end(&mut self, id: u32, offset: u32) -> u32 {
        self.renderer.paragraphs.get(&id)
            .map(|p| p.get_word_boundary(offset).end as u32)
            .unwrap_or(offset)
    }

    // ── Task 50 — per-line metrics ──────────────────────────────────────
    //
    // SkiaParagraph.getCursorRect uses `lineMetrics[lineForOffset(offset)]`
    // for the cursor's vertical position. Without these calls, skiko-wasi's
    // Paragraph.lineMetrics returns emptyArray() → binary search degenerates
    // to "always line 0" → cursor renders on line 1 for any selection
    // offset. See tasks/50-cursor-render-multiline.md.

    fn prepare_line_metrics(&mut self, id: u32) -> u32 {
        let Some(p) = self.renderer.paragraphs.get(&id) else {
            self.renderer.para_line_metrics_cache.clear();
            return 0;
        };
        let metrics = p.get_line_metrics();
        self.renderer.para_line_metrics_cache = metrics.iter().map(|lm| {
            crate::canvas_impl::CachedLineMetrics {
                start_index:                lm.start_index as u32,
                end_index:                  lm.end_index as u32,
                end_excluding_whitespaces:  lm.end_excluding_whitespaces as u32,
                end_including_newline:      lm.end_including_newline as u32,
                hard_break:                 lm.hard_break,
                ascent:                     lm.ascent,
                descent:                    lm.descent,
                unscaled_ascent:            lm.unscaled_ascent,
                height:                     lm.height,
                width:                      lm.width,
                left:                       lm.left,
                baseline:                   lm.baseline,
                line_number:                lm.line_number as u32,
            }
        }).collect();
        self.renderer.para_line_metrics_cache.len() as u32
    }

    fn get_cached_line_start_index(&mut self, idx: u32) -> u32 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.start_index).unwrap_or(0)
    }
    fn get_cached_line_end_index(&mut self, idx: u32) -> u32 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.end_index).unwrap_or(0)
    }
    fn get_cached_line_end_excluding_whitespaces(&mut self, idx: u32) -> u32 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.end_excluding_whitespaces).unwrap_or(0)
    }
    fn get_cached_line_end_including_newline(&mut self, idx: u32) -> u32 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.end_including_newline).unwrap_or(0)
    }
    fn get_cached_line_is_hard_break(&mut self, idx: u32) -> bool {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.hard_break).unwrap_or(false)
    }
    fn get_cached_line_ascent(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.ascent).unwrap_or(0.0)
    }
    fn get_cached_line_descent(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.descent).unwrap_or(0.0)
    }
    fn get_cached_line_unscaled_ascent(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.unscaled_ascent).unwrap_or(0.0)
    }
    fn get_cached_line_height(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.height).unwrap_or(0.0)
    }
    fn get_cached_line_width(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.width).unwrap_or(0.0)
    }
    fn get_cached_line_left(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.left).unwrap_or(0.0)
    }
    fn get_cached_line_baseline(&mut self, idx: u32) -> f64 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.baseline).unwrap_or(0.0)
    }
    fn get_cached_line_number(&mut self, idx: u32) -> u32 {
        self.renderer.para_line_metrics_cache.get(idx as usize)
            .map(|m| m.line_number).unwrap_or(0)
    }

    fn drop_paragraph(&mut self, id: u32) {
        self.renderer.paragraphs.remove(&id);
    }
}

fn wit_text_style_to_skia(s: &WitTextStyle) -> skia_safe::textlayout::TextStyle {
    let weight = skia_safe::font_style::Weight::from(s.font_weight as i32);
    let slant  = if s.italic {
        skia_safe::font_style::Slant::Italic
    } else {
        skia_safe::font_style::Slant::Upright
    };
    let font_style = skia_safe::FontStyle::new(
        weight, skia_safe::font_style::Width::NORMAL, slant,
    );
    let c = s.color;
    let color = skia_safe::Color::from_argb(
        (c >> 24) as u8, (c >> 16) as u8, (c >> 8) as u8, c as u8,
    );
    let mut ts = skia_safe::textlayout::TextStyle::new();
    ts.set_font_size(s.font_size);
    ts.set_font_style(font_style);
    ts.set_color(color);
    let family = String::from_utf8_lossy(&s.font_family).into_owned();
    if !family.is_empty() {
        ts.set_font_families(&[family]);
    }
    ts
}
