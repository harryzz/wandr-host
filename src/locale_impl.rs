use crate::ui_shell_bindings::wandr::ui_shell::locale::{Host, LayoutDirection};

#[cfg(target_os = "android")]
fn read_prop(name: &str) -> Option<String> {
    use android_system_properties::AndroidSystemProperties;
    AndroidSystemProperties::new().get(name)
}

#[cfg(not(target_os = "android"))]
fn read_prop(_: &str) -> Option<String> { None }

fn primary_locale_tag() -> String {
    read_prop("persist.sys.locale")
        .or_else(|| read_prop("ro.product.locale"))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}

/// Two-letter language subtag of a BCP-47 tag (everything before the first
/// '-' or '_'), lowercased.
fn lang_of(tag: &str) -> String {
    tag.split(|c| c == '-' || c == '_')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

impl Host for crate::HostState {
    fn primary_locale(&mut self) -> String {
        primary_locale_tag()
    }

    fn is_twenty_four_hour_format(&mut self) -> bool {
        // Explicit user override wins.
        match read_prop("persist.sys.timeFormat").as_deref() {
            Some("24") => return true,
            Some("12") => return false,
            _ => {}
        }
        // No override → default by locale. US/Canada/Australia/Mexico/some
        // others default to 12-hour. Most of the world is 24-hour.
        let lang = lang_of(&primary_locale_tag());
        let tag  = primary_locale_tag();
        let region = tag.split(|c| c == '-' || c == '_')
            .nth(1).unwrap_or("").to_ascii_uppercase();
        let twelve_hour_regions = ["US", "CA", "AU", "NZ", "PH", "IN", "EG", "MX", "CO"];
        if lang == "en" && twelve_hour_regions.contains(&region.as_str()) {
            return false;
        }
        true
    }

    fn get_layout_direction(&mut self) -> LayoutDirection {
        let lang = lang_of(&primary_locale_tag());
        // Standard set of RTL languages — same list Android's
        // TextDirectionHeuristics uses.
        match lang.as_str() {
            "ar" | "fa" | "he" | "iw" | "ur" | "yi" | "ji" | "dv" | "ps" =>
                LayoutDirection::Rtl,
            _ => LayoutDirection::Ltr,
        }
    }
}
