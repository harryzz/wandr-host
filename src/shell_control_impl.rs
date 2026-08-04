//! wandr:ui-shell/shell-control — runtime immersive + orientation-lock overrides
//! (task 120). The guest calls these while foreground; the host forwards them to the
//! arbiter's EXISTING `set-immersive` / `set-orientation-lock` commands (the same ones
//! the host already sends from the manifest on foreground-change), so a media app can
//! hide the chrome + lock orientation during fullscreen playback and restore on exit.
//! The arbiter keys on the visible app, so these no-op unless this guest is foreground.

use crate::ui_shell_bindings::wandr::ui_shell::shell_control::Host;

/// Fire a one-line command at the arbiter control socket (best-effort; the arbiter may
/// be down on desktop, in which case immersive/orientation are simply not applied).
fn arbiter_send(line: String) {
    use std::io::{Read, Write};
    let Ok(mut s) = crate::arbiter_sock::UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())
    else {
        return;
    };
    let _ = s.write_all(line.as_bytes());
    let _ = s.flush();
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf);
}

impl Host for crate::HostState {
    fn set_immersive(&mut self, on: bool) {
        arbiter_send(format!("set-immersive {}\n", on as u8));
    }
    fn set_orientation_lock(&mut self, on: bool) {
        arbiter_send(format!("set-orientation-lock {}\n", on as u8));
    }
}
