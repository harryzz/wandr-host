use crate::bindings::my::skiko_gfx::thermal::{Host, Kind, Temperature, Throttle};

// ── Binder path (Android, stable AIDL HAL) ───────────────────────────────────
//
// Reaches android.hardware.thermal.IThermal/default via libbinder_ndk.
// Stable AIDL since Android 13. SELinux on stock devices typically denies
// untrusted_app → hal_thermal_default; setenforce 0 for dev.
//
// Read-only WIT: getTemperatures + getTemperaturesWithType. Listener APIs
// (registerThermalChangedCallback) are deferred — they'd require Bn-side
// callback infrastructure similar to vibrator's NopCallback that was
// never finished.

#[cfg(target_os = "android")]
mod binder_path {
    use crate::binder_aidl::android::hardware::thermal::{
        IThermal::IThermal,
        TemperatureType::TemperatureType   as AidlTempType,
        ThrottlingSeverity::ThrottlingSeverity as AidlThrottle,
    };
    use std::sync::OnceLock;

    static SVC: OnceLock<Option<rsbinder::Strong<dyn IThermal>>> = OnceLock::new();

    fn service() -> Option<&'static rsbinder::Strong<dyn IThermal>> {
        SVC.get_or_init(|| {
            rsbinder::hub::get_interface::<dyn IThermal>(
                "android.hardware.thermal.IThermal/default"
            ).ok()
        }).as_ref()
    }

    // WIT Kind → AIDL TemperatureType (i32 value). Only the WIT-exposed
    // subset; other AIDL TemperatureType values (USB_PORT, BCL_*, TPU,
    // FLASHLIGHT, POGO) have no WIT representation.
    fn wit_kind_to_aidl(k: super::Kind) -> AidlTempType {
        AidlTempType(match k {
            super::Kind::Cpu      => 0,
            super::Kind::Gpu      => 1,
            super::Kind::Battery  => 2,
            super::Kind::Skin     => 3,
            super::Kind::Modem    => 12,
            super::Kind::Npu      => 9,
            super::Kind::Display  => 11,
            super::Kind::Soc      => 13,
            super::Kind::Wifi     => 14,
            super::Kind::Camera   => 15,
            super::Kind::Speaker  => 17,
            super::Kind::Ambient  => 18,
        })
    }

    // Inverse: AIDL TemperatureType → Option<WIT Kind>. Returns None for
    // types we don't expose; the calling code filters those samples out.
    fn aidl_to_wit_kind(t: AidlTempType) -> Option<super::Kind> {
        Some(match t.0 {
            0  => super::Kind::Cpu,
            1  => super::Kind::Gpu,
            2  => super::Kind::Battery,
            3  => super::Kind::Skin,
            9  => super::Kind::Npu,
            11 => super::Kind::Display,
            12 => super::Kind::Modem,
            13 => super::Kind::Soc,
            14 => super::Kind::Wifi,
            15 => super::Kind::Camera,
            17 => super::Kind::Speaker,
            18 => super::Kind::Ambient,
            _  => return None,
        })
    }

    fn aidl_throttle_to_wit(s: AidlThrottle) -> super::Throttle {
        match s.0 {
            0 => super::Throttle::None,
            1 => super::Throttle::Light,
            2 => super::Throttle::Moderate,
            3 => super::Throttle::Severe,
            4 => super::Throttle::Critical,
            5 => super::Throttle::Emergency,
            6 => super::Throttle::Shutdown,
            _ => super::Throttle::None,  // unknown → assume normal
        }
    }

    fn map_temps(raw: rsbinder::status::Result<Vec<crate::binder_aidl::android::hardware::thermal::Temperature::Temperature>>) -> Vec<super::Temperature> {
        let Ok(temps) = raw else { return Vec::new() };
        temps.into_iter()
            .filter_map(|t| {
                aidl_to_wit_kind(t.r#type).map(|kind| super::Temperature {
                    kind,
                    celsius:  t.r#value,
                    throttle: aidl_throttle_to_wit(t.r#throttlingStatus),
                })
            })
            .collect()
    }

    pub fn list_temperatures() -> Vec<super::Temperature> {
        let Some(svc) = service() else { return Vec::new() };
        map_temps(svc.r#getTemperatures())
    }

    pub fn list_temperatures_of(kind: super::Kind) -> Vec<super::Temperature> {
        let Some(svc) = service() else { return Vec::new() };
        map_temps(svc.r#getTemperaturesWithType(wit_kind_to_aidl(kind)))
    }

    pub fn overall_throttle() -> super::Throttle {
        let Some(svc) = service() else { return super::Throttle::None };
        let Ok(temps) = svc.r#getTemperatures() else { return super::Throttle::None };
        // Max severity across all sensors. ThrottlingSeverity.0 ordering
        // matches severity (NONE=0 .. SHUTDOWN=6) so a simple max() works.
        let max = temps.iter().map(|t| t.r#throttlingStatus.0).max().unwrap_or(0);
        aidl_throttle_to_wit(AidlThrottle(max))
    }
}

impl Host for crate::HostState {
    fn list_temperatures(&mut self) -> Vec<Temperature> {
        #[cfg(target_os = "android")]
        { return binder_path::list_temperatures(); }
        #[cfg(not(target_os = "android"))]
        { Vec::new() }
    }

    fn list_temperatures_of(&mut self, kind: Kind) -> Vec<Temperature> {
        #[cfg(target_os = "android")]
        { return binder_path::list_temperatures_of(kind); }
        #[cfg(not(target_os = "android"))]
        { let _ = kind; Vec::new() }
    }

    fn overall_throttle(&mut self) -> Throttle {
        #[cfg(target_os = "android")]
        { return binder_path::overall_throttle(); }
        #[cfg(not(target_os = "android"))]
        { Throttle::None }
    }
}
