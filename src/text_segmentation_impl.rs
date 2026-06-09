use crate::bindings::my::skiko_gfx::text_segmentation::{BoundaryKind, Host};
use icu_segmenter::{
    GraphemeClusterSegmenter, LineSegmenter, SentenceSegmenter, WordSegmenter,
    options::{LineBreakOptions, SentenceBreakInvariantOptions, WordBreakInvariantOptions},
};

// icu_segmenter constructors are cheap (data is statically linked); no caching
// needed. Caching via OnceLock would require T: Sync but the segmenter
// internals use Rc, which isn't Sync.
fn boundaries(text: &str, kind: BoundaryKind) -> Vec<usize> {
    match kind {
        BoundaryKind::Grapheme => GraphemeClusterSegmenter::new().segment_str(text).collect(),
        BoundaryKind::Word     => WordSegmenter::new_auto(WordBreakInvariantOptions::default()).segment_str(text).collect(),
        BoundaryKind::Line     => LineSegmenter::new_auto(LineBreakOptions::default()).segment_str(text).collect(),
        BoundaryKind::Sentence => SentenceSegmenter::new(SentenceBreakInvariantOptions::default()).segment_str(text).collect(),
    }
}

impl Host for crate::HostState {
    fn next_boundary(&mut self, text: String, kind: BoundaryKind, start_offset: u32) -> u32 {
        let bounds = boundaries(&text, kind);
        let from = start_offset as usize;
        bounds.into_iter().find(|&b| b >= from).unwrap_or(text.len()) as u32
    }

    fn prev_boundary(&mut self, text: String, kind: BoundaryKind, end_offset: u32) -> u32 {
        let bounds = boundaries(&text, kind);
        let to = end_offset as usize;
        bounds.into_iter().rev().find(|&b| b <= to).unwrap_or(0) as u32
    }
}
