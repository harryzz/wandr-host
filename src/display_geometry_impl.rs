//! Unified display/surface geometry — `my:skiko-gfx/display` WIT impl (task 71).
//!
//! Read-only view of the three nested rectangles (display ⊃ content ⊃ safe)
//! plus the coarse device orientation, exposed identically to fullscreen apps
//! and overlays (the IME). All inputs already live in `SkiaRenderer` —
//! orientation, chrome insets, soft-keyboard reservation — so this module is a
//! thin projection over [`crate::canvas_impl::SkiaRenderer`]'s geometry helpers
//! (`display_size` / `content_size` / `safe_size` / `orientation_code`).
//!
//! Geometry only — input routing stays in `keyboard` (`keyboard_host_impl`) and
//! overlay sizing stays on `keyboard.request-overlay-height` (the host keeps
//! owning the per-orientation px scaling; task 71 did NOT move that to percent).

use crate::bindings::my::skiko_gfx::display::{Host, Orientation, Size};

/// One-shot sanity log (process-global) — prints the geometry snapshot the
/// first time any guest reads a size, so on-device verification can confirm
/// `safe ≤ content ≤ display` and that the keyboard inset == content − safe.
static GEOM_LOG: std::sync::Once = std::sync::Once::new();

impl crate::HostState {
    fn log_geometry_once(&self) {
        GEOM_LOG.call_once(|| {
            let (dw, dh) = self.renderer.display_size();
            let (cw, ch) = self.renderer.content_size();
            let (sw, sh) = self.renderer.safe_size();
            log::info!(
                "display(task71): orient={} display={dw}x{dh} content={cw}x{ch} \
                 safe={sw}x{sh} (kb-inset≈{} px)",
                self.renderer.current_orient,
                ch.saturating_sub(sh),
            );
        });
    }
}

impl Host for crate::HostState {
    fn display_size(&mut self) -> Size {
        self.log_geometry_once();
        let (width, height) = self.renderer.display_size();
        Size { width, height }
    }

    fn content_size(&mut self) -> Size {
        self.log_geometry_once();
        let (width, height) = self.renderer.content_size();
        Size { width, height }
    }

    fn safe_size(&mut self) -> Size {
        self.log_geometry_once();
        let (width, height) = self.renderer.safe_size();
        Size { width, height }
    }

    fn current_orientation(&mut self) -> Orientation {
        match self.renderer.orientation_code() {
            1 => Orientation::Landscape,
            _ => Orientation::Portrait,
        }
    }
}
