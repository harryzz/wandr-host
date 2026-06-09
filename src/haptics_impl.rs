use crate::bindings::my::skiko_gfx::haptics::{Feedback, Host};
use std::fs;
use std::path::Path;

// ── Binder path (Android, stable AIDL HAL) ───────────────────────────────────
//
// Reaches /vendor/bin/hw/android.hardware.vibrator-service via libbinder_ndk.
// The service is registered as "android.hardware.vibrator.IVibrator/default"
// in servicemanager on Android 11+. SELinux on stock devices may block this
// from an untrusted_app domain; `setenforce 0` is required during dev.
//
// `on()` and `perform()` in AOSP IVibrator.aidl take a `@nullable
// IVibratorCallback`. Older HALs (Pixel 2 XL, etc.) return
// `getCapabilities() & CAP_*_CALLBACK = 0` and **require** the callback
// argument to be null; passing a real binder gets EX_UNSUPPORTED. Newer
// HALs accept either.
//
// rsbinder-aidl 0.7.0 doesn't translate @nullable to `Option<&Strong>` —
// the generated proxy methods take `&Strong<dyn IVibratorCallback>`,
// non-null only. We therefore bypass the generated proxy entirely for
// these two methods and write the parcel by hand, using
// `Option::<Strong>::None` (which rsbinder's blanket Serialize impl
// writes as a null binder reference). Works on every HAL regardless of
// CAP_*_CALLBACK because we never want completion notifications.

#[cfg(target_os = "android")]
mod binder_path {
    use crate::binder_aidl::android::hardware::vibrator::{
        Effect::Effect, EffectStrength::EffectStrength,
        IVibrator::IVibrator,
        IVibratorCallback::IVibratorCallback,
    };
    use std::sync::OnceLock;

    // IVibrator transaction codes — order from r48 IVibrator.aidl method
    // declarations: getCapabilities, off, on, perform, ...
    const TXN_ON:      rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 2;
    const TXN_PERFORM: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 3;

    static VIB: OnceLock<Option<rsbinder::Strong<dyn IVibrator>>> = OnceLock::new();

    fn service() -> Option<&'static rsbinder::Strong<dyn IVibrator>> {
        VIB.get_or_init(|| {
            rsbinder::hub::get_interface::<dyn IVibrator>(
                "android.hardware.vibrator.IVibrator/default"
            ).ok()
        }).as_ref()
    }

    /// Send IVibrator.on(timeout_ms, null) directly as a binder
    /// transaction, bypassing the generated proxy (which can't pass
    /// null in rsbinder-aidl 0.7.0).
    fn transact_on(svc: &rsbinder::Strong<dyn IVibrator>, ms: i32) -> bool {
        let binder = svc.as_binder();
        let proxy = match binder.as_proxy() {
            Some(p) => p,
            None => { log::warn!("haptics: svc binder is not a proxy"); return false; }
        };
        let mut data = match proxy.prepare_transact(true) {
            Ok(d) => d,
            Err(e) => { log::warn!("haptics: prepare_transact: {e:?}"); return false; }
        };
        if let Err(e) = data.write(&ms) {
            log::warn!("haptics: parcel write timeout: {e:?}"); return false;
        }
        let null_cb: Option<rsbinder::Strong<dyn IVibratorCallback>> = None;
        if let Err(e) = data.write(&null_cb) {
            log::warn!("haptics: parcel write null callback: {e:?}"); return false;
        }
        match proxy.submit_transact(TXN_ON, &data, 0) {
            Ok(_)  => { log::info!("haptics: IVibrator.on({ms}ms, null) → ok"); true }
            Err(e) => { log::warn!("haptics: IVibrator.on({ms}ms, null) err={e:?}"); false }
        }
    }

    /// Send IVibrator.perform(effect, strength, null) directly.
    fn transact_perform(svc: &rsbinder::Strong<dyn IVibrator>, e: Effect, s: EffectStrength) -> bool {
        let binder = svc.as_binder();
        let proxy = match binder.as_proxy() {
            Some(p) => p,
            None => return false,
        };
        let mut data = match proxy.prepare_transact(true) {
            Ok(d) => d, Err(_) => return false,
        };
        if data.write(&e).is_err() { return false; }
        if data.write(&s).is_err() { return false; }
        let null_cb: Option<rsbinder::Strong<dyn IVibratorCallback>> = None;
        if data.write(&null_cb).is_err() { return false; }
        match proxy.submit_transact(TXN_PERFORM, &data, 0) {
            Ok(_)  => { log::info!("haptics: IVibrator.perform({e:?},{s:?},null) → ok"); true }
            Err(err) => {
                log::warn!("haptics: IVibrator.perform({e:?},{s:?},null) err={err:?}");
                false
            }
        }
    }

    pub fn vibrate_ms(ms: u32) -> bool {
        let svc = match service() {
            Some(s) => s,
            None => { log::warn!("haptics: vibrator service not available"); return false; }
        };
        transact_on(svc, ms as i32)
    }

    pub fn perform(f: super::Feedback) -> bool {
        let svc = match service() {
            Some(s) => s,
            None => return false,
        };
        let (effect, strength) = map_feedback(f);
        if transact_perform(svc, effect, strength) {
            return true;
        }
        // Effect unsupported on this device — try a raw timed vibration.
        transact_on(svc, super::feedback_duration(f) as i32)
    }

    fn map_feedback(f: super::Feedback) -> (Effect, EffectStrength) {
        // Mirrors the framework's HapticFeedbackConstants → VibrationEffect
        // mapping in services/core/java/com/android/server/vibrator/.
        match f {
            super::Feedback::Tap         => (Effect::TICK,         EffectStrength::LIGHT),
            super::Feedback::VirtualKey  => (Effect::TICK,         EffectStrength::MEDIUM),
            super::Feedback::Click       => (Effect::CLICK,        EffectStrength::MEDIUM),
            super::Feedback::LongPress   => (Effect::HEAVY_CLICK,  EffectStrength::STRONG),
            super::Feedback::DoubleClick => (Effect::DOUBLE_CLICK, EffectStrength::MEDIUM),
        }
    }
}

// ── Sysfs fallback path ──────────────────────────────────────────────────────
//
// On Android the binder path covers the common case. Sysfs is kept for two
// niche scenarios: (1) custom rooted ROMs that have sysfs vibrator nodes but
// no AIDL HAL registered, (2) non-Android Linux devices where we cross-build
// the host. Both writes require write access to the nodes; on most devices
// EACCES, in which case we return false and the caller sees no buzz.

fn try_vibrate_sysfs(ms: u32) -> bool {
    let ms_str = ms.to_string();

    let legacy = Path::new("/sys/class/timed_output/vibrator/enable");
    if legacy.exists() {
        if fs::write(legacy, &ms_str).is_ok() {
            return true;
        }
    }

    let leds_dir = Path::new("/sys/class/leds/vibrator");
    if leds_dir.exists() {
        let dur = leds_dir.join("duration");
        let act = leds_dir.join("activate");
        if fs::write(&dur, &ms_str).is_ok() && fs::write(&act, "1").is_ok() {
            return true;
        }
    }

    false
}

fn feedback_duration(f: Feedback) -> u32 {
    match f {
        Feedback::Tap         => 10,
        Feedback::VirtualKey  => 10,
        Feedback::Click       => 10,
        Feedback::LongPress   => 40,
        Feedback::DoubleClick => 20,
    }
}

/// Host-internal vibrate (for the ringer) — same path as the WIT `vibrate_ms`:
/// vibrator HAL first, then the sysfs fallback. Free fn so a background thread can
/// call it without the `Host` trait (`&mut HostState`).
pub fn vibrate_ms(duration_ms: u32) -> bool {
    let clamped = duration_ms.clamp(1, 1000);
    #[cfg(target_os = "android")]
    if binder_path::vibrate_ms(clamped) {
        return true;
    }
    try_vibrate_sysfs(clamped)
}

impl Host for crate::HostState {
    fn perform(&mut self, feedback: Feedback) -> bool {
        #[cfg(target_os = "android")]
        if binder_path::perform(feedback) { return true; }
        try_vibrate_sysfs(feedback_duration(feedback))
    }

    fn vibrate_ms(&mut self, duration_ms: u32) -> bool {
        let clamped = duration_ms.clamp(1, 1000);
        #[cfg(target_os = "android")]
        if binder_path::vibrate_ms(clamped) { return true; }
        try_vibrate_sysfs(clamped)
    }
}
