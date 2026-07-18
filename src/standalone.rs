//! Standalone (no-`NativeActivity`) launch mode — task 33 boot-model.
//!
//! Reached via `wandr-host --standalone`. The runtime runs as a plain
//! privileged process: it acquires a fullscreen surface from SurfaceFlinger
//! through the `libsf_surface` shim (no Activity, no winit `EventLoop`),
//! brings up EGL/Skia on it, and runs the WASM/Compose render loop.
//!
//! The render loop mirrors `lib.rs`'s `WindowEvent::RedrawRequested` handler
//! and the cold-start in `App::resumed`, minus winit. If no cwasm is present
//! it falls back to drawing the renderer test frame.

use std::path::Path;

use anyhow::Result;
use wasmtime::component::ResourceTable;
use wasmtime::{Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::app_loader::{self, AppLoader, AppRef, LoadedApp};
use crate::{App, HostState};

/// Where the `libsf_surface` shim is deployed on the device.
const SHIM_SO: &str = "/data/local/tmp/libsf_surface.so";
/// Where the deployable AOT component is deployed on the device.
const CWASM_PATH: &str = "/data/local/tmp/skiko-component.cwasm";

/// Where + whether this standalone process takes an overlay strip vs a
/// fullscreen surface.
///   - `None`      → fullscreen app (launcher, regular apps).
///   - `Bottom`    → bottom-strip overlay, tall + resizable (IME keyboard, task 47).
///   - `BottomBar` → thin bottom nav strip, always-visible (taskbar, task 56).
///   - `Top`       → top-strip overlay (status bar, task 55).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OverlayMode {
    None,
    Bottom,
    BottomBar,
    Top,
    /// Keyguard/lockscreen — a full-screen surface (like `None`) but at a high
    /// layer (above app + nav, below the status bar), shown/hidden by the arbiter
    /// keyguard module via the foreground signals (`Role::Lockscreen`).
    Lock,
}

pub fn run(app_id: Option<&str>, mode: OverlayMode) -> Result<()> {
    let engine = App::make_engine();
    run_with_engine(&engine, app_id, mode)
}

/// Initial bottom-overlay (IME) panel height in physical pixels — only the
/// pre-arbiter fallback; the IME guest resizes to its real height via
/// `my:skiko-gfx/keyboard.request-overlay-height`.
const INITIAL_OVERLAY_PX: i32 = 1200;

// ── Chrome strip heights, true-dp (Arbiter Inc. 3b) ───────────────────
//
// The arbiter is the authority: it authors the chrome heights (dp×density) and
// hands them to the host via the `register-chrome` reply (this overlay's own
// strip) and the `geometry` line (inset_top=sb, inset_bottom=tb, for everyone's
// `overlay_rect`). The host caches the last arbiter-provided values here.
// `status_bar_height_px()` / `taskbar_height_px()` return the cache, falling
// back to dp×`read_dpi` ONLY if the arbiter never provided one (degraded /
// arbiter-down). The fallback dp consts MIRROR wandr-arbiter-core's
// STATUS_BAR_DP/TASKBAR_DP (the authority) — keep them in lockstep.
use std::sync::atomic::{AtomicU32, Ordering};
static CHROME_SB_PX: AtomicU32 = AtomicU32::new(0);
static CHROME_TB_PX: AtomicU32 = AtomicU32::new(0);
/// Fallback only (arbiter authoritative): mirror of wandr-arbiter-core dp consts.
const FALLBACK_STATUS_BAR_DP: u32 = 38;
const FALLBACK_TASKBAR_DP: u32 = 43;

fn dp_to_px(dp: u32) -> u32 {
    (dp as f32 * (crate::window_impl::read_dpi() as f32 / 160.0)).round() as u32
}

/// Cache the arbiter-provided chrome heights (0 = leave unchanged).
pub fn cache_chrome_heights(sb: u32, tb: u32) {
    if sb > 0 {
        CHROME_SB_PX.store(sb, Ordering::Relaxed);
    }
    if tb > 0 {
        CHROME_TB_PX.store(tb, Ordering::Relaxed);
    }
}

/// Status-bar strip height, px — arbiter-authored (cached), else dp×density.
pub fn status_bar_height_px() -> u32 {
    match CHROME_SB_PX.load(Ordering::Relaxed) {
        0 => dp_to_px(FALLBACK_STATUS_BAR_DP),
        v => v,
    }
}

/// Taskbar strip height, px — arbiter-authored (cached), else dp×density.
pub fn taskbar_height_px() -> u32 {
    match CHROME_TB_PX.load(Ordering::Relaxed) {
        0 => dp_to_px(FALLBACK_TASKBAR_DP),
        v => v,
    }
}


/// Same as `run` but uses a caller-supplied engine. The task-45 zygote
/// child path (`LAUNCH_GUI <app-id>`) goes through here so the wasmtime
/// `Engine` allocated by the parent before `fork()` is reused (COW-
/// shared with siblings), instead of each child re-allocating a fresh
/// one — see [[project-app-lifecycle-and-packaging]] (Hybrid zygote
/// architecture lock).
///
/// `overlay=true` (task 47 step 3c) requests a bottom-strip overlay
/// SurfaceControl from the shim. Falls back to fullscreen with a
/// logged warning if the shim doesn't export `sf_create_overlay_surface`
/// (e.g. an older `libsf_surface.so` predating step 3c).
pub fn run_with_engine(engine: &Engine, app_id: Option<&str>, mode: OverlayMode) -> Result<()> {
    // Debug by default (dev convenience — see everything without extra setup). Verbose per-frame
    // debug!() calls (e.g. layout_text_style, once per text-shape) measurably cost frame time when
    // profiling — set WANDR_LOG_LEVEL=info (or warn/error) to drop them without editing code.
    let level = std::env::var("WANDR_LOG_LEVEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(log::LevelFilter::Debug);
    android_logger::init_once(android_logger::Config::default().with_max_level(level));
    // Surface guest WASI stderr + host panics to logcat (same as android_main).
    crate::wasi_stderr::redirect_stderr_to_logcat();
    log::info!("standalone: starting — no NativeActivity");

    // Task 33 Step 5 — clean-shutdown signals, crash marker, screen-state.
    crate::lifecycle_standalone::install_signal_handlers();
    crate::lifecycle_standalone::install_panic_hook();
    crate::lifecycle_standalone::drain_prior_crash_marker();

    // Task 46 step 4 — arbiter-driven foreground/background role.
    // Default is Foreground; SIGUSR1 demotes, SIGUSR2 promotes.
    crate::app_role::install_signal_handlers();

    // The shim's SurfaceComposerClient talks to SurfaceFlinger over binder.
    if let Err(e) = crate::binder::init() {
        log::warn!("standalone: binder init: {e}");
    }

    let sf = match mode {
        OverlayMode::Bottom | OverlayMode::BottomBar | OverlayMode::Top => {
            // Geometry: full panel width (w=0). Top status bar at y=0,
            // height STATUS_BAR_PX; bottom IME anchored (y=-1), height
            // INITIAL_OVERLAY_PX. The runtime owns the semantics; the
            // shim is geometry-generic (per the surface-abstraction
            // discussion — task 55).
            // True-dp + chrome-coherence: the chrome overlays (top/bottom-bar)
            // self-register with the arbiter and size their strip to the
            // arbiter-authored height (dp×density) it replies — the arbiter is
            // the single source. Done before surface creation so we create at
            // the right height. The IME (Bottom) is arbiter-launched, not chrome.
            let (y, h, label) = match mode {
                OverlayMode::Top => {
                    let px = app_id
                        .and_then(|id| register_chrome_with_arbiter(id, "top"))
                        .map(|px| {
                            cache_chrome_heights(px, 0);
                            px
                        })
                        .unwrap_or_else(status_bar_height_px);
                    (0, px as i32, "top")
                }
                OverlayMode::BottomBar => {
                    let px = app_id
                        .and_then(|id| register_chrome_with_arbiter(id, "bottom-bar"))
                        .map(|px| {
                            cache_chrome_heights(0, px);
                            px
                        })
                        .unwrap_or_else(taskbar_height_px);
                    (-1, px as i32, "bottom-bar")
                }
                _ => (-1, INITIAL_OVERLAY_PX, "bottom"),
            };
            match crate::sf_surface::SfSurface::create_overlay(SHIM_SO, 0, y, 0, h) {
                Ok(sf) => {
                    log::info!(
                        "standalone: {label} overlay surface {}x{} transform 0x{:x} \
                         (h={} px, ANativeWindow={:p})",
                        sf.width, sf.height, sf.transform, h, sf.native_window,
                    );
                    sf
                }
                Err(e) => {
                    log::warn!(
                        "standalone: overlay surface unavailable ({e:#}) — \
                         falling back to fullscreen. Rebuild libsf_surface.so on a-03."
                    );
                    crate::sf_surface::SfSurface::create(SHIM_SO)?
                }
            }
        }
        // Fullscreen surface — the app (None) and the keyguard (Lock) both get a
        // full-panel SF surface; z-order is the `fg_layer` (below) + set_layer.
        OverlayMode::None | OverlayMode::Lock => {
            let sf = crate::sf_surface::SfSurface::create(SHIM_SO)?;
            log::info!(
                "standalone: surface {}x{} transform 0x{:x} (ANativeWindow={:p}, mode={mode:?})",
                sf.width, sf.height, sf.transform, sf.native_window,
            );
            sf
        }
    };

    // Keyguard self-registers with the arbiter so it's tracked (`wandr.keyguard →
    // pid`); the keyguard module flips it from the registered Chrome surface to
    // Role::Lockscreen on lock. Anchor "lock" → no strip height (it's fullscreen).
    if mode == OverlayMode::Lock {
        if let Some(id) = app_id {
            register_chrome_with_arbiter(id, "lock");
        }
    }

    // The producer transform hint is only valid once EGL connects, so the
    // renderer queries it through this closure mid-`from_native_window`.
    let mut renderer = crate::canvas_impl::SkiaRenderer::from_native_window(
        sf.native_window, sf.width as u32, sf.height as u32,
        || sf.query_transform_hint(),
    )?;
    log::info!(
        "standalone: renderer up — EGL/Skia on the SurfaceFlinger window ({}x{})",
        renderer.width, renderer.height,
    );

    // Chrome content insets for a fullscreen app (true-dp, Arbiter Inc. 3b):
    // report the panel size + density up and reserve the arbiter-authored chrome
    // strips (dp×density) so the app never draws under the chrome. Pulling the
    // insets synchronously from the report-panel reply (vs. relying on a push to
    // a not-yet-bound control socket) sets them before the first frame — no
    // launch-time flicker. The arbiter is the single source; on arbiter-down we
    // render full and a later geometry push fills them in. Only fullscreen mode
    // insets — the chrome overlays render full strips.
    if mode == OverlayMode::None {
        let dpi = crate::window_impl::read_dpi();
        if let Some((top, bottom)) =
            report_panel_to_arbiter(sf.panel_w.max(0) as u32, sf.panel_h.max(0) as u32, dpi)
        {
            cache_chrome_heights(top, bottom);
            renderer.set_insets(top, bottom);
        }
    }

    let loader = app_loader::default_for_target();
    // Whether this host was launched for a specific installed app (via the arbiter)
    // vs. a dev bring-up with no app. Decides the load-failure behavior below.
    let is_launched_app = app_id.is_some();
    let app_ref = match app_id {
        Some(id) => AppRef::Installed { app_id: id, version: None },
        None => AppRef::DevCwasm { candidates: &[Path::new(CWASM_PATH)] },
    };
    let result = match loader.load(engine, app_ref) {
        Ok(loaded) => {
            log::info!("standalone: loaded {}", loaded.source_label);
            // Z-stack (task 56): fullscreen apps at the bottom; the
            // taskbar nav strip above apps but below the IME/status-bar
            // chrome, so the keyboard (and status bar) draw over it; the
            // IME + status bar at the top.
            let fg_layer = match mode {
                OverlayMode::None      => 0x4000_0000,
                OverlayMode::BottomBar => 0x6000_0000,
                // Keyguard: above app + nav, below the status bar (i32::MAX) so the
                // status-bar clock/battery stays visible on the lock screen.
                OverlayMode::Lock      => 0x7000_0000,
                OverlayMode::Bottom | OverlayMode::Top => i32::MAX,
            };
            // Task 62/63 — rotation gate. Fullscreen apps rotate UNLESS they
            // explicitly declare `orientation = "locked"` (absent ⇒ rotates,
            // preserving task-43); overlays rotate only on explicit
            // `orientation = "auto"`. A locked fullscreen app additionally
            // publishes a system lock (below) so the chrome stays portrait.
            let is_locked = loaded.orientation_locked();
            let rotates = if mode == OverlayMode::None {
                !is_locked
            } else {
                loaded.rotation_policy()
            };
            run_cwasm_loop(engine, loaded, renderer, sf, mode, rotates, is_locked, fg_layer)
        }
        Err(e) => {
            if is_launched_app {
                // A launched/installed app failed to load (bad cwasm, ABI drift, missing
                // dep, …). Do NOT take over the panel with the fullscreen test-frame loop:
                // it paints a black screen + white rect over the (separate, still-alive)
                // status-bar/taskbar overlays and keeps input focus, so the user is stuck
                // with no way to switch or kill it. Exit instead — the arbiter observes the
                // app's death and restores the launcher + chrome (normal app-death recovery).
                log::error!(
                    "standalone: app load failed ({e:#}) — exiting so the arbiter can recover \
                     (no test-frame takeover for launched apps)"
                );
                Err(e)
            } else {
                // Dev bring-up: no cwasm deployed at CWASM_PATH. The test frame is the
                // intended "renderer works" indicator (run `wandr-host` with no app).
                log::warn!("standalone: no cwasm deployed ({e:#}) — test-frame loop");
                run_test_loop(renderer)
            }
        }
    };

    if result.is_ok() {
        crate::lifecycle_standalone::record_clean_exit();
    }
    result
}

// The Device Orientation HAL value → renderer dihedral `orient` mapping
// (Surface.ROTATION_* index → 0→0, 1→4, 2→3, 3→7) now lives ONLY in the arbiter
// WM (`device_rotation_to_orient`, wandr-arbiter-wm). Task 94 made the arbiter the
// sole device-orientation consumer: it reads the HAL sensor itself and pushes the
// decided content `orient` down via a `geometry` line. The host no longer reads
// the sensor — it is a pure applier of that push (see `authoritative_orient`).

// Chrome-coherence (Arbiter Increment 3a) — the orientation lock used to be a
// polled file (`/data/local/tmp/wandr-orient-lock`) because the chrome overlays
// weren't arbiter-tracked. They now self-register (`register-chrome`) as
// `Role::Chrome` surfaces and the arbiter fans the decided/locked orient to
// them, so the file is gone: the foreground app reports its lock via
// `set-orientation-lock` and the arbiter is the single orientation authority.

/// Task 73 — the arbiter socket (wandr-arbiter-wm owns the orientation
/// decision). Same path the IME/keyboard host clients use.
// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

/// One-shot arbiter command (connect, write line, drain + drop reply, close) —
/// the fire-and-forget shape shared by the chrome-coherence reports.
fn send_arbiter_oneshot(line: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};
    use crate::arbiter_sock::UnixStream;
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap_or(0);
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Parse a `key=<u32>` token out of an arbiter reply line (true-dp replies).
fn parse_reply_u32(reply: &str, key: &str) -> Option<u32> {
    let needle = format!("{key}=");
    let after = &reply[reply.find(&needle)? + needle.len()..];
    let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Chrome-coherence + true-dp — a host-spawned chrome overlay (statusbar/taskbar)
/// self-registers with the arbiter so it's tracked as a `Role::Chrome` surface
/// (the arbiter fans orientation to its control socket) AND learns its strip
/// `height` (dp×density, the arbiter-authored chrome height). A few short retries
/// since `run-hybrid-stack.sh` spawns chrome right as the arbiter comes up.
/// Returns the strip height px (`None` ⇒ arbiter unreachable / density unknown →
/// host falls back).
fn register_chrome_with_arbiter(app_id: &str, anchor: &str) -> Option<u32> {
    let line = format!("register-chrome {app_id} {} {anchor}\n", std::process::id());
    for attempt in 0..10 {
        match send_arbiter_oneshot(&line) {
            Ok(reply) => {
                let height = parse_reply_u32(&reply, "height").filter(|&h| h > 0);
                log::info!("standalone: registered chrome {app_id} ({anchor}) with arbiter → height={height:?}");
                return height;
            }
            Err(e) => {
                log::debug!("standalone: register-chrome attempt {attempt} failed: {e}; retrying");
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    log::warn!("standalone: register-chrome {app_id} gave up — arbiter unreachable");
    None
}

/// True-dp — the foreground fullscreen app reports the panel size + density on
/// startup; the arbiter stores density and replies with the authored content
/// insets `(inset_top, inset_bottom)` so the app reserves the chrome strips
/// before its first frame (no launch-time push race). `None` ⇒ arbiter
/// unreachable (host renders full until a later geometry push).
fn report_panel_to_arbiter(w: u32, h: u32, dpi: u32) -> Option<(u32, u32)> {
    let line = format!("report-panel {w} {h} {dpi}\n");
    match send_arbiter_oneshot(&line) {
        Ok(reply) => {
            let top = parse_reply_u32(&reply, "inset-top")?;
            let bottom = parse_reply_u32(&reply, "inset-bottom")?;
            log::info!("standalone: report-panel {w}x{h} dpi={dpi} → insets ({top},{bottom})");
            Some((top, bottom))
        }
        Err(e) => {
            log::debug!("standalone: report-panel failed ({e}); arbiter down?");
            None
        }
    }
}

/// Chrome-coherence — the foreground fullscreen app reports whether it pins
/// orientation; the arbiter gates the orient it fans to chrome/IME on this
/// (replaces the old `wandr-orient-lock` file write). Best-effort.
fn report_orientation_lock_to_arbiter(locked: bool) {
    let line = format!("set-orientation-lock {}\n", locked as u8);
    if let Err(e) = send_arbiter_oneshot(&line) {
        log::debug!("standalone: set-orientation-lock failed ({e}); arbiter down?");
    }
}

/// PowerManager — the loader reports this app's power class to the arbiter at
/// startup so the arbiter (`wandr-arbiter-power`) can apply a class-based doze
/// policy (a background-service gets a lenient maintenance cadence; everyone else
/// backs off harder when the screen is off). "Host reads/reports, arbiter
/// owns/decides": the host parses the manifest `background` flag; the arbiter owns
/// the class + the policy. Best-effort.
fn report_power_class_to_arbiter(bg_service: bool) {
    let class = if bg_service { "bg-service" } else { "normal" };
    let line = format!("report-power-class {} {class}\n", std::process::id());
    if let Err(e) = send_arbiter_oneshot(&line) {
        log::debug!("standalone: report-power-class failed ({e}); arbiter down?");
    }
}

/// Physical compass edge of the portrait panel buffer.
#[derive(Clone, Copy)]
enum Edge { North, South, East, West }

/// Task 62 — the panel-buffer-space rect `(x, y, w, h)` an anchored chrome
/// overlay must occupy for a given content-rotation `orient` (0/4/3/7 from
/// [`device_rotation_to_orient`]), so it stays on the *user's* anchored
/// edge after the device rotates. Unified anchor-aware model so the status
/// bar, taskbar and IME all rotate coherently (no longer assumes the bars
/// stay at the physical top/bottom):
///
/// - status bar (`Top`)      → the USER's TOP edge, thickness `sb`.
/// - taskbar (`BottomBar`)   → the USER's BOTTOM edge, thickness `tb`.
/// - IME (`Bottom`)          → the USER's BOTTOM edge, thickness = the
///                             keyboard depth, offset `tb` inward so it sits
///                             just above the taskbar.
///
/// The panel buffer is fixed portrait `pw × ph`. Which physical edge is the
/// user's bottom depends on `orient` (device-verified handedness): 0→South,
/// 3→North, 4→West, 7→East; the user's top is the opposite edge. A strip is
/// `th` thick along the edge normal, full-span along the edge, pushed `off`
/// px inward. For 90°/270° the strip is vertical (`w = th, h = ph`); after
/// the host resizes the GL buffer to that, the renderer's dihedral transform
/// swaps logical dims so the guest re-lays-out landscape-wide.
///
/// `None` (fullscreen) never rotates its rect (gated by `is_overlay`).
///
/// Handedness: if a strip lands on the wrong physical side in landscape on a
/// given panel, swap the `4`/`7` arms in `user_bottom_edge` (mirrors the
/// caveat on [`device_rotation_to_orient`]). Host-side only — no shim rebuild.
fn overlay_rect(mode: OverlayMode, orient: u32, pw: i32, ph: i32, t: i32, sb: i32, tb: i32) -> (i32, i32, i32, i32) {
    // Task 71 — `t` is the exact keyboard depth the IME requested for the
    // CURRENT orientation; the host applies it verbatim (the IME re-requests a
    // smaller px in landscape itself). No host-side portrait/landscape scaling;
    // bars keep their fixed thickness (sb/tb) in any orientation.
    let ime_depth = t;
    // (at the user's bottom edge?, thickness, inward offset)
    let (at_bottom, th, off) = match mode {
        OverlayMode::Top       => (false, sb, 0),         // status bar — user top
        OverlayMode::BottomBar => (true,  tb, 0),         // taskbar — user bottom
        OverlayMode::Bottom    => (true,  ime_depth, tb), // IME — above the taskbar
        // fullscreen — no anchored-strip flip (app + keyguard)
        OverlayMode::None | OverlayMode::Lock => return (0, 0, pw, ph),
    };
    let user_bottom_edge = match orient {
        0 => Edge::South, // portrait — physical bottom
        3 => Edge::North, // 180°
        4 => Edge::West,  // 90°  — physical left
        7 => Edge::East,  // 270° — physical right
        _ => Edge::South,
    };
    let edge = if at_bottom {
        user_bottom_edge
    } else {
        match user_bottom_edge { // user top = opposite edge
            Edge::South => Edge::North,
            Edge::North => Edge::South,
            Edge::West  => Edge::East,
            Edge::East  => Edge::West,
        }
    };
    // Place a `th`-thick, full-span strip `off` px inward from `edge`.
    match edge {
        Edge::South => (0, ph - off - th, pw, th),
        Edge::North => (0, off, pw, th),
        Edge::West  => (off, 0, th, ph),
        Edge::East  => (pw - off - th, 0, th, ph),
    }
}

/// The real render loop: instantiate the component and drive `render_frame`.
fn run_cwasm_loop(
    engine: &wasmtime::Engine,
    loaded: LoadedApp,
    // `mut` because load_asset_fonts (below) now takes &mut self (app-bundled font registration).
    mut renderer: crate::canvas_impl::SkiaRenderer,
    sf: crate::sf_surface::SfSurface,
    // Task 62 — the surface's anchor (fullscreen vs which overlay edge).
    // Drives the rotated-rect geometry flip for overlays.
    mode: OverlayMode,
    // Task 43/62 — auto-follow device screen rotation. Obsolete on the host since
    // task 94 (the arbiter decides orientation and pushes it down; the host is a
    // pure applier), kept in the signature for the callers. Prefixed `_` = unused.
    _enable_rotation: bool,
    // Task 63 — this app explicitly declared `orientation = "locked"`. When
    // it's a foreground fullscreen app it publishes the system orientation
    // lock so the chrome (overlays) stays portrait too.
    orientation_locked: bool,
    // Task 55 — SurfaceFlinger layer for the Foreground role. System
    // chrome (status bar / IME overlays) uses i32::MAX; fullscreen apps
    // use a lower band so chrome always composites above them (otherwise
    // a newly-launched app, created after the status bar, wins the
    // equal-layer tie-break and covers it).
    fg_layer: i32,
) -> Result<()> {
    use crate::ui_shell_bindings::wandr::ui_shell::lifecycle::State;

    // Logical surface size the guest must lay its UI out to. The winit path
    // gets this from a `WindowEvent::Resized`; standalone has no winit, so we
    // drive the guest's `on-resize` export explicitly once, below.
    let (logical_w, logical_h) = (renderer.logical_width, renderer.logical_height);

    // Task 71 step 3 — record the REAL panel size for this process so an overlay
    // guest (whose own surface is just a strip) can read the true screen via
    // `display.display-size`. `sf.panel_w/panel_h` came from the `sf_panel_dims`
    // shim; for a fullscreen app they equal the surface size (no-op).
    crate::canvas_impl::set_panel_dims(sf.panel_w as u32, sf.panel_h as u32);

    // ── Cold start — mirrors App::resumed's cold path ────────────────────
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdin().inherit_stdout();
    wasi_builder.stderr(crate::wasi_stderr::LogcatStderr);
    // Task 38 — installed apps with an `assets/` dir get it preopened
    // read-only at `/assets` in the guest. Dev paths skip this (no
    // install dir exists). Failure to preopen is non-fatal — log + run
    // without filesystem; guest reads will return ENOENT.
    if let Some(assets) = loaded.assets_dir() {
        match wasi_builder.preopened_dir(&assets, "/assets", DirPerms::READ, FilePerms::READ) {
            Ok(_)  => log::info!("standalone: preopened {} → /assets (read-only)", assets.display()),
            Err(e) => log::warn!("standalone: preopen {} failed: {e:#}", assets.display()),
        }
        renderer.load_asset_fonts(&assets);
    }
    // Task 41 — /system/fonts/ preopen for the system-fonts dep.
    // Always-on, read-only. Guests that don't need fonts pay nothing
    // (just an unused preopen entry).
    match wasi_builder.preopened_dir("/system/fonts", "/system-fonts", DirPerms::READ, FilePerms::READ) {
        Ok(_)  => log::info!("standalone: preopened /system/fonts → /system-fonts (read-only)"),
        Err(e) => log::warn!("standalone: preopen /system/fonts failed: {e:#}"),
    }
    // (The device music library at /data/media/0/Music → /music is no longer
    // hardcoded here — audio.player declares it as a `[[mounts]]` entry in its
    // package.toml, applied below via apply_mounts. No per-app paths in the host.)
    // Task 67 — writable /state for guest persistence (e.g. the Signal engine's
    // account + protocol snapshot + history). Read-write; created on demand.
    if let Some(state) = loaded.state_dir() {
        match wasi_builder.preopened_dir(&state, "/state", DirPerms::all(), FilePerms::all()) {
            Ok(_)  => log::info!("standalone: preopened {} → /state (read-write)", state.display()),
            Err(e) => log::warn!("standalone: preopen {} failed: {e:#}", state.display()),
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
            current: State::Resumed,
            pending: Some(State::Resumed),
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
    #[cfg(feature = "profile")]
    {
        store.limiter(|h| &mut h.growth_log);
        store.call_hook(|_cx, kind| {
            crate::profiling::on_call_hook(kind);
            Ok(())
        });
    }

    let inst = loaded.instantiate(&mut store)?;
    // Task 115 — a WASI-0.3-importing guest (e.g. the Signal engine composite)
    // has native async tasks that advance only while the host drives the store
    // event loop; its naps must pump instead of plain-sleep. Sync guests keep
    // the plain sleep (nothing to drive).
    #[cfg(feature = "p3-async")]
    let pump_naps = loaded.requires_async_drive();
    #[cfg(feature = "p3-async")]
    if pump_naps {
        log::info!("standalone: guest imports WASI 0.3 — naps will pump the CM-async event loop");
    }
    let ime_events = inst.ime_events;
    // Arbiter Inc. 3c — Some(...) only if the guest exports wandr:alarm/alarm-handler.
    let alarm_events = inst.alarm_events;
    // Signal bg-receipt M3 — Some(...) only if the guest exports wandr:notify/notify-handler.
    let notify_events = inst.notify_events;
    // wandr-arbiter-audio M2 — Some(...) only if the guest exports wandr:audio-focus/focus-handler.
    let audio_focus_events = inst.audio_focus_events;
    // Task 108 M2 — Some(...) only if the guest exports wasi:media-session/session-handler.
    let media_session_handler = inst.media_session_handler;
    // Task 90 event bus — Some(...) only if the guest exports wandr:events/incoming-handler.
    // The `evt-subscribe` send is DEFERRED until after the control socket is bound
    // (below), so the arbiter's retained-value-on-subscribe delivery isn't dropped
    // by a not-yet-listening socket.
    let events_incoming = inst.events_incoming;
    // Phase B — wandr:ui-shell export probes. shell-events takes lifecycle +
    // scheduler callbacks EXCLUSIVELY when bound; shell frame-pacing likewise
    // wins over the legacy my:skiko-gfx/frame-pacing probe.
    let shell_events = inst.shell_events;
    let shell_pacing = inst.shell_pacing;
    // wasi:input-handlers probes — exclusive routing per input type.
    let guest_input = inst.guest_input;
    // Signal bg-receipt (M2) — a background-service keeps pumping its engine
    // (`bg-tick`) while backgrounded instead of freezing. `bg_service` is the
    // manifest opt-in; `bg_tick` is the typed export (both required to pump).
    let bg_tick = inst.bg_tick;
    let bg_service = loaded.background_service();
    if bg_service && bg_tick.is_some() {
        log::info!("standalone: background-service pump active (pumps bg-tick while backgrounded)");
    }
    // PowerManager — report our power class so the arbiter applies class-based doze.
    report_power_class_to_arbiter(bg_service);
    // Task 64 follow-up — per-app render-rate cap. Resolution order:
    // WANDR_MAX_FPS env (global, for testing) > package.toml `max_fps` > 60.
    // Enforced host-side as a floor on the render interval, so it caps every
    // app (Compose / dioxus / canvas) with no guest code. Input is still
    // polled at POLL_MS regardless, so touch latency is unchanged.
    let target_fps: u32 = std::env::var("WANDR_MAX_FPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n >= 1 && *n <= 240)
        .unwrap_or_else(|| loaded.max_fps());
    let frame_interval = std::time::Duration::from_millis((1000 / target_fps.max(1)) as u64);
    log::info!(
        "standalone: render cap {target_fps} fps (min interval {} ms)",
        frame_interval.as_millis()
    );
    log::info!("standalone: component instantiated — entering render loop");

    // Tell the guest the surface size before the first frame, so Compose lays
    // out to the full panel (no winit `Resized` event to do this for us).
    if let Err(e) = crate::input::dispatch_resize_routed(&mut store, &guest_input, logical_w, logical_h)
    {
        log::warn!("standalone: on_resize({logical_w}x{logical_h}) failed: {e:#}");
    }

    // Guest Paused/Resumed is driven SOLELY by the arbiter's role transitions
    // (fg→Resumed / bg→Paused, handled below). The old host-side screen-state
    // watcher (`spawn_screen_state_watcher`, polling `debug.tracing.screen_state`)
    // was removed: that sysprop is SurfaceFlinger's debug echo of the last
    // setPowerMode and goes STALE with ART stopped, AND it can't tell a transient
    // proximity blank from a real screen-off — so under --no-art it paused the
    // foreground guest on every cheek-at-the-ear blank during a call (killing its
    // frame-coupled audio pump + input) and could leave it stuck Paused on a stale
    // value. The arbiter is the sole power/screen authority; on a REAL screen-off it
    // demotes the fg app → Background → the role path below pauses us, while a
    // proximity blank (no role change) correctly leaves the call running.

    // Re-request input focus periodically — activity-backed windows AMS
    // resumes (launcher, last app) steal focus despite wandr owning the
    // z-top SurfaceFlinger layer. Refresh roughly once per second.
    let focus_refresh_interval: u64 = 60; // frames @ ~60fps target

    // Task 46 step 4 — track previous arbiter-driven role so we react
    // exactly once per transition (not every frame). Newly forked
    // children default to Foreground, so the first frame logs a
    // promote-to-fg and ensures z-order + lifecycle.
    let mut last_role: Option<crate::app_role::AppRole> = None;

    // Task 47 step 3a — per-host control socket for arbiter-pushed
    // events. The accept thread listens on
    // /data/local/tmp/wandr-host-<pid>.sock; the queue drain below
    // dispatches each event into the guest in the render-loop
    // thread (where the Store lives — wasmtime Store is !Send).
    // Hold the bound socket path so the graceful-shutdown break below
    // can unlink it (task 54 part B). SIGKILL (the LMK case) skips this
    // — covered instead by the arbiter's death-driven unlink + the
    // zygote's startup sweep.
    let ime_inbound_sock: Option<String> = match crate::ime_inbound::spawn_listener() {
        Ok(path) => {
            log::info!("standalone: ime-inbound listening on {path}");
            Some(path)
        }
        Err(e) => {
            log::warn!("standalone: ime-inbound spawn failed: {e:#}");
            None
        }
    };

    // Task 90 event bus — subscribe NOW that the control socket is bound, so the
    // arbiter's retained-value delivery (sent in reply to `evt-subscribe`) lands on
    // a listening socket. Subscription is host-config (not a WIT call): register the
    // guest's `package.toml [events] subscribe` topics. The arbiter delivers each
    // topic's retained value immediately + every later change as an
    // `event <topic> <payload>` line → the drain calls `handle`.
    if events_incoming.is_some() {
        let pid = std::process::id();
        for topic in loaded.event_subscriptions() {
            if let Err(e) = send_arbiter_oneshot(&format!("evt-subscribe {pid} {topic}\n")) {
                log::debug!("standalone: evt-subscribe {topic} failed ({e}); arbiter down?");
            } else {
                log::info!("standalone: subscribed to event topic {topic:?}");
            }
        }
    }

    // (Chrome overlays already self-registered with the arbiter during surface
    // creation above — register-chrome's reply gave their strip height. The
    // control socket bound here is for the arbiter's ongoing geometry pushes.)

    // Task 94 — runtime screen orientation is now ARBITER-SOURCED. The arbiter's
    // sensor-driver reads the native Device Orientation HAL sensor itself, the WM
    // decides the content orient (honoring the foreground lock), and pushes it down
    // to this host as a `geometry … <orient>` line (applied via `authoritative_orient`
    // below). The host no longer opens its own device-orientation handle or reports
    // the raw rotation up — that duplicated the arbiter's sensorservice client and
    // its rotation pipeline (one HAL reading, one decision authority). Overlays were
    // already arbiter-push-only; fullscreen apps now match them. A manual `WANDR_ORIENT`
    // override still pins a fixed orientation in the renderer ctor for stationary tests.

    // ── Render loop — mirrors WindowEvent::RedrawRequested, no winit ─────
    // (Frame pacing is task 64's `next_render_at` gate, set up below.)
    let mut frame: u64 = 0;
    // Task 62 — the overlay's strip thickness (its portrait height). Seeded
    // from the created surface; updated whenever the guest requests a new
    // overlay height. Feeds `overlay_rect` so a rotation re-derives the
    // side-strip geometry at the current thickness. Unused for fullscreen.
    let mut strip_t: i32 = sf.height;
    // Task 73/94 — orientation authority is the arbiter (wandr-arbiter-wm).
    // `authoritative_orient` is the last orient the arbiter pushed down via a
    // `geometry` line (None until the first push). The host applies it and never
    // sources rotation itself — task 94 removed the host's device-orientation
    // read/report, so the arbiter (which reads the HAL sensor + holds the lock
    // policy) is the single source. Both fullscreen apps and overlays are now
    // pure appliers of arbiter pushes.
    let mut authoritative_orient: Option<u32> = None;
    // Task 64 — on-demand rendering. The cheap input/IME/scheduler poll
    // still runs every iteration (≤POLL_MS latency), but the expensive
    // render-frame + buffer swap is gated: it fires only when something
    // changed this iteration (`dirty`) or the guest's requested deadline
    // (`next_render_at`) has arrived. A guest that exports `frame-pacing`
    // drives `next_render_at`; one that doesn't falls back to POLL_MS (the
    // legacy unconditional 60 fps). IDLE_CAP bounds how long a fully static
    // guest can sleep, so e.g. the status-bar clock still ticks ~1/sec.
    const IDLE_CAP_MS: u64 = 1000;
    const POLL_MS: u64 = 16;
    // Idle-adaptive input poll. simpleperf (taskbar, idle) showed the 60 Hz loop
    // ITSELF — InputConsumer::consume + sf_input_poll + scheduler drain + the kernel
    // wakeup — is ~2-4% of a core per surface with zero events flowing, while a
    // Background surface at 200 ms polling costs ~0%. So after IDLE_AFTER_MS with no
    // event activity (`dirty` never set), stretch the poll to IDLE_POLL_MS; the first
    // event that does arrive sets `dirty` and snaps the cadence back to POLL_MS.
    // IDLE_POLL_MS = 3 frame periods: cuts the idle wake rate 3× while the worst-case
    // added latency on the FIRST touch after idle (~paint two frames later) stays
    // under the ~50 ms threshold where a tap starts feeling late; a gesture in
    // progress keeps `dirty` set, so drag/scroll latency is unchanged at 60 Hz.
    const IDLE_POLL_MS: u64 = 3 * POLL_MS;
    const IDLE_AFTER_MS: u64 = 1000;
    let mut last_activity = std::time::Instant::now();
    let mut next_render_at = std::time::Instant::now();
    // Render-INDEPENDENT engine-pump cadence (bg-tick). Separate from next_render_at
    // so a live call can pump the socket/executor/audio at ~60/s even in the
    // foreground, where render is fps-capped (frame_interval) and too slow to keep
    // the ~32 ms audio ring fed. Drives the M2 backgrounded-service pump too.
    let mut next_bg_tick_at = std::time::Instant::now();
    let mut bg_ticks: u64 = 0; // M2 — background-service pump count (throttled log)
    // Doze (PowerManager) — the ARBITER decides dozing (screen-off grace) and pushes
    // `doze <cadence-ms>` to this host; we are a dumb applier (like geometry/orient).
    // While dozing we stretch the per-frame cadence (render AND bg-tick) to that
    // coarse value so a backgrounded/locked guest stops pumping its engine ~1 Hz for
    // an off-screen surface. `0` = not dozing (normal pacing). See wandr-arbiter-power.
    let mut doze_cadence_ms: u64 = 0;
    loop {
        // Task 64 — set true by any event source below that did real work
        // this iteration; forces a render regardless of the pacing deadline.
        let mut dirty = false;
        // Step 5 — SIGTERM / SIGINT / SIGHUP from launcher trap or operator.
        if crate::lifecycle_standalone::should_shutdown() {
            log::info!("standalone: shutdown signal — exiting render loop");
            // Task 54 part B — graceful-path unlink of our control
            // socket so it doesn't linger after a clean exit.
            if let Some(ref p) = ime_inbound_sock {
                let _ = std::fs::remove_file(p);
                log::info!("standalone: removed ime-inbound socket {p}");
            }
            break;
        }

        // Task 46 step 4 — arbiter role transition. SIGUSR1/SIGUSR2
        // updates an atomic; we observe it once per frame. On change:
        //   Foreground → Background: SF set_layer(0), set_visible(false),
        //                            lifecycle Paused.
        //   Background → Foreground: SF set_layer(MAX), set_visible(true),
        //                            request_focus, lifecycle Resumed.
        // Children unaware of the new role (older shim, no signals
        // received) stay Foreground — same behavior as pre-step-4.
        let cur_role = crate::app_role::role();
        if last_role != Some(cur_role) {
            use crate::app_role::AppRole;
            log::info!("standalone: role transition {last_role:?} → {cur_role:?}");
            dirty = true; // task 64 — z-order/lifecycle change needs a frame
            match cur_role {
                AppRole::Foreground => {
                    sf.set_layer(fg_layer);
                    sf.set_visible(true);
                    sf.request_focus();
                    // Chrome-coherence — the foreground fullscreen app reports its
                    // orientation-lock policy to the arbiter, which gates the orient
                    // it fans to the chrome/IME overlays (replaces the old
                    // wandr-orient-lock file).
                    if mode == OverlayMode::None {
                        report_orientation_lock_to_arbiter(orientation_locked);
                    }
                    let target = State::Resumed;
                    if store.data().lifecycle.current != target {
                        store.data_mut().lifecycle.current = target;
                        if let Err(e) = crate::input::dispatch_lifecycle(
                            shell_events.as_ref(), &mut store, target as u32)
                        {
                            log::warn!("standalone: on_lifecycle_changed(fg→Resumed) failed: {e:#}");
                        }
                    }
                }
                AppRole::Background => {
                    sf.set_layer(0);
                    sf.set_visible(false);
                    let target = State::Paused;
                    if store.data().lifecycle.current != target {
                        store.data_mut().lifecycle.current = target;
                        if let Err(e) = crate::input::dispatch_lifecycle(
                            shell_events.as_ref(), &mut store, target as u32)
                        {
                            log::warn!("standalone: on_lifecycle_changed(bg→Paused) failed: {e:#}");
                        }
                    }
                }
                AppRole::OverlayBehind => {
                    // Task 47 step 3c. Stays visible (so the cursor in
                    // the focused editor keeps blinking), demoted in z
                    // (so the IME overlay panel composites on top), and
                    // lifecycle stays Resumed (no Paused fire — the
                    // editor needs to keep rendering text mutations
                    // from the IME). Layer 0 is the same z as the
                    // background pool; IME at i32::MAX or MAX-1 wins.
                    sf.set_layer(0);
                    sf.set_visible(true);
                    let target = State::Resumed;
                    if store.data().lifecycle.current != target {
                        store.data_mut().lifecycle.current = target;
                        if let Err(e) = crate::input::dispatch_lifecycle(
                            shell_events.as_ref(), &mut store, target as u32)
                        {
                            log::warn!("standalone: on_lifecycle_changed(overlay-behind→Resumed) failed: {e:#}");
                        }
                    }
                }
            }
            last_role = Some(cur_role);
        }

        // Task 47 step 3c — drain any pending overlay-height request from
        // the `my:skiko-gfx/keyboard.request-overlay-height` Host impl.
        // No-op on fullscreen surfaces (the SfSurface gate inside
        // `resize_overlay` warns); on overlay surfaces, this re-issues
        // setSize/setPosition + ANativeWindow_setBuffersGeometry so the
        // next frame draws at the new dimensions. EGL/Skia will pick up
        // the new size via the producer-side geometry update.
        if let Some(new_h) = crate::sf_surface::take_pending_overlay_resize() {
            // Task 62 — the guest's requested thickness. Route it through
            // `overlay_rect` (not the raw bottom-anchored `resize_overlay`)
            // so the keyboard lands in the SAFE AREA above the taskbar /
            // below the status bar at the CURRENT orientation — both in
            // portrait (re-anchor above the taskbar) and while rotated
            // (re-derive the side strip at the new depth).
            strip_t = new_h;
            if sf.is_overlay() {
                let orient = store.data().renderer.current_orient;
                let sb = crate::status_impl::status_bar_height_px() as i32;
                let tb = taskbar_height_px() as i32;
                let (rx, ry, rw, rh) =
                    overlay_rect(mode, orient, sf.panel_w, sf.panel_h, new_h, sb, tb);
                if sf.set_overlay_geometry(rx, ry, rw, rh, rw, rh) {
                    dirty = true; // task 64 — geometry change needs a frame
                    store.data_mut().renderer.resize(rw as u32, rh as u32);
                    let (lw, lh) = {
                        let r = &store.data().renderer;
                        (r.logical_width, r.logical_height)
                    };
                    if let Err(e) = crate::input::dispatch_resize_routed(&mut store, &guest_input, lw, lh)
                    {
                        log::warn!("standalone: overlay-resize on_resize({lw}x{lh}) failed: {e:#}");
                    }
                    log::info!(
                        "standalone: overlay resize → rect ({rx},{ry},{rw},{rh}) logical {lw}x{lh}"
                    );
                }
            }
        }

        // (Guest Paused/Resumed is driven by arbiter role transitions above — the
        // host-side screen-state sysprop watcher was removed; see the note at the
        // `screen_rx` removal site.)

        // Task 94 — apply any screen-rotation change. The host no longer reads the
        // HAL device-orientation sensor (the arbiter does that and decides the
        // orient, gating on the foreground lock); it applies whatever orient the
        // arbiter last pushed down via `geometry` (`authoritative_orient`). This
        // holds for both fullscreen apps and overlays — all are pure appliers now.
        // A freshly-foregrounded app stays at its current orient until the arbiter
        // pushes (via `ForegroundChanged`); a manual `WANDR_ORIENT` pins it.
        let cur_orient = store.data().renderer.current_orient;
        let target_orient = authoritative_orient.unwrap_or(cur_orient);
        if target_orient != cur_orient {
            dirty = true; // task 64 — rotation needs a frame
            // Task 62 — for an overlay, the anchored rect itself must flip
            // (bottom strip ⇄ side strip). Do the SF move/resize + GL-buffer
            // resize BEFORE set_orientation, which reads the buffer dims.
            // Fullscreen leaves the buffer alone (task 43).
            if sf.is_overlay() {
                let sb = crate::status_impl::status_bar_height_px() as i32;
                let tb = taskbar_height_px() as i32;
                let (rx, ry, rw, rh) =
                    overlay_rect(mode, target_orient, sf.panel_w, sf.panel_h, strip_t, sb, tb);
                if sf.set_overlay_geometry(rx, ry, rw, rh, rw, rh) {
                    store.data_mut().renderer.resize(rw as u32, rh as u32);
                    log::info!(
                        "standalone: overlay rect flip → orient {target_orient} rect ({rx},{ry},{rw},{rh})"
                    );
                }
            }
            if store.data_mut().renderer.set_orientation(target_orient) {
                let (lw, lh) = {
                    let r = &store.data().renderer;
                    (r.logical_width, r.logical_height)
                };
                log::info!("standalone: orient change → {target_orient} logical {lw}x{lh}");
                if let Err(e) = crate::input::dispatch_resize_routed(&mut store, &guest_input, lw, lh)
                {
                    log::warn!("standalone: rotation on_resize({lw}x{lh}) failed: {e:#}");
                }
            }
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // Drain InputFlinger events and dispatch them to the guest. The
        // shim's input channel is non-blocking; this is the standalone
        // equivalent of winit's touch/key events (task 33 Step 3).
        let mut input_buf = [crate::sf_surface::SfInputEvent::default(); 32];
        for ev in sf.poll_input(&mut input_buf) {
            dirty = true; // task 64 — any input (touch/key) wants a frame
            if ev.kind <= 3 {
                // Task 43 — touch coords arrive in physical-buffer space.
                // When the content is rotated, map them back into logical
                // space via the inverse of the renderer's base_matrix so
                // taps land where the (rotated) UI actually drew. Identity
                // matrix (orient 0) ⇒ inverse is identity ⇒ no-op, so the
                // common unrotated path is unchanged.
                let (lx, ly) = {
                    let base = store.data().renderer.base_matrix;
                    match base.invert() {
                        Some(inv) => {
                            let p = inv.map_point((ev.x, ev.y));
                            (p.x, p.y)
                        }
                        None => (ev.x, ev.y),
                    }
                };
                if let Err(e) = crate::input::dispatch_pointer_routed(
                    &mut store, &guest_input, ev.kind as u8,
                    ev.pointer_id as u32, lx, ly, ev.pressure, [false; 4],
                    crate::input::PointerMeta::touch_contact(ev.kind != 1),
                ) {
                    log::warn!("standalone: dispatch_pointer_v2 failed: {e:#}");
                }
            } else if ev.kind == 10 || ev.kind == 11 {
                // 10=key-down, 11=key-up. Action byte (0/1) matches the
                // dispatch_*_v1/v2 contract.
                // Task 76 P8 — VOLUME_UP(24)/VOLUME_DOWN(25) are system keys: the
                // host (this is the focused app's process) steps the media volume
                // on the active output device and swallows the event so it never
                // reaches the guest.
                if ev.key_code == 24 || ev.key_code == 25 {
                    // Forward to the arbiter (single volume decider); it picks
                    // the target device + owner host and pushes back the apply.
                    if ev.kind == 10 {
                        crate::audio_policy_impl::forward_volume_key(ev.key_code == 24);
                    }
                } else if ev.key_code == 26 {
                    // Task 81/110 — KEYCODE_POWER. Forward DOWN + UP to the arbiter,
                    // which times the press (single decider; dedups the per-host
                    // fan-in): a short press toggles the panel, a hold ≥1 s opens
                    // the power menu. Swallowed from the guest.
                    if ev.kind == 10 {
                        crate::audio_policy_impl::forward_power_down();
                    } else if ev.kind == 11 {
                        crate::audio_policy_impl::forward_power_up();
                    }
                } else {
                    let action = if ev.kind == 10 { 0u8 } else { 1u8 };
                    if let Err(e) = crate::input::dispatch_android_key(
                        &mut store, &guest_input, action, ev.key_code, ev.meta_state,
                    ) {
                        log::warn!("standalone: dispatch_android_key failed: {e:#}");
                    }
                }
            }
        }

        // Task 47 step 3a — drain arbiter-pushed IME events (key
        // synthesis from a virtual keyboard). Same per-frame
        // pattern as the InputFlinger drain above.
        for ev in crate::ime_inbound::drain_queue() {
            dirty = true; // task 64 — IME key/editor event wants a frame
            match ev {
                crate::ime_inbound::InboundEvent::KeyEvent { code_point, key_id, action } => {
                    // wasi:input-handlers key-handler supersedes legacy.
                    let ev4 = crate::input::KeyEvent {
                        down: action == 0,
                        repeat: false,
                        code: crate::input::key_id_to_w3c(key_id).to_string(),
                        text: char::from_u32(code_point).filter(|c| *c != '\0')
                            .map(String::from).unwrap_or_default(),
                        alt: false, ctrl: false, meta: false, shift: false,
                    };
                    match crate::input::dispatch_key_routed(&guest_input, &mut store, &ev4) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            // {:?} includes the wasm backtrace for guest traps.
                            log::warn!("standalone: key-handler (ime-inbound) failed: {e:?}");
                            continue;
                        }
                    }
                }
                // Task 49 step 1a — IME-bound events. Only meaningful
                // for hosts running in IME role (--standalone-overlay).
                // Step 1a logs; step 1b adds the call into the guest's
                // exported wandr:ime/ime.on-editor-attached(info).
                crate::ime_inbound::InboundEvent::EditorAttached { info } => {
                    let Some(ie) = ime_events.as_ref() else {
                        log::warn!(
                            "ime-inbound: editor-attached received but host has \
                             no IME bindings (component doesn't export wandr:ime/ime)"
                        );
                        continue;
                    };
                    // Convert the wire string → typed WIT enum. Unknown
                    // tags fall back to Text — defensive.
                    let wit_input_type = match info.input_type.as_str() {
                        "text"           => crate::ime_bindings::wandr::ime::types::InputType::Text,
                        "number"         => crate::ime_bindings::wandr::ime::types::InputType::Number,
                        "phone"          => crate::ime_bindings::wandr::ime::types::InputType::Phone,
                        "email"          => crate::ime_bindings::wandr::ime::types::InputType::Email,
                        "url"            => crate::ime_bindings::wandr::ime::types::InputType::Url,
                        "password"       => crate::ime_bindings::wandr::ime::types::InputType::Password,
                        "multiline-text" => crate::ime_bindings::wandr::ime::types::InputType::MultilineText,
                        other => {
                            log::warn!(
                                "ime-inbound: unknown input-type {other:?} — defaulting to Text"
                            );
                            crate::ime_bindings::wandr::ime::types::InputType::Text
                        }
                    };
                    if let Err(e) = crate::guest_call!(ie.wandr_ime_ime()
                        .call_on_editor_attached(&mut store, wit_input_type))
                    {
                        log::warn!(
                            "ime-inbound: on-editor-attached failed: {e:#}"
                        );
                        if e.downcast_ref::<wasmtime::ThrownException>().is_some() {
                            if let Some(exn_ref) = store.take_pending_exception() {
                                let _ = log_kotlin_exception_msg(&mut store, &exn_ref);
                            }
                        }
                    } else {
                        log::info!(
                            "ime-inbound: dispatched on-editor-attached input-type={:?} \
                             (hint/text dropped on wire — see ime.wit)",
                            info.input_type,
                        );
                    }
                }
                crate::ime_inbound::InboundEvent::EditorDetached => {
                    let Some(ie) = ime_events.as_ref() else {
                        log::warn!(
                            "ime-inbound: editor-detached received but host has \
                             no IME bindings"
                        );
                        continue;
                    };
                    if let Err(e) = crate::guest_call!(ie.wandr_ime_ime().call_on_editor_detached(&mut store)) {
                        log::warn!("ime-inbound: on-editor-detached failed: {e:#}");
                    } else {
                        log::info!("ime-inbound: dispatched on-editor-detached");
                    }
                }
                crate::ime_inbound::InboundEvent::AlarmFired { id } => {
                    // Arbiter Inc. 3c — a scheduled alarm fired; call the guest's
                    // wandr:alarm/alarm-handler.on-alarm(id). Inert if the guest
                    // doesn't export it (alarm_events == None).
                    dirty = true; // doing guest work this iteration warrants a frame
                    match alarm_events.as_ref() {
                        Some(ae) => match crate::guest_call!(ae.wandr_alarm_alarm_handler().call_on_alarm(&mut store, id)) {
                            Ok(()) => log::info!("alarm-inbound: dispatched on-alarm({id})"),
                            Err(e) => log::warn!("alarm-inbound: on-alarm({id}) failed: {e:#}"),
                        },
                        None => log::warn!(
                            "alarm-inbound: alarm-fired({id}) but guest exports no wandr:alarm/alarm-handler"
                        ),
                    }
                }
                crate::ime_inbound::InboundEvent::NotificationClicked { id } => {
                    // Signal bg-receipt M3 — the user tapped this app's notification
                    // (the arbiter also foregrounded us); call on-notification-click.
                    dirty = true;
                    match notify_events.as_ref() {
                        Some(ne) => match crate::guest_call!(ne
                            .wandr_notify_notify_handler()
                            .call_on_notification_click(&mut store, id))
                        {
                            Ok(()) => log::info!("notify-inbound: dispatched on-notification-click({id})"),
                            Err(e) => log::warn!("notify-inbound: on-notification-click({id}) failed: {e:#}"),
                        },
                        None => log::warn!(
                            "notify-inbound: notification-clicked({id}) but guest exports no wandr:notify/notify-handler"
                        ),
                    }
                }
                crate::ime_inbound::InboundEvent::Event { topic, data } => {
                    // Task 90 event bus — the arbiter fanned an event on a subscribed
                    // topic; call the guest's wandr:events/incoming-handler.handle(msg).
                    // Inert if the guest doesn't export it (events_incoming == None).
                    dirty = true; // delivering guest work warrants a frame
                    match events_incoming.as_ref() {
                        Some(ei) => {
                            let msg = crate::events_incoming_bindings::wandr::events::types::Message {
                                topic: topic.clone(),
                                content_type: None,
                                data,
                            };
                            match crate::guest_call!(ei.wandr_events_incoming_handler().call_handle(&mut store, &msg)) {
                                Ok(()) => log::info!("event-inbound: dispatched handle(topic={topic:?})"),
                                Err(e) => log::warn!("event-inbound: handle(topic={topic:?}) failed: {e:#}"),
                            }
                        }
                        None => log::warn!(
                            "event-inbound: event(topic={topic:?}) but guest exports no wandr:events/incoming-handler"
                        ),
                    }
                }
                crate::ime_inbound::InboundEvent::Doze { cadence_ms } => {
                    // PowerManager — arbiter decided the doze state; apply it (dumb
                    // applier). The cadence-extension at the end of the loop reads
                    // `doze_cadence_ms`. A change wants a frame (resume render
                    // promptly on `doze 0`).
                    if doze_cadence_ms != cadence_ms {
                        doze_cadence_ms = cadence_ms;
                        dirty = true;
                        log::info!("standalone: doze cadence ← {cadence_ms}ms (arbiter)");
                    }
                }
                crate::ime_inbound::InboundEvent::FocusChanged { change } => {
                    // wandr-arbiter-audio M2 — the audio-focus arbiter changed our
                    // focus; call the guest's on-focus-changed (it pauses/ducks/
                    // resumes). Inert if the guest exports no focus-handler.
                    use crate::audio_focus_events_bindings::exports::wandr::audio_focus::focus_handler::FocusChange;
                    let fc = match change {
                        0 => FocusChange::Loss,
                        1 => FocusChange::LossTransient,
                        2 => FocusChange::Duck,
                        _ => FocusChange::Gain,
                    };
                    dirty = true;
                    match audio_focus_events.as_ref() {
                        Some(fe) => match crate::guest_call!(fe
                            .wandr_audio_focus_focus_handler()
                            .call_on_focus_changed(&mut store, fc))
                        {
                            Ok(()) => log::info!("focus-inbound: dispatched on-focus-changed({change})"),
                            Err(e) => log::warn!("focus-inbound: on-focus-changed({change}) failed: {e:#}"),
                        },
                        None => log::warn!(
                            "focus-inbound: on-focus-changed({change}) but guest exports no wandr:audio-focus/focus-handler"
                        ),
                    }
                }
                crate::ime_inbound::InboundEvent::MediaSessionAction { action, seek_time_s } => {
                    // Task 108 M2 — the media-session arbiter routed a transport
                    // intent here (lockscreen tap / headset button). Call the
                    // guest's on-action (it applies play/pause/seek/skip). Inert
                    // if the guest exports no session-handler.
                    use crate::media_session_events_bindings::exports::wasi::media_session::session_handler::{Action, ActionDetails};
                    let act = match action.as_str() {
                        "play" => Some(Action::Play),
                        "pause" => Some(Action::Pause),
                        "stop" => Some(Action::Stop),
                        "seek-to" => Some(Action::SeekTo),
                        "seek-forward" => Some(Action::SeekForward),
                        "seek-backward" => Some(Action::SeekBackward),
                        "previous-track" => Some(Action::PreviousTrack),
                        "next-track" => Some(Action::NextTrack),
                        _ => None,
                    };
                    match (act, media_session_handler.as_ref()) {
                        (Some(action), Some(msh)) => {
                            let details = ActionDetails { action, seek_time_s };
                            dirty = true;
                            match crate::guest_call!(msh
                                .wasi_media_session_session_handler()
                                .call_on_action(&mut store, details))
                            {
                                Ok(()) => log::info!("media-session-inbound: dispatched on-action({action:?})"),
                                Err(e) => log::warn!("media-session-inbound: on-action failed: {e:#}"),
                            }
                        }
                        (None, _) => log::warn!("media-session-inbound: bad action {action:?}"),
                        (_, None) => log::warn!(
                            "media-session-inbound: on-action({action}) but guest exports no wasi:media-session/session-handler"
                        ),
                    }
                }
                crate::ime_inbound::InboundEvent::CommMode { comm } => {
                    // wandr-arbiter-audio M3 — the arbiter started/ended a comms
                    // session on us; apply the call-audio mode recipe (the dumb
                    // applier: mirrors AudioService.onUpdateAudioMode).
                    crate::audio_policy_impl::on_update_audio_mode(comm);
                    // On call-end, drop the MEDIA-strategy route override so
                    // non-call media returns to the policy default (task 97 bug #5).
                    if !comm { crate::audio_impl::clear_comms_route(); }
                }
                crate::ime_inbound::InboundEvent::VolumeAdjust { up, speaker } => {
                    // Arbiter-decided volume step (task 76 P8): apply on the
                    // device the arbiter chose. We are the owner it picked.
                    crate::audio_policy_impl::adjust_volume_on(speaker, up);
                }
                crate::ime_inbound::InboundEvent::MuteSet { muted, speaker } => {
                    // Arbiter-decided output mute (task 76 P8): apply on the chosen device.
                    crate::audio_policy_impl::set_media_mute(speaker, muted);
                }
                crate::ime_inbound::InboundEvent::AppMute { muted } => {
                    // Arbiter-decided per-app mute (task 76 P8): gate our PCM write path.
                    crate::audio_impl::set_app_output_muted(muted);
                }
                crate::ime_inbound::InboundEvent::MicMute { muted } => {
                    // Arbiter-decided mic-mute (task 76 P8): gate our capture read path.
                    crate::audio_impl::set_mic_muted(muted);
                }
                crate::ime_inbound::InboundEvent::CommRoute { speaker } => {
                    // wandr-arbiter-audio — apply the arbiter's call route decision.
                    // The call output is NOT deviceId-pinned (that -889s — task 97
                    // bug #5); set_comms_route re-routes the shared MEDIA output via
                    // a strategy device-role, which takes effect MID-CALL with no
                    // re-open. setForceUse(COMMUNICATION) is kept as the comms-mode
                    // lever (AEC/SCO). See [[project_audio_routing_arbiter]].
                    crate::audio_policy_impl::set_route(speaker);
                    crate::audio_impl::set_comms_route(speaker);
                }
                crate::ime_inbound::InboundEvent::Ringtone { start } => {
                    // wandr-arbiter-audio Ringer — play/stop the incoming-call ringtone.
                    if start { crate::ringer_impl::ringtone_start(); }
                    else { crate::ringer_impl::ringtone_stop(); }
                }
                crate::ime_inbound::InboundEvent::RingVibrate { start } => {
                    // wandr-arbiter-audio Ringer — start/stop the ring-vibrate.
                    if start { crate::ringer_impl::vibrate_start(); }
                    else { crate::ringer_impl::vibrate_stop(); }
                }
                // Task 68 — the soft keyboard occludes `px` of our surface. Add
                // it to the base bottom inset → the guest's logical height shrinks
                // → re-issue on_resize so bottom-anchored content rises above the
                // keyboard. px=0 restores the base (keyboard hidden).
                // Task 71 — explicit repaint request from the arbiter. The loop
                // already set `dirty = true` for every drained event above, so a
                // full frame renders into the now-visible surface this iteration.
                // No extra work needed beyond consuming the event.
                crate::ime_inbound::InboundEvent::Present => {}
                crate::ime_inbound::InboundEvent::KeyboardInset { px } => {
                    // `px` is the portrait-reference keyboard height; the renderer
                    // scales it per-orientation and reduces the USER-bottom of the
                    // logical area (auto-recomputed on rotation too).
                    store.data_mut().renderer.set_keyboard_base(px);
                    let (lw, lh) = {
                        let r = &store.data().renderer;
                        (r.logical_width, r.logical_height)
                    };
                    if let Err(e) = crate::input::dispatch_resize_routed(&mut store, &guest_input, lw, lh)
                    {
                        log::warn!("standalone: keyboard-inset on_resize({lw}x{lh}) failed: {e:#}");
                    }
                    log::info!(
                        "standalone: keyboard-inset base={px}px → logical {lw}x{lh}"
                    );
                }
                // Task 73 (modular WM) — the arbiter is the source of this
                // surface's window geometry. Apply what it pushed (host = dumb
                // applier; the dihedral skia matrix stays local). Subsumes the
                // legacy KeyboardInset path. Sentinels: inset 0xFFFF = keep the
                // host's env inset; orient 255 = keep local rotation.
                crate::ime_inbound::InboundEvent::Geometry {
                    inset_top, inset_bottom, keyboard_px, orient,
                } => {
                    use crate::ime_inbound::{GEOM_INSET_KEEP, GEOM_ORIENT_KEEP};
                    // Task 93 Phase 5 — the in-call video CVO follows the live
                    // device rotation; the arbiter's orient push is its source.
                    if orient != GEOM_ORIENT_KEEP {
                        crate::video::set_device_orientation_code(orient);
                    }
                    // True-dp: the inset fields ARE the arbiter-authored chrome
                    // heights (sb, tb). Cache them for `overlay_rect` (every
                    // overlay's anchoring math) on ALL surfaces…
                    if inset_top != GEOM_INSET_KEEP && inset_bottom != GEOM_INSET_KEEP {
                        cache_chrome_heights(inset_top, inset_bottom);
                    }
                    {
                        let r = &mut store.data_mut().renderer;
                        // …but apply them as CONTENT insets only on a FULLSCREEN
                        // app (it reserves the chrome strips). An overlay IS the
                        // chrome — it renders its full strip, so insetting it by
                        // (sb,tb) would shrink its content to nothing (blank status
                        // bar / taskbar). Overlays keep zero content insets.
                        if mode == OverlayMode::None
                            && inset_top != GEOM_INSET_KEEP
                            && inset_bottom != GEOM_INSET_KEEP
                        {
                            r.set_insets(inset_top, inset_bottom);
                        }
                        // Keyboard occlusion is always authoritative (0 = hidden).
                        r.set_keyboard_base(keyboard_px);
                    }
                    // Task 80 Step 2 — the fullscreen app accepts touches only in
                    // its content rect (panel minus the chrome strips), so under the
                    // ART-less InputReader path a tap on the statusbar/taskbar
                    // doesn't leak to the app behind it. Overlays self-set their
                    // strip at create; the inputflinger path ignores this.
                    if mode == OverlayMode::None
                        && inset_top != GEOM_INSET_KEEP
                        && inset_bottom != GEOM_INSET_KEEP
                    {
                        let content_h = sf.panel_h - inset_top as i32 - inset_bottom as i32;
                        sf.set_input_rect(0, inset_top as i32, sf.panel_w, content_h);
                    }
                    // Orientation: record the arbiter's decision; the orientation
                    // block above applies it next iteration (with the overlay-rect
                    // flip / set_orientation path). 255 = keep (no decision pushed
                    // yet — stay at the current orient).
                    if orient != GEOM_ORIENT_KEEP {
                        authoritative_orient = Some(orient);
                    }
                    let (lw, lh) = {
                        let r = &store.data().renderer;
                        (r.logical_width, r.logical_height)
                    };
                    if let Err(e) = crate::input::dispatch_resize_routed(&mut store, &guest_input, lw, lh)
                    {
                        log::warn!("standalone: geometry on_resize({lw}x{lh}) failed: {e:#}");
                    }
                    log::info!(
                        "standalone: geometry insets=({inset_top},{inset_bottom}) \
                         kb={keyboard_px} orient={orient} → logical {lw}x{lh}"
                    );
                }
            }
        }

        // Drain scheduler callbacks whose deadline has passed.
        let due = store.data_mut().scheduler.drain_due(std::time::Instant::now());
        if !due.is_empty() {
            dirty = true; // task 64 — a timer came due (delay()/animation tick)
        }
        for cb in due {
            if let Err(e) = crate::input::dispatch_scheduled_callback(
                shell_events.as_ref(), &mut store, cb)
            {
                log::warn!("standalone: on_scheduled_callback({cb}) failed: {e:#}");
            }
        }

        // Task 64 — gate the expensive render-frame + buffer swap. Render
        // when something changed this iteration (`dirty`), the guest's pacing
        // deadline arrived, or during the first few warm-up frames. `dirty`
        // (input / IME / lifecycle) always renders promptly — the fps cap must
        // NOT add latency to a discrete tap (a down-then-up within one frame
        // interval would otherwise defer the click to the idle deadline). The
        // cap instead floors the TIMED/animation cadence via `next_render_at`
        // below, so unconditional/animating guests stay at `target_fps`.
        // Signal bg-receipt (M2) — a backgrounded background-service keeps its
        // engine alive WITHOUT rendering the hidden surface: call `bg-tick`
        // (cheap; the guest pumps its socket/executor) on its own idle-adaptive
        // cadence, in place of the expensive render_frame + buffer swap. This is
        // also the ONLY guest entry for a wake-from-dead service relaunched
        // straight into Background (it never foregrounds, so render_frame, the
        // warm-up frames, and on_resize never run — `bg-tick` drives it). The
        // foreground / non-bg-service paths fall through to the render gate below.
        // Engine pump on the guest's bg-tick cadence — render-INDEPENDENT, runs in
        // EVERY role. Foreground render is fps-capped (frame_interval), too slow to
        // keep a live call's ~32 ms audio ring fed; bg-tick (cheap — the guest pumps
        // its socket/executor/audio, no buffer swap) runs at its guest-authored
        // cadence (≈16 ms ≈ 60/s during a call; ramps to ~1 Hz when idle). The M2
        // backgrounded-service pump is now just the Background case of this.
        if bg_service && bg_tick.is_some() && std::time::Instant::now() >= next_bg_tick_at {
            // Host SAFETY clamp only — the cadence is guest-authored. MIN floors a
            // runaway 0; the ceiling reuses IDLE_CAP_MS so a quiet service can't spin.
            const BG_TICK_MIN_MS: u64 = 16;
            const BG_TICK_DEFAULT_MS: u64 = 200;
            let delay = crate::guest_call!(bg_tick
                .as_ref()
                .unwrap()
                .wandr_background_background()
                .call_bg_tick(&mut store))
                .unwrap_or(BG_TICK_DEFAULT_MS as u32)
                .clamp(BG_TICK_MIN_MS as u32, IDLE_CAP_MS as u32) as u64;
            next_bg_tick_at =
                std::time::Instant::now() + std::time::Duration::from_millis(delay);
            bg_ticks += 1;
            if bg_ticks == 1 || bg_ticks % 50 == 0 {
                log::info!("standalone: bg-tick #{bg_ticks} next={delay}ms role={cur_role:?}");
            }
        }

        // Render the visible UI — foreground only (a Background surface is hidden, so
        // skip the expensive render_frame + buffer swap; bg-tick keeps it alive).
        let backgrounded = matches!(cur_role, crate::app_role::AppRole::Background);
        if !backgrounded && (frame < 3 || std::time::Instant::now() >= next_render_at || dirty) {
            let result = crate::input::dispatch_frame(&mut store, &guest_input, nanos);

            // Fire the pending lifecycle transition after the first successful
            // frame (gives appMain a chance to register its observer first).
            if result.is_ok() {
                if let Some(state) = store.data_mut().lifecycle.pending.take() {
                    if let Err(e) = crate::input::dispatch_lifecycle(
                        shell_events.as_ref(), &mut store, state as u32)
                    {
                        log::warn!("standalone: on_lifecycle_changed failed: {e:#}");
                    }
                }
            }

            if let Err(e) = result {
                let msg = format!("{e:?}");
                if msg.contains("cannot enter component instance") {
                    log::error!("standalone: component instance poisoned — exiting");
                    return Err(anyhow::anyhow!("render_frame fatal: {msg}"));
                }
                log::error!("standalone: render_frame #{frame} error: {e:#}");
            }

            // Ask the guest how long it may sleep before the next frame.
            // A wandr:ui-shell/frame-pacing export wins over the legacy
            // my:skiko-gfx probe; neither bound ⇒ 0 (render every interval —
            // the legacy unconditional path, rate-limited by the fps cap below).
            let guest_delay = if let Some(fp) = shell_pacing.as_ref() {
                crate::guest_call!(fp.wandr_ui_shell_frame_pacing()
                    .call_next_frame_delay(&mut store))
                    .unwrap_or(0)
                    .min(IDLE_CAP_MS as u32) as u64
            } else {
                0
            };
            // Floor the next-frame delay by the fps cap so an animating /
            // unconditional guest never renders faster than `target_fps`.
            let delay_ms = guest_delay.max(frame_interval.as_millis() as u64);
            next_render_at =
                std::time::Instant::now() + std::time::Duration::from_millis(delay_ms);

            if frame <= 3 || frame % 600 == 0 {
                log::info!("standalone: rendered frame {frame}");
            }
        }

        // Doze — apply the arbiter-pushed cadence (0 = not dozing): push the next
        // wake out to the coarse value, whichever branch above set it (render OR
        // bg-tick). The arbiter sends `doze 0` (+ the guest's screen-on dirty) to
        // resume promptly.
        if doze_cadence_ms > 0 {
            next_render_at = next_render_at
                .max(std::time::Instant::now() + std::time::Duration::from_millis(doze_cadence_ms));
        }

        frame += 1;
        // Task 46 step 4 — only the foreground app fights for focus.
        // Background apps that keep stealing focus would defeat the
        // arbiter's policy + spam the launcher. Frequency unchanged
        // (~1 s) when foreground.
        if frame % focus_refresh_interval == 0 && crate::app_role::is_foreground() {
            sf.request_focus();
        }

        // Task 64 — cheap poll cadence. Sleep until the next render is due, but
        // never longer than the input-poll cap so idle input latency stays low
        // while the expensive render is skipped. Task 72 — a BACKGROUND-role app's
        // surface is hidden and receives no input, yet `sf.poll_input()` (a native
        // libgui InputConsumer::consume) at 60 Hz was the dominant background cost
        // (~4% measured, app-agnostic). So poll input slowly when backgrounded;
        // re-foregrounding is still detected within `BG_POLL_MS` (role() is read
        // every iteration). Overlays (taskbar/statusbar) are never Background, so
        // their tap latency is unchanged.
        const BG_POLL_MS: u64 = 200;
        let now_nap = std::time::Instant::now();
        if dirty {
            last_activity = now_nap;
        }
        let poll_cap = if matches!(cur_role, crate::app_role::AppRole::Background) {
            BG_POLL_MS
        } else if now_nap.duration_since(last_activity).as_millis() as u64 >= IDLE_AFTER_MS {
            IDLE_POLL_MS // idle-adaptive: no events for a while → slow the wake rate
        } else {
            POLL_MS
        };
        let mut nap = std::time::Duration::from_millis(poll_cap);
        // The render deadline gates the nap ONLY when we actually render
        // (foreground). When backgrounded the render block is skipped, so
        // `next_render_at` is never advanced — it sits in the past and would zero
        // the nap → 100% busy-spin on every backgrounded process. (This is what
        // made the device hot.) Just poll-cap (BG_POLL_MS) when backgrounded.
        if !backgrounded {
            nap = nap.min(next_render_at.saturating_duration_since(now_nap));
        }
        // Also wake for the render-independent bg-tick pump — but ONLY for a
        // bg-service app that actually runs it. Otherwise `next_bg_tick_at` stays
        // at its init value (never advanced) → always in the past → nap 0 → spin.
        if bg_service && bg_tick.is_some() {
            nap = nap.min(next_bg_tick_at.saturating_duration_since(now_nap));
        }
        if !nap.is_zero() {
            // Task 115 — for a CM-async guest the nap doubles as the event-loop
            // pump: socket reads + keepalive timers advance while we'd
            // otherwise just sleep. Same wake cadence either way (tokio parks
            // the thread); an idle guest stays quiescent.
            #[cfg(feature = "p3-async")]
            {
                if pump_naps {
                    if let Err(e) = crate::async_app::pump_nap(&mut store, nap) {
                        log::warn!("standalone: nap pump failed: {e:#}");
                        std::thread::sleep(nap);
                    }
                } else {
                    std::thread::sleep(nap);
                }
            }
            #[cfg(not(feature = "p3-async"))]
            std::thread::sleep(nap);
        }
    }

    // ── Shutdown — fire Destroyed so the LifecycleRegistry walks
    //              Resumed → Paused → Stopped → Created → Destroyed, giving
    //              Compose observers a chance to flush state. Drain a few
    //              frames after so the resulting recompositions render
    //              before EGL/binder teardown via the SfSurface Drop chain.
    log::info!("standalone: dispatching Destroyed → drain frames → exit");
    let final_state = State::Destroyed;
    store.data_mut().lifecycle.current = final_state;
    if let Err(e) = crate::input::dispatch_lifecycle(
        shell_events.as_ref(), &mut store, final_state as u32)
    {
        log::warn!("standalone: on_lifecycle_changed(Destroyed) failed: {e:#}");
    }
    let drain_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for _ in 0..3 {
        if let Err(e) = crate::input::dispatch_frame(&mut store, &guest_input, drain_nanos)
        {
            log::warn!("standalone: drain render_frame failed: {e:#}");
            break;
        }
    }
    log::info!("standalone: clean exit");
    Ok(())
}

/// Fallback when no cwasm is deployed — draws the built-in test frame.
fn run_test_loop(mut renderer: crate::canvas_impl::SkiaRenderer) -> Result<()> {
    log::info!("standalone: test-frame loop (no cwasm)");
    let mut frame: u64 = 0;
    loop {
        if crate::lifecycle_standalone::should_shutdown() {
            log::info!("standalone: shutdown signal — exiting test loop");
            return Ok(());
        }
        renderer.draw_test_frame();
        frame += 1;
        if frame <= 3 || frame % 300 == 0 {
            log::info!("standalone: test frame {frame}");
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Walk a Kotlin Throwable's anyref to extract `.message: String?` and
/// log it. Mirrors the render_frame error path in lib.rs:392-440 —
/// kept here as a private helper so the ime-inbound dispatch can use
/// the same exception-payload format. Returns Ok(()) regardless;
/// failures to walk the struct are logged via log::error.
///
/// Kotlin/Wasm Throwable struct layout (offsets):
///   0=vtable 1=itable 2=rtti 3=_hashCode 4=message 5=cause 6=suppressed
/// Kotlin/Wasm String struct layout:
///   0=vtable 1=itable 2=rtti 3=_hashCode 4=leftIfInSum 5=length 6=_chars
/// _chars is an Array<i16> of UTF-16 code units.
fn log_kotlin_exception_msg(
    store: &mut Store<HostState>,
    exn_ref: &wasmtime::ExnRef,
) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use wasmtime::Val;
    let throwable_val = exn_ref.field(&mut *store, 0)?;
    let throwable_anyref = throwable_val.unwrap_anyref()
        .ok_or_else(|| anyhow!("exn field 0 null/not anyref"))?
        .clone();
    let throwable_struct = throwable_anyref.unwrap_struct(&mut *store)?;
    let msg_val = throwable_struct.field(&mut *store, 4)?;
    let msg_anyref = match msg_val.unwrap_anyref() {
        Some(a) => a.clone(),
        None => {
            log::error!("  exception message: <null>");
            return Ok(());
        }
    };
    let str_struct = msg_anyref.unwrap_struct(&mut *store)?;
    let len_val = str_struct.field(&mut *store, 5)?;
    let length = match len_val {
        Val::I32(i) => i as usize,
        other => return Err(anyhow!("length not i32: {:?}", other)),
    };
    let chars_val = str_struct.field(&mut *store, 6)?;
    let chars_anyref = chars_val.unwrap_anyref()
        .ok_or_else(|| anyhow!("_chars null/not anyref"))?
        .clone();
    let chars_array = chars_anyref.unwrap_array(&mut *store)?;
    let mut out = Vec::<u16>::with_capacity(length);
    for v in chars_array.elems(&mut *store)?.take(length) {
        let c = match v { Val::I32(i) => i as u16, _ => 0 };
        out.push(c);
    }
    log::error!("  exception message: {}", String::from_utf16_lossy(&out));
    Ok(())
}
