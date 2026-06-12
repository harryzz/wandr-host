//! System theme — `my:skiko-gfx/theme` WIT impl.
//!
//! v1 reads via `cmd uimode night` (stdout parsing). It's a shell-out
//! per call; current consumers only read once at composition time, so
//! the cost is negligible. If a live-watcher / per-frame poll becomes
//! a thing, switch to caching + a sysprop watcher or rsbinder to
//! `IUiModeManager`.
//!
//! Material You accent reads are deferred — returns 0 (caller picks
//! a fallback palette). Pixel 2 XL stock is pre-Material-You anyway.

use std::process::Command;

use crate::ui_shell_bindings::wandr::ui_shell::theme::{Host, NightMode};

impl Host for crate::HostState {
    fn get_night_mode(&mut self) -> NightMode {
        match read_night_mode_via_cmd() {
            Some(n) => n,
            None => {
                log::warn!("theme: get_night_mode — cmd uimode night failed, returning Auto");
                NightMode::Auto
            }
        }
    }

    fn get_accent_color(&mut self) -> u32 {
        // v1 reads from `persist.sys.wandr.accent` sysprop (ARGB u32, e.g.
        // 0xFF34A853 for Google green). User-settable via
        // `setprop persist.sys.wandr.accent 0x...`. Returns 0 when unset
        // → consumer falls back to its default scheme.
        //
        // Real Material You wallpaper-driven extraction isn't available
        // on the Pixel 2 XL / LineageOS stack (`isColorExtracted=false`
        // in dumpsys wallpaper). When a device that DOES have it shows
        // up, swap this for an rsbinder call to IWallpaperManager.
        read_accent_via_sysprop().unwrap_or(0)
    }
}

#[cfg(target_os = "android")]
fn read_accent_via_sysprop() -> Option<u32> {
    use android_system_properties::AndroidSystemProperties;
    let raw = AndroidSystemProperties::new().get("persist.sys.wandr.accent")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() { return None; }
    // Accept "0x..." / "0X..." hex or plain decimal.
    let parsed = if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u32>().ok()
    };
    if let Some(v) = parsed {
        log::info!("theme: read accent-color=0x{v:08X} (raw={trimmed:?})");
    } else {
        log::warn!("theme: persist.sys.wandr.accent unparseable: {trimmed:?}");
    }
    parsed
}

#[cfg(not(target_os = "android"))]
fn read_accent_via_sysprop() -> Option<u32> { None }

/// Parses output like:
///   Night mode: yes
///   Night mode: no
///   Night mode: auto
fn read_night_mode_via_cmd() -> Option<NightMode> {
    let out = Command::new("cmd").args(["uimode", "night"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let after = s.split(':').nth(1)?.trim().to_ascii_lowercase();
    let mode = match after.as_str() {
        "yes" => NightMode::On,
        "no"  => NightMode::Off,
        _     => NightMode::Auto,
    };
    log::info!("theme: read night-mode={mode:?} (raw={:?})", s.trim());
    Some(mode)
}
