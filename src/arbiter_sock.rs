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

// ── Cross-platform UnixStream ────────────────────────────────────────────────
// Real on unix (android + linux desktop); a stub on Windows, where there is no
// wandr-arbiter — `connect()` always fails, so every arbiter-forward path no-ops
// exactly as it does on desktop with the arbiter down. The stub only has to
// compile: the write/read/shutdown methods are never reached once connect errs.
#[cfg(unix)]
pub use std::os::unix::net::UnixStream;

#[cfg(not(unix))]
pub use win_stub::UnixStream;
#[cfg(not(unix))]
mod win_stub {
    use std::io::{self, Read, Write};
    use std::path::Path;
    pub struct UnixStream;
    impl UnixStream {
        pub fn connect<P: AsRef<Path>>(_p: P) -> io::Result<Self> {
            Err(io::Error::new(io::ErrorKind::NotFound, "no arbiter on Windows"))
        }
        pub fn shutdown(&self, _how: std::net::Shutdown) -> io::Result<()> { Ok(()) }
        pub fn set_read_timeout(&self, _d: Option<std::time::Duration>) -> io::Result<()> { Ok(()) }
        pub fn set_write_timeout(&self, _d: Option<std::time::Duration>) -> io::Result<()> { Ok(()) }
    }
    impl Read for UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> { Ok(0) }
    }
    impl Write for UnixStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> { Ok(buf.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
}
