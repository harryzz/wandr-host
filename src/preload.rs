//! Component preload registry — task 46 step 2.
//!
//! Holds `wasmtime::component::Component` values deserialized in the
//! wandr-zygote parent process, keyed by their on-disk `.cwasm` path.
//! After `fork()` the child COW-inherits the registry contents; the
//! loader (`app_loader.rs`) consults the registry before calling
//! `Component::deserialize_file` and uses the preloaded `Component`
//! (cheap `Arc`-internal clone) when present.
//!
//! **Design notes**:
//! - Keyed by absolute path so both top-level apps (`load_installed`)
//!   and same-store deps (`load_dep_components`) hit the same lookup
//!   without duplicating the registry shape.
//! - `Mutex<HashMap>` rather than `RwLock`: the parent's
//!   `preload_app` calls do mutate (insert / replace), but they happen
//!   at zygote startup or as one-off socket commands — uncontended.
//!   Children never write; they read once at load time and the held
//!   mutex is released before any heavy work.
//! - A miss is not an error. The loader falls through to
//!   `Component::deserialize_file` on miss, which gives us the same
//!   behavior as before preload existed.
//! - Population is two-staged: at zygote startup we walk
//!   `<APPS_ROOT>/system-apps/*` and preload every installed system
//!   bundle (they're always-shared by Compose apps). User apps under
//!   `apps/*` are preloaded on demand via the `PRELOAD <app-id>`
//!   socket command (also called by the installer after upgrades, by
//!   the future wandr-arbiter predictively before launch, or by tests).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use wasmtime::component::Component;
use wasmtime::Engine;

/// Where preloaded components live. Keyed by absolute path of the
/// `.cwasm` file the loader would otherwise pass to
/// `Component::deserialize_file`.
fn registry() -> &'static Mutex<HashMap<PathBuf, Component>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Component>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Loader hook — return a preloaded `Component` for the given cwasm
/// path, or `None` if not preloaded. The returned `Component` is
/// cheap to clone (`Arc`-internal); callers can use it directly.
pub fn get(path: &Path) -> Option<Component> {
    registry().lock().ok()?.get(path).cloned()
}

/// Insert / replace a single component. Used by `preload_app` below
/// and by future PRELOAD socket-command handling.
pub fn insert(path: PathBuf, component: Component) {
    if let Ok(mut reg) = registry().lock() {
        reg.insert(path, component);
    }
}

/// Return how many components are preloaded. Logging-only.
pub fn count() -> usize {
    registry().lock().map(|m| m.len()).unwrap_or(0)
}

/// Drop every preloaded `Component` keyed under `<APPS_ROOT>/<kind>/<app_id>/…`.
/// Used by the PRELOAD socket command to invalidate a stale app's
/// entries before re-deserializing the new version. The match is by
/// path prefix (every component of the same app lives under a common
/// `<app_id>/<version>/cache/` subtree).
pub fn drop_prefix(prefix: &Path) -> usize {
    let mut dropped = 0;
    if let Ok(mut reg) = registry().lock() {
        let keys: Vec<PathBuf> = reg.keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for k in keys {
            reg.remove(&k);
            dropped += 1;
        }
    }
    dropped
}

/// Preload every `.cwasm` under `<root>/<kind_dir>/<app_id>/<latest_version>/cache/`.
///
/// `kind_dir` is either `"apps"` or `"system-apps"`. If `app_id`
/// doesn't exist under `kind_dir`, this is an error.
///
/// Errors on individual `.cwasm`s (e.g. engine-config drift produces
/// a deserialize error) are logged and skipped — the loader's
/// fall-through to `deserialize_file` will run the
/// re-precompile-on-drift path lazily.
pub fn preload_app(engine: &Engine, root: &Path, kind_dir: &str, app_id: &str) -> Result<usize> {
    let app_dir = root.join(kind_dir).join(app_id);
    if !app_dir.is_dir() {
        bail!("preload: {} not found", app_dir.display());
    }
    let version = pick_latest_version(&app_dir)
        .with_context(|| format!("preload: pick version for {}", app_dir.display()))?;
    let cache_dir = app_dir.join(&version).join("cache");
    if !cache_dir.is_dir() {
        bail!("preload: cache dir missing: {}", cache_dir.display());
    }

    // Drop any prior preloads for this app (under any version) so a
    // PRELOAD after an in-place upgrade replaces stale entries.
    let dropped = drop_prefix(&app_dir);
    if dropped > 0 {
        log::info!("preload: dropped {dropped} stale entry(ies) for {app_id}");
    }

    let mut added = 0usize;
    for entry in fs::read_dir(&cache_dir)
        .with_context(|| format!("read_dir {}", cache_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cwasm") {
            continue;
        }
        let canon = path.canonicalize().unwrap_or(path.clone());
        match unsafe { Component::deserialize_file(engine, &canon) } {
            Ok(component) => {
                log::info!("preload: + {}", canon.display());
                insert(canon, component);
                added += 1;
            }
            Err(e) => {
                log::warn!(
                    "preload: skip {} — deserialize failed: {e:#}. \
                     Loader will re-precompile on first launch.",
                    canon.display(),
                );
            }
        }
    }
    Ok(added)
}

/// Walk every immediate subdirectory of `<root>/system-apps/` and call
/// `preload_app` for each. Called once at zygote startup.
///
/// Per-app errors are logged + skipped; we want startup to succeed
/// even if one bundle is broken.
pub fn preload_all_system_apps(engine: &Engine, root: &Path) -> usize {
    let system_dir = root.join("system-apps");
    if !system_dir.is_dir() {
        log::info!(
            "preload: no system-apps dir at {} — skipping startup auto-preload",
            system_dir.display(),
        );
        return 0;
    }
    let mut total = 0;
    let read = match fs::read_dir(&system_dir) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("preload: read_dir {}: {e:#}", system_dir.display());
            return 0;
        }
    };
    for entry in read {
        let Ok(entry) = entry else { continue };
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        match preload_app(engine, root, "system-apps", &name) {
            Ok(n) => {
                log::info!("preload: system-app `{name}` — {n} component(s)");
                total += n;
            }
            Err(e) => {
                log::warn!("preload: system-app `{name}`: {e:#}");
            }
        }
    }
    total
}

/// Same as `app_loader::pick_latest_version` but kept local so this
/// module can be used from `zygote.rs` without pulling in a cyclic
/// dep on the loader's private fns. Lexicographic sort.
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
    versions.pop()
        .ok_or_else(|| anyhow!("no versions under {}", app_dir.display()))
}

/// Public for the PRELOAD socket command — try the user-app dir
/// first, fall back to system-apps. Returns the kind_dir that won
/// (for logging) and the component count.
pub fn preload_either(engine: &Engine, root: &Path, app_id: &str) -> Result<(&'static str, usize)> {
    let apps_path = root.join("apps").join(app_id);
    if apps_path.is_dir() {
        let n = preload_app(engine, root, "apps", app_id)?;
        return Ok(("apps", n));
    }
    let sys_path = root.join("system-apps").join(app_id);
    if sys_path.is_dir() {
        let n = preload_app(engine, root, "system-apps", app_id)?;
        return Ok(("system-apps", n));
    }
    bail!(
        "preload: app `{app_id}` not found under {}/{{apps,system-apps}}",
        root.display(),
    )
}
