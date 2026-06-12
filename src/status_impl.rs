//! System-shell status data — `my:skiko-gfx/status` WIT impl (task 55).
//!
//! ART-free: clock from the device local time, battery from sysfs — no
//! `system_server` (per [[feedback_no_art_layer_dependencies]]). Consumed
//! by the `wandr.statusbar` guest, which polls these ~1 Hz and draws the
//! top-overlay strip.

use crate::chrome_bindings::wandr::chrome::status::Host;

/// Status-bar strip height in physical pixels. True-dp (Arbiter Inc. 3b): the
/// arbiter authors it (dp×density) and the host caches it, so `bar_height()` +
/// the launcher's top inset both resolve to the same arbiter-authored value.
/// Delegates to the standalone chrome-height cache (dp×density fallback if the
/// arbiter hasn't provided one yet).
pub fn status_bar_height_px() -> u32 {
    #[cfg(target_os = "android")]
    {
        crate::standalone::status_bar_height_px()
    }
    // Desktop (winit dev host): no arbiter chrome — no status-bar inset.
    #[cfg(not(target_os = "android"))]
    {
        0
    }
}

impl Host for crate::HostState {
    fn bar_height(&mut self) -> u32 {
        status_bar_height_px()
    }

    fn clock_text(&mut self) -> String {
        // Local wall-clock, in-process. Rust std has no localtime, but bionic's
        // localtime_r does the timezone work (tzset reads $TZ, falling back to
        // persist.sys.timezone) — still ART-free, no system_server. This used to
        // shell out to `date`, but simpleperf showed each ~1 Hz call forking the
        // whole wasmtime host (copy_page_range on a ~57 MB process) + exec'ing +
        // relinking toybox ≈ 4% of a core, the statusbar's dominant idle cost.
        let t = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            log::warn!("status: localtime_r failed");
            return String::new();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }

    fn battery_text(&mut self) -> String {
        // sysfs — no binder, no system_server. Path is the standard
        // power_supply class node; absent on some emulators (returns "").
        const CAP: &str = "/sys/class/power_supply/battery/capacity";
        match std::fs::read_to_string(CAP) {
            Ok(s) => {
                let pct = s.trim();
                if pct.is_empty() { String::new() } else { format!("{pct}%") }
            }
            Err(e) => {
                log::debug!("status: read {CAP} failed: {e}");
                String::new()
            }
        }
    }
}
