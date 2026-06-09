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

/// Scan `<APPS_ROOT>/apps/*` (user apps only — system-apps are
/// library/headless components, not launchable). For each app-id dir,
/// read `[package].label` from the latest version's `package.toml`,
/// falling back to the app-id. Returns `app-id\tlabel\n` lines, sorted.
fn scan_installed_apps() -> String {
    let dir = crate::app_loader::apps_root().join("apps");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("launcher: read_dir {} failed: {e}", dir.display());
            return String::new();
        }
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    ids.sort();

    let mut out = String::new();
    for app_id in &ids {
        let label = read_label(&dir.join(app_id)).unwrap_or_else(|| app_id.clone());
        out.push_str(app_id);
        out.push('\t');
        out.push_str(&label);
        out.push('\n');
    }
    log::info!("launcher: list-apps → {} app(s)", ids.len());
    out
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
