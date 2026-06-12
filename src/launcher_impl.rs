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

use crate::chrome_bindings::wandr::chrome::launcher::Host;

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

/// Build the launcher tile list. User apps under `<APPS_ROOT>/apps/*` are launchable
/// unless they opt out with `show-in-launcher = false`; under `system-apps/*` only the
/// few that opt in with `show-in-launcher = true` (settings / hub apps — e.g.
/// `wandr.settings.wifi`) appear, so the pure chrome (statusbar / taskbar / ime /
/// keyguard) stays hidden. In BOTH roots a `wasi:cli/command` (console) guest is never
/// listed — it has no renderer, so it can't draw a tile. For each, read `label` from
/// the latest version's `package.toml` (falling back to the app-id). Returns
/// `app-id\tlabel\n` lines, sorted by app-id.
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

        // A `wasi:cli/command` guest is a console / diagnostic tool — it exports no
        // renderer, so it can't draw a launcher tile and tapping it does nothing.
        // Never list it, whether installed under apps/ or system-apps/ (derived from
        // the declared world, so no per-app flag to remember). A GUI app can still
        // opt out explicitly with `show-in-launcher = false`.
        if read_pkg_str(&app_dir, "world").as_deref() == Some("wasi:cli/command") {
            continue;
        }
        let opt = read_pkg_bool(&app_dir, "show-in-launcher");
        let listed = if require_flag {
            opt == Some(true) // system-apps: hidden unless they opt in
        } else {
            opt != Some(false) // user apps: shown unless they opt out
        };
        if !listed {
            continue;
        }

        let label = read_label(&app_dir).unwrap_or_else(|| app_id.clone());
        out.push((app_id, label));
    }
}

/// Parse the lexically-latest version dir's flat `package.toml` manifest. None if
/// the app dir has no version / no readable manifest.
fn latest_manifest(app_dir: &Path) -> Option<toml::Value> {
    let ver = std::fs::read_dir(app_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .max()?;
    let body = std::fs::read_to_string(app_dir.join(&ver).join("package.toml")).ok()?;
    toml::from_str(&body).ok()
}

/// A top-level boolean key from the manifest — `None` if absent / not a bool (so
/// callers can distinguish "unset" from an explicit `true`/`false`).
fn read_pkg_bool(app_dir: &Path, key: &str) -> Option<bool> {
    latest_manifest(app_dir)?.get(key)?.as_bool()
}

/// A top-level string key from the manifest (e.g. `world`, `label`).
fn read_pkg_str(app_dir: &Path, key: &str) -> Option<String> {
    latest_manifest(app_dir)?.get(key)?.as_str().map(|s| s.to_string())
}

/// Read the flat manifest's top-level `label`. None if absent (caller falls back to
/// the app-id).
fn read_label(app_dir: &Path) -> Option<String> {
    read_pkg_str(app_dir, "label")
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
