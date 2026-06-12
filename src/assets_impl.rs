//! Asset reader — task 38. Implements the `my:skiko-gfx/assets` WIT
//! interface (`read(name) -> option<list<u8>>`) by reading from
//! `HostState::assets_dir`. Set when the loader builds a `LoadedApp`
//! for an `AppRef::Installed` path that shipped an `assets/` directory;
//! `None` for dev paths or bundles with no assets.
//!
//! The host enforces path safety here: `name` is interpreted relative
//! to the install's `assets/` root, with no `..` components and no
//! absolute path. Violations return `None` and log a warning — the
//! guest gets the same signal as a missing-file case.

use std::path::{Component, Path};

use crate::assets_pkg_bindings::wandr::assets::assets::Host;

impl Host for crate::HostState {
    fn read(&mut self, name: String) -> Option<Vec<u8>> {
        let Some(root) = self.assets_dir.as_ref() else {
            log::warn!("assets: read(\"{name}\") — no assets dir for this load");
            return None;
        };
        if !is_safe_relative(&name) {
            log::warn!("assets: read(\"{name}\") rejected — unsafe path");
            return None;
        }
        let path = root.join(&name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                log::debug!("assets: read(\"{name}\") → {} bytes", bytes.len());
                Some(bytes)
            }
            Err(e) => {
                log::warn!("assets: read(\"{name}\") failed: {e}");
                None
            }
        }
    }
}

/// Reject absolute paths and any `..` traversal. Empty names also
/// rejected (`<root>/""` quietly opens the dir on some platforms).
fn is_safe_relative(name: &str) -> bool {
    if name.is_empty() { return false; }
    let p = Path::new(name);
    if p.is_absolute() { return false; }
    for comp in p.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {}              // `./foo.md` is fine
            _ => return false,                   // ParentDir, RootDir, Prefix → reject
        }
    }
    true
}
