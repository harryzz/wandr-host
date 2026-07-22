//! THE host timeline (task 117 M2, step 0).
//!
//! Before this module there were THREE monotonic clocks in the host, none of
//! which agreed, and the disagreement made `wandr:video`'s `present(at-ns)`
//! unusable by any guest:
//!
//!   * `on-frame(nanos)` was `SystemTime::now()` — UNIX epoch, ~1.75e18 ns
//!   * `wasi:clocks/monotonic-clock` was wasmtime-wasi's default, which counts
//!     from the moment THAT guest's `WasiCtx` was built — a per-guest zero
//!   * `video_desktop::monotonic_now_ns()` counted from its own first call
//!
//! A guest could therefore take the only timestamp it was handed (`on-frame`),
//! add a frame budget, pass it to `present(at-ns)`, and schedule the frame
//! roughly 55 years in the future. Nothing errored; the frame simply never
//! appeared. The player worked around it by passing `at-ns = 0` and pacing
//! itself, and the workaround was recorded as a contract flaw ("a guest cannot
//! name a host monotonic instant") when it was really three unrelated origins.
//!
//! Now there is one source and everything reads it: `wasi:clocks`, `on-frame`,
//! and the video presentation scheduler. A guest can mix values from all three
//! freely because they are the same line.
//!
//! ‼️ WHY `CLOCK_MONOTONIC` SPECIFICALLY, and not an `Instant` origin: Android's
//! `AMediaCodec_releaseOutputBufferAtTime` takes `CLOCK_MONOTONIC` nanoseconds.
//! Choosing anything else means a conversion at the one boundary where being
//! wrong is invisible — SurfaceFlinger silently drops or clamps a deadline far
//! outside its window, so the failure mode is "no video, no error". Picking the
//! platform's own clock removes that conversion entirely.
//!
//! It is deliberately NOT wall-clock: a frame clock must not step when NTP or a
//! DST change moves the wall. Anything animating off `on-frame` (Compose's
//! `withFrameNanos`, wandr.tetris) would jump with it.

/// Nanoseconds on the host's monotonic timeline.
///
/// On unix this is raw `CLOCK_MONOTONIC` — nanoseconds since boot, the same
/// number Android's media stack wants. Elsewhere it is nanoseconds since the
/// first call, which is monotonic and self-consistent; the platforms without
/// `CLOCK_MONOTONIC` (Windows) have no MediaCodec to agree with anyway.
#[cfg(unix)]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` writes a `timespec` we own; CLOCK_MONOTONIC is
    // always available on Linux/Android.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64)
}

#[cfg(not(unix))]
pub fn now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// The same timeline, exposed to guests as `wasi:clocks/monotonic-clock`.
///
/// Installed on EVERY `WasiCtxBuilder` (see `install`). Without this the guest
/// reads wasmtime-wasi's default clock, whose origin is the moment its own
/// `WasiCtx` was constructed — self-consistent, but sharing no zero with the
/// host, and therefore useless for naming an instant the host will act on.
pub struct HostMonotonicClock;

impl wasmtime_wasi::HostMonotonicClock for HostMonotonicClock {
    fn now(&self) -> u64 {
        now_ns()
    }

    fn resolution(&self) -> u64 {
        // CLOCK_MONOTONIC is nanosecond-resolution on every platform we target;
        // claiming 1 ns is honest here and matches what the default clock
        // reports from cap-std.
        1
    }
}

/// Point a guest's `wasi:clocks/monotonic-clock` at the host timeline.
///
/// Call on every `WasiCtxBuilder` — a guest that reads a different clock than
/// the one the host schedules against is exactly the bug this module exists to
/// remove.
pub fn install(builder: &mut wasmtime_wasi::WasiCtxBuilder) {
    builder.monotonic_clock(HostMonotonicClock);
}
