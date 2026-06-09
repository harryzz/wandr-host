//! App installer — task 35 step 4.
//!
//! Reads a `.wandrpkg` bundle (a directory containing `package.toml` +
//! `components/*.wasm` + optional `assets/`), AOT-precompiles each
//! component for the device via `Engine::precompile_component`, and
//! writes the install dir at `<root>/<app_id>/<version>/` with a
//! `cache-key.toml` for the loader's invalidation check.
//!
//! Layout written:
//! ```text
//! <root>/<app_id>/<version>/
//!   package.toml             # copied verbatim
//!   components/<name>.wasm   # one per [components] entry
//!   cache/<name>.cwasm       # AOT artefact for this device's engine
//!   assets/                  # copied verbatim if bundle has one
//!   cache-key.toml           # wasmtime_version + engine_config_hash + per-component sha256
//! ```
//!
//! Step 5 (loader) re-reads `cache-key.toml` to decide whether to use
//! the cached cwasm or re-call `precompile_component`.
//!
//! See `tasks/35-app-install.md`.

use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use wasmtime::Engine;

/// Wasmtime version pinned in `Cargo.toml`. Diagnostic field in
/// `cache-key.toml`; the *cache-invalidation* signal is
/// `engine_config_hash` (which folds in the wasmtime version via
/// `precompile_compatibility_hash`).
const WASMTIME_PINNED_VERSION: &str = "44";

/// Input to `install` — an unpacked `.wandrpkg` directory.
pub struct PackageBundle<'a> {
    pub dir: &'a Path,
}

impl<'a> PackageBundle<'a> {
    pub fn from_dir(dir: &'a Path) -> Self { Self { dir } }
}

/// What the registry / caller learns from a successful install.
pub struct InstalledApp {
    pub app_id: String,
    pub version: String,
    pub install_dir: PathBuf,
}

pub trait AppInstaller {
    fn install(&self, engine: &Engine, bundle: PackageBundle<'_>) -> Result<InstalledApp>;
}

/// Default installer. Writes under `<root>/<app_id>/<version>/`.
pub struct WandrInstaller {
    pub root: PathBuf,
}

pub fn default_for_target() -> WandrInstaller {
    WandrInstaller { root: crate::app_loader::apps_root() }
}

impl AppInstaller for WandrInstaller {
    fn install(&self, engine: &Engine, bundle: PackageBundle<'_>) -> Result<InstalledApp> {
        let manifest = parse_manifest(bundle.dir)?;
        log::info!(
            "installer: {} v{} ({} component(s)) world={}",
            manifest.app_id, manifest.version, manifest.components.len(), manifest.world,
        );

        // Q5b: signing format not picked yet — placeholder always Ok.
        verify_signature(&bundle)?;

        // Resolve deps FIRST — fail fast on missing deps, before any
        // expensive AOT compile or disk writes.
        let resolved_deps = resolve_dependencies(&self.root, &manifest.dependencies)?;

        let install_dir = self.root
            .join(manifest.kind.root_subdir())
            .join(&manifest.app_id)
            .join(&manifest.version);
        if install_dir.exists() {
            log::warn!("installer: {} already exists — overwriting", install_dir.display());
        }
        let components_dir = install_dir.join("components");
        let cache_dir = install_dir.join("cache");
        fs::create_dir_all(&components_dir)
            .with_context(|| format!("create_dir_all {}", components_dir.display()))?;
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create_dir_all {}", cache_dir.display()))?;

        copy_file(&bundle.dir.join("package.toml"), &install_dir.join("package.toml"))?;
        let assets_src = bundle.dir.join("assets");
        if assets_src.is_dir() {
            copy_dir_recursive(&assets_src, &install_dir.join("assets"))?;
        }

        let mut cache_entries: Vec<(String, ComponentCacheEntry)> = Vec::new();
        for (name, rel_path) in &manifest.components {
            let wasm_src = bundle.dir.join(rel_path);
            let wasm_dst = components_dir.join(format!("{name}.wasm"));
            let wasm_bytes = fs::read(&wasm_src)
                .with_context(|| format!("read {}", wasm_src.display()))?;
            fs::write(&wasm_dst, &wasm_bytes)
                .with_context(|| format!("write {}", wasm_dst.display()))?;
            let wasm_sha = sha256_hex(&wasm_bytes);

            log::info!("installer: AOT-compiling {name} ({} bytes) …", wasm_bytes.len());
            let cwasm_bytes = engine.precompile_component(&wasm_bytes)
                .map_err(|e| anyhow!("precompile_component({name}): {e:#}"))?;
            let cwasm_path = cache_dir.join(format!("{name}.cwasm"));
            fs::write(&cwasm_path, &cwasm_bytes)
                .with_context(|| format!("write {}", cwasm_path.display()))?;
            let cwasm_sha = sha256_hex(&cwasm_bytes);
            log::info!(
                "installer: {name}.cwasm {} bytes → {}",
                cwasm_bytes.len(), cwasm_path.display(),
            );
            cache_entries.push((
                name.clone(),
                ComponentCacheEntry { wasm_sha256: wasm_sha, cwasm_sha256: cwasm_sha },
            ));
        }

        let key_doc = format_cache_key(engine, &cache_entries, &resolved_deps);
        fs::write(install_dir.join("cache-key.toml"), key_doc)
            .with_context(|| format!("write cache-key.toml at {}", install_dir.display()))?;

        Ok(InstalledApp {
            app_id: manifest.app_id,
            version: manifest.version,
            install_dir,
        })
    }
}

pub(crate) struct Manifest {
    pub app_id: String,
    pub version: String,
    pub world: String,
    /// Task 36: routes the install dir layout — `App` bundles land in
    /// `<root>/apps/<app_id>/<version>/`, `System` bundles in
    /// `<root>/system-apps/<app_id>/<version>/`. Default `App` when
    /// `kind` is omitted in the manifest (matches task-35 fixtures).
    pub kind: PackageKind,
    /// Task 36 (Q6): every package must declare its composition mode.
    /// Producer-authoritative; consumers cannot override.
    pub composition: Composition,
    /// Task 62: whether this app's surface follows device orientation.
    /// `Auto` = rotate with the device; `Locked` (default) = stay portrait.
    /// Drives the overlay-rotation gate in `standalone.rs`. Pure policy
    /// hint — NOT part of the AOT cache key.
    pub orientation: Orientation,
    pub components: Vec<(String, PathBuf)>,
    /// Task 36: empty for task-35-style standalone apps. Each entry is
    /// one dep edge to a host WIT / runtime-bundled system component /
    /// installed user app.
    pub dependencies: Vec<Dependency>,
}

/// Bundle role — distinguishes a user-installable app from a
/// runtime-bundled system component. Defaults to `App`; system bundles
/// must declare `kind = "system"` in their manifest.
///
/// Note on identifiers: a bundle's `app_id` is a *filesystem-safe*
/// identifier (e.g. `wandr.markdown.renderer`, dot-separated). The
/// WIT-package qualified name used inside `.wit` files (e.g.
/// `wandr:markdown/renderer@0.1.0`) lives in the WIT layer; the install
/// path uses `app_id`. Consumer `[dependencies]` entries reference the
/// producer's `app_id` via `system = "..."` / `app = "..."`, and may
/// disambiguate which interface via `interface = "..."`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageKind {
    App,
    System,
}

impl PackageKind {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "app"    => Some(Self::App),
            "system" => Some(Self::System),
            _ => None,
        }
    }
    /// Top-level directory under the WandrInstaller / WandrLoader root.
    pub(crate) fn root_subdir(self) -> &'static str {
        match self { Self::App => "apps", Self::System => "system-apps" }
    }
}

/// Same-Store = library-like (shared GC, shared crash domain;
/// composed via `Linker::instance` at instantiation time).
/// Separate-Store = service-like (its own `Store`, accessed through a
/// host proxy that marshals between Stores). Task 36 step 5
/// implements the same-store path; separate-store is a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Composition {
    SameStore,
    SeparateStore,
}

impl Composition {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "same-store"     => Some(Self::SameStore),
            "separate-store" => Some(Self::SeparateStore),
            _ => None,
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self { Self::SameStore => "same-store", Self::SeparateStore => "separate-store" }
    }
}

/// Task 62: orientation policy for a package's surface. `Auto` follows the
/// device's Device-Orientation HAL sensor; `Locked` (default) stays in the
/// panel's native portrait. Used by `standalone.rs` to gate the overlay /
/// fullscreen auto-rotate path. Same string-enum shape as `Composition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Orientation {
    Auto,
    Locked,
}

impl Orientation {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "auto"   => Some(Self::Auto),
            "locked" => Some(Self::Locked),
            _ => None,
        }
    }
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self { Self::Auto => "auto", Self::Locked => "locked" }
    }
}

pub(crate) struct Dependency {
    /// Local alias the consumer uses for this dep (LHS in
    /// `[dependencies] <alias> = { … }`). Carries no global meaning;
    /// the actual resolution target is in `kind`.
    pub name: String,
    pub kind: DependencyKind,
    /// Semver range, e.g. "^0.1", "1.0", "*". Range matching is the
    /// installer resolver's job (step 4); parsing only validates
    /// presence + non-empty.
    pub version: String,
    /// Qualified WIT interface name, e.g. "wandr:markdown/renderer@0.1.0".
    /// Required for system / app deps so the consumer can pick one
    /// interface out of a multi-interface producer; optional for host
    /// deps where the host = "…" identifier already names a WIT.
    pub interface: Option<String>,
}

/// Three resolution flavours. The discriminator key in the manifest
/// (`host` / `system` / `app`) chooses the variant; exactly one must
/// be set per dep entry.
pub(crate) enum DependencyKind {
    /// Host-provided WIT — implemented in the wandr-host Rust binary
    /// and bound into every Linker via `add_to_linker_sync`. The
    /// string is the WIT identifier the runtime must satisfy.
    Host(String),
    /// Runtime-bundled component under `<root>/system-apps/<id>/`.
    System(String),
    /// User app under `<root>/apps/<id>/`. Reverse-deps tracked at
    /// uninstall (step 4 scope).
    App(String),
}

pub(crate) struct ComponentCacheEntry {
    pub wasm_sha256: String,
    pub cwasm_sha256: String,
}

/// One concrete `[dependencies]` entry after the resolver located it
/// on disk (or, for host deps, in the runtime's compiled-in set).
/// Recorded verbatim in the consumer's `cache-key.toml` so any dep
/// update flips the consumer's hash → re-precompile on next launch.
pub(crate) struct ResolvedDependency {
    /// Local alias from the consumer's manifest LHS.
    pub name: String,
    pub kind: ResolvedKind,
    /// Concrete version picked from the version range. Always pinned
    /// — no `*` survives past the resolver.
    pub resolved_version: String,
    /// `some(...)` for system/app deps (sha256 of the dep's
    /// `components/<entry>.wasm`); `none` for host deps (the host
    /// implementation lives in the wandr-host binary itself, so the
    /// dep is invalidated by changes to the wandr-host build —
    /// captured by `engine_config_hash` indirectly).
    pub wasm_sha256: Option<String>,
    /// Optional WIT-qualified interface name from the dep entry.
    pub interface: Option<String>,
}

pub(crate) enum ResolvedKind {
    Host(String),
    System(String),
    App(String),
}

impl ResolvedKind {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            ResolvedKind::Host(_)   => "host",
            ResolvedKind::System(_) => "system",
            ResolvedKind::App(_)    => "app",
        }
    }
    pub(crate) fn id(&self) -> &str {
        match self {
            ResolvedKind::Host(s) | ResolvedKind::System(s) | ResolvedKind::App(s) => s,
        }
    }
}

/// Inverse of `format_cache_key`'s `[dependencies_resolved]` emission
/// — the loader uses this when it has to re-stamp on cache drift
/// (engine or wasm change) so the existing dep list is preserved.
pub(crate) fn parse_resolved_deps_from_key(doc: &toml::Value) -> Vec<ResolvedDependency> {
    let mut out = Vec::new();
    let Some(tbl) = doc.get("dependencies_resolved").and_then(|v| v.as_table()) else {
        return out;
    };
    for (name, val) in tbl {
        let Some(entry) = val.as_table() else { continue };
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kind = match entry.get("kind").and_then(|v| v.as_str()) {
            Some("host")   => ResolvedKind::Host(id),
            Some("system") => ResolvedKind::System(id),
            Some("app")    => ResolvedKind::App(id),
            _ => continue,
        };
        out.push(ResolvedDependency {
            name: name.clone(),
            kind,
            resolved_version: entry.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            wasm_sha256: entry.get("wasm_sha256").and_then(|v| v.as_str()).map(str::to_string),
            interface: entry.get("interface").and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    out
}

/// Walk a manifest's `[dependencies]` list and locate each entry on
/// disk (or, for host kind, accept verbatim). Aborts the install with
/// a clear error on any unresolved entry — refuses before any AOT
/// compile or disk write happens.
///
/// Version-range matching is *minimal* in this pass:
///   - `"*"` → highest installed (lex sort).
///   - Anything else → exact match.
/// Full semver range matching (e.g. `^0.1`) is deferred. See
/// `tasks/36-cross-app-deps.md` "What stays out of scope".
fn resolve_dependencies(
    root: &Path,
    deps: &[Dependency],
) -> Result<Vec<ResolvedDependency>> {
    let mut resolved: Vec<ResolvedDependency> = Vec::new();
    for dep in deps {
        let r = match &dep.kind {
            DependencyKind::Host(wit) => resolve_host(dep, wit)?,
            DependencyKind::System(id) => {
                resolve_filesystem(root, "system-apps", id, dep, ResolvedKind::System(id.clone()))?
            }
            DependencyKind::App(id) => {
                resolve_filesystem(root, "apps", id, dep, ResolvedKind::App(id.clone()))?
            }
        };
        resolved.push(r);
    }
    Ok(resolved)
}

/// Host deps pass through — we don't track a per-version manifest of
/// what the runtime offers. Version pinning for host deps is captured
/// by `engine_config_hash` (any wandr-host rebuild flips it) rather
/// than per-dep hashing.
fn resolve_host(dep: &Dependency, wit: &str) -> Result<ResolvedDependency> {
    Ok(ResolvedDependency {
        name: dep.name.clone(),
        kind: ResolvedKind::Host(wit.to_string()),
        resolved_version: dep.version.clone(),
        wasm_sha256: None,
        interface: dep.interface.clone(),
    })
}

fn resolve_filesystem(
    root: &Path,
    subdir: &str,
    id: &str,
    dep: &Dependency,
    kind: ResolvedKind,
) -> Result<ResolvedDependency> {
    let id_dir = root.join(subdir).join(id);
    if !id_dir.is_dir() {
        bail!(
            "missing dependency: {} (kind={}, id={id:?}) — expected at {}",
            dep.name, kind.label(), id_dir.display(),
        );
    }
    let resolved_version = pick_version(&id_dir, &dep.version)
        .with_context(|| format!("dependency {}: no version matches {:?}", dep.name, dep.version))?;
    let dep_dir = id_dir.join(&resolved_version);
    if !dep_dir.is_dir() {
        bail!("missing dependency: {} at {}", dep.name, dep_dir.display());
    }
    // Locate the dep's primary component. The dep's own manifest names
    // its components; for this iteration we hash the first `.wasm`
    // under `<dep_dir>/components/`. Multi-component deps require the
    // consumer to name which interface in `interface = "..."`, but the
    // resolver still hashes the whole dep for cache invalidation.
    let wasm_sha = hash_first_component_wasm(&dep_dir).with_context(|| {
        format!("dependency {}: failed to hash components under {}", dep.name, dep_dir.display())
    })?;
    Ok(ResolvedDependency {
        name: dep.name.clone(),
        kind,
        resolved_version,
        wasm_sha256: Some(wasm_sha),
        interface: dep.interface.clone(),
    })
}

fn pick_version(id_dir: &Path, range: &str) -> Result<String> {
    let mut versions: Vec<String> = Vec::new();
    for entry in fs::read_dir(id_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(n) = entry.file_name().to_str() { versions.push(n.to_string()); }
        }
    }
    versions.sort();
    if range == "*" {
        versions.pop().ok_or_else(|| anyhow!("no versions installed under {}", id_dir.display()))
    } else if versions.iter().any(|v| v == range) {
        Ok(range.to_string())
    } else {
        bail!(
            "no installed version matches {range:?} under {} (have: {:?})",
            id_dir.display(), versions,
        );
    }
}

fn hash_first_component_wasm(dep_dir: &Path) -> Result<String> {
    let components_dir = dep_dir.join("components");
    for entry in fs::read_dir(&components_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            let bytes = fs::read(&path)?;
            return Ok(sha256_hex(&bytes));
        }
    }
    bail!("no .wasm under {}", components_dir.display())
}

fn parse_manifest(bundle_dir: &Path) -> Result<Manifest> {
    let pkg_path = bundle_dir.join("package.toml");
    let src = fs::read_to_string(&pkg_path)
        .with_context(|| format!("read {}", pkg_path.display()))?;
    let doc: toml::Value = src.parse()
        .with_context(|| format!("parse {}", pkg_path.display()))?;

    let app_id = doc.get("app_id").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("package.toml: missing app_id"))?
        .to_string();
    let version = doc.get("version").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("package.toml: missing version"))?
        .to_string();
    let world = doc.get("world").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("package.toml: missing world"))?
        .to_string();

    // Task 36: optional kind discriminator — "app" (default) or "system".
    // Routes the install dir layout — see `PackageKind` doc.
    let kind = match doc.get("kind").and_then(|v| v.as_str()) {
        None => PackageKind::App,
        Some(s) => PackageKind::from_str(s).ok_or_else(|| anyhow!(
            "package.toml: kind = \"{s}\" — must be \"app\" or \"system\""
        ))?,
    };

    // Q6 (task 36): every package must declare its composition mode.
    // Required field; no default. See tasks/36-cross-app-deps.md.
    let composition_str = doc.get("composition").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!(
            "package.toml: missing composition (must be \"same-store\" or \"separate-store\")"
        ))?;
    let composition = Composition::from_str(composition_str).ok_or_else(|| anyhow!(
        "package.toml: composition = \"{composition_str}\" — must be \"same-store\" or \"separate-store\""
    ))?;

    // Task 62: optional orientation policy — "auto" or "locked" (default).
    // Omitted ⇒ Locked, preserving every existing manifest's behavior.
    let orientation = match doc.get("orientation").and_then(|v| v.as_str()) {
        None => Orientation::Locked,
        Some(s) => Orientation::from_str(s).ok_or_else(|| anyhow!(
            "package.toml: orientation = \"{s}\" — must be \"auto\" or \"locked\""
        ))?,
    };

    let components_tbl = doc.get("components").and_then(|v| v.as_table())
        .ok_or_else(|| anyhow!("package.toml: missing [components] table"))?;
    if components_tbl.is_empty() {
        bail!("package.toml: [components] is empty");
    }
    let mut components: Vec<(String, PathBuf)> = Vec::new();
    for (name, val) in components_tbl {
        let rel = val.as_str().ok_or_else(|| {
            anyhow!("package.toml: components.{name} must be a string path")
        })?;
        components.push((name.clone(), PathBuf::from(rel)));
    }

    let dependencies = parse_dependencies(doc.get("dependencies"))?;

    Ok(Manifest { app_id, version, world, kind, composition, orientation, components, dependencies })
}

/// Parse `[dependencies]` table. Returns an empty Vec if the table is
/// absent (task-35-style apps with no deps). Each entry must contain
/// exactly one of `host` / `system` / `app` and a `version` string.
fn parse_dependencies(node: Option<&toml::Value>) -> Result<Vec<Dependency>> {
    let Some(tbl) = node.and_then(|v| v.as_table()) else {
        return Ok(Vec::new());
    };
    let mut deps: Vec<Dependency> = Vec::new();
    for (name, val) in tbl {
        let entry = val.as_table().ok_or_else(|| anyhow!(
            "package.toml: dependencies.{name} must be a table (e.g. \
             {{ system = \"...\", version = \"...\" }})"
        ))?;

        let host_v = entry.get("host").and_then(|v| v.as_str());
        let system_v = entry.get("system").and_then(|v| v.as_str());
        let app_v = entry.get("app").and_then(|v| v.as_str());
        let kinds_set = [host_v, system_v, app_v].iter().filter(|v| v.is_some()).count();
        if kinds_set != 1 {
            bail!(
                "package.toml: dependencies.{name} must set exactly one of \
                 `host` / `system` / `app` ({kinds_set} set)"
            );
        }
        let kind = if let Some(s) = host_v {
            DependencyKind::Host(s.to_string())
        } else if let Some(s) = system_v {
            DependencyKind::System(s.to_string())
        } else {
            DependencyKind::App(app_v.unwrap().to_string())
        };

        let version = entry.get("version").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("package.toml: dependencies.{name} missing version"))?;
        if version.is_empty() {
            bail!("package.toml: dependencies.{name}.version is empty");
        }
        let interface = entry.get("interface").and_then(|v| v.as_str()).map(str::to_string);

        deps.push(Dependency {
            name: name.clone(),
            kind,
            version: version.to_string(),
            interface,
        });
    }
    Ok(deps)
}

pub(crate) fn format_cache_key(
    engine: &Engine,
    entries: &[(String, ComponentCacheEntry)],
    resolved_deps: &[ResolvedDependency],
) -> String {
    let cfg_hash = engine_compatibility_hash_hex(engine);
    let mut out = String::new();
    out.push_str(&format!("wasmtime_version = \"{WASMTIME_PINNED_VERSION}\"\n"));
    out.push_str(&format!("engine_config_hash = \"{cfg_hash}\"\n\n"));
    for (name, entry) in entries {
        out.push_str(&format!("[components.{name}]\n"));
        out.push_str(&format!("wasm_sha256  = \"{}\"\n", entry.wasm_sha256));
        out.push_str(&format!("cwasm_sha256 = \"{}\"\n\n", entry.cwasm_sha256));
    }
    // Per scope (task 36 §"Cache key extension"): any dep update flips
    // a hash → A's cache invalidates → re-precompile on next launch.
    // Same mechanism as wasmtime upgrade. The loader (step 5) reads
    // these entries to compose deps at instantiation time.
    for dep in resolved_deps {
        out.push_str(&format!("[dependencies_resolved.{}]\n", dep.name));
        out.push_str(&format!("kind    = \"{}\"\n", dep.kind.label()));
        out.push_str(&format!("id      = \"{}\"\n", dep.kind.id()));
        out.push_str(&format!("version = \"{}\"\n", dep.resolved_version));
        if let Some(sha) = &dep.wasm_sha256 {
            out.push_str(&format!("wasm_sha256 = \"{sha}\"\n"));
        }
        if let Some(iface) = &dep.interface {
            out.push_str(&format!("interface   = \"{iface}\"\n"));
        }
        out.push('\n');
    }
    out
}

/// wasmtime's `precompile_compatibility_hash` returns an opaque `impl Hash`
/// that covers every compile flag + the wasmtime version. We feed it
/// through Sha256 for a stable hex fingerprint.
pub(crate) fn engine_compatibility_hash_hex(engine: &Engine) -> String {
    struct Sha256Hasher(Sha256);
    impl Hasher for Sha256Hasher {
        fn finish(&self) -> u64 { 0 }
        fn write(&mut self, bytes: &[u8]) { self.0.update(bytes); }
    }
    let mut h = Sha256Hasher(Sha256::new());
    engine.precompile_compatibility_hash().hash(&mut h);
    format!("sha256:{}", hex_lower(&h.0.finalize()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut d = Sha256::new();
    d.update(bytes);
    format!("sha256:{}", hex_lower(&d.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn verify_signature(_bundle: &PackageBundle<'_>) -> Result<()> { Ok(()) }

fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst)
        .map(|_| ())
        .with_context(|| format!("copy {} → {}", src.display(), dst.display()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}
