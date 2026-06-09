//! `wandr:keyguard/keyguard` host impl (M3). The keyguard guest calls `unlock()` on
//! the swipe-up gesture; the host forwards `unlock` to the arbiter (one-shot,
//! mirroring `notify_host_impl`/`alarm_host_impl`), which hides the lock screen +
//! restores the covered app.

use std::io::Write;
use std::os::unix::net::UnixStream;

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
}
