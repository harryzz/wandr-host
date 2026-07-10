//! `wandr:task-manager/task-manager` host impl (task 92).
//!
//! A task-manager guest calls `list-apps` / `system-mem` / `kill-app`; the host
//! is the authority's proxy: it forwards the running-app set + kill to the arbiter
//! over the control socket (`task-list` / `task-kill`, see
//! `wandr-arbiter-shell::am`) and enriches each entry with a `/proc/<pid>` sample
//! (CPU jiffies, RSS/PSS, thread count) + the install-class `kind` and `label`
//! (which the host already knows from the on-disk layout — `apps/` vs
//! `system-apps/` under `app_loader::apps_root()`). The arbiter stays the single
//! authority for the set + pid + role + uptime; nothing new is exposed to the
//! guest beyond what `wandr-arbiter task-list` + a proc read already know.
//!
//! Polling model (decided 2026-06-06): the guest re-calls `list-apps` on a timer.
//! `cpu-permille` is delta-based, so it reads 0 on the first poll for a freshly
//! seen pid and becomes meaningful from the second. The per-pid CPU baseline lives
//! in this host process (`CPU_SAMPLES`) — correctly scoped, since each guest runs
//! in its own zygote-forked host.

use std::collections::HashMap;
use std::io::{Read, Write};
use crate::arbiter_sock::UnixStream;
use std::sync::Mutex;
use std::time::Instant;

use crate::task_manager_host_bindings::wandr::task_manager::task_manager::Host;
use crate::task_manager_host_bindings::wandr::task_manager::types::{
    AppInfo, AppKind, AppState, KillError, ResourceUsage, SystemMemory,
};

/// Per-pid CPU baseline for the delta that yields `cpu-permille`:
/// pid → (cumulative (utime+stime) clock ticks, the instant it was sampled).
static CPU_SAMPLES: Mutex<Option<HashMap<u32, (u64, Instant)>>> = Mutex::new(None);

/// `_SC_CLK_TCK` — kernel clock ticks per second (usually 100); `/proc/<pid>/stat`
/// utime/stime are in these units. Read once.
fn clk_tck() -> u64 {
    // SAFETY: sysconf with a static name is always safe; clamp the (always
    // positive on Linux) result and fall back to the universal 100.
    #[cfg(unix)]
    { let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }; if v > 0 { v as u64 } else { 100 } }
    // Windows has no /proc and no sysconf; this task-manager path is unix-only,
    // so the universal CLK_TCK fallback is never actually consumed there.
    #[cfg(not(unix))]
    { 100 }
}

/// Connect to the arbiter, write one line, read the WHOLE reply to EOF (the
/// `list` snapshot is multi-line and far exceeds a fixed buffer). Returns the
/// reply body, or an error if the arbiter is unreachable.
fn query_full(line: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Derive (kind, label) for an app-id from the on-disk install layout. The host
/// owns this: an app under `apps/` is `user`, under `system-apps/` is `system`;
/// `label` is the flat top-level `label` key in the latest version's
/// `package.toml`, falling back to the app-id.
fn kind_and_label(app_id: &str) -> (AppKind, String) {
    let root = crate::app_loader::apps_root();
    let user_dir = root.join("apps").join(app_id);
    let sys_dir = root.join("system-apps").join(app_id);
    let (kind, app_dir) = if user_dir.is_dir() {
        (AppKind::User, user_dir)
    } else if sys_dir.is_dir() {
        (AppKind::System, sys_dir)
    } else {
        // Not found on disk (e.g. a dev `--cwasm` launch): treat as user, no label.
        (AppKind::User, user_dir)
    };
    let label = read_label(&app_dir).unwrap_or_else(|| app_id.to_string());
    (kind, label)
}

/// Read the flat top-level `label` from the lexically-latest version dir's
/// `package.toml` (same shape `launcher_impl` reads).
fn read_label(app_dir: &std::path::Path) -> Option<String> {
    let ver = std::fs::read_dir(app_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .max()?;
    let body = std::fs::read_to_string(app_dir.join(&ver).join("package.toml")).ok()?;
    let val: toml::Value = toml::from_str(&body).ok()?;
    val.get("label")?.as_str().map(|s| s.to_string())
}

/// Map the arbiter's `state=` token to the guest-facing `app-state` enum.
fn parse_state(tok: &str) -> AppState {
    match tok {
        "foreground" => AppState::Foreground,
        "overlay" => AppState::Overlay,
        "headless" => AppState::Headless,
        _ => AppState::Background,
    }
}

/// `(utime+stime)` clock ticks + live thread count for a pid, from
/// `/proc/<pid>/stat`. Returns `(cpu_ticks, threads)`; `None` if the process is
/// gone or unreadable.
fn read_stat(pid: u32) -> Option<(u64, u32)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesised and may contain spaces/parens — split on
    // the LAST ')' so the numeric fields after it tokenise cleanly. After the
    // ')', token[0] is `state` (field 3); utime is field 14, stime field 15 →
    // indices 11 and 12 in this post-')' split.
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((utime + stime, 0)) // threads come from /proc/<pid>/status (more robust)
}

/// Pull `VmRSS` (KiB) + `Threads` from `/proc/<pid>/status`.
fn read_status(pid: u32) -> (u64, u32) {
    let mut rss = 0;
    let mut threads = 0;
    if let Ok(body) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for ln in body.lines() {
            if let Some(v) = ln.strip_prefix("VmRSS:") {
                rss = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
            } else if let Some(v) = ln.strip_prefix("Threads:") {
                threads = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
            }
        }
    }
    (rss, threads)
}

/// `Pss` (KiB) from `/proc/<pid>/smaps_rollup` — the honest per-app footprint
/// under the zygote COW model. 0 if unreadable (host is root, so usually fine).
fn read_pss(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|body| {
            body.lines().find_map(|ln| {
                ln.strip_prefix("Pss:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|n| n.parse().ok())
            })
        })
        .unwrap_or(0)
}

/// Recent CPU load in per-mille of one core for `pid`, from the delta of
/// cumulative ticks vs the last sample this host took. 0 on the first sighting
/// (no baseline yet) — the WIT contract. Records the new baseline.
fn cpu_permille(pid: u32, cpu_ticks: u64, samples: &mut HashMap<u32, (u64, Instant)>) -> u32 {
    let now = Instant::now();
    let permille = match samples.get(&pid) {
        Some(&(prev_ticks, prev_when)) => {
            let wall = now.duration_since(prev_when).as_secs_f64();
            let dticks = cpu_ticks.saturating_sub(prev_ticks);
            if wall > 0.0 {
                ((dticks as f64) * 1000.0 / (clk_tck() as f64 * wall)).round() as u32
            } else {
                0
            }
        }
        None => 0,
    };
    samples.insert(pid, (cpu_ticks, now));
    permille
}

// The `types` interface declares only records/enums (no functions); bindgen still
// emits an empty marker `Host` trait that `add_to_linker` requires.
impl crate::task_manager_host_bindings::wandr::task_manager::types::Host for crate::HostState {}

impl Host for crate::HostState {
    fn list_apps(&mut self) -> Vec<AppInfo> {
        let body = match query_full("task-list\n") {
            Ok(b) => b,
            Err(e) => {
                log::warn!("task-manager: task-list forward failed: {e:#} (arbiter down?)");
                return Vec::new();
            }
        };
        let tck = clk_tck();
        let mut guard = CPU_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
        let samples = guard.get_or_insert_with(HashMap::new);
        let mut out = Vec::new();
        // Prune baselines for pids no longer listed so the map doesn't grow.
        let mut seen = Vec::new();
        for ln in body.lines() {
            // Skip the `OK count=N` / `ERR …` status line; rows start `app=`.
            let Some(rest) = ln.strip_prefix("app=") else { continue };
            let mut app_id = "";
            let mut pid: u32 = 0;
            let mut state = AppState::Background;
            let mut uptime_ms: u64 = 0;
            // First token after `app=` is the id; the rest are `k=v`.
            let mut it = rest.split_whitespace();
            if let Some(id) = it.next() {
                app_id = id;
            }
            for kv in it {
                if let Some(v) = kv.strip_prefix("pid=") {
                    pid = v.parse().unwrap_or(0);
                } else if let Some(v) = kv.strip_prefix("state=") {
                    state = parse_state(v);
                } else if let Some(v) = kv.strip_prefix("uptime_ms=") {
                    uptime_ms = v.parse().unwrap_or(0);
                }
            }
            if app_id.is_empty() || pid == 0 {
                continue;
            }
            seen.push(pid);
            let (kind, label) = kind_and_label(app_id);
            let (cpu_ticks, _) = read_stat(pid).unwrap_or((0, 0));
            let (rss, threads) = read_status(pid);
            let pss = read_pss(pid);
            let permille = cpu_permille(pid, cpu_ticks, samples);
            out.push(AppInfo {
                app_id: app_id.to_string(),
                label,
                pid,
                kind,
                state,
                uptime_ms,
                usage: ResourceUsage {
                    cpu_permille: permille,
                    cpu_time_ms: cpu_ticks.saturating_mul(1000) / tck,
                    mem_rss_kb: rss,
                    mem_pss_kb: pss,
                    threads,
                },
            });
        }
        samples.retain(|pid, _| seen.contains(pid));
        out
    }

    fn system_mem(&mut self) -> SystemMemory {
        let mut total = 0;
        let mut available = 0;
        if let Ok(body) = std::fs::read_to_string("/proc/meminfo") {
            for ln in body.lines() {
                if let Some(v) = ln.strip_prefix("MemTotal:") {
                    total = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                } else if let Some(v) = ln.strip_prefix("MemAvailable:") {
                    available = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                }
            }
        }
        // Sum PSS across the running wandr app set (a fresh task-list query).
        let mut wandr_pss = 0;
        if let Ok(b) = query_full("task-list\n") {
            for ln in b.lines() {
                if let Some(rest) = ln.strip_prefix("app=") {
                    if let Some(pid) = rest
                        .split_whitespace()
                        .find_map(|kv| kv.strip_prefix("pid=").and_then(|v| v.parse::<u32>().ok()))
                    {
                        wandr_pss += read_pss(pid);
                    }
                }
            }
        }
        SystemMemory { total_kb: total, available_kb: available, wandr_pss_kb: wandr_pss }
    }

    fn kill_app(&mut self, app_id: String) -> Result<(), KillError> {
        match query_full(&format!("task-kill {app_id}\n")) {
            Ok(reply) => {
                let r = reply.trim();
                if r.starts_with("OK") {
                    Ok(())
                } else if r.contains("protected") {
                    Err(KillError::Protected)
                } else if r.contains("not-found") {
                    Err(KillError::NotFound)
                } else {
                    Err(KillError::Failed(r.to_string()))
                }
            }
            Err(e) => Err(KillError::Failed(format!("arbiter unreachable: {e}"))),
        }
    }
}
