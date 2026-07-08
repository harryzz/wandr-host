//! Task 115 — driving helpers for CM-async (p3) guests.
//!
//! With the `p3-async` feature on, a guest that imports WASI 0.3 interfaces
//! matches async host functions, which makes its instance **async-required**:
//! every entrypoint must go through the `_async` wasmtime variants. All guest
//! export calls route through [`crate::guest_call!`] = `rt().block_on(...)`,
//! and the standalone loop's nap becomes [`pump_nap`] so the guest's native
//! async tasks (e.g. the Signal engine's receive/keepalive loop) advance
//! while the host would otherwise just sleep.
//!
//! ONE current-thread tokio runtime owns every store operation — the p3 host
//! impls create tokio IO/timer objects that bind to the driver active at
//! creation, so instantiate, calls, and pumps must all enter the same runtime.
//! (Proven end-to-end in repros/cma-cross-call-spike.)

use std::sync::OnceLock;
use std::time::Duration;

/// The process-wide CM-async driving runtime (current-thread; time + io).
pub(crate) fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .expect("p3-async: build current-thread tokio runtime")
    })
}

/// Nap that pumps the store's CM-async event loop: guest background tasks
/// (socket reads, keepalive timers) advance during the nap, and the nap still
/// bounds the host's wake cadence — the battery envelope is unchanged (tokio
/// parks the thread between wakes; an idle guest stays quiescent).
pub(crate) fn pump_nap(
    store: &mut wasmtime::Store<crate::HostState>,
    nap: Duration,
) -> anyhow::Result<()> {
    rt().block_on(store.run_concurrent(async |_| {
        tokio::time::sleep(nap).await;
    }))?;
    Ok(())
}
