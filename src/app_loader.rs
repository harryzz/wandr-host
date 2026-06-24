//! App loader — task 35 steps 1 + 5.
//!
//! Uniform interface for resolving a `Component` + skiko-wired `Linker`
//! from one of three sources: an installed app (`AppRef::Installed`,
//! re-verifies + self-heals the AOT cache), a dev-machine `.cwasm`/`.wasm`
//! path search, or APK-asset cwasm bytes.
//!
//! The loader does NOT build `HostState` and does NOT call `instantiate`.
//! Callers bring a `Store<HostState>` to `LoadedApp::instantiate`.
//!
//! See `tasks/35-app-install.md` for scope.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

use crate::app_installer::{
    engine_compatibility_hash_hex, format_cache_key, parse_resolved_deps_from_key,
    sha256_hex, ComponentCacheEntry,
};
use crate::HostState;

/// What the caller wants to load.
pub enum AppRef<'a> {
    /// Installed app under the loader root — `<root>/<app_id>/<version>/`.
    /// If `version` is `None`, the loader picks the lexicographically
    /// highest version dir present.
    Installed { app_id: &'a str, version: Option<&'a str> },
    /// Dev shortcut: try `.cwasm` (AOT) and `.wasm` (JIT) paths in order,
    /// take the first that loads. Subsumes both today's `argv[1]` flow
    /// and `find_cwasm_on_filesystem`'s candidate list.
    DevCwasm { candidates: &'a [&'a Path] },
    /// Dev shortcut: AOT cwasm bytes already in memory (APK asset).
    DevAsset { bytes: &'a [u8] },
}

/// A loaded component ready to instantiate.
///
/// The `linker` is built fresh in `instantiate()` rather than cached on
/// the struct so the consumer's deps (loaded into the Store at
/// instantiation time) can register their exports as proxy entries
/// into the linker. wasmtime's `Linker::instantiate` takes `&self`, so
/// any wiring that depends on a live Store must be done *during*
/// `instantiate()`, not at `load()` time.
pub struct LoadedApp {
    /// Human-readable origin for logs, e.g. `"cwasm:/data/local/tmp/skiko-component.cwasm"`.
    pub source_label: String,
    entry: Component,
    /// Cloned `Engine` (cheap — Arc-backed) so `instantiate()` can build
    /// a fresh `Linker::new(engine)` without needing the caller to
    /// re-pass it.
    engine: Engine,
    /// Same-Store deps resolved from `[dependencies_resolved]`. Empty
    /// for `DevCwasm` / `DevAsset` (task-35-style) loads. Each entry
    /// is deserialized but not yet instantiated; instantiation happens
    /// inside `instantiate()` against the caller's Store.
    deps: Vec<LoadedDep>,
    /// Install directory for `AppRef::Installed` loads — `None` for
    /// `DevCwasm` / `DevAsset` (no install dir exists). Callers use
    /// this for asset preopens (task 38: `<install_dir>/assets/` →
    /// `/assets` in WASI ctx) and similar install-local lookups.
    install_dir: Option<PathBuf>,
}

impl LoadedApp {
    /// Path to `<install_dir>/assets/` if the bundle shipped an `assets/`
    /// directory and the load was from an installed app. `None` for dev
    /// paths or when the bundle had no assets.
    pub fn assets_dir(&self) -> Option<PathBuf> {
        self.install_dir.as_ref()
            .map(|d| d.join("assets"))
            .filter(|p| p.is_dir())
    }

    /// Writable per-app state dir `<install_dir>/state/`, created on demand and
    /// preopened as `/state` in the WASI ctx (task 67). Lets a Rust guest (e.g.
    /// the Signal engine) persist account + protocol snapshot + message history
    /// across runs via `std::fs`. Distinct from the read-only `assets/` bundle.
    /// `None` for dev `.cwasm`/asset loads (no install dir) or if the dir can't
    /// be created.
    pub fn state_dir(&self) -> Option<PathBuf> {
        let dir = self.install_dir.as_ref()?.join("state");
        match fs::create_dir_all(&dir) {
            Ok(_) => Some(dir),
            Err(e) => {
                log::warn!("state_dir: create {} failed: {e}", dir.display());
                None
            }
        }
    }

    /// Task 62: whether this app's surface should follow device
    /// orientation. Reads the `orientation` field from the installed
    /// `<install_dir>/package.toml` (`"auto"` ⇒ rotate, anything else /
    /// absent ⇒ locked). `false` for dev `.cwasm`/asset loads
    /// (`install_dir == None`) — they have no manifest, so the safe
    /// default is "no rotation". `standalone.rs` ORs this with the
    /// fullscreen default so fullscreen apps keep task-43 behavior.
    pub fn rotation_policy(&self) -> bool {
        self.orientation_field().as_deref() == Some("auto")
    }

    /// Task 63: whether this app EXPLICITLY declares `orientation = "locked"`
    /// (portrait-locked). Distinct from "field absent" — a fullscreen app
    /// with no field still rotates (task 43 default), but an explicitly
    /// locked one stays portrait AND the arbiter-published lock makes the
    /// system chrome (status bar / taskbar / IME) stay portrait too while
    /// it is foreground. See `standalone.rs` orientation-lock handling.
    pub fn orientation_locked(&self) -> bool {
        self.orientation_field().as_deref() == Some("locked")
    }

    /// The raw `orientation` string from the installed manifest, if any.
    fn orientation_field(&self) -> Option<String> {
        let dir = self.install_dir.as_ref()?;
        let src = fs::read_to_string(dir.join("package.toml")).ok()?;
        let doc = src.parse::<toml::Value>().ok()?;
        doc.get("orientation")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Task 64 follow-up: per-app render-rate cap. Reads `max_fps` from the
    /// installed `package.toml` (top-level key, NOT in the AOT cache-key —
    /// same as `orientation`). Returns 60 when absent / out of range / for
    /// dev loads (no manifest). The host throttles the standalone render
    /// loop to this rate; capping a non-game app to 30 roughly halves its
    /// render CPU. It is enforced HOST-SIDE and applies to every app
    /// (Compose / dioxus / canvas) with no app- or library-side code — a
    /// `WANDR_MAX_FPS` env var overrides it globally (for testing).
    pub fn max_fps(&self) -> u32 {
        self.install_dir.as_ref()
            .and_then(|dir| fs::read_to_string(dir.join("package.toml")).ok())
            .and_then(|src| src.parse::<toml::Value>().ok())
            .and_then(|doc| doc.get("max_fps").and_then(|v| v.as_integer()))
            .filter(|n| *n >= 1 && *n <= 240)
            .map(|n| n as u32)
            .unwrap_or(60)
    }

    /// Signal bg-receipt (M2): whether this app is a background-service — it keeps
    /// pumping (via `wandr:background/background.bg-tick`, called by the standalone
    /// loop in place of render-frame) while backgrounded, instead of freezing.
    /// Reads `background = true` from the installed `package.toml` (top-level key,
    /// NOT in the AOT cache-key — same as `orientation`/`max_fps`). `false` when
    /// absent / for dev loads (no manifest).
    pub fn background_service(&self) -> bool {
        self.install_dir.as_ref()
            .and_then(|dir| fs::read_to_string(dir.join("package.toml")).ok())
            .and_then(|src| src.parse::<toml::Value>().ok())
            .and_then(|doc| doc.get("background").and_then(|v| v.as_bool()))
            .unwrap_or(false)
    }

    /// Task 90 — the event-bus topics this app subscribes to. Read from the
    /// installed `package.toml` `[events] subscribe = ["net.status", …]` (an
    /// array of strings; absent / dev-load → empty). Subscription is host-config,
    /// not a WIT call (matching wasi:messaging's delivery model): the host
    /// registers each topic with the arbiter (`evt-subscribe <pid> <topic>`) for a
    /// guest that also exports `wandr:events/incoming-handler`.
    pub fn event_subscriptions(&self) -> Vec<String> {
        self.install_dir.as_ref()
            .and_then(|dir| fs::read_to_string(dir.join("package.toml")).ok())
            .and_then(|src| src.parse::<toml::Value>().ok())
            .and_then(|doc| doc.get("events").and_then(|e| e.get("subscribe")).and_then(|v| v.as_array()).cloned())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }

    /// Task 90 M2 — whether this guest may import the privileged WiFi-management
    /// interface (`wandr:connectivity/wifi`). The gate is the host's, not the
    /// manifest's alone: a guest gets `wifi` only if it is BOTH
    ///   1. installed under `system-apps/` (came from the trusted system image,
    ///      not a user-sideloaded `apps/` bundle — the same install-class boundary
    ///      `task_manager_host_impl::kind_and_label` derives), AND
    ///   2. explicitly opts in with `wifi-control = true` in its `package.toml`
    ///      (least-privilege — even system apps must ask).
    /// Both conditions derive from on-disk facts the host owns; no hardcoded
    /// app-id allowlist. Dev loads (`install_dir == None`) are never privileged.
    /// When this returns false the host simply doesn't `add_to_linker` `wifi`, so a
    /// guest that imports it fails to instantiate — that *is* the denial.
    pub fn wifi_privileged(&self) -> bool {
        let Some(dir) = self.install_dir.as_ref() else { return false };
        let system_install = dir
            .components()
            .any(|c| c.as_os_str() == "system-apps");
        let opted_in = fs::read_to_string(dir.join("package.toml"))
            .ok()
            .and_then(|src| src.parse::<toml::Value>().ok())
            .and_then(|doc| doc.get("wifi-control").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        system_install && opted_in
    }
}

/// One resolved + deserialized same-Store dep.
struct LoadedDep {
    /// Local alias from the consumer's manifest (LHS in `[dependencies]`).
    /// Used for log lines + error messages.
    name:      String,
    /// WIT-qualified interface name (e.g. `"wandr:markdown/renderer@0.1.0"`).
    /// Dispatched by `wire_dep_into_linker` to pick the right host-side
    /// bindgen module.
    interface: String,
    component: Component,
}

/// Result of a successful `LoadedApp::instantiate`. Task 49 step 1b
/// grew the return from a single `bindings::SkikoUi` to a struct so
/// IME apps can ALSO carry typed bindings for the
/// `wandr:ime/ime-events` world export (`on-editor-attached(info)` /
/// `on-editor-detached()`). Non-IME apps (wandr-app, system-bundle
/// helpers) get `ime_events: None` because their components don't
/// satisfy the ime-events world's exports.
pub struct InstantiatedApp {
    /// `Some(...)` if the component exports `wandr:ime/ime`. The host's
    /// `ime_inbound.rs` drain calls into these when an
    /// `editor-attached`/`editor-detached` message arrives.
    pub ime_events: Option<crate::ime_bindings::ImeEvents>,
    /// wasi:input-handlers probes (push-model input; new-style guests).
    /// Per input type, dispatch routes EXCLUSIVELY to a bound handler.
    pub guest_input: crate::input::GuestInput,
    /// Phase B — wandr:ui-shell export probes: shell-events receives
    /// lifecycle + scheduled callbacks EXCLUSIVELY when bound; the
    /// ui-shell frame-pacing probe is preferred over the legacy one.
    pub shell_events: Option<crate::ui_shell_export_bindings::events::ShellEventsWorld>,
    pub shell_pacing: Option<crate::ui_shell_export_bindings::pacing::FramePacingWorld>,
    /// Arbiter Inc. 3c — `Some(...)` if the component exports
    /// `wandr:alarm/alarm-handler`. `ime_inbound`'s `alarm-fired` drain calls
    /// `on-alarm(id)` on these; `None` for guests that don't use alarms.
    pub alarm_events: Option<crate::alarm_events_bindings::AlarmEvents>,
    /// Signal bg-receipt (M2) — `Some(...)` if the component exports
    /// `wandr:background/background`. The standalone loop calls `bg-tick` on these
    /// in place of render-frame while the guest is a backgrounded background-
    /// service; `None` for guests that don't opt in.
    pub bg_tick: Option<crate::background_events_bindings::BackgroundEvents>,
    /// Signal bg-receipt (M3) — `Some(...)` if the component exports
    /// `wandr:notify/notify-handler`. The host's `ime_inbound` drain calls
    /// `on-notification-click(id)` on these when a `notification-clicked` push
    /// arrives; `None` for guests that don't handle taps.
    pub notify_events: Option<crate::notify_events_bindings::NotifyEvents>,
    /// wandr-arbiter-audio (M2) — `Some(...)` if the component exports
    /// `wandr:audio-focus/focus-handler`. The host's `ime_inbound` drain calls
    /// `on-focus-changed(change)` on these when an `on-focus-changed` push
    /// arrives; `None` for guests that don't track focus.
    pub audio_focus_events: Option<crate::audio_focus_events_bindings::AudioFocusEvents>,
    /// Task 90 — `Some(...)` if the component exports `wandr:events/incoming-handler`.
    /// The standalone loop calls `handle(msg)` on these when the arbiter fans an
    /// event on a topic the guest subscribed to (`package.toml [events]`); `None`
    /// for guests that don't receive events.
    pub events_incoming: Option<crate::events_incoming_bindings::EventsIncoming>,
    /// Task 108 M2 — `Some(...)` if the component exports
    /// `wasi:media-session/session-handler`. The host's `ime_inbound` drain calls
    /// `on-action(details)` on these when the arbiter routes a transport intent
    /// (lockscreen tap / headset button) here; `None` for guests that don't
    /// publish a media session.
    pub media_session_handler: Option<crate::media_session_events_bindings::MediaSessionEvents>,
}

impl LoadedApp {
    pub fn instantiate(&self, store: &mut Store<HostState>) -> Result<InstantiatedApp> {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("wasmtime_wasi::p2::add_to_linker_sync: {e:#}"))?;
        crate::signal_tls::add_to_linker(&mut linker) // task 66 — wasi:tls (Signal CA)
            .map_err(|e| anyhow!("signal_tls::add_to_linker: {e:#}"))?;
        crate::alarm_host_bindings::AlarmHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("AlarmHost::add_to_linker: {e:#}"))?; // Arbiter Inc. 3c
        crate::task_manager_host_bindings::TaskManagerHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("TaskManagerHost::add_to_linker: {e:#}"))?; // task 92
        // Task 90 M2 — privileged wifi-management interface, gated: only a
        // system-install guest that opts in (`wifi-control = true`) gets it. A
        // non-privileged guest importing `wifi` fails to instantiate (denied).
        if self.wifi_privileged() {
            crate::wifi_host_bindings::WifiHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
                .map_err(|e| anyhow!("WifiHost::add_to_linker: {e:#}"))?;
            log::info!("app_loader: wifi-management linked (privileged) for {}", self.source_label);
        }
        crate::events_host_bindings::EventsHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("EventsHost::add_to_linker: {e:#}"))?; // task 90 event bus
        crate::crypto_host_bindings::CryptoHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("CryptoHost::add_to_linker: {e:#}"))?; // task 93 Phase A wandr:crypto
        crate::video_host_bindings::VideoHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("VideoHost::add_to_linker: {e:#}"))?; // task 93 Phase 1 wandr:video
        crate::notify_host_bindings::NotifyHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("NotifyHost::add_to_linker: {e:#}"))?; // Signal bg-receipt M3
        crate::keyguard_host_bindings::KeyguardHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("KeyguardHost::add_to_linker: {e:#}"))?; // keyguard M3
        crate::audio_focus_host_bindings::AudioFocusHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("AudioFocusHost::add_to_linker: {e:#}"))?; // wandr-arbiter-audio M2
        crate::media_session_host_bindings::MediaSessionHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("MediaSessionHost::add_to_linker: {e:#}"))?; // task 108 M2 wasi:media-session
        #[cfg(feature = "wasi-canvas")]
        crate::wasi_canvas_002_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("wasi-canvas-0.0.2 add_to_linker: {e:#}"))?; // R3 side-by-side
        crate::consolidated_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("consolidation add_to_linker: {e:#}"))?; // Phase A: ui-shell/device/chrome/assets/keyboard-send/logging
        crate::wasi_audio_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("wasi-audio add_to_linker: {e:#}"))?; // Phase A: the charter's audio slot

        for dep in &self.deps {
            wire_dep_into_linker(&mut linker, store, dep)?;
        }

        // Manual instantiate-then-wrap so we can produce TWO typed
        // wrappers (skiko + optional ime_events) over the same
        // Instance. `bindings::SkikoUi::instantiate(...)` would combine
        // both steps but only return SkikoUi.
        let instance = linker
            .instantiate(&mut *store, &self.entry)
            .map_err(|e| anyhow!("linker.instantiate failed: {e:#}"))?;
        // Optional — IME apps (whose world `include`s
        // `wandr:ime/ime-events`) satisfy these exports; non-IME apps
        // don't. `.ok()` swallows the bind-failure into None.
        let ime_events = crate::ime_bindings::ImeEvents::new(&mut *store, &instance).ok();
        if ime_events.is_some() {
            log::info!("loader: app exports wandr:ime/ime — IME-events bindings enabled");
        }
        // wasi:input-handlers@0.0.2 probes (each interface independently).
        let guest_input = crate::input::GuestInput {
            pointer2: crate::input_handlers_002_bindings::pointer::PointerHandlerWorld::new(&mut *store, &instance).ok(),
            key2: crate::input_handlers_002_bindings::key::KeyHandlerWorld::new(&mut *store, &instance).ok(),
            frame2: crate::input_handlers_002_bindings::frame::FrameHandlerWorld::new(&mut *store, &instance).ok(),
        };
        let shell_events =
            crate::ui_shell_export_bindings::events::ShellEventsWorld::new(&mut *store, &instance).ok();
        let shell_pacing =
            crate::ui_shell_export_bindings::pacing::FramePacingWorld::new(&mut *store, &instance).ok();

        // Arbiter Inc. 3c — optional alarm handler (same .ok() probe).
        let alarm_events =
            crate::alarm_events_bindings::AlarmEvents::new(&mut *store, &instance).ok();
        if alarm_events.is_some() {
            log::info!("loader: app exports wandr:alarm/alarm-handler — alarm wakes enabled");
        }
        // Signal bg-receipt (M2) — optional background-service pump (same .ok() probe).
        let bg_tick =
            crate::background_events_bindings::BackgroundEvents::new(&mut *store, &instance).ok();
        if bg_tick.is_some() {
            log::info!("loader: app exports wandr:background/background — background-service pump enabled");
        }
        // Signal bg-receipt (M3) — optional notification tap handler (same .ok() probe).
        let notify_events =
            crate::notify_events_bindings::NotifyEvents::new(&mut *store, &instance).ok();
        if notify_events.is_some() {
            log::info!("loader: app exports wandr:notify/notify-handler — notification taps enabled");
        }
        // wandr-arbiter-audio (M2) — optional audio-focus handler (same .ok() probe).
        let audio_focus_events =
            crate::audio_focus_events_bindings::AudioFocusEvents::new(&mut *store, &instance).ok();
        if audio_focus_events.is_some() {
            log::info!("loader: app exports wandr:audio-focus/focus-handler — focus changes enabled");
        }
        // Task 90 — optional event-bus receiver (same .ok() probe).
        let events_incoming =
            crate::events_incoming_bindings::EventsIncoming::new(&mut *store, &instance).ok();
        if events_incoming.is_some() {
            log::info!("loader: app exports wandr:events/incoming-handler — event-bus delivery enabled");
        }
        // Task 108 M2 — optional media-session transport handler (same .ok() probe).
        let media_session_handler =
            crate::media_session_events_bindings::MediaSessionEvents::new(&mut *store, &instance).ok();
        if media_session_handler.is_some() {
            log::info!("loader: app exports wasi:media-session/session-handler — transport intents enabled");
        }
        Ok(InstantiatedApp {
            ime_events, guest_input,
            shell_events,
            shell_pacing, alarm_events, bg_tick,
            notify_events, audio_focus_events, events_incoming,
            media_session_handler,
        })
    }

    /// One-shot CLI consumers (`wasi:cli/command` world) — task 36 step 7.
    /// Same linker setup as `instantiate`: WASI + skiko + dep proxies. Skiko
    /// is added defensively (a CLI consumer that doesn't import it pays
    /// nothing for the registration). The returned `Command` is invoked
    /// from `run_once::run` via `command.wasi_cli_run().call_run(store)`.
    pub fn instantiate_command(
        &self,
        store: &mut Store<HostState>,
    ) -> Result<wasmtime_wasi::p2::bindings::sync::Command> {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("wasmtime_wasi::p2::add_to_linker_sync: {e:#}"))?;
        crate::signal_tls::add_to_linker(&mut linker) // task 66 — wasi:tls (Signal CA)
            .map_err(|e| anyhow!("signal_tls::add_to_linker: {e:#}"))?;
        crate::alarm_host_bindings::AlarmHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("AlarmHost::add_to_linker: {e:#}"))?; // Arbiter Inc. 3c
        crate::task_manager_host_bindings::TaskManagerHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("TaskManagerHost::add_to_linker: {e:#}"))?; // task 92
        // Task 90 M2 — privileged wifi-management interface, gated: only a
        // system-install guest that opts in (`wifi-control = true`) gets it. A
        // non-privileged guest importing `wifi` fails to instantiate (denied).
        if self.wifi_privileged() {
            crate::wifi_host_bindings::WifiHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
                .map_err(|e| anyhow!("WifiHost::add_to_linker: {e:#}"))?;
            log::info!("app_loader: wifi-management linked (privileged) for {}", self.source_label);
        }
        crate::events_host_bindings::EventsHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("EventsHost::add_to_linker: {e:#}"))?; // task 90 event bus
        crate::crypto_host_bindings::CryptoHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("CryptoHost::add_to_linker: {e:#}"))?; // task 93 Phase A wandr:crypto
        crate::video_host_bindings::VideoHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("VideoHost::add_to_linker: {e:#}"))?; // task 93 Phase 1 wandr:video
        crate::notify_host_bindings::NotifyHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("NotifyHost::add_to_linker: {e:#}"))?; // Signal bg-receipt M3
        crate::keyguard_host_bindings::KeyguardHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("KeyguardHost::add_to_linker: {e:#}"))?; // keyguard M3
        crate::audio_focus_host_bindings::AudioFocusHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("AudioFocusHost::add_to_linker: {e:#}"))?; // wandr-arbiter-audio M2
        crate::media_session_host_bindings::MediaSessionHost::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| anyhow!("MediaSessionHost::add_to_linker: {e:#}"))?; // task 108 M2 wasi:media-session
        #[cfg(feature = "wasi-canvas")]
        crate::wasi_canvas_002_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("wasi-canvas-0.0.2 add_to_linker: {e:#}"))?; // R3 side-by-side
        crate::consolidated_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("consolidation add_to_linker: {e:#}"))?; // Phase A: ui-shell/device/chrome/assets/keyboard-send/logging
        crate::wasi_audio_impl::add_to_linker(&mut linker)
            .map_err(|e| anyhow!("wasi-audio add_to_linker: {e:#}"))?; // Phase A: the charter's audio slot

        for dep in &self.deps {
            wire_dep_into_linker(&mut linker, store, dep)?;
        }

        wasmtime_wasi::p2::bindings::sync::Command::instantiate(store, &self.entry, &linker)
            .map_err(|e| anyhow!("Command::instantiate failed: {e:#}"))
    }
}

pub trait AppLoader {
    fn load(&self, engine: &Engine, r: AppRef<'_>) -> Result<LoadedApp>;
}

/// Default loader. `root` is reserved for `AppRef::Installed` (task 35
/// step 5); the dev variants ignore it.
pub struct WandrLoader {
    pub root: PathBuf,
}

/// The single source of truth for the on-device app-registry root path. Used
/// as the fallback default when `WANDR_APPS_ROOT` is unset.
///
/// `/data/local/tmp/wandr-apps` is the world-readable path **every** launcher
/// actually uses on this rooted device — `run-hybrid-stack.sh`, the magisk
/// module, and the build/smoke scripts all set `WANDR_APPS_ROOT` to it, and the
/// live stack (zygote + installer + loader) reads from it. Keeping the in-code
/// default equal to that value means a bare `wandr-host --install` (no env) lands
/// in the same place the running stack reads, instead of a stray
/// `/data/wandr/apps` nothing consults. Override with `WANDR_APPS_ROOT` for a
/// sepolicy'd production root or a test sandbox.
pub const DEFAULT_APPS_ROOT: &str = "/data/local/tmp/wandr-apps";

/// Resolve the app-registry root: `WANDR_APPS_ROOT` if set, else
/// [`DEFAULT_APPS_ROOT`]. The one place the env var name + default live; every
/// loader / installer / zygote / launcher call site resolves through here.
pub fn apps_root() -> PathBuf {
    std::env::var("WANDR_APPS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_APPS_ROOT))
}

/// Convenience entry point — a loader rooted at [`apps_root`].
pub fn default_for_target() -> WandrLoader {
    WandrLoader { root: apps_root() }
}

impl AppLoader for WandrLoader {
    fn load(&self, engine: &Engine, r: AppRef<'_>) -> Result<LoadedApp> {
        let (entry, source_label, deps, install_dir) = match r {
            AppRef::Installed { app_id, version } => {
                let (entry, label, deps, dir) =
                    load_installed(engine, &self.root, app_id, version)?;
                (entry, label, deps, Some(dir))
            }
            AppRef::DevCwasm { candidates } => {
                let (entry, label) = load_dev_path(engine, candidates)?;
                (entry, label, Vec::new(), None)
            }
            AppRef::DevAsset { bytes } => {
                let (entry, label) = load_dev_asset(engine, bytes)?;
                (entry, label, Vec::new(), None)
            }
        };
        Ok(LoadedApp { source_label, entry, engine: engine.clone(), deps, install_dir })
    }
}

/// Load from `<root>/apps/<app_id>/<version>/`. Reads `cache-key.toml`,
/// recomputes the engine-compat + per-component wasm hashes; on any
/// drift (host upgrade, manifest mutation, file corruption) re-calls
/// `Engine::precompile_component`, rewrites `cache/<name>.cwasm`, and
/// re-stamps `cache-key.toml`. Then `deserialize_file`s the cwasm.
///
/// Single-component apps only — bails on multi-component (link.wac
/// composition is deferred to `tasks/36-cross-app-deps.md` step 5).
///
/// Task 36 layout: `AppRef::Installed` always targets the `apps/`
/// subtree; system components are reached via the consumer's resolved
/// deps, not directly through `Installed`.
fn load_installed(
    engine: &Engine,
    root: &Path,
    app_id: &str,
    version: Option<&str>,
) -> Result<(Component, String, Vec<LoadedDep>, PathBuf)> {
    // Search apps/ first, then system-apps/ — so GUI system chrome
    // (status bar, future taskbar) can live under system-apps/ (where the
    // launcher's apps/-only scan won't list it) yet still load via
    // AppRef::Installed (task 55).
    let app_dir = {
        let user = root.join("apps").join(app_id);
        if user.is_dir() {
            user
        } else {
            let sys = root.join("system-apps").join(app_id);
            if sys.is_dir() {
                sys
            } else {
                bail!(
                    "installed: app dir not found in apps/ or system-apps/: {}",
                    user.display()
                );
            }
        }
    };
    let version_str = match version {
        Some(v) => v.to_string(),
        None => pick_latest_version(&app_dir)?,
    };
    let install_dir = app_dir.join(&version_str);
    if !install_dir.is_dir() {
        bail!("installed: install dir not found: {}", install_dir.display());
    }

    let key_path = install_dir.join("cache-key.toml");
    let key_src = fs::read_to_string(&key_path)
        .with_context(|| format!("read {}", key_path.display()))?;
    let key: toml::Value = key_src.parse()
        .with_context(|| format!("parse {}", key_path.display()))?;
    let stored_engine_hash = key.get("engine_config_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{}: missing engine_config_hash", key_path.display()))?
        .to_string();
    let components_tbl = key.get("components").and_then(|v| v.as_table())
        .ok_or_else(|| anyhow!("{}: missing [components] table", key_path.display()))?;
    if components_tbl.is_empty() {
        bail!("{}: [components] is empty", key_path.display());
    }
    if components_tbl.len() > 1 {
        bail!(
            "{}: {} components — loader only supports single-component apps. \
             Multi-component composition is the scope-cross-app-deps task.",
            key_path.display(), components_tbl.len(),
        );
    }
    let (component_name, entry_val) = components_tbl.iter().next().unwrap();
    let component_name = component_name.clone();
    let stored_wasm_sha = entry_val.get("wasm_sha256").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{}: components.{component_name}.wasm_sha256 missing", key_path.display()))?
        .to_string();

    let wasm_path = install_dir.join("components").join(format!("{component_name}.wasm"));
    let cwasm_path = install_dir.join("cache").join(format!("{component_name}.cwasm"));
    let wasm_bytes = fs::read(&wasm_path)
        .with_context(|| format!("read {}", wasm_path.display()))?;
    let current_wasm_sha = sha256_hex(&wasm_bytes);
    let current_engine_hash = engine_compatibility_hash_hex(engine);

    let engine_match = current_engine_hash == stored_engine_hash;
    let wasm_match = current_wasm_sha == stored_wasm_sha;
    let cache_present = cwasm_path.exists();

    if !engine_match || !wasm_match || !cache_present {
        log::info!(
            "loader: cache drift for {app_id} {version_str} \
             (engine={engine_match} wasm={wasm_match} cwasm_present={cache_present}) — re-precompiling",
        );
        let cwasm_bytes = engine.precompile_component(&wasm_bytes)
            .map_err(|e| anyhow!("precompile_component({component_name}): {e:#}"))?;
        fs::write(&cwasm_path, &cwasm_bytes)
            .with_context(|| format!("write {}", cwasm_path.display()))?;
        let new_cwasm_sha = sha256_hex(&cwasm_bytes);
        // Preserve the existing `[dependencies_resolved]` section
        // verbatim — engine/wasm drift doesn't invalidate dep entries
        // (those are checked separately in step 5).
        let preserved_deps = parse_resolved_deps_from_key(&key);
        let new_key = format_cache_key(
            engine,
            &[(
                component_name.clone(),
                ComponentCacheEntry {
                    wasm_sha256: current_wasm_sha,
                    cwasm_sha256: new_cwasm_sha,
                },
            )],
            &preserved_deps,
        );
        fs::write(&key_path, new_key)
            .with_context(|| format!("write {}", key_path.display()))?;
        log::info!("loader: re-stamped {}", key_path.display());
    } else {
        log::debug!("loader: cache fresh for {app_id} {version_str}");
    }

    // Task 46 step 2 — preload registry hit avoids the deserialize.
    // The cwasm-on-disk path is canonicalized before lookup so the
    // registry's keying (`preload::preload_app` uses the same
    // canonicalization) matches.
    let cwasm_canon = cwasm_path.canonicalize().unwrap_or(cwasm_path.clone());
    let component = if let Some(c) = crate::preload::get(&cwasm_canon) {
        log::debug!("loader: preload hit for {}", cwasm_canon.display());
        c
    } else {
        unsafe { Component::deserialize_file(engine, &cwasm_path) }
            .map_err(|e| anyhow!("Component::deserialize_file({}): {e:#}", cwasm_path.display()))?
    };
    let label = format!("installed:{app_id}:{version_str}:{component_name}");

    // Task 36 step 5 — load same-Store deps. Reads `[dependencies_resolved]`
    // from cache-key.toml (parsed above as `key`), looks up each dep's
    // cwasm under `<root>/<kind_dir>/<id>/<version>/cache/<entry>.cwasm`,
    // verifies the dep's on-disk wasm sha256 still matches the stored
    // value (warn-and-load on mismatch; full re-precompile-the-dep is a
    // follow-up), and deserializes the dep's cwasm for instantiation at
    // `LoadedApp::instantiate` time.
    let deps = load_dep_components(engine, root, &key)?;

    Ok((component, label, deps, install_dir))
}

fn load_dep_components(
    engine: &Engine,
    root: &Path,
    key: &toml::Value,
) -> Result<Vec<LoadedDep>> {
    let mut deps: Vec<LoadedDep> = Vec::new();
    let Some(tbl) = key.get("dependencies_resolved").and_then(|v| v.as_table()) else {
        return Ok(deps);
    };
    for (name, val) in tbl {
        let Some(entry) = val.as_table() else { continue };
        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let version = entry.get("version").and_then(|v| v.as_str()).unwrap_or("");
        let stored_sha = entry.get("wasm_sha256").and_then(|v| v.as_str());
        let interface = entry.get("interface").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!(
                "dependency {name}: missing interface in cache-key.toml — \
                 same-store composition requires the WIT-qualified interface name"
            ))?
            .to_string();

        // Host deps are satisfied by `wasmtime_wasi::p2::add_to_linker_sync`
        // + `bindings::SkikoUi::add_to_linker` already applied in
        // `LoadedApp::instantiate`. No per-load wiring needed.
        if kind == "host" {
            log::debug!("loader: host dep `{name}` ({interface}) — already in linker");
            continue;
        }

        let kind_dir = match kind {
            "system" => "system-apps",
            "app"    => "apps",
            other    => bail!("dependency {name}: unknown kind {other:?}"),
        };
        let dep_dir = root.join(kind_dir).join(id).join(version);
        if !dep_dir.is_dir() {
            bail!(
                "dependency {name}: missing install dir {} \
                 (was the dep uninstalled after consumer install?)",
                dep_dir.display(),
            );
        }

        // Verify the dep's on-disk wasm hasn't drifted since consumer
        // install. Full re-precompile (touching the DEP's own
        // cache-key.toml) is a follow-up; for now, warn-and-load.
        if let Some(expected) = stored_sha {
            let (_dep_wasm_path, current) = hash_dep_wasm(&dep_dir).with_context(|| {
                format!("dependency {name}: failed to hash wasm under {}", dep_dir.display())
            })?;
            if current != expected {
                log::warn!(
                    "loader: dependency `{name}` wasm sha256 drifted \
                     (stored={expected}, current={current}) — loading anyway. \
                     Re-install the consumer to refresh cache-key.toml."
                );
            }
        }

        let cwasm_path = first_cwasm(&dep_dir.join("cache")).with_context(|| {
            format!("dependency {name}: no cwasm under {}/cache", dep_dir.display())
        })?;
        let cwasm_canon = cwasm_path.canonicalize().unwrap_or(cwasm_path.clone());
        let component = if let Some(c) = crate::preload::get(&cwasm_canon) {
            log::debug!("loader: preload hit for dep `{name}` at {}", cwasm_canon.display());
            c
        } else {
            deserialize_dep_or_reprecompile(engine, &cwasm_path, &dep_dir, name)?
        };
        log::info!("loader: loaded dep `{name}` ({interface}) from {}", cwasm_path.display());
        deps.push(LoadedDep { name: name.clone(), interface, component });
    }
    Ok(deps)
}

/// Deserialize a dependency's AOT cwasm, self-healing on engine drift. The
/// top-level app component recomputes its engine hash and re-precompiles in
/// `load_installed`, but a dep's cwasm is `deserialize_file`d directly — so a
/// host `wasmtime` upgrade (e.g. 44→45) leaves dep cwasms AOT'd by an
/// incompatible engine, and `deserialize_file` fails with "Module was compiled
/// with incompatible version". Recover the same way the app does: re-precompile
/// from the dep's source `.wasm`, overwrite the cwasm, and retry. Without this,
/// a runtime bump silently drops the consumer to the test-frame fallback
/// (blank screen) until every dep is manually reinstalled.
fn deserialize_dep_or_reprecompile(
    engine: &Engine,
    cwasm_path: &Path,
    dep_dir: &Path,
    name: &str,
) -> Result<Component> {
    match unsafe { Component::deserialize_file(engine, cwasm_path) } {
        Ok(c) => Ok(c),
        Err(first) => {
            log::warn!(
                "loader: dependency `{name}` cwasm load failed ({first:#}) — \
                 re-precompiling from source wasm (likely a wasmtime-version bump)"
            );
            let (wasm_path, _) = hash_dep_wasm(dep_dir).with_context(|| {
                format!("dependency {name}: no source wasm to re-precompile under {}", dep_dir.display())
            })?;
            let wasm_bytes = fs::read(&wasm_path)
                .with_context(|| format!("dependency {name}: read {}", wasm_path.display()))?;
            let cwasm_bytes = engine.precompile_component(&wasm_bytes)
                .map_err(|e| anyhow!("dependency {name}: precompile_component: {e:#}"))?;
            fs::write(cwasm_path, &cwasm_bytes)
                .map_err(|e| anyhow!("dependency {name}: write cwasm {}: {e:#}", cwasm_path.display()))?;
            log::info!("loader: dependency `{name}` re-precompiled → {}", cwasm_path.display());
            unsafe { Component::deserialize_file(engine, cwasm_path) }
                .map_err(|e| anyhow!(
                    "dependency {name}: deserialize after re-precompile ({}): {e:#}",
                    cwasm_path.display()
                ))
        }
    }
}

fn hash_dep_wasm(dep_dir: &Path) -> Result<(PathBuf, String)> {
    let components_dir = dep_dir.join("components");
    for entry in fs::read_dir(&components_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            let bytes = fs::read(&path)?;
            return Ok((path, sha256_hex(&bytes)));
        }
    }
    bail!("no .wasm under {}", components_dir.display())
}

fn first_cwasm(cache_dir: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("cwasm") {
            return Ok(path);
        }
    }
    bail!("no .cwasm under {}", cache_dir.display())
}

/// Pick the lexicographically highest subdirectory of `app_dir`. Works
/// for `MAJOR.MINOR.PATCH` versions; not a proper semver sort (e.g.
/// `0.10.0` < `0.2.0` lexicographically). When that bites, callers
/// should pass `version: Some(...)` explicitly.
fn pick_latest_version(app_dir: &Path) -> Result<String> {
    let mut versions: Vec<String> = Vec::new();
    for entry in fs::read_dir(app_dir)
        .with_context(|| format!("read_dir {}", app_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                versions.push(name.to_string());
            }
        }
    }
    versions.sort();
    versions.pop().ok_or_else(|| anyhow!("no versions installed under {}", app_dir.display()))
}

/// `.cwasm` → AOT `deserialize_file`; anything else → JIT `from_file`.
/// First successful load wins.
fn load_dev_path(engine: &Engine, candidates: &[&Path]) -> Result<(Component, String)> {
    if candidates.is_empty() {
        bail!("AppRef::DevCwasm: empty candidates list");
    }
    let mut last_err: Option<anyhow::Error> = None;
    for path in candidates {
        let is_cwasm = path.extension().map_or(false, |e| e == "cwasm");
        let r = if is_cwasm {
            unsafe { Component::deserialize_file(engine, path) }
        } else {
            Component::from_file(engine, path)
        };
        match r {
            Ok(c) => {
                let label = format!(
                    "{}:{}",
                    if is_cwasm { "cwasm" } else { "wasm" },
                    path.display(),
                );
                return Ok((c, label));
            }
            Err(e) => {
                log::debug!("app_loader: {} miss: {e}", path.display());
                last_err = Some(e.into());
            }
        }
    }
    let detail = last_err
        .map(|e| format!("last error: {e:#}"))
        .unwrap_or_default();
    bail!("no candidate loaded out of {} path(s); {detail}", candidates.len())
}

fn load_dev_asset(engine: &Engine, bytes: &[u8]) -> Result<(Component, String)> {
    let entry = unsafe { Component::deserialize(engine, bytes) }
        .map_err(|e| anyhow!("Component::deserialize (asset, {} bytes): {e:#}", bytes.len()))?;
    Ok((entry, format!("asset:{}B", bytes.len())))
}

/// Same-Store composition glue — task 36 step 5 + task 39 generic refactor.
///
/// Instantiates the dep into the consumer's `Store`, walks its
/// component type to discover every exported interface + function,
/// and registers a proxy on the consumer's `Linker` for each one. The
/// consumer's subsequent instantiate finds its imports satisfied by
/// proxies that delegate back through the dep instance via dynamic
/// `Func::call` with `Val` params/results.
///
/// **No per-dep code in wandr-host.** The dep's WIT type is encoded in
/// the component binary (`Component::component_type()`); wandr-host
/// introspects it at load time. Any future system component with any
/// WIT interface installs + runs without wandr-host changes.
///
/// Trade-off vs the old typed `bindgen!` path: per-call `Val` boxing
/// instead of compile-time-generated typed wrappers. For ~1 Hz calls
/// (markdown render at composition time, emoji list at composition)
/// the overhead is negligible. If a future dep needs 60 Hz calls,
/// revisit with a typed fast-path for that specific interface.
///
/// Resources, top-level exported functions, and async dispatch are
/// out of scope — see `tasks/39-generic-dep-wiring.md` "Out of scope".
fn wire_dep_into_linker(
    linker: &mut Linker<HostState>,
    store: &mut Store<HostState>,
    dep: &LoadedDep,
) -> Result<()> {
    use wasmtime::component::types::ComponentItem;

    let engine = store.engine().clone();

    // Build a dep-local linker. Today's deps only import WASI — no
    // skiko, no cross-dep references. If a future dep imports skiko
    // (e.g. wants to draw), add that bind here.
    let mut dep_linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut dep_linker)
        .map_err(|e| anyhow!("dep linker wasi add: {e:#}"))?;

    // Instantiate the dep into the consumer's store. Its exports
    // become reachable via `instance.get_export` / `instance.get_func`.
    let instance = dep_linker.instantiate(&mut *store, &dep.component)
        .map_err(|e| anyhow!("instantiate dep `{}`: {e:#}", dep.name))?;

    // Walk the dep's component type. Top-level exports are usually
    // ComponentInstance (i.e. an interface like
    // `wandr:markdown/renderer@0.1.0`); inside each instance live the
    // exported functions.
    let component_type = dep.component.component_type();
    let mut wired_fns = 0usize;
    let mut wired_ifaces = 0usize;

    for (export_name, item) in component_type.exports(&engine) {
        match item.ty {
            ComponentItem::ComponentInstance(inst_ty) => {
                // `get_export` returns (ComponentItem, ComponentExportIndex);
                // we only need the index to look up funcs inside.
                let (_, instance_idx) = instance
                    .get_export(&mut *store, None, export_name)
                    .ok_or_else(|| anyhow!(
                        "dep `{}`: instance missing exported interface {export_name:?}",
                        dep.name,
                    ))?;
                let mut consumer_inst = linker.instance(export_name)
                    .map_err(|e| anyhow!("linker.instance({export_name:?}): {e:#}"))?;
                wired_ifaces += 1;
                for (fn_name, fn_item) in inst_ty.exports(&engine) {
                    if !matches!(fn_item.ty, ComponentItem::ComponentFunc(_)) {
                        // Resources / types / nested instances — not
                        // registered as call-able linker entries.
                        continue;
                    }
                    let (_, fn_idx) = instance
                        .get_export(&mut *store, Some(&instance_idx), fn_name)
                        .ok_or_else(|| anyhow!(
                            "dep `{}`: interface {export_name:?} missing fn {fn_name:?}",
                            dep.name,
                        ))?;
                    let func = instance
                        .get_func(&mut *store, &fn_idx)
                        .ok_or_else(|| anyhow!(
                            "dep `{}`: interface {export_name:?}.{fn_name:?}: get_func returned None",
                            dep.name,
                        ))?;
                    // `Func` is `Copy` — capture it directly into the
                    // closure for `'static`-safe registration. The 2nd
                    // closure arg (the function's static type) is unused;
                    // wasmtime validates at call time.
                    consumer_inst.func_new(fn_name, move |mut store, _ty, params, results| {
                        func.call(&mut store, params, results)
                    }).map_err(|e| anyhow!(
                        "linker proxy {export_name:?}.{fn_name:?}: {e:#}"
                    ))?;
                    wired_fns += 1;
                }
            }
            ComponentItem::ComponentFunc(_) => {
                log::warn!(
                    "loader: dep `{}` exports top-level fn `{export_name}` — \
                     wiring at the world level isn't implemented; skipping.",
                    dep.name,
                );
            }
            _ => { /* Resources / types — not runtime call sites. */ }
        }
    }

    log::info!(
        "loader: dep `{}` instantiated; wired {wired_fns} fn(s) across {wired_ifaces} \
         interface(s) into consumer linker (generic)",
        dep.name,
    );
    Ok(())
}
