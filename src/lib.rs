pub mod arbiter_sock;
mod egl;
mod canvas_impl;
mod paragraph_impl;
mod window_impl;
mod scheduler_impl;
mod text_segmentation_impl;
mod lifecycle_impl;
mod haptics_impl;
mod ringer_impl;
mod lights_impl;
mod power_impl;
mod thermal_impl;
mod sensors_impl;
pub mod audio_impl;
pub mod audio_policy_impl;
pub mod audio_caps;
pub mod audio_routing;
pub mod video_probe;
mod locale_impl;
mod clipboard_impl;
mod pointer_icon_impl;
mod input;
mod binder;
mod binder_aidl;
mod binder_shared_memory;
mod display_impl;
pub mod ime_impl;
pub mod wms_impl;
mod ime_host_impl;
mod keyboard_host_impl;
mod alarm_host_impl;
mod task_manager_host_impl;
mod connectivity_wifi_impl;
pub mod crypto;
mod events_host_impl;
mod notify_host_impl;
mod keyguard_host_impl;
mod audio_focus_host_impl;
mod display_geometry_impl;
#[cfg(target_os = "android")]
mod ime_inbound;
#[cfg(target_os = "android")]
pub mod zygote;
mod preload;
#[cfg(target_os = "android")]
mod app_role;
mod eventfd_signal;
mod assets_impl;
mod theme_impl;
mod launcher_impl;
mod status_impl;
// Task 35 step 1: app loader skeleton (no callers wired yet).
mod app_loader;
// Task 35 step 4: app installer skeleton (no CLI wired yet — step 6).
mod app_installer;
#[cfg(feature = "profile")]
mod profiling;
#[cfg(target_os = "android")]
mod bionic_compat;
// Task 66 — host-delegated TLS for guests with Signal's pinned CA trusted.
mod signal_tls;
#[cfg(target_os = "android")]
mod wasi_stderr;
// Task 33 boot-model: standalone (no-NativeActivity) launch path.
#[cfg(target_os = "android")]
mod sf_surface;
#[cfg(target_os = "android")]
mod lifecycle_standalone;
#[cfg(target_os = "android")]
pub mod standalone;
// Task 36 step 7: one-shot CLI launch path for wasi:cli/command consumers.
#[cfg(target_os = "android")]
pub mod run_once;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/skiko-gfx.wit",
        world: "skiko-ui",
    });
}

/// Task 49 step 1b — typed bindings for the IME-events export side of
/// the IME-client contract (`wandr:ime/ime.on-editor-attached(info)` /
/// `on-editor-detached()`). The host instantiates an IME component +
/// uses these bindings to call into the guest's exported functions
/// when the arbiter delivers an `editor-attached`/`editor-detached`
/// message over the per-host control socket (see ime_inbound.rs).
///
/// Uses the `ime-events` world (stripped sibling of `ime-client-world`
/// — no input-connection import) defined in wit/ime.wit. The IME's
/// own world (e.g. `wandr:ime-keyboard/ime-keyboard`) `include`s
/// ime-events, so any IME app's component satisfies these typed
/// bindings.
mod ime_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/ime.wit",
        world: "ime-events",
    });
}

/// Task 64 — typed bindings for the OPTIONAL `my:skiko-gfx/frame-pacing`
/// export (`next-frame-delay() -> u32`). Guests that want on-demand
/// rendering export `frame-pacing-world` (a probe-only world) alongside
/// `renderer`. The host instantiates the component, then tries to bind
/// these via `FramePacingWorld::new(...).ok()` (same pattern as
/// `ime_bindings`); `Some` ⇒ the standalone loop gates render-frame on the
/// returned delay, `None` ⇒ legacy unconditional 60 fps. See task 64.
mod frame_pacing_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/skiko-gfx.wit",
        world: "frame-pacing-world",
    });
}

/// Arbiter Inc. 3c — host-import side of `wandr:alarm`. The host implements
/// `scheduler` (schedule/cancel → forwarded to the arbiter; see
/// `alarm_host_impl.rs`) and `add_to_linker`s it onto every guest's linker.
mod alarm_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/alarm.wit",
        world: "alarm-host",
    });
}

/// Task 92 — host-import side of `wandr:task-manager`. The host implements
/// `task-manager` (list-apps/system-mem/kill-app → forwarded to the arbiter +
/// `/proc` enrichment; see `task_manager_host_impl.rs`) and `add_to_linker`s it
/// onto every guest's linker.
mod task_manager_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/task-manager.wit",
        world: "task-manager-host",
    });
}

/// Task 90 M2 — host-import side of the privileged WiFi-management interface
/// (`wandr:connectivity/wifi`). The host implements `wifi` (forwarding `scan` /
/// `connect-new` / `set-enabled` to the arbiter `wifi-*` relay → the wandr-net
/// daemon; see `connectivity_wifi_impl.rs`) and `add_to_linker`s it ONLY onto a
/// privileged guest's linker (`LoadedApp::wifi_privileged` — the Settings /
/// wifi-picker chrome). Ordinary guests can't import it (instantiation fails on
/// the missing import) — that *is* the gate.
mod wifi_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/connectivity.wit",
        world: "wifi-host",
    });
}

/// Task 90 — host-import side of the wandr event bus (`wandr:events`, vocabulary
/// aligned to `wasi:messaging`). The host implements `producer` (`publish` →
/// forwarded to the arbiter `evt-publish`; see `events_host_impl.rs`) and
/// `add_to_linker`s it onto every guest's linker. Subscription is via
/// `package.toml [events] subscribe`.
mod events_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/events.wit",
        world: "events-host",
    });
}

/// Task 90 — export side: typed `call_handle` for guests that export
/// `wandr:events/incoming-handler`. The standalone loop calls it when the arbiter
/// fans an event on a subscribed topic. Bound conditionally per instance (like
/// `alarm_events`); guests that don't export it yield `None` (inert).
mod events_incoming_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/events.wit",
        world: "events-incoming",
    });
}

/// Arbiter Inc. 3c — export side: typed `call_on_alarm` for guests that export
/// `wandr:alarm/alarm-handler`. Bound conditionally per instance (like
/// `ime_bindings`); guests that don't export it yield `None` (inert).
mod alarm_events_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/alarm.wit",
        world: "alarm-events",
    });
}

/// Signal bg-receipt (M2) — export side: typed `call_bg_tick` for guests that
/// declare `background = true` + export `wandr:background/background`. The
/// standalone loop calls it in place of render-frame while the guest is a
/// backgrounded background-service. Bound conditionally per instance (like
/// `alarm_events`); other guests yield `None` (inert).
mod background_events_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/background.wit",
        world: "background-events",
    });
}

/// Signal bg-receipt (M3) — host-import side of `wandr:notify`. The host implements
/// `notifier` (post/cancel → forwarded to the arbiter; see `notify_host_impl.rs`)
/// and `add_to_linker`s it onto every guest's linker.
mod notify_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/notify.wit",
        world: "notify-host",
    });
}

/// Keyguard (M3) — host-import side of `wandr:keyguard`. The host implements
/// `keyguard.unlock` (forwarded to the arbiter; see `keyguard_host_impl.rs`) and
/// `add_to_linker`s it onto guests (the keyguard guest imports it).
mod keyguard_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/keyguard.wit",
        world: "keyguard-host",
    });
}

/// Signal bg-receipt (M3) — export side: typed `call_on_notification_click` for
/// guests that export `wandr:notify/notify-handler`. Bound conditionally per
/// instance (like `alarm_events`); other guests yield `None` (inert).
mod notify_events_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/notify.wit",
        world: "notify-events",
    });
}

/// wandr-arbiter-audio (M2) — host-import side of `wandr:audio-focus`. The host
/// implements `focus` (request/abandon → forwarded to the arbiter; see
/// `audio_focus_host_impl.rs`) and `add_to_linker`s it onto every guest.
mod audio_focus_host_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/audio-focus.wit",
        world: "audio-focus-host",
    });
}

/// wandr-arbiter-audio (M2) — export side: typed `call_on_focus_changed` for
/// guests that export `wandr:audio-focus/focus-handler`. Bound conditionally per
/// instance (like `alarm_events`); other guests yield `None` (inert).
mod audio_focus_events_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/audio-focus.wit",
        world: "audio-focus-events",
    });
}

// markdown_bindings module deleted (task 39 — replaced by generic
// dep wiring via wasmtime introspection in app_loader.rs). Per-dep
// `bindgen!` modules are no longer needed; any cross-app dep wires
// up automatically via `wire_dep_into_linker`'s component-type walk.

use winit::{
    application::ApplicationHandler,
    event::{ElementState, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key, NamedKey, PhysicalKey},
    window::{Window, WindowId},
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::{Engine, Store};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::app_loader::{AppLoader, AppRef, LoadedApp};

pub struct HostState {
    pub renderer:  canvas_impl::SkiaRenderer,
    pub scheduler: scheduler_impl::SchedulerState,
    pub lifecycle: lifecycle_impl::LifecycleState,
    pub clipboard: Option<String>,
    pub wasi:      WasiCtx,
    pub table:     ResourceTable,
    /// Task 66 — `wasi:tls` host context (Signal-aware trust store). Shared
    /// `wasi:io` resources live in `table`. See `signal_tls`.
    pub wasi_tls:  wasmtime_wasi_tls::WasiTlsCtx,
    /// Root of the install's `assets/` dir for the `my:skiko-gfx/assets.read`
    /// host impl (task 38). `None` for dev paths / bundles with no
    /// assets — guest `read()` calls then return `option::none`.
    pub assets_dir: Option<PathBuf>,
    #[cfg(feature = "profile")]
    pub growth_log: profiling::GrowthLog,
    #[cfg(feature = "profile")]
    pub frame_snapshot: profiling::FrameSnapshotState,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl wasmtime_wasi_tls::WasiTlsView for HostState {
    fn tls(&mut self) -> wasmtime_wasi_tls::WasiTlsCtxView<'_> {
        wasmtime_wasi_tls::WasiTlsCtxView { ctx: &mut self.wasi_tls, table: &mut self.table }
    }
}

pub struct App {
    window:          Option<Arc<Window>>,
    engine:          Engine,
    loaded:          Option<LoadedApp>,
    store:           Option<Store<HostState>>,
    bindings:        Option<bindings::SkikoUi>,
    // Renderer owned directly when running without a WASM component.
    test_renderer:   Option<canvas_impl::SkiaRenderer>,
    last_cursor:     (f32, f32),
}

impl App {
    // pub(crate) so the task-33 standalone path (src/standalone.rs) builds an
    // identically-configured Engine — the cwasm contract depends on it.
    pub(crate) fn make_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.wasm_gc(true);
        config.wasm_function_references(true);
        config.wasm_exceptions(true);
        // Compose's composition + layout passes on a real Material3 tree get
        // deeply recursive (Composer/snapshot diffing + LayoutNode placement).
        // Default 1 MiB wasm stack overflows; bump to 4 MiB. async_stack_size
        // defaults to 2 MiB though, and wasmtime requires async_stack > max_wasm_stack;
        // bump async first.
        config.async_stack_size(8 * 1024 * 1024);
        config.max_wasm_stack(4 * 1024 * 1024);
        // Note: `epoch_interruption(true)` would be needed here to drive
        // GuestProfiler sampling, but it changes the AOT cwasm contract —
        // the pre-compiled cwasm currently on the device was compiled
        // without it and refuses to load if we flip the flag at runtime.
        // Recompiling cwasm with matching config is a separate follow-up
        // (see tasks/23-profiling-hooks.md "Out of scope"); for now the
        // `profile` cargo feature wires only the ResourceLimiter +
        // call-hook trio, which don't require engine config changes.
        Engine::new(&config).expect("wasmtime engine init")
    }

    fn from_parts(engine: Engine, loaded: Option<LoadedApp>) -> Self {
        Self {
            window: None,
            engine,
            loaded,
            store: None,
            bindings: None,
            test_renderer: None,
            last_cursor: (0.0, 0.0),
        }
    }

    fn renderer_mut(&mut self) -> Option<&mut canvas_impl::SkiaRenderer> {
        if let Some(store) = &mut self.store {
            Some(&mut store.data_mut().renderer)
        } else {
            self.test_renderer.as_mut()
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(reason) = binder::init() {
            log::warn!("binder init: {reason} — HAL calls will fall back to sysfs");
        }
        // §5 de-risk: probe SurfaceFlinger via rsbinder. One-shot,
        // read-only, no behavior change. Only runs cold once because
        // resumed() handles cold/warm split below.
        if self.store.is_none() {
            display_impl::probe();
        }
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes()
                    .with_title("WASM Android Runtime"))
                .expect("window creation failed"),
        );

        let renderer = canvas_impl::SkiaRenderer::new(window.clone())
            .expect("renderer init failed");

        // Warm resume: a store + bindings are already alive from a previous
        // cold start, but our renderer's EGL surface points at a NativeWindow
        // that Android destroyed when the activity was backgrounded. Swap in
        // a fresh renderer bound to the new NativeWindow, inheriting the
        // old renderer's CPU-side caches (pictures, recorders, text blobs,
        // typefaces, shaders, paragraphs, paragraph-builders + their id
        // counters) so wasm-side handles that Compose still holds remain
        // valid. GPU-resident caches (text image cache, images) are NOT
        // inherited — their textures lived in the dying gr_context.
        // Composition, scheduler state, and lifecycle observers persist
        // because the wasmtime Store is preserved.
        if self.store.is_some() && self.bindings.is_some() {
            log::info!("resumed (warm) — swapping renderer in existing store, inheriting CPU caches");
            let store = self.store.as_mut().unwrap();
            let mut new_renderer = renderer;
            let old_renderer = &mut store.data_mut().renderer;
            new_renderer.inherit_caches_from(old_renderer);
            let _stale = std::mem::replace(old_renderer, new_renderer);
            drop(_stale);
            self.window = Some(window);
            self.set_lifecycle(bindings::my::skiko_gfx::lifecycle::State::Resumed);
            if let Some(w) = &self.window { w.request_redraw(); }
            return;
        }

        log::info!("resumed (cold) — creating store and instantiating component");
        if let Some(loaded) = &self.loaded {
            // Task 30 step 1: route guest stderr to logcat *synchronously*
            // — wasmtime-wasi 44's inherit_stderr otherwise enqueues bytes
            // on a worker task that won't drain before a SIGILL trap kills
            // the process. See wasi_stderr.rs for details.
            let mut wasi_builder = WasiCtxBuilder::new();
            wasi_builder.inherit_stdin().inherit_stdout();
            #[cfg(target_os = "android")]
            { wasi_builder.stderr(wasi_stderr::LogcatStderr); }
            #[cfg(not(target_os = "android"))]
            { wasi_builder.inherit_stderr(); }
            signal_tls::grant_network(&mut wasi_builder); // task 66
            // Task 67 — writable /state for guest persistence (Signal engine
            // account + protocol snapshot + history). Created on demand.
            if let Some(state) = loaded.state_dir() {
                match wasi_builder.preopened_dir(&state, "/state", DirPerms::all(), FilePerms::all()) {
                    Ok(_)  => log::info!("preopened {} → /state (read-write)", state.display()),
                    Err(e) => log::warn!("preopen {} failed: {e:#}", state.display()),
                }
            }
            let wasi = wasi_builder.build();
            let host = HostState {
                renderer,
                scheduler: scheduler_impl::SchedulerState::default(),
                lifecycle: lifecycle_impl::LifecycleState {
                    current: bindings::my::skiko_gfx::lifecycle::State::Resumed,
                    pending: Some(bindings::my::skiko_gfx::lifecycle::State::Resumed),
                },
                clipboard: None,
                wasi,
                table: ResourceTable::new(),
                wasi_tls: signal_tls::wasi_tls_ctx(),
                assets_dir: loaded.assets_dir(),
                #[cfg(feature = "profile")]
                growth_log: profiling::GrowthLog::new(),
                #[cfg(feature = "profile")]
                frame_snapshot: profiling::FrameSnapshotState::new(),
            };
            let mut store = Store::new(&self.engine, host);

            // ── Profiling hooks (cargo feature `profile` only) ────────
            #[cfg(feature = "profile")]
            {
                // (1) ResourceLimiter logs every memory.grow event.
                store.limiter(|h| &mut h.growth_log);
                // (2) call_hook bumps HOST_CALLS_TOTAL on each CallingHost.
                store.call_hook(|_cx, kind| {
                    profiling::on_call_hook(kind);
                    Ok(())
                });
                // GuestProfiler sampling is intentionally NOT wired here —
                // it requires `Config::epoch_interruption(true)` which
                // breaks AOT-cwasm load (the cwasm was compiled without
                // that flag). Deferred to a follow-up that ships a
                // matched profile-build cwasm. See tasks/23-profiling-hooks.md.
            }

            // The winit/NativeActivity path doesn't host IME apps — only
            // editor-bearing guests. `ime_events` from the refactored
            // instantiate (task 49 step 1b) is unused here; discard it.
            let inst = loaded.instantiate(&mut store)
                .expect("instantiate component");
            log::info!("WASM component instantiated");

            self.store    = Some(store);
            self.bindings = Some(inst.skiko);
        } else {
            log::info!("no WASM component — running in renderer-test mode");
            self.test_renderer = Some(renderer);
        }
        self.window = Some(window);

        if let Some(w) = &self.window { w.request_redraw(); }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        log::info!("suspended — dispatching Stopped, dropping window (store kept alive)");
        // Dispatch Stopped through the guest BEFORE releasing the window —
        // wasm-side observers can react while the renderer is still valid.
        self.set_lifecycle(bindings::my::skiko_gfx::lifecycle::State::Stopped);
        // Drop the Window reference so winit / android-activity can release
        // the NativeWindow cleanly. The renderer's EGL surface inside the
        // wasmtime Store will become invalid as a side effect; nothing
        // touches it until resumed() swaps in a fresh one.
        self.window         = None;
        self.test_renderer  = None;
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::RedrawRequested => {
                if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
                    // No init() call needed — appMain() runs on the first render-frame.
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;

                    // Drain any scheduler callbacks whose deadline has passed.
                    let due = s.data_mut().scheduler.drain_due(std::time::Instant::now());
                    for callback_id in due {
                        if let Err(e) = b.my_skiko_gfx_renderer()
                            .call_on_scheduled_callback(&mut *s, callback_id)
                        {
                            log::warn!("on_scheduled_callback({callback_id}) failed: {e:#}");
                        }
                    }

                    let t0 = std::time::Instant::now();
                    let result = b.my_skiko_gfx_renderer().call_render_frame(&mut *s, nanos);

                    // Fire any pending lifecycle transition AFTER the first
                    // render_frame succeeds (gives appMain a chance to register
                    // its observer before the event arrives).
                    if result.is_ok() {
                        let pending = s.data_mut().lifecycle.pending.take();
                        if let Some(state) = pending {
                            if let Err(e) = b.my_skiko_gfx_renderer()
                                .call_on_lifecycle_changed(&mut *s, state as u32)
                            {
                                log::warn!("on_lifecycle_changed failed: {e:#}");
                            }
                        }
                    }
                    let elapsed = t0.elapsed();
                    {
                        static FRAME_COUNT: std::sync::atomic::AtomicU32 =
                            std::sync::atomic::AtomicU32::new(0);
                        let n = FRAME_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Profile feature: per-frame host-call snapshot.
                        // Linmem growth comes from the event-driven
                        // ResourceLimiter log (more accurate than polling).
                        // Periodic gc trigger was tried + reverted —
                        // see profiling.rs comment and
                        // tasks/24-bisect-wasm-leak.md.
                        #[cfg(feature = "profile")]
                        {
                            profiling::frame_tick(
                                &mut s.data_mut().frame_snapshot,
                                n as u64,
                                60,
                            );
                        }
                        // Note on continuous-animation leak (~0.4 MB/s in wasm
                        // linear memory under indeterminate ProgressIndicator +
                        // LaunchedEffect+withFrameNanos workloads): wasmtime's
                        // automatic GC heuristic isn't aggressive enough.
                        // Tried a periodic Store::gc(None) every 600 frames
                        // (~10 s) at task 28 closeout — it caused ANR
                        // because Store::gc is synchronous and per
                        // feedback_indeterminate_progress_leak the per-call
                        // cost grows monotonically with retained
                        // continuations. Mid-bisect we'd already confirmed
                        // gc isn't load-bearing for the chevron-tap crash
                        // either. Left off; use static widgets / accept
                        // the leak as the practical mitigation.
                        if n < 5 {
                            log::info!("render_frame #{n}: {:?} ok={}", elapsed, result.is_ok());
                        }
                        // Always extract Kotlin exception message on error
                        // so late-firing throws are visible in logcat, not
                        // just suppressed past frame 5.
                        {
                            if let Err(ref e) = result {
                                log::error!("render_frame #{n} error: {e:#}");
                                if e.downcast_ref::<wasmtime::ThrownException>().is_some() {
                                    if let Some(exn_ref) = s.take_pending_exception() {
                                        // Walk Throwable struct -> message: String?
                                        // Throwable: 0=vtable 1=itable 2=rtti 3=_hashCode 4=message 5=cause 6=suppressed
                                        // String:    0=vtable 1=itable 2=rtti 3=_hashCode 4=leftIfInSum 5=length 6=_chars
                                        // _chars: array of i16 (UTF-16)
                                        let msg = (|| -> anyhow::Result<String> {
                                            use anyhow::anyhow;
                                            use wasmtime::Val;
                                            let throwable_val = exn_ref.field(&mut *s, 0)?;
                                            let throwable_anyref = throwable_val.unwrap_anyref()
                                                .ok_or_else(|| anyhow!("exn field 0 null/not anyref"))?
                                                .clone();
                                            let throwable_struct = throwable_anyref.unwrap_struct(&mut *s)?;
                                            let msg_val = throwable_struct.field(&mut *s, 4)?;
                                            let msg_anyref = match msg_val.unwrap_anyref() {
                                                Some(a) => a.clone(),
                                                None => return Ok("<null message>".into()),
                                            };
                                            let str_struct = msg_anyref.unwrap_struct(&mut *s)?;
                                            let len_val = str_struct.field(&mut *s, 5)?;
                                            let length = match len_val {
                                                Val::I32(i) => i as usize,
                                                other => return Err(anyhow!("length not i32: {:?}", other)),
                                            };
                                            let chars_val = str_struct.field(&mut *s, 6)?;
                                            let chars_anyref = chars_val.unwrap_anyref()
                                                .ok_or_else(|| anyhow!("_chars null/not anyref"))?
                                                .clone();
                                            let chars_array = chars_anyref.unwrap_array(&mut *s)?;
                                            let mut out = Vec::<u16>::with_capacity(length);
                                            for v in chars_array.elems(&mut *s)?.take(length) {
                                                let c = match v { Val::I32(i) => i as u16, _ => 0 };
                                                out.push(c);
                                            }
                                            Ok(String::from_utf16_lossy(&out))
                                        })();
                                        match msg {
                                            Ok(text) => log::error!("  exception message: {text}"),
                                            Err(why) => log::error!("  exception message read failed: {why:#}"),
                                        }
                                    } else {
                                        log::error!("  no pending exception object on store");
                                    }
                                }
                            }
                        }
                    }
                    if let Err(e) = result {
                        let msg = format!("{e:?}");
                        if msg.contains("cannot enter component instance") {
                            // skip — store is poisoned, keep rendering test frame
                        } else {
                            log::error!("render_frame fatal: {msg}");
                            event_loop.exit();
                            return;
                        }
                    }
                } else if let Some(r) = self.renderer_mut() {
                    r.draw_test_frame();
                }
                if let Some(w) = &self.window { w.request_redraw(); }
            }

            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer_mut() {
                    r.resize(size.width, size.height);
                }
                if let Some(w) = &self.window { w.request_redraw(); }
            }

            WindowEvent::Touch(t) => {
                let kind: u8 = match t.phase {
                    TouchPhase::Started               => 0,
                    TouchPhase::Ended | TouchPhase::Cancelled => 1,
                    TouchPhase::Moved                 => 2,
                };
                // Normalize pressure to 0.0..1.0. winit's Force is either
                // Normalized(f64) already in [0,1], or Calibrated { force,
                // max_possible } where the ratio gives us [0,1].
                let pressure: f32 = match t.force {
                    Some(winit::event::Force::Normalized(p)) => p as f32,
                    Some(winit::event::Force::Calibrated { force, max_possible_force, .. }) => {
                        if max_possible_force > 0.0 {
                            (force / max_possible_force) as f32
                        } else { 0.0 }
                    }
                    None => 0.0,
                };
                let pointer_id: u32 = (t.id & 0xFFFF_FFFF) as u32;
                if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
                    let _ = input::dispatch_pointer_v2(
                        b, s, kind, pointer_id,
                        t.location.x as f32, t.location.y as f32,
                        pressure,
                    );
                }
                if let Some(w) = &self.window { w.request_redraw(); }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.last_cursor = (position.x as f32, position.y as f32);
                if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
                    let _ = input::dispatch_pointer_v2(
                        b, s, 2, 0,
                        self.last_cursor.0, self.last_cursor.1,
                        0.0,
                    );
                }
            }

            WindowEvent::MouseInput { state, .. } => {
                let kind: u8 = if state == ElementState::Pressed { 0 } else { 1 };
                let (cx, cy) = self.last_cursor;
                if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
                    let _ = input::dispatch_pointer_v2(b, s, kind, 0, cx, cy, 1.0);
                }
                if let Some(w) = &self.window { w.request_redraw(); }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let kind: u8 = if event.state == ElementState::Pressed { 0 } else { 1 };
                let code: u32 = match event.physical_key {
                    PhysicalKey::Code(c) => c as u32,
                    _ => 0,
                };
                // v2: resolved UTF-32 codepoint + Compose-compatible key-id.
                // First char of `event.text` is the codepoint of the typed
                // character (handles Shift, AltGr, etc.); 0 if no text
                // (modifiers, named keys without a char).
                let code_point: u32 = event
                    .text
                    .as_ref()
                    .and_then(|s| s.chars().next())
                    .map(|c| c as u32)
                    .unwrap_or(0);
                // Numeric IDs match the values upstream Compose's webMain
                // hard-codes for `Key.Backspace`, `Key.Enter`, etc., so the
                // guest can pass `key-id` straight into `Key(keyCode.toLong())`
                // without a translation table.
                let key_id: u32 = match &event.logical_key {
                    Key::Named(NamedKey::Backspace)  => 8,
                    Key::Named(NamedKey::Tab)        => 9,
                    Key::Named(NamedKey::Enter)      => 13,
                    Key::Named(NamedKey::Escape)     => 27,
                    Key::Named(NamedKey::Space)      => 32,
                    Key::Named(NamedKey::PageUp)     => 33,
                    Key::Named(NamedKey::PageDown)   => 34,
                    Key::Named(NamedKey::End)        => 35,
                    Key::Named(NamedKey::Home)       => 36,
                    Key::Named(NamedKey::ArrowLeft)  => 37,
                    Key::Named(NamedKey::ArrowUp)    => 38,
                    Key::Named(NamedKey::ArrowRight) => 39,
                    Key::Named(NamedKey::ArrowDown)  => 40,
                    Key::Named(NamedKey::Insert)     => 45,
                    Key::Named(NamedKey::Delete)     => 46,
                    _ => 0,  // Unknown — guest falls back to code-point if non-zero
                };
                if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
                    let _ = input::dispatch_key(b, s, kind, code);
                    let _ = input::dispatch_key_v2(b, s, kind, code_point, key_id);
                }
            }

            // winit collapses Activity.onResume/onPause/onStart/onStop down to
            // its own resumed/suspended (which actually track NativeWindow
            // create/terminate, not the Activity state). But Android dispatches
            // a Focus change adjacent to onPause/onResume — LostFocus fires
            // immediately before onPause, GainedFocus immediately after
            // onResume. Use this signal to emit Paused / Resumed transitions
            // between the cold-start Resumed (host::resumed) and the eventual
            // Stopped (host::suspended). The bridge in test-app advances the
            // LifecycleRegistry through CREATED → STARTED for free when state
            // increases, so the guest sees ON_PAUSE / ON_RESUME events in the
            // right order.
            WindowEvent::Focused(focused) => {
                self.set_lifecycle(if focused {
                    bindings::my::skiko_gfx::lifecycle::State::Resumed
                } else {
                    bindings::my::skiko_gfx::lifecycle::State::Paused
                });
            }

            _ => {}
        }
    }
}

impl App {
    /// Dispatch a host-driven lifecycle transition into the guest. No-op if
    /// the new state matches what the guest last saw (lifecycle events are
    /// edge-triggered, not level-triggered). Avoids spamming
    /// on_lifecycle_changed when winit raises the same window-focus state
    /// multiple times.
    #[cfg(target_os = "android")]
    fn set_lifecycle(&mut self, state: bindings::my::skiko_gfx::lifecycle::State) {
        if let (Some(b), Some(s)) = (&self.bindings, self.store.as_mut()) {
            if s.data().lifecycle.current == state {
                return;
            }
            s.data_mut().lifecycle.current = state;
            if let Err(e) = b.my_skiko_gfx_renderer()
                .call_on_lifecycle_changed(&mut *s, state as u32)
            {
                log::warn!("on_lifecycle_changed({state:?}) failed: {e:#}");
            }
        }
    }

    #[cfg(not(target_os = "android"))]
    fn set_lifecycle(&mut self, _state: bindings::my::skiko_gfx::lifecycle::State) {}
}

#[cfg(target_os = "android")]
fn load_asset_bytes(app: &winit::platform::android::activity::AndroidApp, name: &str) -> Option<Vec<u8>> {
    use std::ffi::CString;
    use std::io::Read;
    let mgr = app.asset_manager();
    let cname = CString::new(name).ok()?;
    let mut asset = mgr.open(&cname)?;
    let len = asset.length();
    let mut buf = Vec::with_capacity(len);
    asset.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Filesystem candidates the loader tries for a hot-reload `.cwasm`.
/// Priority: public Downloads (drop via MTP / file manager) →
///           app-owned external dir (`adb push`, no permission needed).
///
/// Deploy via MTP or adb:
///   adb push skiko-component.cwasm /sdcard/Download/skiko-component.cwasm
#[cfg(target_os = "android")]
fn cwasm_filesystem_candidates(app: &winit::platform::android::activity::AndroidApp) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for path in &[
        "/sdcard/Download/skiko-component.cwasm",
        "/sdcard/Downloads/skiko-component.cwasm",
        "/storage/emulated/0/Download/skiko-component.cwasm",
    ] {
        candidates.push(PathBuf::from(path));
    }
    if let Some(ext) = app.external_data_path() {
        candidates.push(ext.join("skiko-component.cwasm"));
    }
    candidates
}

#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );
    log::info!("android_main start");
    // Task 30 step 1: surface wasi guest stderr (assertion text + line
    // number from preview1 adapter's `assert_fail`) and host panics into
    // logcat. Must run before any WasiCtxBuilder so inherit_stdio sees
    // the redirected fd 2.
    wasi_stderr::redirect_stderr_to_logcat();

    let engine = App::make_engine();
    let loader = app_loader::default_for_target();

    // Priority: filesystem cwasm (hot-reload) → APK asset bytes.
    let fs_candidates = cwasm_filesystem_candidates(&app);
    let fs_refs: Vec<&Path> = fs_candidates.iter().map(|p| p.as_path()).collect();
    let loaded = match loader.load(&engine, AppRef::DevCwasm { candidates: &fs_refs }) {
        Ok(l) => { log::info!("loaded: {}", l.source_label); Some(l) }
        Err(e) => {
            log::debug!("no filesystem cwasm ({e:#}) — trying APK asset");
            load_asset_bytes(&app, "skiko-component.cwasm")
                .and_then(|bytes| {
                    match loader.load(&engine, AppRef::DevAsset { bytes: &bytes }) {
                        Ok(l) => { log::info!("loaded: {}", l.source_label); Some(l) }
                        Err(e) => { log::warn!("APK asset cwasm failed: {e:#}"); None }
                    }
                })
        }
    };
    if loaded.is_none() {
        log::warn!("no cwasm found on filesystem or in assets — running renderer-test mode");
    }

    let mut runner = App::from_parts(engine, loaded);
    let event_loop = EventLoop::builder()
        .with_android_app(app)
        .build()
        .unwrap();
    event_loop.run_app(&mut runner).unwrap();
}

#[cfg(not(target_os = "android"))]
pub fn run() {
    env_logger::init();
    log::info!("desktop start");

    let engine = App::make_engine();
    let loader = app_loader::default_for_target();
    let argv1 = std::env::args().nth(1)
        .unwrap_or_else(|| "skiko-component.cwasm".to_string());
    let argv_path = Path::new(&argv1);
    let loaded = match loader.load(&engine, AppRef::DevCwasm { candidates: &[argv_path] }) {
        Ok(l) => { log::info!("loaded: {}", l.source_label); Some(l) }
        Err(e) => { log::warn!("no component at {argv1}: {e:#}"); None }
    };

    let event_loop = EventLoop::new().unwrap();
    event_loop.run_app(&mut App::from_parts(engine, loaded)).unwrap();
}

/// CLI entry for `wandr-host --install <wandrpkg-dir>`. Reads the bundle,
/// AOT-precompiles each component on-device, writes the install dir,
/// and stamps `cache-key.toml`. Honors `WANDR_APPS_ROOT` for sandboxed
/// smoke testing.
pub fn install_wandrpkg(wandrpkg_dir: &Path) -> anyhow::Result<app_installer::InstalledApp> {
    use app_installer::{AppInstaller, PackageBundle};
    let engine = App::make_engine();
    let installer = app_installer::default_for_target();
    let bundle = PackageBundle::from_dir(wandrpkg_dir);
    log::info!("install: bundle={} root={}", wandrpkg_dir.display(), installer.root.display());
    let installed = installer.install(&engine, bundle)?;
    log::info!(
        "install: {} v{} → {}",
        installed.app_id, installed.version, installed.install_dir.display(),
    );
    Ok(installed)
}
