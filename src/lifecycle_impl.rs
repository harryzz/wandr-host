use crate::bindings::my::skiko_gfx::lifecycle::{Host, State};

#[derive(Default)]
pub struct LifecycleState {
    /// Current observed state. Default = initialized (before the activity
    /// has run its first onResume).
    pub current: State,
    /// Transition queued by the host that should be fired into the guest
    /// after the next render_frame (so the guest has had a chance to
    /// register its observer in main()).
    pub pending: Option<State>,
}

// State is a generated WIT enum; provide a Default so the field can use it.
impl Default for State {
    fn default() -> Self {
        State::Initialized
    }
}

impl Host for crate::HostState {
    fn get_state(&mut self) -> State {
        self.lifecycle.current
    }
}
