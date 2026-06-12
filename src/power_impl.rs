use crate::device_bindings::wandr::device::power::{Hint, Host, Mode};

// ── Binder path (Android, stable AIDL HAL) ───────────────────────────────────
//
// Reaches android.hardware.power.IPower/default via libbinder_ndk. The HAL
// is mandatory from Android 13+; older Android typically ships the legacy
// HIDL power@1.x HAL which we don't reach. SELinux on stock devices
// commonly denies untrusted_app → hal_power_default; setenforce 0 needed
// for dev. Production access waits for roadmap §6.1 boot-model work.
//
// IPower's setBoost/setMode are declared `oneway` in AIDL — fire and
// forget. Our generated client methods still return Result for transport
// errors. isBoostSupported/isModeSupported are blocking queries.

#[cfg(target_os = "android")]
mod binder_path {
    use crate::binder_aidl::android::hardware::power::{
        Boost::Boost     as AidlBoost,
        Mode::Mode       as AidlMode,
        IPower::IPower,
    };
    use std::sync::OnceLock;

    static SVC: OnceLock<Option<rsbinder::Strong<dyn IPower>>> = OnceLock::new();

    fn service() -> Option<&'static rsbinder::Strong<dyn IPower>> {
        SVC.get_or_init(|| {
            rsbinder::hub::get_interface::<dyn IPower>(
                "android.hardware.power.IPower/default"
            ).ok()
        }).as_ref()
    }

    fn wit_hint_to_boost(h: super::Hint) -> AidlBoost {
        AidlBoost(match h {
            super::Hint::Interaction           => 0,  // INTERACTION
            super::Hint::DisplayUpdateImminent => 1,  // DISPLAY_UPDATE_IMMINENT
        })
    }

    fn wit_mode_to_mode(m: super::Mode) -> AidlMode {
        AidlMode(match m {
            super::Mode::LowPower             => 1,   // LOW_POWER
            super::Mode::SustainedPerformance => 2,   // SUSTAINED_PERFORMANCE
            super::Mode::FixedPerformance     => 3,   // FIXED_PERFORMANCE
            super::Mode::ExpensiveRendering   => 6,   // EXPENSIVE_RENDERING
            super::Mode::Game                 => 15,  // GAME
            super::Mode::Interactive          => 7,   // INTERACTIVE
        })
    }

    pub fn boost(h: super::Hint, duration_ms: u32) {
        let Some(svc) = service() else { return };
        let _ = svc.r#setBoost(wit_hint_to_boost(h), duration_ms as i32);
    }

    pub fn set_mode(m: super::Mode, enabled: bool) {
        let Some(svc) = service() else { return };
        let _ = svc.r#setMode(wit_mode_to_mode(m), enabled);
    }

    pub fn is_hint_supported(h: super::Hint) -> bool {
        let Some(svc) = service() else { return false };
        svc.r#isBoostSupported(wit_hint_to_boost(h)).unwrap_or(false)
    }

    pub fn is_mode_supported(m: super::Mode) -> bool {
        let Some(svc) = service() else { return false };
        svc.r#isModeSupported(wit_mode_to_mode(m)).unwrap_or(false)
    }
}

impl Host for crate::HostState {
    fn boost(&mut self, kind: Hint, duration_ms: u32) {
        #[cfg(target_os = "android")]
        binder_path::boost(kind, duration_ms);
        #[cfg(not(target_os = "android"))]
        { let _ = (kind, duration_ms); }
    }

    fn set_mode(&mut self, kind: Mode, enabled: bool) {
        #[cfg(target_os = "android")]
        binder_path::set_mode(kind, enabled);
        #[cfg(not(target_os = "android"))]
        { let _ = (kind, enabled); }
    }

    fn is_hint_supported(&mut self, kind: Hint) -> bool {
        #[cfg(target_os = "android")]
        { return binder_path::is_hint_supported(kind); }
        #[cfg(not(target_os = "android"))]
        { let _ = kind; false }
    }

    fn is_mode_supported(&mut self, kind: Mode) -> bool {
        #[cfg(target_os = "android")]
        { return binder_path::is_mode_supported(kind); }
        #[cfg(not(target_os = "android"))]
        { let _ = kind; false }
    }
}
