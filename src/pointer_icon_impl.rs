use crate::chrome_bindings::wandr::chrome::pointer_icon::{Host, Kind};

/// No-op on Android touch devices. Hooked into the WIT so guests can call
/// it unconditionally; the actual cursor change would require
/// `View.setPointerIcon()` via JNI when targeting stylus / mouse surfaces.
impl Host for crate::HostState {
    fn set(&mut self, kind: Kind) {
        let _ = kind;
    }
}
