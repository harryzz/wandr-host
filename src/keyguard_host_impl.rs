//! `wandr:keyguard/keyguard` host impl (M3). The keyguard guest calls `unlock()` on
//! the swipe-up gesture; the host forwards `unlock` to the arbiter (one-shot,
//! mirroring `notify_host_impl`/`alarm_host_impl`), which hides the lock screen +
//! restores the covered app.

use std::io::Write;
use crate::arbiter_sock::UnixStream;

use crate::keyguard_host_bindings::wandr::keyguard::keyguard::Host;

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

impl Host for crate::HostState {
    fn unlock(&mut self) {
        match send_oneshot("unlock\n") {
            Ok(()) => log::info!("keyguard-host: unlock → arbiter"),
            Err(e) => log::warn!("keyguard-host: unlock forward failed: {e:#} (arbiter down?)"),
        }
    }

    // ── Power menu (task 110) — forward each button tap to the arbiter. ──
    fn pm_dismiss(&mut self) {
        let _ = send_oneshot("pm-dismiss\n");
    }
    fn pm_lock(&mut self) {
        let _ = send_oneshot("pm-lock\n");
    }
    fn pm_power_off(&mut self) {
        let _ = send_oneshot("pm-poweroff\n");
    }
    fn pm_restart(&mut self) {
        let _ = send_oneshot("pm-restart\n");
    }
    fn pm_emergency(&mut self) {
        let _ = send_oneshot("pm-emergency\n");
    }
}
