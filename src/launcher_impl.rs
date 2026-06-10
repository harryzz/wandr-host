//! Launcher / home support — `my:skiko-gfx/launcher` WIT impl (task 57).
//!
//! Two verbs:
//!   - `list-apps` scans the install dir for user apps + their labels
//!     (no PackageManager — the install-dir layout IS the app list, per
//!     the roadmap §6.3 component-graph loader). Returns newline-delimited
//!     `app-id\tlabel` (flat string keeps the Kotlin binding simple).
//!   - `launch-app` forwards to the arbiter socket (one-shot connect,
//!     mirroring `ime_host_impl::send_oneshot`); the arbiter launches +
//!     foregrounds the target, demoting this launcher to the background.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::bindings::my::skiko_gfx::launcher::Host;

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

impl Host for crate::HostState {
    fn list_apps(&mut self) -> String {
        scan_installed_apps()
    }

    fn launch_app(&mut self, app_id: String) {
        let app_id = app_id.trim();
        if app_id.is_empty() {
            log::warn!("launcher: launch-app with empty id — ignored");
            return;
        }
        forward("launch", app_id);
    }

    // ── Shell navigation (task 56 taskbar) ────────────────────────────
    fn go_home(&mut self) {
        forward("go-home", "");
    }
    fn go_back(&mut self) {
        forward("back", "");
    }
    fn recents(&mut self) {
        forward("cycle-task", "");
    }
}

/// Forward a bare/`<verb> <arg>` command to the arbiter socket
/// (fire-and-forget). Shared by launch + the nav verbs.
fn forward(verb: &str, arg: &str) {
    let line = if arg.is_empty() {
        format!("{verb}\n")
    } else {
        format!("{verb} {arg}\n")
    };
    match send_oneshot(&line) {
        Ok(()) => log::info!("launcher: forwarded `{}` to arbiter", line.trim()),
        Err(e) => log::warn!("launcher: forward `{}` failed: {e:#} (arbiter down?)", line.trim()),
    }
}

/// Build the launcher tile list. Every app under `<APPS_ROOT>/apps/*` (user apps)
/// is launchable; under `system-apps/*` only the few that opt in with
/// `show-in-launcher = true` (settings / hub apps — e.g. `wandr.settings.wifi`)
/// appear, so the pure chrome (statusbar / taskbar / ime / keyguard) stays hidden.
/// For each, read `label` from the latest version's `package.toml` (falling back
/// to the app-id). Returns `app-id\tlabel\n` lines, sorted by app-id.
fn scan_installed_apps() -> String {
    let root = crate::app_loader::apps_root();
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_apps(&root.join("apps"), false, &mut entries);
    collect_apps(&root.join("system-apps"), true, &mut entries);
    entries.sort();

    let mut out = String::new();
    for (app_id, label) in &entries {
        out.push_str(app_id);
        out.push('\t');
        out.push_str(label);
        out.push('\n');
    }
    log::info!("launcher: list-apps → {} app(s)", entries.len());
    out
}

/// Append the launchable apps under `dir` to `out` as `(app_id, label)`. When
/// `require_flag`, only include apps whose manifest sets `show-in-launcher = true`
/// (the system-apps opt-in gate).
fn collect_apps(dir: &Path, require_flag: bool, out: &mut Vec<(String, String)>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            log::debug!("launcher: read_dir {} → {e} (none)", dir.display());
            return;
        }
    };
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(app_id) = entry.file_name().into_string() else { continue };
        let app_dir = dir.join(&app_id);
        if require_flag && !read_bool_flag(&app_dir, "show-in-launcher") {
            continue;
        }
        let label = read_label(&app_dir).unwrap_or_else(|| app_id.clone());
        out.push((app_id, label));
    }
}

/// Read a top-level boolean flag from the lexically-latest version dir's
/// `package.toml` (the flat wandrpkg manifest). `false` if absent / unreadable.
fn read_bool_flag(app_dir: &Path, key: &str) -> bool {
    (|| {
        let ver = std::fs::read_dir(app_dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .max()?;
        let body = std::fs::read_to_string(app_dir.join(&ver).join("package.toml")).ok()?;
        let val: toml::Value = toml::from_str(&body).ok()?;
        val.get(key)?.as_bool()
    })()
    .unwrap_or(false)
}

/// Read `[package].label` from the lexically-latest version dir's
/// `package.toml`. None if absent (caller falls back to the app-id).
fn read_label(app_dir: &Path) -> Option<String> {
    let ver = std::fs::read_dir(app_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .max()?;
    let body = std::fs::read_to_string(app_dir.join(&ver).join("package.toml")).ok()?;
    let val: toml::Value = toml::from_str(&body).ok()?;
    // The wandrpkg manifest is flat (top-level `app_id`/`version`/… keys),
    // so `label` is a top-level key — not under a `[package]` table.
    val.get("label")?.as_str().map(|s| s.to_string())
}

/// Connect, write one line, drain + drop the reply, close. Matches the
/// one-shot pattern in `ime_host_impl`.
fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    Ok(())
}
