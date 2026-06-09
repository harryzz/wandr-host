//! Arbiter-driven foreground/background role for a forked wandr child
//! (task 46 step 4).
//!
//! The wandr-arbiter assigns at most one foreground app at a time. The
//! signal contract:
//!
//!   SIGUSR1     →  child becomes BACKGROUND
//!   SIGUSR2     →  child becomes FOREGROUND
//!   SIGRTMIN+1  →  child becomes OVERLAY-BEHIND (task 47 step 3c) —
//!                  visible, layered below the foreground (which is the
//!                  IME), lifecycle stays Resumed so the cursor keeps
//!                  blinking inside the focused editor.
//!
//! The handler just stores a new value into an atomic — async-signal-safe,
//! no allocation, no locks (mirrors the shutdown handler in
//! `lifecycle_standalone.rs`). The render loop reads the atomic per
//! frame and reacts: SF z-order via the libsf_surface shim, guest
//! lifecycle Paused/Resumed, focus-refresh throttle.
//!
//! Newly-forked children default to FOREGROUND on the assumption that
//! the arbiter's standard launch flow is "make this app the
//! foreground." If the arbiter wants a background-launch, it can
//! `kill(pid, SIGUSR1)` immediately after the OK fork ack.

use std::sync::atomic::{AtomicI32, Ordering};

/// Numeric values match the protocol — `0` is foreground (default),
/// `1` is background, `2` is overlay-behind (task 47 step 3c). Stored
/// in an `AtomicI32` so the signal handler can update it without locks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AppRole {
    Foreground = 0,
    Background = 1,
    OverlayBehind = 2,
}

static ROLE: AtomicI32 = AtomicI32::new(AppRole::Foreground as i32);

/// Real-time signal used for the overlay-behind transition (task 47
/// step 3c). `SIGRTMIN+1` is reserved by libc to avoid clashing with
/// the standard SIGRTMIN, which some runtimes use internally.
fn overlay_signal() -> libc::c_int {
    // bionic exposes SIGRTMIN as a macro that calls __libc_current_sigrtmin();
    // the `libc` crate routes through the same. SIGRTMIN+1 = first
    // user-available rt-signal on Linux/Android.
    unsafe { libc::SIGRTMIN() + 1 }
}

/// Read the current role. Cheap (atomic load); safe to call per-frame.
pub fn role() -> AppRole {
    match ROLE.load(Ordering::SeqCst) {
        1 => AppRole::Background,
        2 => AppRole::OverlayBehind,
        _ => AppRole::Foreground,
    }
}

pub fn is_foreground() -> bool {
    role() == AppRole::Foreground
}

extern "C" fn role_handler(sig: libc::c_int) {
    // Async-signal-safe: single atomic store, no allocation, no logging.
    // Logging the transition happens once-per-frame in the render loop
    // when it observes a change vs. its last-seen role.
    let overlay_sig = unsafe { libc::SIGRTMIN() + 1 };
    let new_role: i32 = if sig == libc::SIGUSR1 {
        AppRole::Background as i32
    } else if sig == libc::SIGUSR2 {
        AppRole::Foreground as i32
    } else if sig == overlay_sig {
        AppRole::OverlayBehind as i32
    } else {
        return;
    };
    ROLE.store(new_role, Ordering::SeqCst);
}

/// Install SIGUSR1/SIGUSR2/SIGRTMIN+1 handlers. Mirrors
/// `lifecycle_standalone::install_signal_handlers` shape — SA_RESTART
/// so an in-flight syscall isn't poisoned, empty mask, no sigaction
/// flags beyond restart.
pub fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = role_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);

        for sig in [libc::SIGUSR1, libc::SIGUSR2, overlay_signal()] {
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                log::warn!(
                    "app_role: sigaction({sig}) failed: errno={}",
                    *libc::__errno()
                );
            }
        }
    }
}
