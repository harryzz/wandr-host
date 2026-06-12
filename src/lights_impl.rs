use crate::device_bindings::wandr::device::lights::{FlashMode, Host, LightState, LightType};

// ── Binder path (Android, stable AIDL HAL) ───────────────────────────────────
//
// Reaches /vendor/bin/hw/android.hardware.light-service via libbinder_ndk.
// Service is registered as "android.hardware.light.ILights/default" on
// Android 11+. SELinux on stock devices commonly blocks untrusted_app from
// talking to hal_light_default — set() degrades to returning false in
// that case. Full production access waits for the boot-model work in
// post-art-roadmap §6.1.

#[cfg(target_os = "android")]
mod binder_path {
    use crate::binder_aidl::android::hardware::light::{
        FlashMode::FlashMode  as AidlFlashMode,
        HwLightState::HwLightState,
        ILights::ILights,
        LightType::LightType  as AidlLightType,
    };
    use std::sync::OnceLock;

    static SVC: OnceLock<Option<rsbinder::Strong<dyn ILights>>> = OnceLock::new();

    fn service() -> Option<&'static rsbinder::Strong<dyn ILights>> {
        SVC.get_or_init(|| {
            rsbinder::hub::get_interface::<dyn ILights>(
                "android.hardware.light.ILights/default"
            ).ok()
        }).as_ref()
    }

    fn wit_to_aidl_type(t: super::LightType) -> AidlLightType {
        // WIT light-type ordinals are designed to match AIDL LightType.
        // Cast preserves the value; the i8 width on the AIDL side is
        // wrapped by the newtype struct LightType(pub i8).
        AidlLightType(match t {
            super::LightType::Backlight     => 0,
            super::LightType::Keyboard      => 1,
            super::LightType::Buttons       => 2,
            super::LightType::Battery       => 3,
            super::LightType::Notifications => 4,
            super::LightType::Attention     => 5,
            super::LightType::Bluetooth     => 6,
            super::LightType::Wifi          => 7,
            super::LightType::Microphone    => 8,
        })
    }

    fn wit_to_aidl_flash(m: super::FlashMode) -> AidlFlashMode {
        AidlFlashMode(match m {
            super::FlashMode::None     => 0,
            super::FlashMode::Timed    => 1,
            super::FlashMode::Hardware => 2,
        })
    }

    pub fn set(kind: super::LightType, state: super::LightState) -> bool {
        let Some(svc) = service() else { return false };
        let lights = match svc.r#getLights() { Ok(v) => v, Err(_) => return false };
        let target = wit_to_aidl_type(kind);
        let hw_state = HwLightState {
            r#color:          state.color_argb as i32,
            r#flashMode:      wit_to_aidl_flash(state.flash_mode),
            r#flashOnMs:      state.flash_on_ms  as i32,
            r#flashOffMs:     state.flash_off_ms as i32,
            // BrightnessMode is device-specific (USER / SENSOR / LOW_PERSISTENCE).
            // USER (0) means honor whatever color/brightness we set; the others
            // let the device override. None of our WIT consumers care, so default.
            r#brightnessMode: Default::default(),
        };
        let mut any = false;
        for hw in lights.iter().filter(|h| h.r#type == target) {
            if svc.r#setLightState(hw.r#id, &hw_state).is_ok() {
                any = true;
            }
        }
        any
    }

    pub fn supports(kind: super::LightType) -> bool {
        let Some(svc) = service() else { return false };
        let lights = match svc.r#getLights() { Ok(v) => v, Err(_) => return false };
        let target = wit_to_aidl_type(kind);
        lights.iter().any(|h| h.r#type == target)
    }
}

impl Host for crate::HostState {
    fn set(&mut self, kind: LightType, state: LightState) -> bool {
        #[cfg(target_os = "android")]
        { return binder_path::set(kind, state); }
        #[cfg(not(target_os = "android"))]
        { let _ = (kind, state); false }
    }

    fn supports(&mut self, kind: LightType) -> bool {
        #[cfg(target_os = "android")]
        { return binder_path::supports(kind); }
        #[cfg(not(target_os = "android"))]
        { let _ = kind; false }
    }
}

#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
fn _unused_imports_silencer(s: LightState, m: FlashMode) {
    // Keeps the imports of LightState/FlashMode/etc from triggering unused
    // warnings on the desktop target where the binder path is gone.
    let _ = (s, m);
}
