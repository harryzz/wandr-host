//! Call-audio control via `media.audio_policy` (`IAudioPolicyService`).
//!
//! AAudioService brokers routing for normal in/out, but VoIP / telephony call
//! audio needs two extra knobs AAudio doesn't expose:
//!   - `setPhoneState(IN_COMMUNICATION)` — switch the platform into comms mode
//!     (routing + AEC tuning) for the duration of a call.
//!   - `setForceUse(COMMUNICATION, SPEAKER|NONE|BT_SCO)` — the speaker / earpiece
//!     / Bluetooth toggle.
//!
//! Both take plain int-backed enums, so we bind a *positional stub* of the
//! interface (only 4 of its 107 methods kept real — see
//! `vendor/aidl-stubs/android/media/IAudioPolicyService.aidl`). These calls are
//! normally gated by MODIFY_AUDIO_ROUTING / MODIFY_PHONE_STATE + a privileged
//! SELinux domain; this module is the de-risk probe answering "does a root/su
//! wandr caller reach them?" before any call feature is built.

#[cfg(target_os = "android")]
mod binder_path {
    use crate::binder_aidl::android::media::{
        AudioPolicyForceUse::AudioPolicyForceUse,
        AudioPolicyForcedConfig::AudioPolicyForcedConfig,
        IAudioPolicyService::IAudioPolicyService,
    };
    use crate::binder_aidl::android::media::audio::common::AudioMode::AudioMode;
    use crate::binder_aidl::android::media::{
        AudioPortFw::AudioPortFw,
        AudioPortRole::AudioPortRole,
        AudioPortType::AudioPortType,
        DeviceRole::DeviceRole,
    };
    use crate::binder_aidl::android::media::audio::common::AudioDevice::AudioDevice;
    use crate::binder_aidl::android::media::audio::common::Int::Int;
    use crate::binder_aidl::android::media::audio::common::AudioPortExt::AudioPortExt;
    use crate::binder_aidl::android::media::audio::common::{
        AudioAttributes::AudioAttributes,
        AudioContentType::AudioContentType,
        AudioDeviceDescription::AudioDeviceDescription,
        AudioDeviceType::AudioDeviceType,
        AudioSource::AudioSource,
        AudioStreamType::AudioStreamType,
        AudioUsage::AudioUsage,
    };

    fn service() -> Option<rsbinder::Strong<dyn IAudioPolicyService>> {
        match rsbinder::hub::get_interface::<dyn IAudioPolicyService>("media.audio_policy") {
            Ok(s)  => { log::info!("audio-policy: media.audio_policy ready"); Some(s) }
            Err(e) => { log::warn!("audio-policy: media.audio_policy unavailable: {e:?}"); None }
        }
    }

    fn mode_name(m: AudioMode) -> &'static str {
        match m {
            AudioMode::NORMAL           => "NORMAL",
            AudioMode::RINGTONE         => "RINGTONE",
            AudioMode::IN_CALL          => "IN_CALL",
            AudioMode::IN_COMMUNICATION => "IN_COMMUNICATION",
            AudioMode::CALL_SCREEN      => "CALL_SCREEN",
            _                           => "other",
        }
    }
    fn cfg_name(c: AudioPolicyForcedConfig) -> &'static str {
        match c {
            AudioPolicyForcedConfig::NONE    => "NONE(earpiece/default)",
            AudioPolicyForcedConfig::SPEAKER => "SPEAKER",
            AudioPolicyForcedConfig::BT_SCO  => "BT_SCO",
            _                                => "other",
        }
    }

    // The originator uid for setPhoneState (the mode owner the policy service
    // tracks). We run as root; report our real uid.
    extern "C" { fn getuid() -> u32; }

    /// Replicates `AudioService.onUpdateAudioMode` (frameworks/base
    /// .../server/audio/AudioService.java:6607) — the call-owner host applies this
    /// when the arbiter starts/ends a comms session. The Java method does three
    /// things in order; we do the two that aren't already covered elsewhere:
    ///   1. `setPhoneState(mode, uid)`            → [`set_phone_state`]
    ///   2. re-apply volume for the new mode      → [`on_update_contextual_volumes`]
    ///   3. route the comms device (`setPreferredDevicesForStrategy`) — wandr's
    ///      equivalent is [`set_route`] (`setForceUse(COMMUNICATION)`) + the guest's
    ///      per-stream `deviceIds` pin, applied via the arbiter's `audio-route`.
    /// `comm=true` → IN_COMMUNICATION; `false` → NORMAL.
    ///
    /// ‼️ Step 1 (`setPhoneState`) is DISABLED by default. On this device under
    /// `--no-art` (no SIM / no telephony), `setPhoneState(IN_COMMUNICATION)` drives the
    /// vendor audio HAL's `setMode` into the voice-call audio path, which doesn't exist
    /// → `DeviceHalHidl::setMode` HANGS → audioserver's TimeCheck watchdog (5s) SIGABRTs
    /// audioserver → `media.audio_policy` dies → the call goes silent on EVERY route
    /// (and respawns with un-initialized volumes). The earpiece route does NOT need the
    /// mode switch: it comes from `setForceUse(COMMUNICATION)` + the per-stream
    /// `deviceIds` pin (`set_route`/`set_comms_route`, the `CommRoute` applier). So we
    /// skip step 1 and keep only step 2 (the volume re-assert — harmless full-scale
    /// MUSIC in NORMAL mode). Opt back in with `WANDR_AUDIO_SETPHONESTATE=1` on a device
    /// that actually has a working telephony audio path. See
    /// [[project_call_audioserver_crash]] (supersedes [[project_artless_call_audio]]).
    pub fn on_update_audio_mode(comm: bool) {
        if std::env::var_os("WANDR_AUDIO_SETPHONESTATE").is_some() {
            set_phone_state(comm);
        }
        on_update_contextual_volumes(comm);
    }

    /// `AudioSystem.setPhoneState(mode, uid)` — the global audio-mode switch
    /// (`AudioService.onUpdateAudioMode` step 1).
    fn set_phone_state(comm: bool) {
        let Some(svc) = service() else { return };
        let state = if comm { AudioMode::IN_COMMUNICATION } else { AudioMode::NORMAL };
        let uid = unsafe { getuid() } as i32;
        match svc.r#setPhoneState(state, uid) {
            Ok(())  => log::info!("audio-policy: setPhoneState {} (uid={uid})", mode_name(state)),
            Err(e)  => log::warn!("audio-policy: setPhoneState {} failed: {e:?}", mode_name(state)),
        }
    }

    /// Slice of `AudioService.onUpdateContextualVolumes` (AudioService.java:6665):
    /// "change of mode may require volume to be re-applied on some devices." For
    /// wandr's call (USAGE_MEDIA → MUSIC stream), entering IN_COMMUNICATION ducks
    /// MUSIC to ~1% (task 75); re-asserting its index on the comms output devices
    /// (earpiece + speaker) after the mode flip restores it. Same
    /// `setStreamVolumeIndex` API + full-scale value `init_audio_policy` uses for
    /// MUSIC. No-op on NORMAL (the boot-init levels already apply).
    fn on_update_contextual_volumes(comm: bool) {
        if !comm {
            return;
        }
        let Some(svc) = service() else { return };
        const STREAM_MUSIC: i32 = 3; // wandr's call output is USAGE_MEDIA → MUSIC.
        const MUSIC_MAX_INDEX: i32 = 15; // matches init_audio_policy's full-scale MUSIC.
        let stream = AudioStreamType(STREAM_MUSIC);
        for d in [AudioDeviceType::OUT_SPEAKER_EARPIECE, AudioDeviceType::OUT_SPEAKER] {
            if let Err(e) = svc.r#setStreamVolumeIndex(stream, &dev_desc(d), MUSIC_MAX_INDEX, false) {
                log::warn!("audio-policy: onUpdateContextualVolumes reapply MUSIC on {d:?} failed: {e:?}");
            }
        }
        log::info!("audio-policy: onUpdateContextualVolumes — re-asserted MUSIC full-scale (comms)");
    }

    /// wandr-arbiter-audio M3 — set the communication routing (the speaker /
    /// earpiece toggle). `speaker=true` → SPEAKER; `false` → NONE (earpiece).
    pub fn set_route(speaker: bool) {
        let Some(svc) = service() else { return };
        let cfg = if speaker { AudioPolicyForcedConfig::SPEAKER } else { AudioPolicyForcedConfig::NONE };
        match svc.r#setForceUse(AudioPolicyForceUse::COMMUNICATION, cfg) {
            Ok(())  => log::info!("audio-policy: setForceUse COMMUNICATION {}", cfg_name(cfg)),
            Err(e)  => log::warn!("audio-policy: setForceUse {} failed: {e:?}", cfg_name(cfg)),
        }
    }

    /// Re-route the MEDIA product strategy (our call/media output rides USAGE_MEDIA)
    /// to the earpiece or speaker by setting a PREFERRED device-role on the strategy.
    ///
    /// This is the correct lever for the call earpiece↔speaker toggle (task 97
    /// bug #5). The two alternatives both fail on this device:
    ///   • `setForceUse` has no earpiece option for the MEDIA strategy (only
    ///     headphones/speaker/none) — it cannot move USAGE_MEDIA to the receiver.
    ///   • A per-stream `deviceIds` pin forces AAudio to open a SECOND MMAP "direct
    ///     output" on the pinned device; the `mmap_no_irq_out` profile is
    ///     `maxOpenCount=1`, so when another output already holds it (e.g. on the
    ///     speaker) the earpiece pin returns `-889` (AAUDIO_ERROR_UNAVAILABLE).
    /// `setDevicesRoleForStrategy` instead RE-ROUTES the existing shared output, so
    /// it never opens a second endpoint and works mid-call without re-opening the
    /// stream. The strategy id is resolved at runtime from the MEDIA attributes
    /// (`getProductStrategyFromAudioAttributes`) — no hard-coded strategy number.
    /// Returns true on success. See [[project_audio_routing_arbiter]].
    pub fn set_media_strategy_route(speaker: bool) -> bool {
        let Some(svc) = service() else { return false };
        let strategy = match svc.r#getProductStrategyFromAudioAttributes(&media_attr(), true) {
            Ok(s)  => s,
            Err(e) => { log::warn!("audio-route: getProductStrategyFromAudioAttributes err: {e:?}"); return false; }
        };
        let dev = if speaker { AudioDeviceType::OUT_SPEAKER } else { AudioDeviceType::OUT_SPEAKER_EARPIECE };
        let device = AudioDevice { r#type: dev_desc(dev), r#address: Default::default() };
        match svc.r#setDevicesRoleForStrategy(strategy, DeviceRole::PREFERRED, &[device]) {
            Ok(())  => { log::info!("audio-route: strategy {strategy} PREFERRED -> {} ({dev:?})",
                            if speaker { "speaker" } else { "earpiece" }); true }
            Err(e)  => { log::warn!("audio-route: setDevicesRoleForStrategy({dev:?}) err: {e:?}"); false }
        }
    }

    /// Clear the MEDIA-strategy PREFERRED device-role set by
    /// [`set_media_strategy_route`] — media returns to the policy default
    /// (speaker). Call on call-end so non-call media isn't stuck on the earpiece.
    pub fn clear_media_strategy_route() {
        let Some(svc) = service() else { return };
        let Ok(strategy) = svc.r#getProductStrategyFromAudioAttributes(&media_attr(), true) else { return };
        match svc.r#clearDevicesRoleForStrategy(strategy, DeviceRole::PREFERRED) {
            Ok(())  => log::info!("audio-route: cleared PREFERRED device-role for strategy {strategy}"),
            Err(e)  => log::warn!("audio-route: clearDevicesRoleForStrategy err: {e:?}"),
        }
    }

    /// Read where the MEDIA strategy currently routes (`getDevicesForAttributes`)
    /// — used by the route-toggle probe to confirm a re-route actually moved the
    /// output. Returns the device-type debug strings.
    pub fn media_route_devices() -> Vec<String> {
        let Some(svc) = service() else { return Vec::new() };
        match svc.r#getDevicesForAttributes(&media_attr(), false) {
            Ok(devs) => devs.iter().map(|d| format!("{:?}", d.r#type.r#type)).collect(),
            Err(_)   => Vec::new(),
        }
    }

    /// Replicate the boot-time audio init that `AudioService.java` normally does in
    /// `system_server` — which is dead under `--no-art`. Without it the policy
    /// service reports a volume index range of `-1` (`initStreamVolume` never ran)
    /// and every stream sits at `-inf dB`, so nothing is audible even though the
    /// stream opens and routes. This is the native (Rust) stand-in for
    /// `AudioService.onReinitVolumes()` + the boot mode/force-use defaults:
    /// for each public stream, set its index range (`initStreamVolume`, values
    /// copied verbatim from `AudioService.MIN_/MAX_STREAM_VOLUME`) and seed a
    /// per-device index (`setStreamVolumeIndex`), then set NORMAL phone state and
    /// clear the comms force-route. Idempotent — safe to re-run after an
    /// `audioserver` restart (the `onAudioServerDied` recovery AudioService does).
    pub fn init_audio_policy() {
        if let Err(e) = crate::binder::init() {
            log::warn!("audio-init: binder init failed: {e}");
            return;
        }
        let Some(svc) = service() else {
            log::warn!("audio-init: media.audio_policy unavailable — skipping");
            return;
        };
        // AudioStreamType value = array index. Order: VOICE_CALL, SYSTEM, RING,
        // MUSIC, ALARM, NOTIFICATION, BLUETOOTH_SCO, ENFORCED_AUDIBLE, DTMF, TTS,
        // ACCESSIBILITY, ASSISTANT (the 12 public streams AudioService inits).
        const MIN: [i32; 12] = [1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0];
        const MAX: [i32; 12] = [5, 7, 7, 15, 7, 7, 15, 7, 15, 15, 15, 15];
        // Output devices to seed. OUT_DEFAULT is the generic fallback APM applies
        // to any device without a specific index; the rest are the built-in /
        // common wired outputs so the index is right whichever one is selected.
        let devices = [
            AudioDeviceType::OUT_DEFAULT,
            AudioDeviceType::OUT_SPEAKER,
            AudioDeviceType::OUT_SPEAKER_EARPIECE,
            AudioDeviceType::OUT_HEADPHONE,
            AudioDeviceType::OUT_HEADSET,
        ];
        let mut ok = 0;
        for s in 0..12i32 {
            let stream = AudioStreamType(s);
            let (min, max) = (MIN[s as usize], MAX[s as usize]);
            if let Err(e) = svc.r#initStreamVolume(stream, min, max) {
                log::warn!("audio-init: initStreamVolume stream={s} ({min}..{max}) failed: {e:?}");
                continue;
            }
            // No settings store under --no-art, so pick a sensible default: full
            // scale for MUSIC (so media/tones are loud) and ~80% of range for the
            // rest. The user can still adjust via the volume keys, which now work
            // (the index range is no longer -1).
            let idx = if s == 3 { max } else { min + (max - min) * 4 / 5 };
            for d in devices {
                let _ = svc.r#setStreamVolumeIndex(stream, &dev_desc(d), idx, false);
            }
            ok += 1;
        }
        log::info!("audio-init: initStreamVolume + setStreamVolumeIndex done for {ok}/12 streams");

        // Boot mode/route defaults: NORMAL phone state, no forced comms route.
        let uid = unsafe { getuid() } as i32;
        match svc.r#setPhoneState(AudioMode::NORMAL, uid) {
            Ok(())  => log::info!("audio-init: setPhoneState NORMAL (uid={uid})"),
            Err(e)  => log::warn!("audio-init: setPhoneState NORMAL failed: {e:?}"),
        }
        if let Err(e) = svc.r#setForceUse(AudioPolicyForceUse::COMMUNICATION, AudioPolicyForcedConfig::NONE) {
            log::warn!("audio-init: setForceUse COMMUNICATION NONE failed: {e:?}");
        }
    }

    /// Task-76 read-only probe: ask the policy service where each usage would
    /// route RIGHT NOW (`getDevicesForAttributes`, transaction index 25). This
    /// is the binder equivalent of the routing the refactor will consult at
    /// runtime instead of shelling `dumpsys`. The `AudioAttributes` wire layout
    /// is the common (HAL) shape; if it differs from the device's framework
    /// shape the call returns an error/garbage — either way the log is
    /// API-of-record evidence ("does AudioDevice[] decode over binder?").
    /// `dumpsys media.audio_policy` strategy→device lines remain authoritative.
    pub fn probe_devices_for_attributes() {
        if let Err(e) = crate::binder::init() {
            log::warn!("audio-caps devices-for-attr: binder init failed: {e}");
            return;
        }
        let Some(svc) = service() else { return };
        let usages: &[(&str, AudioUsage, AudioContentType, AudioSource)] = &[
            ("MEDIA",               AudioUsage::MEDIA,               AudioContentType::MUSIC,   AudioSource::DEFAULT),
            ("VOICE_COMMUNICATION", AudioUsage::VOICE_COMMUNICATION, AudioContentType::SPEECH,  AudioSource::DEFAULT),
            ("NOTIFICATION",        AudioUsage::NOTIFICATION,        AudioContentType::SONIFICATION, AudioSource::DEFAULT),
            ("ALARM",               AudioUsage::ALARM,               AudioContentType::SONIFICATION, AudioSource::DEFAULT),
        ];
        for (label, usage, content, source) in usages {
            let attr = AudioAttributes {
                r#contentType: *content,
                r#usage:       *usage,
                r#source:      *source,
                r#flags:       0,
                r#tags:        Vec::new(),
                ..Default::default()
            };
            match svc.r#getDevicesForAttributes(&attr, false) {
                Ok(devs) => log::info!(
                    "audio-caps: getDevicesForAttributes({label}) -> {} device(s): {:?}",
                    devs.len(), devs,
                ),
                Err(e) => log::warn!(
                    "audio-caps: getDevicesForAttributes({label}) binder err / decode gap: {e:?}",
                ),
            }
        }
    }

    // ── Volume (task 76 P8) ──────────────────────────────────────────────────
    // The attributes-based volume API on the policy service (verified indices
    // 20-23). Volume is stored per (attributes/stream, device); the index runs
    // over a device-independent [min,max] range (media = 0..25 on this device).
    // The arbiter decides the policy (target stream, level); these are the host
    // appliers + read accessors.

    fn media_attr() -> AudioAttributes {
        AudioAttributes {
            r#contentType: AudioContentType::MUSIC,
            r#usage:       AudioUsage::MEDIA,
            r#source:      AudioSource::DEFAULT,
            r#flags:       0,
            r#tags:        Vec::new(),
            ..Default::default()
        }
    }
    fn dev_desc(t: AudioDeviceType) -> AudioDeviceDescription {
        AudioDeviceDescription { r#type: t, r#connection: String::new() }
    }

    /// Media volume range `[min, max]` (device-independent). `None` if the
    /// service is unreachable.
    pub fn media_volume_range() -> Option<(i32, i32)> {
        let svc = service()?;
        let attr = media_attr();
        let max = svc.r#getMaxVolumeIndexForAttributes(&attr).ok()?;
        let min = svc.r#getMinVolumeIndexForAttributes(&attr).ok()?;
        Some((min, max))
    }
    /// Self-heal the audio policy after an audioserver (re)start. Under `--no-art`
    /// system_server normally runs `initStreamVolume` at boot; it is dead, so a
    /// respawned audioserver comes up with the MUSIC volume range UNINITIALIZED
    /// (`min=max=-1`) → every index is invalid → the stream plays at no gain → the
    /// call is SILENT (see project_call_audioserver_crash). Cheap (one binder read);
    /// if the range is uninitialized (or the service was just unreachable), re-run
    /// `init_audio_policy`. Called on every track open so a silent call is impossible
    /// after any audioserver restart.
    pub fn ensure_initialized() {
        let needs_init = match media_volume_range() {
            Some((min, max)) => min < 0 || max < 0,
            None => true,
        };
        if needs_init {
            log::warn!("audio-init: MUSIC volume range uninitialized — self-healing (init_audio_policy)");
            init_audio_policy();
        }
    }
    /// Current media volume index on `device` (e.g. `OUT_SPEAKER`).
    pub fn get_media_volume(device: AudioDeviceType) -> Option<i32> {
        let svc = service()?;
        svc.r#getVolumeIndexForAttributes(&media_attr(), &dev_desc(device)).ok()
    }
    /// Set the media volume index on `device`, clamped to `[min, max]`. Returns
    /// the index actually applied (post-clamp), or `None` on failure.
    pub fn set_media_volume(device: AudioDeviceType, index: i32) -> Option<i32> {
        let svc = service()?;
        let (min, max) = media_volume_range().unwrap_or((0, index.max(0)));
        let idx = index.clamp(min, max);
        match svc.r#setVolumeIndexForAttributes(&media_attr(), &dev_desc(device), idx, false) {
            Ok(())  => { log::info!("audio-policy: media volume {idx} on {device:?} [{min}..{max}]"); Some(idx) }
            Err(e)  => { log::warn!("audio-policy: setVolumeIndexForAttributes err: {e:?}"); None }
        }
    }

    /// Apply a one-step MEDIA volume change on `device` (the **arbiter** picks
    /// which device — speaker or earpiece — and which host applies; this is the
    /// pure applier). `speaker` selects OUT_SPEAKER vs OUT_SPEAKER_EARPIECE. Our
    /// call audio rides the MEDIA stream (USAGE_MEDIA), so MEDIA volume is the
    /// lever for both call and media. Step ≈ 1/10 of the range (≥1).
    pub fn adjust_volume_on(speaker: bool, up: bool) {
        let device = if speaker { AudioDeviceType::OUT_SPEAKER } else { AudioDeviceType::OUT_SPEAKER_EARPIECE };
        let (min, max) = media_volume_range().unwrap_or((0, 15));
        let step = ((max - min) / 10).max(1);
        let Some(cur) = get_media_volume(device) else {
            log::warn!("audio-policy: volume — read failed");
            return;
        };
        let next = if up { cur + step } else { cur - step };
        set_media_volume(device, next);
    }

    /// Absolute output volume (`wandr:audio-focus/controls.set-volume`): clamp `level`
    /// to 0.0..=1.0 and map it onto the MEDIA stream index range for the chosen device.
    pub fn set_media_volume_level(speaker: bool, level: f32) {
        let device = if speaker { AudioDeviceType::OUT_SPEAKER } else { AudioDeviceType::OUT_SPEAKER_EARPIECE };
        let (min, max) = media_volume_range().unwrap_or((0, 15));
        let lvl = level.clamp(0.0, 1.0);
        let idx = min + (((max - min) as f32) * lvl).round() as i32;
        set_media_volume(device, idx);
    }
    /// Read the current MEDIA volume on the chosen device as 0.0..=1.0 (1.0 if unknown).
    pub fn get_media_volume_level(speaker: bool) -> f32 {
        let device = if speaker { AudioDeviceType::OUT_SPEAKER } else { AudioDeviceType::OUT_SPEAKER_EARPIECE };
        let (min, max) = media_volume_range().unwrap_or((0, 15));
        match get_media_volume(device) {
            Some(cur) if max > min => (((cur - min) as f32) / ((max - min) as f32)).clamp(0.0, 1.0),
            _ => 1.0,
        }
    }

    /// Apply output mute/unmute on `device` (arbiter-decided). Uses the policy
    /// volume setter's `muted` flag, preserving the current index so unmute
    /// restores the prior level. `speaker` selects OUT_SPEAKER vs earpiece.
    pub fn set_media_mute(speaker: bool, muted: bool) {
        let Some(svc) = service() else { return };
        let device = if speaker { AudioDeviceType::OUT_SPEAKER } else { AudioDeviceType::OUT_SPEAKER_EARPIECE };
        let attr = media_attr();
        let dev = dev_desc(device);
        let cur = svc.r#getVolumeIndexForAttributes(&attr, &dev).unwrap_or(0);
        match svc.r#setVolumeIndexForAttributes(&attr, &dev, cur, muted) {
            Ok(())  => log::info!("audio-policy: media {} on {device:?} (idx={cur})", if muted { "MUTED" } else { "unmuted" }),
            Err(e)  => log::warn!("audio-policy: setVolumeIndexForAttributes(mute) err: {e:?}"),
        }
    }

    /// Read-only-ish volume probe (`--probe-audio-volume`): reads the media
    /// range + current index on speaker & earpiece, then sets the speaker index
    /// to max, reads it back, and restores the previous value (self-restoring,
    /// like `probe_route`). Proves the write path before keys/arbiter wire it.
    pub fn probe_volume() {
        if let Err(e) = crate::binder::init() {
            log::warn!("audio-caps volume: binder init failed: {e}");
            return;
        }
        let Some(svc) = service() else { return };
        let attr = media_attr();
        log::info!("audio-caps: media volume range min={:?} max={:?}",
            svc.r#getMinVolumeIndexForAttributes(&attr),
            svc.r#getMaxVolumeIndexForAttributes(&attr));
        for (label, t) in [("speaker", AudioDeviceType::OUT_SPEAKER),
                           ("earpiece", AudioDeviceType::OUT_SPEAKER_EARPIECE)] {
            match svc.r#getVolumeIndexForAttributes(&attr, &dev_desc(t)) {
                Ok(v)  => log::info!("audio-caps: media volume on {label} = {v}"),
                Err(e) => log::warn!("audio-caps: getVolumeIndexForAttributes({label}) err: {e:?}"),
            }
        }
        let dev = dev_desc(AudioDeviceType::OUT_SPEAKER);
        let prev = match svc.r#getVolumeIndexForAttributes(&attr, &dev) {
            Ok(v)  => v,
            Err(e) => { log::warn!("audio-caps: volume read err: {e:?}"); return; }
        };
        let max = svc.r#getMaxVolumeIndexForAttributes(&attr).unwrap_or(prev);
        match svc.r#setVolumeIndexForAttributes(&attr, &dev, max, false) {
            Ok(())  => log::info!("audio-caps: set speaker media volume {prev} -> {max} — WRITE ACCESS OK"),
            Err(e)  => { log::warn!("audio-caps: set volume DENIED/err: {e:?} — perm/SELinux?"); return; }
        }
        if let Ok(v) = svc.r#getVolumeIndexForAttributes(&attr, &dev) {
            log::info!("audio-caps: confirmed speaker media volume = {v}");
        }
        match svc.r#setVolumeIndexForAttributes(&attr, &dev, prev, false) {
            Ok(())  => log::info!("audio-caps: restored speaker media volume to {prev}"),
            Err(e)  => log::warn!("audio-caps: RESTORE FAILED: {e:?}"),
        }
    }

    /// Map the common `AudioDeviceType` to a legacy `AUDIO_DEVICE_(OUT|IN)_*`
    /// token — the same shape `dumpsys` produced — so the routing core's
    /// type-token lookup works unchanged. Speaker/earpiece are mapped precisely
    /// (routing needs them); others descriptively.
    fn device_type_token(t: AudioDeviceType) -> String {
        if t == AudioDeviceType::OUT_SPEAKER          { "AUDIO_DEVICE_OUT_SPEAKER".into() }
        else if t == AudioDeviceType::OUT_SPEAKER_EARPIECE { "AUDIO_DEVICE_OUT_EARPIECE".into() }
        else if t == AudioDeviceType::OUT_SPEAKER_SAFE     { "AUDIO_DEVICE_OUT_SPEAKER_SAFE".into() }
        else if t == AudioDeviceType::OUT_TELEPHONY_TX     { "AUDIO_DEVICE_OUT_TELEPHONY_TX".into() }
        else { format!("AUDIO_DEVICE_TYPE_{}", t.0) }
    }

    /// Task 76 #6 — enumerate audio **device** ports over binder (native
    /// audioserver, `listAudioPorts`) instead of parsing `dumpsys`. Returns the
    /// device-independent port table the routing core consumes (port id + type +
    /// direction). Requires rsbinder ≥ master/0.9.0 (0.8.0 mis-decoded
    /// `AudioPortFw`). Empty on error. Two-pass: count, then fetch (the ports
    /// vec stays empty — the service allocates + fills it).
    pub fn enumerate_device_ports() -> Vec<crate::audio_routing::AudioDeviceCaps> {
        use crate::audio_routing::{AudioDeviceCaps, Direction};
        let mut out = Vec::new();
        if crate::binder::init().is_err() { return out; }
        let Some(svc) = service() else { return out };
        let mut count = Int { r#value: 0 };
        let mut tmp: Vec<AudioPortFw> = Vec::new();
        if svc.r#listAudioPorts(AudioPortRole::NONE, AudioPortType::DEVICE, &mut count, &mut tmp).is_err() {
            log::warn!("audio-routing: listAudioPorts(count) failed"); return out;
        }
        let mut count2 = Int { r#value: count.r#value.max(0) };
        let mut ports: Vec<AudioPortFw> = Vec::new();
        if let Err(e) = svc.r#listAudioPorts(AudioPortRole::NONE, AudioPortType::DEVICE, &mut count2, &mut ports) {
            log::warn!("audio-routing: listAudioPorts(fetch) failed: {e:?}"); return out;
        }
        for p in &ports {
            // A device port's ext carries the AudioDevice (type + address).
            let dev_type = match &p.r#hal.r#ext {
                AudioPortExt::r#Device(d) => d.r#device.r#type.r#type,
                _ => continue,
            };
            let direction = if dev_type.0 >= AudioDeviceType::OUT_DEFAULT.0 {
                Direction::Output
            } else {
                Direction::Input
            };
            out.push(AudioDeviceCaps {
                direction,
                port_id: p.r#hal.r#id,
                name: p.r#hal.r#name.clone(),
                type_token: device_type_token(dev_type),
                formats: Vec::new(),
                sample_rates: Vec::new(),
                channel_masks: Vec::new(),
            });
        }
        out
    }

    /// `--probe-audio-ports`: log the binder-enumerated device ports.
    pub fn probe_list_audio_ports() {
        let ports = enumerate_device_ports();
        log::info!("audio-caps: listAudioPorts (binder) -> {} device ports", ports.len());
        for d in &ports {
            log::info!("audio-caps: port id={} {:?} {} name={:?}",
                d.port_id, d.direction, d.type_token, d.name);
        }
    }

    /// Read-only probe: does a root/su caller reach the policy service, and what
    /// are the current phone state + communication routing? No side effects.
    pub fn probe() {
        if let Err(e) = crate::binder::init() {
            log::warn!("audio-policy probe: binder init failed: {e}");
            return;
        }
        let Some(svc) = service() else { return };

        match svc.r#getPhoneState() {
            Ok(m)  => log::info!("audio-policy: getPhoneState = {} ({})", m.0, mode_name(m)),
            Err(e) => log::warn!("audio-policy: getPhoneState DENIED/err: {e:?}"),
        }
        match svc.r#getForceUse(AudioPolicyForceUse::COMMUNICATION) {
            Ok(c)  => log::info!(
                "audio-policy: getForceUse(COMMUNICATION) = {} ({}) — READ ACCESS OK",
                c.0, cfg_name(c),
            ),
            Err(e) => log::warn!("audio-policy: getForceUse DENIED/err: {e:?}"),
        }
    }

    /// Write probe (the speaker/earpiece toggle): read the current COMMUNICATION
    /// routing, force it to `speaker` (or NONE/earpiece), confirm via read-back,
    /// then RESTORE the previous value. Proves we can drive routing without
    /// leaving the device reconfigured. Still a global change for the brief
    /// window, so it's behind its own explicit flag.
    pub fn probe_route(speaker: bool) {
        if let Err(e) = crate::binder::init() {
            log::warn!("audio-policy route: binder init failed: {e}");
            return;
        }
        let Some(svc) = service() else { return };

        let prev = match svc.r#getForceUse(AudioPolicyForceUse::COMMUNICATION) {
            Ok(c)  => { log::info!("audio-policy route: prev = {} ({})", c.0, cfg_name(c)); c }
            Err(e) => { log::warn!("audio-policy route: getForceUse DENIED: {e:?}"); return; }
        };
        let want = if speaker { AudioPolicyForcedConfig::SPEAKER } else { AudioPolicyForcedConfig::NONE };
        match svc.r#setForceUse(AudioPolicyForceUse::COMMUNICATION, want) {
            Ok(())  => log::info!("audio-policy route: setForceUse(COMMUNICATION, {}) OK — WRITE ACCESS GRANTED", cfg_name(want)),
            Err(e)  => { log::warn!("audio-policy route: setForceUse DENIED/err: {e:?} — perm/SELinux?"); return; }
        }
        match svc.r#getForceUse(AudioPolicyForceUse::COMMUNICATION) {
            Ok(c)  => log::info!("audio-policy route: confirmed = {} ({})", c.0, cfg_name(c)),
            Err(e) => log::warn!("audio-policy route: confirm read err: {e:?}"),
        }
        // Restore the previous routing so the device is left as we found it.
        match svc.r#setForceUse(AudioPolicyForceUse::COMMUNICATION, prev) {
            Ok(())  => log::info!("audio-policy route: restored to {} ({})", prev.0, cfg_name(prev)),
            Err(e)  => log::warn!("audio-policy route: RESTORE FAILED: {e:?} — device left in {}", cfg_name(want)),
        }
    }
}

/// Read-only call-audio reachability probe (`--probe-audio-policy`).
#[cfg(target_os = "android")]
pub fn probe() { binder_path::probe(); }
#[cfg(not(target_os = "android"))]
pub fn probe() { log::warn!("audio-policy probe: android-only build"); }

/// `--init-audio-policy`: replicate AudioService's boot volume/device init so audio
/// is audible under `--no-art` (run by run-hybrid-stack after audioserver is up).
#[cfg(target_os = "android")]
pub fn init_audio_policy() { binder_path::init_audio_policy(); }
#[cfg(not(target_os = "android"))]
pub fn init_audio_policy() { log::warn!("audio-init: android-only build"); }

/// Self-heal: re-init the audio policy if its volume range is uninitialized (-1),
/// which a respawned audioserver has under --no-art. Called on every track open.
#[cfg(target_os = "android")]
pub fn ensure_initialized() { binder_path::ensure_initialized(); }
#[cfg(not(target_os = "android"))]
pub fn ensure_initialized() {}

/// Task-76 read-only routing probe (`getDevicesForAttributes` per usage).
#[cfg(target_os = "android")]
pub fn probe_devices_for_attributes() { binder_path::probe_devices_for_attributes(); }
#[cfg(not(target_os = "android"))]
pub fn probe_devices_for_attributes() { log::warn!("audio-caps devices-for-attr: android-only build"); }

/// Task-76 P8 volume probe (`--probe-audio-volume`): read range + speaker/
/// earpiece media volume, set speaker to max, read back, restore.
#[cfg(target_os = "android")]
pub fn probe_volume() { binder_path::probe_volume(); }
#[cfg(not(target_os = "android"))]
pub fn probe_volume() { log::warn!("audio-caps volume: android-only build"); }

/// Task-76 #6 port-enum probe (`--probe-audio-ports`): listAudioPorts over binder.
#[cfg(target_os = "android")]
pub fn probe_list_audio_ports() { binder_path::probe_list_audio_ports(); }
#[cfg(not(target_os = "android"))]
pub fn probe_list_audio_ports() { log::warn!("audio-caps ports: android-only build"); }

/// Task-76 #6 — enumerate device ports over binder (the routing core's source).
#[cfg(target_os = "android")]
pub fn enumerate_device_ports() -> Vec<crate::audio_routing::AudioDeviceCaps> {
    binder_path::enumerate_device_ports()
}
#[cfg(not(target_os = "android"))]
pub fn enumerate_device_ports() -> Vec<crate::audio_routing::AudioDeviceCaps> { Vec::new() }

/// Routing write probe (`--probe-audio-policy-route <speaker|earpiece>`):
/// drives the COMMUNICATION force-use then restores it.
#[cfg(target_os = "android")]
pub fn probe_route(speaker: bool) { binder_path::probe_route(speaker); }
#[cfg(not(target_os = "android"))]
pub fn probe_route(_speaker: bool) { log::warn!("audio-policy route: android-only build"); }

/// Comms-session start/end audio recipe — mirrors `AudioService.onUpdateAudioMode`
/// (setPhoneState + contextual-volume re-apply). See `binder_path`.
#[cfg(target_os = "android")]
pub fn on_update_audio_mode(comm: bool) { binder_path::on_update_audio_mode(comm); }
#[cfg(not(target_os = "android"))]
pub fn on_update_audio_mode(_comm: bool) {}

/// wandr-arbiter-audio M3 — set the communication routing (speaker/earpiece).
#[cfg(target_os = "android")]
pub fn set_route(speaker: bool) { binder_path::set_route(speaker); }
#[cfg(not(target_os = "android"))]
pub fn set_route(_speaker: bool) {}

/// Task 97 bug #5 — re-route the MEDIA strategy to speaker/earpiece via a
/// PREFERRED device-role (re-routes the existing shared output; no 2nd MMAP
/// endpoint → no -889; works mid-call). Returns true on success.
#[cfg(target_os = "android")]
pub fn set_media_strategy_route(speaker: bool) -> bool { binder_path::set_media_strategy_route(speaker) }
#[cfg(not(target_os = "android"))]
pub fn set_media_strategy_route(_speaker: bool) -> bool { false }

/// Clear the MEDIA-strategy PREFERRED device-role (call-end → media back to default).
#[cfg(target_os = "android")]
pub fn clear_media_strategy_route() { binder_path::clear_media_strategy_route(); }
#[cfg(not(target_os = "android"))]
pub fn clear_media_strategy_route() {}

/// Where the MEDIA strategy currently routes (device-type strings) — probe/verify.
#[cfg(target_os = "android")]
pub fn media_route_devices() -> Vec<String> { binder_path::media_route_devices() }
#[cfg(not(target_os = "android"))]
pub fn media_route_devices() -> Vec<String> { Vec::new() }

/// Task-76 P8 — apply a media-volume step on the arbiter-chosen device
/// (`speaker` = loudspeaker, else earpiece). The host applier.
#[cfg(target_os = "android")]
pub fn adjust_volume_on(speaker: bool, up: bool) { binder_path::adjust_volume_on(speaker, up); }
#[cfg(not(target_os = "android"))]
pub fn adjust_volume_on(_speaker: bool, _up: bool) {}

/// Task-76 — apply output mute/unmute on the arbiter-chosen device.
#[cfg(target_os = "android")]
pub fn set_media_mute(speaker: bool, muted: bool) { binder_path::set_media_mute(speaker, muted); }
#[cfg(not(target_os = "android"))]
pub fn set_media_mute(_speaker: bool, _muted: bool) {}

/// `wandr:audio-focus/controls` — absolute output volume (0.0..=1.0) get/set.
#[cfg(target_os = "android")]
pub fn set_media_volume_level(speaker: bool, level: f32) { binder_path::set_media_volume_level(speaker, level); }
#[cfg(not(target_os = "android"))]
pub fn set_media_volume_level(_speaker: bool, _level: f32) {}
#[cfg(target_os = "android")]
pub fn get_media_volume_level(speaker: bool) -> f32 { binder_path::get_media_volume_level(speaker) }
#[cfg(not(target_os = "android"))]
pub fn get_media_volume_level(_speaker: bool) -> f32 { 1.0 }

/// Task-76 P8 — forward a hardware VOLUME_UP(true)/DOWN(false) press to the
/// arbiter, the single volume decider. The arbiter picks the target device +
/// owner host and pushes back `audio-policy volume <dir> <dev>`. Forwarding
/// (rather than acting locally) dedups the key — the framework delivers it to
/// several wandr surfaces, but only the arbiter-chosen host applies.
#[cfg(target_os = "android")]
pub fn forward_volume_key(up: bool) {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    // Include our pid so the arbiter can target this (live) host when there is
    // no Foreground slot (e.g. keyguard locked). During a call the arbiter
    // overrides this to the comms owner on the call route.
    let dir = if up { "up" } else { "down" };
    let line = format!("volume {dir} {}\n", std::process::id());
    match UnixStream::connect(crate::arbiter_sock::arbiter_sock_path()) {
        Ok(mut s) => { let _ = s.write_all(line.as_bytes()); let _ = s.flush(); }
        Err(e)    => log::warn!("audio: volume-key forward failed: {e} (arbiter down?)"),
    }
}
#[cfg(not(target_os = "android"))]
pub fn forward_volume_key(_up: bool) {}

/// Task 81 — forward a KEYCODE_POWER press to the arbiter (the single display-power
/// authority). Every host's InputReader sees the key under the ART-less path; the
/// arbiter dedups the fan-in and toggles the panel via setPowerMode. (Lives here
/// alongside `forward_volume_key` — the established host→arbiter key-forward spot.)
#[cfg(target_os = "android")]
pub fn forward_power_key() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let line = format!("power-key {}\n", std::process::id());
    match UnixStream::connect(crate::arbiter_sock::arbiter_sock_path()) {
        Ok(mut s) => { let _ = s.write_all(line.as_bytes()); let _ = s.flush(); }
        Err(e)    => log::warn!("power: power-key forward failed: {e} (arbiter down?)"),
    }
}
#[cfg(not(target_os = "android"))]
pub fn forward_power_key() {}
