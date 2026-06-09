//! `EventfdSignal` — wraps an `eventfd(2)` for cross-process signaling
//! between the host rust process and Android binder peer services.
//!
//! Linux's eventfd is an 8-byte counter living in a fd. Writers
//! `write(fd, &u64_le_bytes)` to increment; readers `read(fd, &mut buf)`
//! to consume. In the default (non-semaphore) mode used here, `read`
//! blocks until the counter is non-zero, then returns the full counter
//! value and resets it to zero in a single atomic step.
//!
//! IAAudioService hands the client an eventfd via `ParcelFileDescriptor`
//! to signal "data-ready" between control-plane RPCs and the shared-memory
//! ring buffer. The same primitive will be reused by CameraService
//! BufferQueue and Codec2 ports. Like `binder_shared_memory`, the
//! rsbinder `ParcelFileDescriptor → OwnedFd` extraction lives at the
//! call site so this primitive stays cross-platform-testable.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub struct EventfdSignal {
    fd: OwnedFd,
}

impl EventfdSignal {
    /// Wrap an eventfd handed to us over binder. The caller has already
    /// extracted the `OwnedFd` from its `ParcelFileDescriptor`.
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Create a fresh eventfd locally. Blocking reads — `wait()` parks
    /// until a notifier writes. Use this for tests and for cases where
    /// the host needs to signal itself.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn create_local(initial_count: u32) -> io::Result<Self> {
        Self::create_local_with_flags(initial_count, 0)
    }

    /// Create a fresh non-blocking eventfd. `wait_nonblocking()` returns
    /// `Ok(None)` instead of parking when the counter is zero — what
    /// the audio render path wants so a quiet frame doesn't stall.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn create_local_nonblocking(initial_count: u32) -> io::Result<Self> {
        Self::create_local_with_flags(initial_count, libc::EFD_NONBLOCK)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn create_local_with_flags(initial_count: u32, flags: libc::c_int) -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: eventfd is a plain syscall returning a new fd or -1.
        let raw = unsafe { libc::eventfd(initial_count, flags) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a freshly-created fd we own exclusively; wrapping
        // it in OwnedFd transfers ownership to us.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd })
    }

    /// Increment the eventfd counter by `count`. Writing `u64::MAX` is
    /// reserved by the kernel and returns `EINVAL`; callers should pick
    /// realistic counts (typically 1).
    pub fn notify(&self, count: u64) -> io::Result<()> {
        // eventfd treats the 8-byte buffer as a native-endian u64.
        let bytes = count.to_ne_bytes();
        // SAFETY: write(2) with a valid fd, valid buffer ptr, and length 8.
        let n = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                8,
            )
        };
        if n == 8 {
            Ok(())
        } else if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("eventfd partial write: wrote {n} of 8 bytes"),
            ))
        }
    }

    /// Block until the counter is non-zero, then return its value and
    /// atomically reset it to zero. Returns `EINTR` if interrupted by
    /// a signal — caller's responsibility to retry if desired.
    pub fn wait(&self) -> io::Result<u64> {
        let mut bytes = [0u8; 8];
        // SAFETY: read(2) with a valid fd, valid buffer ptr, and length 8.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                bytes.as_mut_ptr() as *mut libc::c_void,
                8,
            )
        };
        if n == 8 {
            Ok(u64::from_ne_bytes(bytes))
        } else if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("eventfd partial read: got {n} of 8 bytes"),
            ))
        }
    }

    /// Non-blocking variant. Returns `Ok(Some(n))` if the counter was
    /// non-zero (and consumed), `Ok(None)` if it was zero and the read
    /// would otherwise block, `Err(_)` on a real error. Requires the
    /// underlying fd to have been opened with `EFD_NONBLOCK` (use
    /// [`Self::create_local_nonblocking`]); a blocking fd will simply
    /// park inside `read` instead of returning `EAGAIN`.
    pub fn wait_nonblocking(&self) -> io::Result<Option<u64>> {
        let mut bytes = [0u8; 8];
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                bytes.as_mut_ptr() as *mut libc::c_void,
                8,
            )
        };
        if n == 8 {
            return Ok(Some(u64::from_ne_bytes(bytes)));
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EAGAIN) => Ok(None),
                // EWOULDBLOCK is the same as EAGAIN on Linux/Android but
                // some libc bindings expose both — handle defensively.
                #[allow(unreachable_patterns)]
                Some(libc::EWOULDBLOCK) => Ok(None),
                _ => Err(err),
            };
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("eventfd partial read: got {n} of 8 bytes"),
        ))
    }

    pub fn as_raw_fd(&self) -> RawFd { self.fd.as_raw_fd() }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn notify_then_wait_returns_count() {
        let ev = EventfdSignal::create_local(0).expect("eventfd");
        ev.notify(5).expect("notify");
        assert_eq!(ev.wait().expect("wait"), 5);
    }

    #[test]
    fn notify_accumulates_across_writes() {
        let ev = EventfdSignal::create_local(0).expect("eventfd");
        ev.notify(1).expect("n1");
        ev.notify(2).expect("n2");
        ev.notify(3).expect("n3");
        // Single read drains the accumulated counter and resets to zero.
        assert_eq!(ev.wait().expect("wait"), 6);
    }

    #[test]
    fn initial_count_preloads_counter() {
        let ev = EventfdSignal::create_local(7).expect("eventfd");
        assert_eq!(ev.wait().expect("wait"), 7);
    }

    #[test]
    fn wait_nonblocking_returns_none_when_idle() {
        let ev = EventfdSignal::create_local_nonblocking(0).expect("eventfd nb");
        match ev.wait_nonblocking() {
            Ok(None)    => {}
            Ok(Some(n)) => panic!("expected None on idle eventfd, got Some({n})"),
            Err(e)      => panic!("expected None on idle eventfd, got error: {e}"),
        }
    }

    #[test]
    fn wait_nonblocking_drains_then_idles() {
        let ev = EventfdSignal::create_local_nonblocking(0).expect("eventfd nb");
        ev.notify(11).expect("notify");
        assert_eq!(ev.wait_nonblocking().expect("drain"), Some(11));
        // Counter back to zero — next call must report idle.
        assert_eq!(ev.wait_nonblocking().expect("second"), None);
    }

    /// Writing `u64::MAX` is reserved by the kernel and returns `EINVAL`.
    /// Catches a regression if anyone "simplifies" the write to drop
    /// the partial-write check or starts swallowing errors.
    #[test]
    fn notify_max_value_is_rejected() {
        let ev = EventfdSignal::create_local(0).expect("eventfd");
        let err = match ev.notify(u64::MAX) {
            Ok(())  => panic!("expected EINVAL"),
            Err(e)  => e,
        };
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    }
}
