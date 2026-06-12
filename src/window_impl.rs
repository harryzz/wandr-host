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
        read_dpi() as f32 / 160.0
    }
    fn get_font_scale(&mut self) -> f32 {
        read_font_scale()
    }
    fn get_dpi(&mut self) -> u32 {
        read_dpi()
    }
}
