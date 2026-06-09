use crate::bindings::my::skiko_gfx::clipboard::Host;

impl Host for crate::HostState {
    fn get_text(&mut self) -> String {
        self.clipboard.clone().unwrap_or_default()
    }
    fn set_text(&mut self, text: String) {
        self.clipboard = Some(text);
    }
    fn has_text(&mut self) -> bool {
        self.clipboard.is_some()
    }
    fn clear(&mut self) {
        self.clipboard = None;
    }
}
