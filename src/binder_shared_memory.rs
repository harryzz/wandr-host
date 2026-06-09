//! `BinderMappedMemory` — wraps a SharedFileRegion-style
//! `(fd, offset, size, writeable)` tuple into an mmap'd byte slice.
//!
//! Every Android shared-memory binder protocol — IAAudioService's ring
//! buffers (task 21), CameraService's BufferQueue, Codec2's input/output
//! ports — hands the client process a `ParcelFileDescriptor` plus an
//! offset/size/writeable triple, and the client mmaps the region. The
//! mmap step is identical across services; only the atomic protocol on
//! top differs. Factoring that step into one primitive keeps each
//! per-service `*_impl.rs` thin.
//!
//! The rsbinder `ParcelFileDescriptor → OwnedFd` extraction is intentionally
//! NOT in this module — it lives at the audio_impl call site so this
//! primitive stays cross-platform and host-testable via memfd_create.

use std::io;
use std::os::fd::OwnedFd;

use memmap2::{Mmap, MmapMut, MmapOptions};

/// One mmap'd byte region. Holds the backing fd alongside the mapping so
/// the fd stays open for the mapping's lifetime; on drop the mmap is
/// torn down (memmap2 handles `munmap`) and then the fd is closed.
pub struct BinderMappedMemory {
    inner: Inner,
    // Kept as a `File` (not `OwnedFd`) because memmap2 requires something
    // implementing the file-descriptor borrow trait when mapping; `File`
    // is the lowest-friction option that owns the fd.
    _file: std::fs::File,
}

enum Inner {
    Ro(Mmap),
    Rw(MmapMut),
}

impl BinderMappedMemory {
    /// Map `size` bytes starting at `offset` of `fd`.
    ///
    /// - `offset` and `size` mirror AIDL `long`; both must be non-negative.
    /// - `offset` must be a multiple of the system page size (mmap requirement).
    /// - `writeable=true` requests `MAP_SHARED` with `PROT_READ|PROT_WRITE`;
    ///   the fd must have been opened for writing or the mmap call fails.
    pub fn map(fd: OwnedFd, offset: i64, size: i64, writeable: bool)
        -> io::Result<Self>
    {
        if offset < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SharedFileRegion offset must be non-negative, got {offset}"),
            ));
        }
        if size < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SharedFileRegion size must be non-negative, got {size}"),
            ));
        }

        let file = std::fs::File::from(fd);
        let mut opts = MmapOptions::new();
        opts.offset(offset as u64).len(size as usize);

        // SAFETY: callers obtained the fd from a binder transaction whose
        // peer service is responsible for keeping the underlying memory
        // alive for the fd's lifetime; the file we hold keeps the fd open
        // for the mapping's lifetime; concurrent writes from the peer are
        // expected and handled by per-service atomic protocols (the
        // primitive itself makes no aliasing claims about the bytes).
        let inner = if writeable {
            Inner::Rw(unsafe { opts.map_mut(&file)? })
        } else {
            Inner::Ro(unsafe { opts.map(&file)? })
        };

        Ok(BinderMappedMemory { inner, _file: file })
    }

    pub fn as_slice(&self) -> &[u8] {
        match &self.inner {
            Inner::Ro(m) => &m[..],
            Inner::Rw(m) => &m[..],
        }
    }

    /// Returns the writable slice if the region was mapped writeable;
    /// `None` otherwise. Callers should hold the mutable borrow only as
    /// long as they need it — the atomic-counter protocols layered on
    /// top of these buffers expect short critical sections.
    pub fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        match &mut self.inner {
            Inner::Ro(_) => None,
            Inner::Rw(m) => Some(&mut m[..]),
        }
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            Inner::Ro(m) => m.len(),
            Inner::Rw(m) => m.len(),
        }
    }

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn is_writeable(&self) -> bool {
        matches!(self.inner, Inner::Rw(_))
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{FromRawFd, IntoRawFd};

    fn memfd(name: &str, contents: &[u8]) -> OwnedFd {
        let c_name = std::ffi::CString::new(name).unwrap();
        let raw = unsafe { libc::memfd_create(c_name.as_ptr(), 0) };
        assert!(
            raw >= 0,
            "memfd_create failed: {}",
            io::Error::last_os_error(),
        );
        // Briefly wrap as File to write the contents, then recover the fd.
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        file.write_all(contents).expect("memfd write");
        let raw = file.into_raw_fd();
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn page_size() -> usize {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    #[test]
    fn round_trip_read_only() {
        let fd = memfd("wandr-bsm-ro", b"hello, world!");
        let m = BinderMappedMemory::map(fd, 0, 13, false).expect("map ro");
        assert_eq!(m.as_slice(), b"hello, world!");
        assert_eq!(m.len(), 13);
        assert!(!m.is_writeable());
        assert!(!m.is_empty());
    }

    #[test]
    fn round_trip_writeable() {
        // Sized to a full page so writes definitely land in the mapping.
        let page = page_size();
        let mut payload = vec![0u8; page];
        payload[0] = 1;
        payload[255] = 255;
        let fd = memfd("wandr-bsm-rw", &payload);
        let mut m = BinderMappedMemory::map(fd, 0, page as i64, true).expect("map rw");
        assert_eq!(m.as_slice()[0], 1);
        assert_eq!(m.as_slice()[255], 255);
        // Mutate through the mapping; verify it's reflected on subsequent reads.
        m.as_mut_slice().unwrap()[0] = 0xab;
        assert_eq!(m.as_slice()[0], 0xab);
        assert!(m.is_writeable());
    }

    #[test]
    fn as_mut_slice_is_none_for_read_only() {
        let fd = memfd("wandr-bsm-romut", b"abc");
        let mut m = BinderMappedMemory::map(fd, 0, 3, false).expect("map ro");
        assert!(m.as_mut_slice().is_none());
    }

    #[test]
    fn rejects_negative_offset() {
        let fd = memfd("wandr-bsm-neg-off", b"abc");
        let err = match BinderMappedMemory::map(fd, -1, 3, false) {
            Ok(_)  => panic!("expected InvalidInput, got Ok"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_negative_size() {
        let fd = memfd("wandr-bsm-neg-size", b"abc");
        let err = match BinderMappedMemory::map(fd, 0, -1, false) {
            Ok(_)  => panic!("expected InvalidInput, got Ok"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Map starting at a non-zero, page-aligned offset and verify the
    /// returned slice reflects the bytes at that offset (not at byte 0).
    #[test]
    fn page_aligned_offset_window() {
        let page = page_size();
        let mut payload = vec![0u8; page * 2];
        payload[page]     = 0x42;
        payload[page + 7] = 0x37;
        let fd = memfd("wandr-bsm-off", &payload);
        let m = BinderMappedMemory::map(fd, page as i64, 16, false).expect("map off");
        assert_eq!(m.as_slice()[0], 0x42);
        assert_eq!(m.as_slice()[7], 0x37);
        assert_eq!(m.len(), 16);
    }
}
