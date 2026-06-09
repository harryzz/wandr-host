//! Task 76 step 4 — audio routing core.
//!
//! The capability-driven replacement for the hard-coded audio layer task 75
//! left behind (`USAGE_MEDIA` pinned everywhere, earpiece pinned to
//! `deviceIds=[2]`, `WANDR_EARPIECE` env, mono/stereo guessed). Two pieces:
//!
//!   - [`DeviceModel`] — the device's real audio port table, built once from
//!     `dumpsys media.audio_policy` (the framework `AudioPortFw` parcelable that
//!     `listAudioPorts` returns is too fragile to hand-decode; see
//!     [[project_audio_capability_model]]). Ports are looked up by **type**
//!     (`AUDIO_DEVICE_OUT_EARPIECE`/`_SPEAKER`/…), never by a hard-coded id, so
//!     this is resolution/device-independent ([[feedback_no_hardcoding]]).
//!
//!   - [`Route`] → [`StreamPlan`] — the shared routing *vocabulary*. Per the
//!     project's decides/applies split ([[project_audio_routing_arbiter]]), the
//!     **arbiter** (`wandr-arbiter-audio`) picks the `Route` for the stateful
//!     cases (call earpiece↔speaker, comms, duck) and the host *applies* it;
//!     non-stateful routes (media→default, ringtone→speaker) are fixed applier
//!     mappings (mechanism). Either way this maps a `Route` to concrete AAudio
//!     open params. The resolution encodes the
//!     task-76 probe findings as deliberate policy:
//!       * output is always **F32 stereo** (mono → `-889`, I16 → `-883`),
//!       * output `usage` is **MEDIA** — the only usage AAudio opens for output
//!         on this device (`VOICE_COMMUNICATION` → `-889` in every phone mode),
//!       * SHARED sharing (so an output + a capture can coexist),
//!       * the target device comes from the model by type, not a magic id.
//!
//! `audio_impl` consumes a `StreamPlan` instead of its old inline hard-codes;
//! `audio_caps` reuses the model + parser here for its probes.

// ── AAudio.h contract constants (single source) ──────────────────────────────
// Stable AAudio.h enum values (we don't link libaaudio). This is the one named
// source the no-hardcoding rule asks for; `audio_impl`/`audio_caps` reference
// these rather than re-declaring them.
pub mod aa {
    pub const DIR_OUTPUT: i32 = 0;
    pub const DIR_INPUT:  i32 = 1;
    pub const SHARING_EXCLUSIVE: i32 = 0;
    pub const SHARING_SHARED:    i32 = 1;
    pub const USAGE_MEDIA:               i32 = 1;
    pub const USAGE_VOICE_COMMUNICATION: i32 = 2;
    pub const CONTENT_SPEECH: i32 = 1;
    pub const CONTENT_MUSIC:  i32 = 2;
    // AAUDIO_CHANNEL_MONO = FRONT_LEFT (0x1); STEREO = FRONT_LEFT|RIGHT (0x3).
    pub const CHANNEL_MONO:   i32 = 0x1;
    pub const CHANNEL_STEREO: i32 = 0x3;
    pub const INPUT_PRESET_NONE: i32 = 0;
    pub const INPUT_PRESET_VOICE_RECOGNITION: i32 = 6;
}

// ── Device-capability model ──────────────────────────────────────────────────

/// Direction of an audio device port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction { Output, Input }

/// One audio device port as reported by the platform policy, parsed from
/// `dumpsys media.audio_policy`'s "Available output/input devices" sections.
#[derive(Debug, Clone)]
pub struct AudioDeviceCaps {
    pub direction: Direction,
    /// Audio-policy **port id** — which the task-76 probe proved is the *same*
    /// namespace AAudio's `StreamParameters.deviceIds` uses (a default MEDIA
    /// open is granted the Speaker port id). So pinning a stream to this id
    /// routes it to this device.
    pub port_id: i32,
    pub name: String,
    /// The `AUDIO_DEVICE_OUT_*` / `AUDIO_DEVICE_IN_*` token (e.g. EARPIECE).
    pub type_token: String,
    /// Supported PCM formats (`AUDIO_FORMAT_*` tokens) across all profiles.
    pub formats: Vec<String>,
    /// Supported sample rates (Hz) across all profiles.
    pub sample_rates: Vec<u32>,
    /// Supported channel masks (raw hex values) across all profiles.
    pub channel_masks: Vec<u32>,
}

/// The device's audio port table, built once and cached.
pub struct DeviceModel {
    devices: Vec<AudioDeviceCaps>,
}

#[cfg(target_os = "android")]
static MODEL: std::sync::OnceLock<DeviceModel> = std::sync::OnceLock::new();

impl DeviceModel {
    /// Process-wide model, built lazily by enumerating ports over binder
    /// (`IAudioPolicyService.listAudioPorts`, native audioserver — task 76 #6).
    /// Replaces the earlier `dumpsys media.audio_policy` parse: no shell, no
    /// ART-layer coupling. Requires rsbinder ≥ 0.9.0 (0.8.0 mis-decoded
    /// `AudioPortFw`).
    #[cfg(target_os = "android")]
    pub fn get() -> &'static DeviceModel {
        MODEL.get_or_init(|| {
            let devices = crate::audio_policy_impl::enumerate_device_ports();
            log::info!("audio-routing: device model built — {} ports (binder listAudioPorts)", devices.len());
            DeviceModel { devices }
        })
    }

    /// Construct directly from already-parsed devices (probe / tests).
    pub fn from_devices(devices: Vec<AudioDeviceCaps>) -> Self {
        DeviceModel { devices }
    }

    pub fn devices(&self) -> &[AudioDeviceCaps] { &self.devices }

    /// Port id of the first output device whose type token contains `needle`
    /// (e.g. `"EARPIECE"`, `"SPEAKER"`). `None` if absent on this device.
    pub fn output_port(&self, needle: &str) -> Option<i32> {
        self.devices.iter()
            .find(|d| d.direction == Direction::Output && d.type_token.contains(needle))
            .map(|d| d.port_id)
    }

    // ── intent → params ──────────────────────────────────────────────────────

    /// Resolve an output [`Route`] to concrete AAudio open params.
    pub fn resolve_output(&self, route: Route) -> StreamPlan {
        // Probe findings → fixed output policy: MEDIA usage, F32, stereo, SHARED.
        // (mono → -889, I16 → -883, VOICE_COMMUNICATION → -889 in every mode.)
        let device_ids = match route {
            // A call NEVER pins a per-stream deviceId: AAudio would open a SECOND
            // MMAP "direct output" on the pinned device, but `mmap_no_irq_out` is
            // maxOpenCount=1, so the earpiece pin `-889`s whenever another output
            // already holds it (task 97 bug #5). Instead the call shares the
            // existing MMAP output and is routed via a PREFERRED device-role on its
            // strategy (`audio_policy_impl::set_media_strategy_route`, driven by
            // `set_comms_route`) — which re-routes the existing output and works
            // mid-call. So leave device_ids empty for calls.
            Route::Call { .. } => Vec::new(),
            _ => match route.target() {
                // Speaker-Safe is avoided; SPEAKER is the loud output. EARPIECE is
                // the in-ear receiver. Media leaves device_ids empty → policy default.
                RouteTarget::Speaker  => self.output_port("OUT_SPEAKER").map(|p| vec![p]).unwrap_or_default(),
                RouteTarget::Earpiece => self.output_port("OUT_EARPIECE").map(|p| vec![p]).unwrap_or_default(),
                RouteTarget::PolicyDefault => Vec::new(),
            },
        };
        // Speech content for a call (tunes the policy/DSP); music otherwise.
        let content = if matches!(route, Route::Call { .. }) { aa::CONTENT_SPEECH } else { aa::CONTENT_MUSIC };
        StreamPlan {
            label: route.label(),
            direction: aa::DIR_OUTPUT,
            usage: aa::USAGE_MEDIA,
            content_type: content,
            channel_mask: aa::CHANNEL_STEREO,
            channels: 2,
            sharing: aa::SHARING_SHARED,
            f32_format: true,
            device_ids,
            input_preset: aa::INPUT_PRESET_NONE,
        }
    }

    /// Resolve a capture stream to concrete AAudio open params. Mic capture is
    /// F32 mono with the VOICE_RECOGNITION preset (raw-ish, low-latency), SHARED
    /// so it coexists with an output stream (probe: SHARED in+out coexist).
    pub fn resolve_capture(&self) -> StreamPlan {
        StreamPlan {
            label: "capture mic",
            direction: aa::DIR_INPUT,
            usage: 0,
            content_type: 0,
            channel_mask: aa::CHANNEL_MONO,
            channels: 1,
            sharing: aa::SHARING_SHARED,
            f32_format: true,
            device_ids: Vec::new(),
            input_preset: aa::INPUT_PRESET_VOICE_RECOGNITION,
        }
    }
}

/// What an output stream is *for* — the guest expresses this; the host picks
/// the device + params. Mirrors the WIT `audio-route` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// General media playback — wherever the policy routes it (usually speaker).
    Media,
    /// Ringtone / alarm — the loud speaker, regardless of media routing.
    Ringtone,
    /// Notification chirp — the loud speaker.
    Notification,
    /// An active voice call. `speaker=false` → earpiece (default), `true` →
    /// loudspeaker (speakerphone).
    Call { speaker: bool },
}

enum RouteTarget { PolicyDefault, Speaker, Earpiece }

impl Route {
    fn target(self) -> RouteTarget {
        match self {
            Route::Media => RouteTarget::PolicyDefault,
            Route::Ringtone | Route::Notification => RouteTarget::Speaker,
            Route::Call { speaker: false } => RouteTarget::Earpiece,
            Route::Call { speaker: true }  => RouteTarget::Speaker,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Route::Media => "media",
            Route::Ringtone => "ringtone",
            Route::Notification => "notification",
            Route::Call { speaker: false } => "call-earpiece",
            Route::Call { speaker: true }  => "call-speaker",
        }
    }
}

/// Concrete, resolved AAudio open parameters — what `audio_impl` builds a
/// `StreamParameters` from. Replaces the old inline hard-codes.
#[derive(Debug, Clone)]
pub struct StreamPlan {
    pub label: &'static str,
    pub direction: i32,
    pub usage: i32,
    pub content_type: i32,
    pub channel_mask: i32,
    pub channels: u32,
    pub sharing: i32,
    pub f32_format: bool,
    pub device_ids: Vec<i32>,
    pub input_preset: i32,
}

// ── verify probe ─────────────────────────────────────────────────────────────

/// `--probe-audio-route`: build the live device model and log the resolved
/// [`StreamPlan`] for every [`Route`] (+ capture). Read-only — proves the core
/// picks the right device/params on THIS device before the live path adopts it.
#[cfg(target_os = "android")]
pub fn probe_routes() {
    let m = DeviceModel::get();
    log::info!("==== audio-routing: route resolution (task 76 step 4) ====");
    for d in m.devices() {
        log::info!("audio-routing: port {} \"{}\" {} ({:?})", d.port_id, d.name, d.type_token, d.direction);
    }
    for route in [
        Route::Media, Route::Ringtone, Route::Notification,
        Route::Call { speaker: false }, Route::Call { speaker: true },
    ] {
        let p = m.resolve_output(route);
        log::info!(
            "audio-routing: {:?} -> [{}] usage={} content={} chMask=0x{:x} ch={} sharing={} f32={} deviceIds={:?}",
            route, p.label, p.usage, p.content_type, p.channel_mask, p.channels,
            p.sharing, p.f32_format, p.device_ids,
        );
    }
    let c = m.resolve_capture();
    log::info!("audio-routing: capture -> [{}] preset={} ch={} f32={} deviceIds={:?}",
        c.label, c.input_preset, c.channels, c.f32_format, c.device_ids);
    log::info!("==== audio-routing: route resolution complete ====");
}

#[cfg(not(target_os = "android"))]
pub fn probe_routes() { log::warn!("audio-routing: android-only build"); }

// ── dumpsys parser ───────────────────────────────────────────────────────────

/// Parse the "Available output devices" / "Available input devices" sections of
/// `dumpsys media.audio_policy`. Device header form:
///   `  1. Port ID: 2; "Earpiece"; {AUDIO_DEVICE_OUT_EARPIECE, @:}`
/// followed by profile lines carrying `AUDIO_FORMAT_*`, `sampling rates: ...`,
/// `channel masks: 0x..`.
pub fn parse_devices(dump: &str) -> Vec<AudioDeviceCaps> {
    let mut out = Vec::new();
    let mut dir: Option<Direction> = None;
    let mut cur: Option<AudioDeviceCaps> = None;

    fn flush(cur: &mut Option<AudioDeviceCaps>, out: &mut Vec<AudioDeviceCaps>) {
        if let Some(d) = cur.take() { out.push(d); }
    }

    for line in dump.lines() {
        let t = line.trim();
        if t.starts_with("Available output devices") { flush(&mut cur, &mut out); dir = Some(Direction::Output); continue; }
        if t.starts_with("Available input devices")  { flush(&mut cur, &mut out); dir = Some(Direction::Input);  continue; }
        if t.starts_with("Hardware modules")          { flush(&mut cur, &mut out); dir = None; continue; }
        let Some(direction) = dir else { continue };

        // Device header: `N. Port ID: <id>; "<name>"; {<TYPE>, ...}`. The
        // "Supported devices" sub-lists inside Hardware modules are already
        // excluded by the section gate; profile lines have no "Port ID:".
        if t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            if let Some((_, rest)) = t.split_once("Port ID:") {
                flush(&mut cur, &mut out);
                let port_id = rest.trim().split(';').next()
                    .and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(-1);
                let name = rest.split('"').nth(1).unwrap_or("").to_string();
                let type_token = rest.split('{').nth(1)
                    .and_then(|s| s.split([',', '}']).next())
                    .unwrap_or("").trim().to_string();
                cur = Some(AudioDeviceCaps {
                    direction, port_id, name, type_token,
                    formats: Vec::new(), sample_rates: Vec::new(), channel_masks: Vec::new(),
                });
                continue;
            }
        }

        // Profile detail lines for the current device.
        if let Some(d) = cur.as_mut() {
            if let Some(idx) = t.find("AUDIO_FORMAT_") {
                let fmt: String = t[idx..].split([' ', ';']).next().unwrap_or("").to_string();
                if fmt != "AUDIO_FORMAT_DEFAULT" && !d.formats.contains(&fmt) { d.formats.push(fmt); }
            }
            if let Some(rates) = t.strip_prefix("sampling rates:") {
                for r in rates.split(',') {
                    if let Ok(v) = r.trim().parse::<u32>() { if !d.sample_rates.contains(&v) { d.sample_rates.push(v); } }
                }
            }
            if let Some(masks) = t.strip_prefix("channel masks:") {
                for m in masks.split(',') {
                    let m = m.trim().trim_start_matches("0x");
                    if let Ok(v) = u32::from_str_radix(m, 16) { if !d.channel_masks.contains(&v) { d.channel_masks.push(v); } }
                }
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
 Available output devices (2):
  1. Port ID: 2; \"Earpiece\"; {AUDIO_DEVICE_OUT_EARPIECE, @:}
   - Profiles (1):
      1. \"\"; [dynamic format][dynamic channels][dynamic rates]; AUDIO_FORMAT_DEFAULT (0x0)
  2. Port ID: 3; \"Speaker\"; {AUDIO_DEVICE_OUT_SPEAKER, @:}
   - Profiles (1):
      1. \"\"; [dynamic format][dynamic channels][dynamic rates]; AUDIO_FORMAT_DEFAULT (0x0)
 Available input devices (1):
  1. Port ID: 19; \"Built-In Mic\"; {AUDIO_DEVICE_IN_BUILTIN_MIC, @:bottom}
      2. \"\"; AUDIO_FORMAT_PCM_8_24_BIT (0x4)
         sampling rates: 8000, 48000
         channel masks: 0x000c, 0x0010
 Hardware modules (1):
  1. Handle: 10; \"primary\"
";

    #[test]
    fn parses_ports_and_resolves_routes() {
        let devices = parse_devices(SAMPLE);
        assert_eq!(devices.len(), 3);
        let model = DeviceModel::from_devices(devices);
        assert_eq!(model.output_port("OUT_EARPIECE"), Some(2));
        assert_eq!(model.output_port("OUT_SPEAKER"), Some(3));

        // Call → earpiece, stereo F32 MEDIA.
        let call = model.resolve_output(Route::Call { speaker: false });
        assert_eq!(call.device_ids, vec![2]);
        assert_eq!(call.usage, aa::USAGE_MEDIA);
        assert_eq!(call.channels, 2);
        assert!(call.f32_format);
        // Ringtone → speaker; Media → policy default (no pin).
        assert_eq!(model.resolve_output(Route::Ringtone).device_ids, vec![3]);
        assert!(model.resolve_output(Route::Media).device_ids.is_empty());
        // Speakerphone → speaker.
        assert_eq!(model.resolve_output(Route::Call { speaker: true }).device_ids, vec![3]);

        // Capture: F32 mono, voice-recognition preset.
        let cap = model.resolve_capture();
        assert_eq!(cap.channels, 1);
        assert_eq!(cap.input_preset, aa::INPUT_PRESET_VOICE_RECOGNITION);

        // Input mic profile parsed.
        let mic = devices_input(&model, "IN_BUILTIN_MIC");
        assert!(mic.sample_rates.contains(&48000));
        assert!(mic.channel_masks.contains(&0xc));
    }

    fn devices_input<'a>(m: &'a DeviceModel, needle: &str) -> &'a AudioDeviceCaps {
        m.devices().iter().find(|d| d.type_token.contains(needle)).unwrap()
    }
}
