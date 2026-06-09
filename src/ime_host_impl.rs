//! `my:skiko-gfx/ime` WIT impl — task 47 step 2.
//!
//! Forwards editor-focus notifications from the guest (Compose's
//! `BasicTextField` focus changes) to the wandr-arbiter via its UNIX
//! socket. The arbiter then routes `on-editor-attached` /
//! `on-editor-detached` to the currently-active IME app — step 1
//! shipped the arbiter-side state + command parsing; this is the
//! editor-side hookup.
//!
//! Cross-process delivery the OTHER direction (IME app's
//! `commit-text` arriving in the focused guest's editor) is the
//! step-4 work — it needs a per-host control socket so the arbiter
//! can push events INTO running children. Step 2 is one-way only.

use std::io::Write;
use std::os::unix::net::UnixStream;

use crate::bindings::my::skiko_gfx::ime::Host;

// Where the arbiter listens — resolved via crate::arbiter_sock::arbiter_sock_path()
// ($WANDR_ARBITER_SOCK, else the canonical default). A production
// /dev/socket/wandr-arbiter lift would just change the default / env value.

impl Host for crate::HostState {
    fn notify_editor_attached(
        &mut self,
        input_type: String,
        hint: String,
        initial_text: String,
        selection_start: u32,
        selection_end: u32,
    ) {
        let pid = std::process::id();
        // Per-call connect, matching the one-shot pattern arbiter's
        // own `zygote_client` uses. No persistent socket — short-
        // lived guest events shouldn't hold connection state.
        //
        // Spaces in hint/initial_text would break the arbiter's
        // positional parser today; sanitize by replacing them with
        // a sentinel for step 2. Step 4's per-host control socket
        // can use a richer (binary / JSON) wire format.
        let safe_hint = hint.replace(' ', "_");
        let safe_text = initial_text.replace(' ', "_");
        let line = format!(
            "attach-editor {pid} {input_type} {safe_hint} {safe_text}\n"
        );
        if let Err(e) = send_oneshot(&line) {
            log::warn!(
                "ime-host: notify-editor-attached forward failed: {e:#}. \
                 selection=[{selection_start}..{selection_end}] \
                 (arbiter down or socket missing?)"
            );
            return;
        }
        log::info!(
            "ime-host: forwarded attach-editor pid={pid} input-type={input_type:?} \
             hint-len={} text-len={} selection=[{selection_start}..{selection_end}]",
            hint.len(),
            initial_text.len(),
        );
    }

    fn notify_editor_detached(&mut self) {
        let pid = std::process::id();
        let line = format!("detach-editor {pid}\n");
        if let Err(e) = send_oneshot(&line) {
            log::warn!("ime-host: notify-editor-detached forward failed: {e:#}");
            return;
        }
        log::info!("ime-host: forwarded detach-editor pid={pid}");
    }
}

/// Connect, write one line, read+drop the reply, close. We don't
/// surface the arbiter's `OK`/`ERR` back to the guest at MVP — the
/// guest's "I have focus" semantics are local; the arbiter's reply
/// is just for logging.
fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    // Half-close so the arbiter's BufReader sees EOF.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    // Drain (discard) the response. Sized small — arbiter's reply
    // is one line "OK ..." or "ERR ...".
    use std::io::Read;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    Ok(())
}
