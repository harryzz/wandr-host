use crate::ui_shell_bindings::wandr::ui_shell::metrics::Host;

#[cfg(target_os = "android")]
pub fn read_dpi() -> u32 {
    use android_system_properties::AndroidSystemProperties;
    AndroidSystemProperties::new()
        .get("ro.sf.lcd_density")
        .and_then(|s| s.parse().ok())
        .unwrap_or(320)
}

#[cfg(target_os = "android")]
fn read_font_scale() -> f32 {
    use android_system_properties::AndroidSystemProperties;
    AndroidSystemProperties::new()
        .get("persist.sys.font_scale")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0)
}

#[cfg(not(target_os = "android"))]
pub fn read_dpi() -> u32 { 320 }

#[cfg(not(target_os = "android"))]
fn read_font_scale() -> f32 { 1.0 }

impl Host for crate::HostState {
    fn get_density(&mut self) -> f32 {
        // Android: real panel density (ro.sf.lcd_density/160).
        #[cfg(target_os = "android")]
        { read_dpi() as f32 / 160.0 }
        // Desktop: 1.0. The host renders the guest's LOGICAL canvas and handles
        // the physical/HiDPI upscaling itself (the base_matrix maps logical →
        // buffer), so the guest works in logical space. Returning the raw window
        // scale_factor here (e.g. 2.0 on a Retina Mac) made dioxus-canvas guests
        // set_scale(2.0) and lay their UI out 2× too big for the logical canvas —
        // the Signal QR/link screen overflowed and clipped regardless of window
        // size. A 1:1 display already reported 1.0, so this only fixes HiDPI.
        #[cfg(not(target_os = "android"))]
        { 1.0 }
    }
    fn get_font_scale(&mut self) -> f32 {
        read_font_scale()
    }
    fn get_dpi(&mut self) -> u32 {
        #[cfg(target_os = "android")]
        { read_dpi() }
        // Desktop: baseline 160 dpi (density 1.0) — see get_density.
        #[cfg(not(target_os = "android"))]
        { 160 }
    }
}
