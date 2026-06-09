//! Lifecycle plumbing for the standalone (no-`NativeActivity`) path —
//! task 33 Step 5. Two small concerns bundled here:
//!
//!   1. **Signal-driven shutdown.** SIGTERM and SIGINT set an atomic flag
//!      the render loop polls each iteration. On set, the loop breaks
//!      and the caller fires `Destroyed` into the guest before returning.
//!   2. **Crash resilience.** A panic hook writes a JSON marker to
//!      `/data/local/tmp/wandr-host-crash.json` so the next launch (or a
//!      future init.rc service) can surface what happened. On clean exit
//!      we remove the marker. On startup we log + remove a prior marker.
//!
//! (A former third concern — a screen on/off watcher polling
//! `debug.tracing.screen_state` to drive guest Paused/Resumed — was removed.
//! That sysprop is SurfaceFlinger's debug echo of the last `setPowerMode`: it
//! goes stale with ART stopped and can't distinguish a transient proximity
//! blank from a real screen-off, so under --no-art it paused the foreground
//! guest on every in-call proximity blank. Guest lifecycle is now driven solely
//! by the arbiter's role transitions in the render loop — the arbiter is the
//! single screen/power authority.)

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const CRASH_MARKER: &str = "/data/local/tmp/wandr-host-crash.json";

// ── Signals ──────────────────────────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn shutdown_handler(_sig: libc::c_int) {
    // Async-signal-safe: no allocation, no logging, no locks.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = shutdown_handler as *const () as usize;
        // SA_RESTART so a SIGTERM mid-`read()` on the input fd doesn't
        // poison the polling loop; the handler just sets the flag and
        // the loop's next iteration sees it.
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);

        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                log::warn!(
                    "standalone: sigaction({sig}) failed: errno={}",
                    *libc::__errno()
                );
            }
        }
    }
}

pub fn should_shutdown() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

// ── Crash marker ─────────────────────────────────────────────────────

pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Forward to the prior hook first so logcat still gets the
        // formatted panic message + backtrace via android_logger.
        prev(info);
        write_crash_marker(&format!("{info}"));
    }));
}

fn write_crash_marker(panic_msg: &str) {
    // Best-effort, no `?` — we're inside a panic.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let escaped = panic_msg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    let body = format!(
        "{{\"ts\":{ts},\"panic\":\"{escaped}\"}}\n"
    );
    let _ = std::fs::write(CRASH_MARKER, body);
}

/// Log + remove a marker left by the prior run (if any).
pub fn drain_prior_crash_marker() {
    let p = Path::new(CRASH_MARKER);
    if !p.exists() { return; }
    match std::fs::read_to_string(p) {
        Ok(s) => log::error!("standalone: prior run crashed — {}", s.trim()),
        Err(e) => log::warn!("standalone: prior crash marker unreadable: {e}"),
    }
    let _ = std::fs::remove_file(p);
}

/// Remove the marker after a clean exit (no prior crash to report on
/// next launch).
pub fn record_clean_exit() {
    let _ = std::fs::remove_file(CRASH_MARKER);
}
