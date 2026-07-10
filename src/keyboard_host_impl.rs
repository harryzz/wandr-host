//! `my:skiko-gfx/keyboard` WIT impl — task 47 step 3b.
//!
//! Mirror of `ime_host_impl.rs` (step 2) for the OPPOSITE direction:
//! where `ime` is the host trait an EDITOR-bearing guest uses to
//! report focus changes, `keyboard` is what an IME-bearing guest
//! (e.g. `wandr.ime.keyboard`) uses to push keystrokes to the
//! currently-focused editor.
//!
//! `send_key_event` forwards each call to the arbiter's
//! `ime-send-key-event` socket cmd. The arbiter then looks up the
//! focused editor's pid and pushes a `key-event` line down its
//! per-host control socket (task 47 step 3a). The editor's
//! `dispatch_key_v2` dispatches it as a synthetic Compose
//! `KeyEvent` — same code path a hardware key press takes.

use std::io::Write;
use crate::arbiter_sock::UnixStream;

use crate::keyboard_send_bindings::wandr::ime::keyboard_send::Host;

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

impl Host for crate::HostState {
    fn send_key_event(&mut self, code_point: u32, key_id: u32, action: u8) {
        let action_str = match action {
            0 => "down",
            1 => "up",
            other => {
                log::warn!(
                    "keyboard-host: send-key-event got bad action={other}, defaulting to down"
                );
                "down"
            }
        };
        let line = format!(
            "ime-send-key-event {code_point} {key_id} {action_str}\n"
        );
        if let Err(e) = send_oneshot(&line) {
            log::warn!(
                "keyboard-host: send-key-event forward failed: {e:#}. \
                 code_point={code_point} key_id={key_id} action={action_str}",
            );
            return;
        }
        log::debug!(
            "keyboard-host: forwarded ime-send-key-event {code_point} {key_id} {action_str}"
        );
    }

    fn request_overlay_height(&mut self, height_px: u32) {
        // Task 47 step 3c — the IME guest declares its preferred
        // panel height. Queue the request for the standalone render
        // loop to pick up next frame; the loop calls
        // `SfSurface::resize_overlay`, which forwards to the libgui
        // shim and flushes the ANativeWindow buffer geometry. No-op
        // (logged inside SfSurface) if the surface isn't an overlay.
        log::debug!(
            "keyboard-host: request-overlay-height({height_px}) queued"
        );
        #[cfg(target_os = "android")]
        crate::sf_surface::request_overlay_resize(height_px as i32);
        // Task 68 — make the IME the source of truth for the keyboard inset: tell
        // the arbiter our portrait-reference height so it pushes the matching
        // `keyboard-inset` to focused editors (the editor host scales it per
        // orientation). Fire-and-forget; the arbiter defaults if we never report.
        if let Err(e) = send_oneshot(&format!("ime-overlay-height {height_px}\n")) {
            log::debug!("keyboard-host: ime-overlay-height report failed: {e:#}");
        }
    }
}

/// Same one-shot connect pattern as `ime_host_impl::send_oneshot`.
/// Open → write one line → half-close → drain reply → close.
fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    use std::io::Read;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    Ok(())
}
