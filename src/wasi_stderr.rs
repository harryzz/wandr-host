//! Route wasi guest stderr + host fd 2 to logcat on Android.
//!
//! Task 30 step 1 — capture the WASI preview1 adapter's `assert_fail`
//! message + line number that precedes the `unreachable` trap behind
//! the TooltipBox SIGILL.
//!
//! Two layers:
//!
//! 1. **wasi p2 stderr override** ([`LogcatStderr`]). wasmtime-wasi 44's
//!    `inherit_stderr` wraps `tokio::io::stderr()` in an
//!    `AsyncWriteStream` worker — guest `fd_write` returns after
//!    enqueueing bytes, and a separate tokio task drains the queue
//!    later. When the wasm guest then traps with `unreachable`, the
//!    SIGILL aborts the process before the worker flushes, so the
//!    assert message is lost. We override `StdoutStream::p2_stream` to
//!    return a synchronous stream that emits one `log::warn!` per line
//!    under the `wasi_stderr` tag from inside the host-call frame —
//!    the bytes hit logcat (via `__android_log_print`) before the
//!    guest resumes and traps.
//!
//! 2. **host fd 2 dup2** ([`redirect_stderr_to_logcat`]). Surfaces any
//!    plain Rust panic message that the default panic hook writes to
//!    `std::io::stderr()` (fd 2). Not on the wasi path, but catches
//!    host-side regressions that otherwise vanish on NativeActivity
//!    (where fd 2 is unrouted).

#[cfg(target_os = "android")]
pub fn redirect_stderr_to_logcat() {
    let mut pipe_fds: [libc::c_int; 2] = [0; 2];
    let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
    if rc != 0 {
        log::warn!("wasi_stderr: pipe() failed; fd 2 stays unrouted");
        return;
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    let rc = unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) };
    if rc < 0 {
        log::warn!("wasi_stderr: dup2 onto fd 2 failed; fd 2 stays unrouted");
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return;
    }
    unsafe { libc::close(write_fd) };

    let spawn = std::thread::Builder::new()
        .name("wasi-stderr-logcat".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            let mut carry: Vec<u8> = Vec::with_capacity(4096);
            loop {
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr() as *mut _, buf.len())
                };
                if n == 0 {
                    break;
                }
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    log::warn!("wasi_stderr: read failed: {err}; reader exiting");
                    break;
                }
                carry.extend_from_slice(&buf[..n as usize]);
                while let Some(nl) = carry.iter().position(|b| *b == b'\n') {
                    let mut line: Vec<u8> = carry.drain(..=nl).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let s = String::from_utf8_lossy(&line);
                    log::warn!(target: "wasi_stderr", "{}", s);
                }
                if carry.len() > 64 * 1024 {
                    let s = String::from_utf8_lossy(&carry);
                    log::warn!(target: "wasi_stderr", "[no-newline] {}", s);
                    carry.clear();
                }
            }
            if !carry.is_empty() {
                let s = String::from_utf8_lossy(&carry);
                log::warn!(target: "wasi_stderr", "[trailing] {}", s);
            }
            log::info!("wasi_stderr: reader thread exiting");
        });

    match spawn {
        Ok(_) => log::info!("wasi_stderr: fd 2 routed to logcat (tag wasi_stderr)"),
        Err(e) => log::warn!("wasi_stderr: failed to spawn reader thread: {e}"),
    }
}

// ── wasi p2 stderr override ──────────────────────────────────────────

use bytes::Bytes;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError, StreamResult};

/// `StdoutStream` for `WasiCtxBuilder::stderr(...)`. Each `write()` call
/// from the wasi adapter is routed *synchronously* into `log::warn!`
/// under the `wasi_stderr` tag — no worker task, no `fd 2`.
pub struct LogcatStderr;

impl IsTerminal for LogcatStderr {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for LogcatStderr {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        // The default `p2_stream` for `StdoutStream` would build an
        // `AsyncWriteStream` around this — exactly the async buffering
        // we are trying to avoid. We override `p2_stream` below, so
        // this is only kept to satisfy the trait. Return a trivial
        // AsyncWrite that consumes bytes silently; never invoked in
        // practice.
        Box::new(DummyAsyncWrite)
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(LogcatLineStream::new())
    }
}

struct LogcatLineStream {
    buf: Vec<u8>,
}

impl LogcatLineStream {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
        }
    }

    fn emit_lines(&mut self) {
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8_lossy(&line);
            log::warn!(target: "wasi_stderr", "{}", s);
        }
        if self.buf.len() > 64 * 1024 {
            let s = String::from_utf8_lossy(&self.buf);
            log::warn!(target: "wasi_stderr", "[no-newline] {}", s);
            self.buf.clear();
        }
    }
}

#[async_trait::async_trait]
impl Pollable for LogcatLineStream {
    async fn ready(&mut self) {}
}

impl OutputStream for LogcatLineStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.buf.extend_from_slice(&bytes);
        self.emit_lines();
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        if !self.buf.is_empty() {
            let s = String::from_utf8_lossy(&self.buf);
            log::warn!(target: "wasi_stderr", "{}", s);
            self.buf.clear();
        }
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(1 << 20)
    }
}

impl Drop for LogcatLineStream {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let s = String::from_utf8_lossy(&self.buf);
            log::warn!(target: "wasi_stderr", "[drop] {}", s);
        }
    }
}

struct DummyAsyncWrite;

impl tokio::io::AsyncWrite for DummyAsyncWrite {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

// Touch StreamError to keep imports honest in case the trait sig changes.
#[allow(dead_code)]
const _: fn() = || {
    let _ = StreamError::Closed;
};
