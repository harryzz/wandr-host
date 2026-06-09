//! Profiling hooks for the wasm runtime (task 23).
//!
//! Wires `Store::limiter` (memory.grow events as a `ResourceLimiter`),
//! `Store::call_hook` (host-call counter), and a per-frame snapshot
//! that logs delta-counts.
//!
//! Entire module is `#[cfg(feature = "profile")]`-gated; production
//! APK builds compile this away to nothing.
//!
//! `GuestProfiler` sampling is **intentionally not in this iteration**:
//! it requires `Config::epoch_interruption(true)`, which changes the
//! AOT-cwasm contract — the pre-compiled cwasm on the device was
//! compiled without that flag and rejects the load if we flip it at
//! runtime. Shipping GuestProfiler needs a matched
//! profile-build cwasm and is a separate follow-up. See
//! `tasks/23-profiling-hooks.md` and
//! `tasks/scope-profiling-tools.md` for the broader inventory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use wasmtime::{CallHook, ResourceLimiter};

// ── ResourceLimiter ────────────────────────────────────────────────

/// Logs every successful `memory.grow` with a wall-clock timestamp.
/// Wired in via `Store::limiter(|host| &mut host.growth_log)`.
pub struct GrowthLog {
    started_at: Instant,
    grow_count: u64,
}

impl GrowthLog {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            grow_count: 0,
        }
    }
}

impl ResourceLimiter for GrowthLog {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.grow_count += 1;
        let delta = desired.saturating_sub(current);
        log::info!(
            "wandr-profile: memory.grow #{:>4} t+{:>7}ms  {} -> {} pages  (Δ {} KB)",
            self.grow_count,
            self.started_at.elapsed().as_millis(),
            current / 65536,
            desired / 65536,
            delta / 1024,
        );
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        log::warn!("wandr-profile: memory.grow FAILED: {error:?}");
        Ok(())
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

// ── Host-call counter (Store::call_hook) ───────────────────────────

/// Cumulative count of CallingHost transitions (host imports the guest
/// has called). Read + reset from frame_tick to derive per-frame counts.
pub static HOST_CALLS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Counter increment hook. Pattern of use:
/// ```ignore
/// store.call_hook(|_cx, kind| {
///     profiling::on_call_hook(kind);
///     Ok(())
/// });
/// ```
pub fn on_call_hook(kind: CallHook) {
    if matches!(kind, CallHook::CallingHost) {
        HOST_CALLS_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
}

// ── Per-frame snapshot ────────────────────────────────────────────

/// State that frame_tick maintains between calls.
///
/// Tracks host-call counts only — linear-memory growth is event-driven
/// via `GrowthLog` (ResourceLimiter), which is more accurate than
/// per-frame polling anyway since it reports the exact event count
/// and timestamps. Direct `Memory::data_size` polling would need
/// per-component-instance memory enumeration which the Component
/// Model doesn't expose cleanly through `Store`.
pub struct FrameSnapshotState {
    started_at: Instant,
    last_host_calls: u64,
}

impl FrameSnapshotState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            last_host_calls: 0,
        }
    }
}

/// Log a one-line snapshot every `every_n_frames` (default 60 → ~1 s
/// at 60 fps; lower for higher resolution).
pub fn frame_tick(
    state: &mut FrameSnapshotState,
    frame_no: u64,
    every_n_frames: u64,
) {
    if every_n_frames > 0 && frame_no.wrapping_rem(every_n_frames) != 0 {
        return;
    }
    let total_calls = HOST_CALLS_TOTAL.load(Ordering::Relaxed);
    let delta_calls = total_calls.saturating_sub(state.last_host_calls);
    state.last_host_calls = total_calls;
    log::info!(
        "wandr-profile: frame {:>6} t+{:>7}ms  host-calls this window={}  total={}",
        frame_no,
        state.started_at.elapsed().as_millis(),
        delta_calls,
        total_calls,
    );
}

// ── Periodic Store::gc trigger — tried + REJECTED ──────────────────
//
// We experimented with a per-300-frame `Store::gc(None)` call to
// reduce the wasm-GC-heap growth pattern identified in the 15-min
// soak (PSS climbed +123 MB without gc; +7.7 MB with gc — 16× cut
// at 0.73 % CPU on the default demo). It works as a band-aid but
// is the wrong fix: enabling ProgressIndicator dramatically raises
// the per-gc cost, and the underlying issue (Kotlin/Wasm
// continuation retention / kotlinx-coroutines wasmWasi weak-ref
// gaps) stays. Findings recorded in
// `feedback_indeterminate_progress_leak.md`; bisect plan in
// `tasks/24-bisect-wasm-leak.md`.

// ── GuestProfiler driver ───────────────────────────────────────────
//
// Deliberately removed in this iteration — see the module-level
// doc-comment for why. Re-add when a matched profile-build cwasm is
// in place.
