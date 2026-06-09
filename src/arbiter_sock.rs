//! The arbiter control-socket path — resolved, never hardcoded per call site.
//!
//! The wandr-arbiter daemon binds one unix socket and every host module connects to
//! it (power/volume keys, IME, keyguard, alarm, notify, audio-focus, launcher …).
//! The path is the same cross-process contract honored by the arbiter (`wandr-arbiter`)
//! and the standalone `wandr-inputflinger` service: resolve `WANDR_ARBITER_SOCK` from
//! the environment, else fall back to the canonical default. This is the ONE place
//! the host crate names it (replacing the per-module `const ARBITER_SOCK_PATH`).

/// Canonical default when `WANDR_ARBITER_SOCK` is unset. Same literal the arbiter
/// owner and the C++ inputflinger service default to.
pub const ARBITER_SOCK_DEFAULT: &str = "/data/local/tmp/wandr-arbiter.sock";

/// The arbiter socket path to connect to: `$WANDR_ARBITER_SOCK` or the default.
pub fn arbiter_sock_path() -> String {
    std::env::var("WANDR_ARBITER_SOCK")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ARBITER_SOCK_DEFAULT.to_string())
}
