//! `wandr-host --run-once <app-id>` — one-shot launch mode for
//! `wasi:cli/command`-shaped consumers (task 36 step 7).
//!
//! Existence rationale: `standalone.rs` instantiates an installed app via
//! `bindings::SkikoUi::instantiate` and drives `renderFrame` 60×/sec. CLI
//! consumers like `wandr-app-md-smoke` export `wasi:cli/run.run` instead
//! and run once-to-completion. This module mirrors `standalone::run`'s
//! setup (engine + SF surface + WASI ctx + HostState + dep-aware loader)
//! but ends with `Command::instantiate` + `call_run` instead of entering
//! the render loop. Returns the wasi exit Result so `main.rs` can map it
//! to a process exit code.
//!
//! See `docs/architecture-host-guest-boundary.md` for the host-driven
//! cardinality-1 framing — same primitive as `renderFrame`, just one
//! invocation.
//!
//! The cross-app dep wiring (`wire_markdown_dep` in `app_loader.rs`)
//! runs identically here; the proxy registration is consumer-shape-
//! agnostic.

use anyhow::{anyhow, Result};
use wasmtime::component::ResourceTable;
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::app_loader::{self, AppLoader, AppRef};
use crate::{App, HostState};

/// Where the `libsf_surface` shim is deployed on the device. Mirrors the
/// path used by `standalone.rs` — same shim handles both modes.
const SHIM_SO: &str = "/data/local/tmp/libsf_surface.so";

pub fn run(app_id: &str) -> Result<()> {
    let engine = App::make_engine();
    run_with_engine(&engine, app_id)
}

/// Same as `run` but uses a caller-supplied engine. The task-45 zygote
/// child path goes through here so the wasmtime `Engine` allocated by
/// the parent before `fork()` is reused (COW-shared with siblings),
/// instead of each child re-allocating a fresh one.
pub fn run_with_engine(engine: &Engine, app_id: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
        );
        // Surface guest WASI stderr to logcat — the difference vs `wasmtime run`'s
        // stdio path blamed for the Kotlin/Wasm command-adapter throw bug (see
        // `feedback_kotlin_wasm_println_throws_wasmtime`).
        crate::wasi_stderr::redirect_stderr_to_logcat();
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = env_logger::builder().try_init(); // main() didn't call run()
    }
    log::info!("run_once: starting — app_id={app_id}");

    // Binder init is needed for any HAL access the dep / consumer might make
    // (android only; cheap if no one uses it).
    #[cfg(target_os = "android")]
    if let Err(e) = crate::binder::init() {
        log::warn!("run_once: binder init: {e}");
    }

    // HostState carries a non-Option SkiaRenderer even for guests that never
    // draw (refactor deferred). Android builds an SF surface + GL renderer;
    // desktop uses a headless CPU raster surface (no window, no flash).
    #[cfg(target_os = "android")]
    let (sf, renderer) = {
        let sf = crate::sf_surface::SfSurface::create(SHIM_SO)?;
        log::info!(
            "run_once: surface {}x{} transform 0x{:x} (ANativeWindow={:p})",
            sf.width, sf.height, sf.transform, sf.native_window,
        );
        let renderer = crate::canvas_impl::SkiaRenderer::from_native_window(
            sf.native_window, sf.width as u32, sf.height as u32,
            || sf.query_transform_hint(),
        )?;
        (sf, renderer)
    };
    #[cfg(not(target_os = "android"))]
    let renderer = crate::canvas_impl::SkiaRenderer::new_headless(640, 480)?;

    let loader = app_loader::default_for_target();
    let loaded = loader.load(engine, AppRef::Installed { app_id, version: None })
        .map_err(|e| anyhow!("run_once: load {app_id}: {e:#}"))?;
    log::info!("run_once: loaded {}", loaded.source_label);

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdin().inherit_stdout();
    #[cfg(target_os = "android")]
    wasi_builder.stderr(crate::wasi_stderr::LogcatStderr);
    #[cfg(not(target_os = "android"))]
    wasi_builder.inherit_stderr();
    // Pass the app id as argv[0] so a curious consumer can see who it
    // thinks it is. Smoke consumer doesn't read argv.
    wasi_builder.args(&[app_id]);
    // Task 38 — same preopen as `standalone::run`. Read-only `/assets`
    // for installed apps that shipped an `assets/` dir.
    if let Some(assets) = loaded.assets_dir() {
        match wasi_builder.preopened_dir(&assets, "/assets", DirPerms::READ, FilePerms::READ) {
            Ok(_)  => log::info!("run_once: preopened {} → /assets (read-only)", assets.display()),
            Err(e) => log::warn!("run_once: preopen {} failed: {e:#}", assets.display()),
        }
    }
    // Task 41 — /system/fonts/ preopen for system-fonts dep (android layout).
    #[cfg(target_os = "android")]
    match wasi_builder.preopened_dir("/system/fonts", "/system-fonts", DirPerms::READ, FilePerms::READ) {
        Ok(_)  => log::info!("run_once: preopened /system/fonts → /system-fonts (read-only)"),
        Err(e) => log::warn!("run_once: preopen /system/fonts failed: {e:#}"),
    }
    // Task 67 — writable /state (read-write) for guest persistence.
    if let Some(state) = loaded.state_dir() {
        match wasi_builder.preopened_dir(&state, "/state", DirPerms::all(), FilePerms::all()) {
            Ok(_)  => log::info!("run_once: preopened {} → /state (read-write)", state.display()),
            Err(e) => log::warn!("run_once: preopen {} failed: {e:#}", state.display()),
        }
    }
    // Docker-style per-app host→guest mounts from the manifest `[[mounts]]`.
    crate::app_loader::apply_mounts(&mut wasi_builder, &loaded.mounts());
    crate::signal_tls::grant_network(&mut wasi_builder); // task 66
    let wasi = wasi_builder.build();

    let host = HostState {
        renderer,
        scheduler: crate::scheduler_impl::SchedulerState::default(),
        lifecycle: crate::lifecycle_impl::LifecycleState {
            current: crate::ui_shell_bindings::wandr::ui_shell::lifecycle::State::Resumed,
            pending: None,
        },
        clipboard: None,
        wasi,
        table: ResourceTable::new(),
        wasi_tls: crate::signal_tls::wasi_tls_ctx(),
        assets_dir: loaded.assets_dir(),
        #[cfg(feature = "profile")]
        growth_log: crate::profiling::GrowthLog::new(),
        #[cfg(feature = "profile")]
        frame_snapshot: crate::profiling::FrameSnapshotState::new(),
    };
    let mut store = Store::new(engine, host);

    let command = loaded.instantiate_command(&mut store)?;
    log::info!("run_once: command instantiated — calling wasi:cli/run.run()");

    // `call_run` returns Result<Result<(), ()>, anyhow::Error>:
    //   outer Err  = host-side trap / instantiation failure
    //   inner Err  = guest returned an "error" exit status (the WASI
    //                command convention for non-zero exit)
    //   inner Ok   = guest returned normally (zero exit)
    let result = crate::guest_call!(command.wasi_cli_run().call_run(&mut store));
    match &result {
        Ok(Ok(())) => log::info!("run_once: call_run returned Ok — guest exited cleanly"),
        Ok(Err(())) => log::warn!("run_once: call_run returned Err — guest exited with WASI error"),
        Err(e)     => log::error!("run_once: call_run trapped: {e:#}"),
    }

    // Drop the store first so the renderer's EGL/SF resources are
    // released before the SfSurface's Drop tears down the binder
    // connection (android only; desktop is a plain raster surface).
    drop(store);
    #[cfg(target_os = "android")]
    drop(sf);

    match result {
        Ok(Ok(()))  => Ok(()),
        Ok(Err(())) => Err(anyhow!("guest exited with WASI error")),
        Err(e)      => Err(anyhow!("call_run trapped: {e:#}")),
    }
}
