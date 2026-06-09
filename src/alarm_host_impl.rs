//! `wandr:alarm/scheduler` host impl (Arbiter Inc. 3c).
//!
//! A guest calls `schedule` / `cancel`; the host forwards to the arbiter's
//! `schedule-alarm` / `cancel-alarm` socket commands (one-shot, mirroring
//! `keyboard_host_impl::send_oneshot`). The arbiter stores the alarm, fires it
//! on its timer, and delivers `alarm-fired <id>` to this host's control socket
//! → the standalone loop calls the guest's `on-alarm` export.
//!
//! Owner identity: the host self-reports its pid (`std::process::id()` — the
//! zygote-forked child pid the arbiter registered), which the arbiter resolves
//! to the app-id. The fire time is absolute unix-ms (`now + delay`) computed
//! here from the same device wall clock the arbiter ticks on. `wake_kind` is
//! `gui` — relaunching a dead owner brings up its render loop to drain the
//! delivered `alarm-fired` (a headless poll kind is a Signal follow-up).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::alarm_host_bindings::wandr::alarm::scheduler::Host;

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Connect, write one line, drop the reply, close. Fire-and-forget (the arbiter
/// is the authority; if it's down the alarm just isn't scheduled).
fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

impl Host for crate::HostState {
    fn schedule(&mut self, id: u64, delay_ms: u64, repeat_ms: u64) {
        let when = now_unix_ms() + delay_ms;
        let pid = std::process::id();
        let line = format!("schedule-alarm {pid} {id} {when} {repeat_ms} gui\n");
        match send_oneshot(&line) {
            Ok(()) => log::info!(
                "alarm-host: scheduled id={id} delay={delay_ms}ms repeat={repeat_ms}ms (when={when})"
            ),
            Err(e) => log::warn!("alarm-host: schedule id={id} forward failed: {e:#} (arbiter down?)"),
        }
    }

    fn cancel(&mut self, id: u64) {
        let pid = std::process::id();
        if let Err(e) = send_oneshot(&format!("cancel-alarm {pid} {id}\n")) {
            log::warn!("alarm-host: cancel id={id} forward failed: {e:#}");
        }
    }
}
