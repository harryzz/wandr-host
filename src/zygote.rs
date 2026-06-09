//! wandr-zygote — native fork+COW launcher for wandr apps (task 45 spike).
//!
//! See `tasks/45-wandr-zygote-spike.md` for the architectural framing.
//! In one line: this is `app_process`-shaped but ART-free — preload
//! `wasmtime::Engine` once, `fork()` per app, child inherits the engine
//! via copy-on-write.
//!
//! Step 1 (this file's current scope): plain UNIX-socket protocol,
//! text wire format (`LAUNCH <app-id>\n` → `OK <pid>\n` / `ERR <reason>\n`).
//! The forked child dispatches to `run_once::run_with_engine(&engine, app_id)`
//! which is `wasi:cli/command`-shaped (one call to `wasi:cli/run.run`,
//! exit). No EGL/SF preload in the parent (D5), no binder preload (D7) —
//! the child first-inits both via the existing `run_once` plumbing.
//!
//! Step 2+ will extend the child path with the full Compose render loop
//! (refactored out of `standalone.rs`) and add multi-app concurrency
//! verification.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use wasmtime::Engine;

use crate::{run_once, standalone, App};

/// Track every pid we've fork()d. Populated by the parent's `handle_one`
/// right after fork() returns the child pid. Drained by the reaper
/// thread when the kernel reports the child exited.
///
/// Used by the `KILL <pid>` / `KILL_FORCE <pid>` socket commands to
/// validate that the caller is asking us to signal ONE OF OUR OWN
/// children — not some arbitrary system process whose pid happened to
/// land in the request. After waitpid reaps a pid the kernel is free
/// to recycle it for another (potentially unrelated) process, which is
/// why removing dead pids from the set promptly matters.
fn child_pids() -> &'static Mutex<HashSet<i32>> {
    static SET: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Long-lived `SUBSCRIBE_EXITS` connections (task 54 part A). The
/// arbiter opens one of these on daemon startup; the reaper thread
/// broadcasts `EXITED <pid> <detail>\n` down every entry whenever a
/// forked child is reaped. We hold the `UnixStream`s here (moved out of
/// `handle_one`) so they aren't closed when the command handler returns.
///
/// Why the zygote is the only process that can do this: it's the
/// `fork()` parent, so it's the only one the kernel delivers SIGCHLD
/// to. The arbiter is a sibling and never sees the deaths directly —
/// hence this push channel.
fn exit_subscribers() -> &'static Mutex<Vec<UnixStream>> {
    static SUBS: OnceLock<Mutex<Vec<UnixStream>>> = OnceLock::new();
    SUBS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Broadcast a single `EXITED <pid> <detail>\n` line to every active
/// subscriber, dropping any whose write fails (disconnected arbiter).
/// Called by the reaper thread right after it reaps a child.
fn broadcast_exit(pid: i32, detail: &str) {
    let line = format!("EXITED {pid} {detail}\n");
    if let Ok(mut subs) = exit_subscribers().lock() {
        let before = subs.len();
        subs.retain_mut(|s| s.write_all(line.as_bytes()).and_then(|_| s.flush()).is_ok());
        let after = subs.len();
        if before != after {
            log::info!(
                "wandr-zygote: dropped {} disconnected exit-subscriber(s) (now {after})",
                before - after,
            );
        }
        if after > 0 {
            log::info!("wandr-zygote: broadcast EXITED {pid} {detail} to {after} subscriber(s)");
        }
    }
}

/// Spawn the SIGCHLD reaper thread.
///
/// Why a thread (and not a SIGCHLD handler + self-pipe + poll
/// multiplex on the accept loop): the thread's `libc::wait()` is the
/// simplest portable primitive that wakes synchronously on any child
/// exit, decodes the status, and removes the pid from `child_pids()`
/// — no async-signal-safety constraints, no FD juggling. The cost
/// (D6 from task 45's scope) is one extra thread in the parent; this
/// thread exists only in the parent (fork() only duplicates the
/// calling thread). Children inherit a process with no reaper, which
/// is what they want.
///
/// The reaper holds the child_pids mutex only for the brief instant
/// of `.remove(&pid)`, so even if fork() races with a reap the child
/// won't inherit a long-held lock. The child never accesses
/// `child_pids()` anyway — that's a parent-only concern.
fn spawn_reaper() {
    std::thread::Builder::new()
        .name("wandr-zygote-reaper".into())
        .spawn(|| {
            loop {
                let mut status: i32 = 0;
                let pid = unsafe { libc::wait(&mut status) };
                if pid < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ECHILD) {
                        // No children to wait on. Sleep briefly to
                        // avoid busy-looping; the next fork() will
                        // restart the cycle.
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    } else {
                        log::warn!("wandr-zygote/reaper: wait failed: {err}");
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    continue;
                }
                let exit_summary = if libc::WIFEXITED(status) {
                    format!("exit={}", libc::WEXITSTATUS(status))
                } else if libc::WIFSIGNALED(status) {
                    format!("signal={}", libc::WTERMSIG(status))
                } else {
                    format!("status=0x{status:x}")
                };
                let removed = child_pids().lock().map(|mut s| s.remove(&pid)).unwrap_or(false);
                log::info!(
                    "wandr-zygote/reaper: pid {pid} reaped ({exit_summary}, tracked={removed})"
                );
                // Task 54 part A — push the death to any subscribed
                // arbiter so it can drop the app from its registry +
                // clean up the orphaned per-host control socket. We
                // broadcast for every reaped pid (tracked or not); the
                // arbiter ignores pids it doesn't know.
                broadcast_exit(pid, &exit_summary);
            }
        })
        .expect("spawn reaper thread");
}

/// Remove `/data/local/tmp/wandr-host-<pid>.sock` files whose `<pid>` is
/// no longer a live process. Returns the count removed. Task 54 part B.
///
/// A live wandr-host child still owns its socket — we leave those alone
/// (their `<pid>` passes the `kill(pid, 0)` liveness probe). Anything
/// else is a leftover from a child that died without unlinking (SIGKILL
/// skips Drop). Best-effort: parse failures + IO errors are logged at
/// debug and skipped, never fatal.
fn sweep_stale_host_sockets() -> usize {
    let dir = "/data/local/tmp";
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            log::debug!("wandr-zygote: sweep — read_dir {dir} failed: {e}");
            return 0;
        }
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Match `wandr-host-<digits>.sock`.
        let Some(pid_str) = name
            .strip_prefix("wandr-host-")
            .and_then(|s| s.strip_suffix(".sock"))
        else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else { continue };
        if pid_alive(pid) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                log::info!("wandr-zygote: sweep — removed stale {}", path.display());
                removed += 1;
            }
            Err(e) => log::debug!("wandr-zygote: sweep — remove {} failed: {e}", path.display()),
        }
    }
    removed
}

/// `kill(pid, 0)` liveness probe — 0 → alive; ESRCH → dead; EPERM →
/// alive but unsignalable (still alive). Mirrors the arbiter's
/// `state::pid_alive`.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What the forked child should do after fork().
#[derive(Copy, Clone, Debug)]
enum ChildAction {
    /// CLI-shaped `wasi:cli/command` consumer (e.g. `md-smoke-rust`).
    /// Goes through `run_once::run_with_engine`. One-shot, no SF surface
    /// would be needed in principle, but `run_once` allocates one today
    /// because `HostState.renderer` is non-Option (refactor deferred).
    RunOnce,
    /// Full Compose render loop (e.g. `wandr-app`). Goes through
    /// `standalone::run_with_engine`. Owns its own SurfaceFlinger
    /// surface + EGL context + input channel for the duration.
    /// `mode` selects fullscreen / bottom-overlay (IME, task 47) /
    /// top-overlay (status bar, task 55).
    Gui { mode: crate::standalone::OverlayMode },
}

/// Where the zygote listens for `LAUNCH` requests.
///
/// `/data/local/tmp/` is the dev path — SELinux-permissive for `su`,
/// SAR-stable. Production would move to `/dev/socket/wandr-zygote` via
/// init.rc + a `wandr_zygote` SELinux domain (task 46+).
pub const ZYGOTE_SOCK_PATH: &str = "/data/local/tmp/wandr-zygote.sock";

/// One-shot preloaded engine. Held in a `OnceLock` so the listen loop
/// can hand a `&Engine` to each forked child without re-allocating.
///
/// On `fork()` the child inherits the OnceLock's slot via COW; the
/// inner `Engine` (with its Cranelift caches, type registry, etc.) is
/// COW-shared with the parent. As long as nothing in the child path
/// mutates the engine in place, all pages stay read-only and shared.
static PRELOADED_ENGINE: OnceLock<Engine> = OnceLock::new();

/// Parent-side entry: bind the socket, accept LAUNCH commands, fork
/// per request. Never returns under normal operation.
///
/// `preload_app_id` is documentary at MVP — we preload only the
/// `wasmtime::Engine` (which is app-agnostic). Per-app `Component`
/// preload comes in a follow-up once we add a real preload-registry.
/// The arg is accepted now so the CLI shape ages well.
pub fn serve(preload_app_id: Option<&str>) -> Result<()> {
    // Logging up first — the zygote parent runs unattended; logcat is the
    // only easy observation channel.
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );
    log::info!(
        "wandr-zygote: starting — sock={} preload_hint={:?}",
        ZYGOTE_SOCK_PATH,
        preload_app_id,
    );

    // Preload the engine. This is the whole point of the zygote — the
    // pages this allocates (wasmtime Engine state, Cranelift tables, etc.)
    // get COW-shared into every forked child.
    let engine = App::make_engine();
    PRELOADED_ENGINE
        .set(engine)
        .map_err(|_| anyhow!("PRELOADED_ENGINE set twice"))?;
    log::info!("wandr-zygote: engine preloaded");

    // Reaper before listen — by the time we accept the first client and
    // possibly fork(), the reaper must be running so the resulting
    // SIGCHLD doesn't pile up. Failing to spawn would mean zombies,
    // not a functional break — but the panic-on-failure is fine here
    // because if the kernel can't give us a thread we have bigger
    // problems.
    spawn_reaper();
    log::info!("wandr-zygote: reaper thread spawned");

    // Task 46 step 4 — install SIGUSR1/SIGUSR2 handlers in the parent
    // so forked children inherit the action via the sigaction table.
    // Without this, the arbiter's promote-to-foreground (SIGUSR2)
    // landing in the race window between fork() and the child's own
    // install_signal_handlers() would hit the kernel default action
    // (terminate) and kill the child. The handler in the parent is
    // harmless since the parent never goes background/foreground —
    // it's just there to give children a non-fatal default.
    crate::app_role::install_signal_handlers();
    log::info!("wandr-zygote: app-role signal handlers installed");

    // Task 46 step 2 — auto-preload every installed system bundle.
    // System apps (markdown, emoji, fonts, …) are imported by every
    // Compose app; preloading them in the parent makes the
    // deserialized `Component` values COW-shareable with every
    // forked child. User apps under `apps/*` are NOT auto-preloaded
    // here — they wait for an explicit `PRELOAD <app-id>` socket
    // command from the arbiter / installer / tests (see
    // `handle_one`).
    let apps_root = crate::app_loader::apps_root();
    let preloaded = crate::preload::preload_all_system_apps(
        PRELOADED_ENGINE.get().expect("engine just set"),
        &apps_root,
    );
    log::info!(
        "wandr-zygote: startup preload — {} system component(s) under {}",
        preloaded,
        apps_root.display(),
    );

    // Task 54 part B — sweep accumulated stale per-host control sockets
    // from prior sessions. Each `wandr-host` child binds
    // `/data/local/tmp/wandr-host-<pid>.sock`; SIGKILL (LMK, OOM) leaves
    // the path behind with no live owner. Do this once at startup so the
    // dir doesn't accrue dozens of dead `.sock` files across reboots.
    let swept = sweep_stale_host_sockets();
    log::info!("wandr-zygote: startup sweep removed {swept} stale wandr-host-*.sock file(s)");

    // Bind the listen socket. Unlink any stale path first — the AF_UNIX
    // bind would otherwise fail with EADDRINUSE on a respawn.
    let sock_path = Path::new(ZYGOTE_SOCK_PATH);
    if sock_path.exists() {
        std::fs::remove_file(sock_path)
            .with_context(|| format!("removing stale socket {ZYGOTE_SOCK_PATH}"))?;
    }
    let listener = UnixListener::bind(ZYGOTE_SOCK_PATH)
        .with_context(|| format!("UnixListener::bind {ZYGOTE_SOCK_PATH}"))?;
    // World-writeable so non-root clients on the dev path can talk to us.
    // (Production wandr-arbiter is root and the path is sepolicy-gated.)
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(
        ZYGOTE_SOCK_PATH,
        std::fs::Permissions::from_mode(0o666),
    );
    log::info!("wandr-zygote: listening on {ZYGOTE_SOCK_PATH}");

    loop {
        let (stream, _addr) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("wandr-zygote: accept failed: {e}");
                continue;
            }
        };
        if let Err(e) = handle_one(&listener, stream) {
            log::warn!("wandr-zygote: client error: {e:#}");
        }
    }
}

/// Handle a single client connection: parse one command, fork if it's
/// a LAUNCH, else respond with an error and return.
fn handle_one(listener: &UnixListener, mut stream: UnixStream) -> Result<()> {
    // Read a single line. Tiny buffer; one-shot text protocol.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(anyhow!("client closed without sending a command"));
    }
    let cmd = line.trim_end_matches('\n').trim_end_matches('\r');
    log::info!("wandr-zygote: cmd={cmd:?}");

    // KILL / KILL_FORCE — task 46 step 1. Validate the pid is ONE OF OUR
    // OWN children (in child_pids()) before signaling. Without that
    // check a malicious or buggy caller could send SIGKILL to any pid
    // on the device — wandr-host runs as root in the dev path.
    //
    // KILL_FORCE prefix is checked first because strip_prefix("KILL ")
    // would otherwise match it (the space-vs-underscore distinction
    // protects us, but keeping the order explicit is clearer).
    if let Some(rest) = cmd.strip_prefix("KILL_FORCE ") {
        return handle_kill(&mut stream, rest, libc::SIGKILL, "KILL_FORCE");
    }
    if let Some(rest) = cmd.strip_prefix("KILL ") {
        return handle_kill(&mut stream, rest, libc::SIGTERM, "KILL");
    }

    // SUBSCRIBE_EXITS — task 54 part A. The caller (arbiter) wants a
    // long-lived connection on which we push `EXITED <pid> <detail>`
    // lines from the reaper. Ack, then MOVE the stream into the
    // subscribers list so it stays open after this handler returns
    // (a plain return would drop+close it). The accept loop continues
    // immediately — we do not block on the subscriber.
    if cmd == "SUBSCRIBE_EXITS" {
        if let Err(e) = writeln!(stream, "OK subscribed") {
            return Err(anyhow!("ack SUBSCRIBE_EXITS: {e}"));
        }
        let _ = stream.flush();
        // Drop our reader clone (the subscriber connection only carries
        // server→client pushes; we never read from it again).
        drop(reader);
        match exit_subscribers().lock() {
            Ok(mut subs) => {
                subs.push(stream);
                log::info!(
                    "wandr-zygote: new exit-subscriber ({} total)",
                    subs.len(),
                );
            }
            Err(_) => return Err(anyhow!("exit_subscribers mutex poisoned")),
        }
        return Ok(());
    }

    // PRELOAD — task 46 step 2. Pre-deserialize a user-app's `.cwasm`
    // (or refresh a system-app's preload after an upgrade) so future
    // forks COW-inherit the Component. Caller is the installer (after
    // writing the new version) or the future arbiter (predictive
    // warm-up before a launch).
    if let Some(rest) = cmd.strip_prefix("PRELOAD ") {
        return handle_preload(&mut stream, rest);
    }

    // Two launch shapes — the client picks. MVP keeps this explicit
    // rather than auto-detecting from package.toml; auto-detect is
    // polish for a later step (read manifest, dispatch by `world`).
    //
    // GUI mode accepts an empty arg → falls back to the dev cwasm at
    // /data/local/tmp/skiko-component.cwasm (same behavior as direct
    // `--standalone` with no `--app`). Useful for the step-2 smoke
    // before wandr-app is properly packaged + installed as a .wandrpkg.
    // Task 47 step 3c — `LAUNCH_GUI_OVERLAY` requests a bottom-strip
    // overlay SurfaceControl (used by IME apps such as
    // `wandr.ime.keyboard`); plain `LAUNCH_GUI` keeps the fullscreen
    // behavior. Order matters — match the longer prefix first.
    use crate::standalone::OverlayMode;
    let (action, app_id) = if let Some(rest) = cmd.strip_prefix("LAUNCH_GUI_OVERLAY_TOP") {
        (ChildAction::Gui { mode: OverlayMode::Top }, rest.trim().to_string())
    } else if let Some(rest) = cmd.strip_prefix("LAUNCH_GUI_OVERLAY") {
        (ChildAction::Gui { mode: OverlayMode::Bottom }, rest.trim().to_string())
    } else if let Some(rest) = cmd.strip_prefix("LAUNCH_GUI") {
        (ChildAction::Gui { mode: OverlayMode::None }, rest.trim().to_string())
    } else if let Some(rest) = cmd.strip_prefix("LAUNCH ") {
        (ChildAction::RunOnce, rest.trim().to_string())
    } else {
        let _ = writeln!(stream, "ERR unknown-command {cmd}");
        return Err(anyhow!("unknown command: {cmd}"));
    };
    // RunOnce always requires an app_id (no dev path defined for
    // wasi:cli/command consumers). Gui can be empty → dev cwasm.
    if app_id.is_empty() && !matches!(action, ChildAction::Gui { .. }) {
        let _ = writeln!(stream, "ERR empty-app-id");
        return Err(anyhow!("empty app-id"));
    }

    // fork(). Returns 0 to child, child-pid to parent, -1 on error.
    //
    // Safety: we do nothing async-signal-unsafe between fork and the
    // child's first action; the child immediately drops fds we don't
    // want it to hold, then enters Rust code that owns its own state.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => {
            let err = std::io::Error::last_os_error();
            let _ = writeln!(stream, "ERR fork {err}");
            Err(anyhow!("fork: {err}"))
        }
        0 => {
            // CHILD path.
            //
            // (1) Close the FDs we inherited from the parent that we
            //     don't want to keep: the listen socket (parent uses
            //     it), and our reader/stream copies (used to ack the
            //     client back through; only the parent should respond).
            //     We do that by dropping the Rust handles — Drop calls
            //     close(2).
            drop(reader);
            drop(stream);
            // The listener is moved by reference into this function; we
            // can't drop it here. Close its FD directly. (Safe because
            // after this the child never touches the listener.)
            let listen_fd = std::os::unix::io::AsRawFd::as_raw_fd(listener);
            unsafe { libc::close(listen_fd) };

            // (2) Optional hold-for-measurement. Env-gated; off by
            //     default. Used by the step-1 COW analysis to freeze
            //     the child right after fork (preloaded engine page-
            //     state still intact, no run_once side effects) so
            //     /proc/<pid>/smaps_rollup can be sampled.
            if let Ok(s) = std::env::var("WANDR_ZYGOTE_HOLD_SECS") {
                if let Ok(secs) = s.parse::<u64>() {
                    log::info!("wandr-zygote/child: holding {secs}s before run (WANDR_ZYGOTE_HOLD_SECS)");
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                }
            }

            // (3) Dispatch based on action. RunOnce → wasi:cli/command;
            //     Gui → full Compose render loop (acquires SF surface,
            //     initializes EGL/Skia, drives render_frame).
            //     In either case the COW-inherited engine is used; the
            //     child first-inits binder + EGL via the existing
            //     run_once / standalone plumbing (D7 — parent must
            //     never touch either).
            let engine = PRELOADED_ENGINE
                .get()
                .expect("PRELOADED_ENGINE not set in child");
            let exit = match action {
                ChildAction::RunOnce => match run_once::run_with_engine(engine, &app_id) {
                    Ok(()) => 0,
                    Err(e) => {
                        log::error!("wandr-zygote/child: run_once failed: {e:#}");
                        1
                    }
                },
                ChildAction::Gui { mode } => {
                    let arg = if app_id.is_empty() { None } else { Some(app_id.as_str()) };
                    match standalone::run_with_engine(engine, arg, mode) {
                        Ok(()) => 0,
                        Err(e) => {
                            log::error!("wandr-zygote/child: standalone failed: {e:#}");
                            1
                        }
                    }
                },
            };
            // (4) Exit immediately. Do not let Rust's normal exit path
            //     run global destructors — those are COW pages we
            //     shouldn't dirty on the way out.
            unsafe { libc::_exit(exit) };
        }
        child_pid => {
            // PARENT path. Track the pid for KILL command validation,
            // ack the client, return to the accept loop.
            //
            // The reaper thread (spawn_reaper) wakes on the eventual
            // SIGCHLD and removes the pid from this set. KILL command
            // checks the set before signaling.
            if let Ok(mut set) = child_pids().lock() {
                set.insert(child_pid);
            }
            writeln!(stream, "OK {child_pid}")
                .with_context(|| format!("ack {child_pid} to client"))?;
            log::info!("wandr-zygote: forked pid={child_pid} for app_id={app_id}");
            Ok(())
        }
    }
}

/// Shared handler for `KILL` / `KILL_FORCE` socket commands.
///
/// Parses the pid out of the rest of the line, validates it's in
/// `child_pids()`, and sends the requested signal. The pid stays in
/// the set until the reaper thread sees its SIGCHLD — so a second
/// KILL of the same pid will succeed at the socket layer even if
/// the first SIGTERM has already done the job; the underlying
/// `kill(2)` returns ESRCH and we surface that as `ERR kill-failed`.
fn handle_kill(
    stream: &mut UnixStream,
    rest: &str,
    sig: libc::c_int,
    verb: &'static str,
) -> Result<()> {
    let pid: i32 = match rest.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            let _ = writeln!(stream, "ERR {verb}-bad-pid");
            return Err(anyhow!("{verb}: bad pid '{}'", rest.trim()));
        }
    };
    let owned = child_pids()
        .lock()
        .map(|s| s.contains(&pid))
        .unwrap_or(false);
    if !owned {
        let _ = writeln!(stream, "ERR not-our-child {pid}");
        log::warn!("wandr-zygote: {verb} {pid} rejected — not one of our children");
        return Ok(());
    }
    let r = unsafe { libc::kill(pid, sig) };
    if r == 0 {
        writeln!(stream, "OK {pid}")
            .with_context(|| format!("{verb} ack {pid} to client"))?;
        log::info!("wandr-zygote: {verb} {pid} sent (sig={sig})");
    } else {
        let err = std::io::Error::last_os_error();
        let _ = writeln!(stream, "ERR kill-failed {pid} {err}");
        log::warn!("wandr-zygote: {verb} {pid} failed: {err}");
    }
    Ok(())
}

/// Shared handler for `PRELOAD <app-id>` socket command.
///
/// Walks the install dir for the given app-id (tries `apps/` first,
/// then `system-apps/`), `Component::deserialize_file`s every `.cwasm`
/// under `<latest-version>/cache/`, and inserts them into the preload
/// registry. Any prior preloads for the same app are dropped first
/// (handles in-place upgrades).
///
/// Replies `OK <kind> <count>` on success, `ERR <reason>` on failure.
fn handle_preload(stream: &mut UnixStream, rest: &str) -> Result<()> {
    let app_id = rest.trim();
    if app_id.is_empty() {
        let _ = writeln!(stream, "ERR preload-empty-app-id");
        return Err(anyhow!("PRELOAD: empty app-id"));
    }
    let apps_root = crate::app_loader::apps_root();
    let engine = PRELOADED_ENGINE.get().expect("PRELOADED_ENGINE not set in PRELOAD handler");
    match crate::preload::preload_either(engine, &apps_root, app_id) {
        Ok((kind, n)) => {
            writeln!(stream, "OK {kind} {n}")
                .with_context(|| format!("PRELOAD ack {app_id}"))?;
            log::info!("wandr-zygote: PRELOAD {app_id} → {kind} ({n} component(s))");
            Ok(())
        }
        Err(e) => {
            let _ = writeln!(stream, "ERR preload-failed {e:#}");
            log::warn!("wandr-zygote: PRELOAD {app_id} failed: {e:#}");
            Ok(())
        }
    }
}

/// Client-side: connect to the zygote, write the launch command, read
/// the response, return the child pid on success.
///
/// `gui` selects between `LAUNCH_GUI <app-id>` (full Compose render
/// loop, owns its own SF surface) and `LAUNCH <app-id>` (one-shot
/// `wasi:cli/command` consumer). `overlay` (task 47 step 3c, only
/// meaningful when `gui=true`) upgrades the verb to
/// `LAUNCH_GUI_OVERLAY` so the child acquires a bottom-strip overlay
/// SurfaceControl instead of a fullscreen one.
pub fn launch_client(app_id: &str, gui: bool, overlay: bool) -> Result<i32> {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );

    let mut stream = UnixStream::connect(ZYGOTE_SOCK_PATH)
        .with_context(|| format!("connect {ZYGOTE_SOCK_PATH} — is the zygote running?"))?;
    let verb = if gui {
        if overlay { "LAUNCH_GUI_OVERLAY" } else { "LAUNCH_GUI" }
    } else {
        "LAUNCH"
    };
    if app_id.is_empty() && gui {
        // Dev mode: empty arg → child uses /data/local/tmp/skiko-component.cwasm.
        writeln!(stream, "{verb}")?;
    } else {
        writeln!(stream, "{verb} {app_id}")?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    let response = response.trim_end();
    log::info!("wandr-zygote/client: response={response:?}");

    if let Some(rest) = response.strip_prefix("OK ") {
        let pid: i32 = rest
            .trim()
            .parse()
            .with_context(|| format!("parse pid from {response:?}"))?;
        println!("launched {app_id} → pid {pid}");
        Ok(pid)
    } else if let Some(rest) = response.strip_prefix("ERR ") {
        Err(anyhow!("zygote rejected: {rest}"))
    } else {
        Err(anyhow!("zygote returned malformed response: {response:?}"))
    }
}

/// Client-side: connect to the zygote, send `PRELOAD <app-id>`,
/// print the response.
pub fn preload_client(app_id: &str) -> Result<()> {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );
    let mut stream = UnixStream::connect(ZYGOTE_SOCK_PATH)
        .with_context(|| format!("connect {ZYGOTE_SOCK_PATH} — is the zygote running?"))?;
    writeln!(stream, "PRELOAD {app_id}")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    let response = response.trim_end();
    log::info!("wandr-zygote/client: response={response:?}");
    if response.starts_with("OK ") {
        println!("PRELOAD {app_id} → {response}");
        Ok(())
    } else {
        Err(anyhow!("zygote rejected: {response}"))
    }
}

/// Client-side: connect to the zygote, send `KILL <pid>` (or
/// `KILL_FORCE` if `force`), print the response.
pub fn kill_client(pid: i32, force: bool) -> Result<()> {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Debug),
    );
    let mut stream = UnixStream::connect(ZYGOTE_SOCK_PATH)
        .with_context(|| format!("connect {ZYGOTE_SOCK_PATH} — is the zygote running?"))?;
    let verb = if force { "KILL_FORCE" } else { "KILL" };
    writeln!(stream, "{verb} {pid}")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    let response = response.trim_end();
    log::info!("wandr-zygote/client: response={response:?}");
    if response.starts_with("OK ") {
        println!("{verb} {pid} → {}", response);
        Ok(())
    } else {
        Err(anyhow!("zygote rejected: {response}"))
    }
}
