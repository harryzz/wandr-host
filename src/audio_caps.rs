//! Task-76 audio capability probe (read-only investigation, steps 1–3).
//!
//! Task 75 shipped working call audio but on a hard-coded, guess-driven audio
//! layer (`USAGE_MEDIA` pinned, earpiece pinned to `deviceIds=[2]`, mono/stereo
//! guessed). Before the capability-driven refactor (task 76 steps 4+) we land
//! the device's *real* audio picture, derived from the platform rather than
//! assumed.
//!
//! Two entry points, both read-only:
//!   - `--probe-audio-caps`   → dump the capability picture (step 1) + parse it
//!                              into a typed device model (step 2). Parses
//!                              `dumpsys media.audio_policy` / `dumpsys audio`
//!                              (the robust source — the framework `AudioPortFw`
//!                              parcelable that `listAudioPorts` returns is too
//!                              fragile to hand-decode over binder), and proves
//!                              binder reachability for the levers the refactor
//!                              needs (`getDevicesForAttributes`, AAudio granted
//!                              device id). Volume is read from `dumpsys audio`
//!                              (full per-stream index/min/max table) — the
//!                              ~230-method `IAudioService` positional stub is
//!                              deferred to the volume-WRITE session (P8), where
//!                              transaction indices can be validated by read-back.
//!   - `--probe-audio-matrix` → fill the task-76 state matrix with targeted,
//!                              self-restoring on-device opens (step 3).
//!
//! Nothing here reconfigures the device beyond brief opens + a self-restoring
//! phone-state toggle for the IN_COMMUNICATION cells (restored to the mode the
//! probe found, which it asserts is NORMAL first).

#[cfg(target_os = "android")]
use crate::{audio_impl, audio_policy_impl};
// The capability model + parser + AAudio constants live in the routing core;
// the probe reuses them (step 2's typed model is now `audio_routing`'s).
#[cfg(target_os = "android")]
use crate::audio_routing::{aa, parse_devices, AudioDeviceCaps, Direction};

// ── Entry points ─────────────────────────────────────────────────────────────

/// `--probe-audio-caps`: dump the capability picture (step 1) + typed model
/// (step 2). Read-only.
#[cfg(target_os = "android")]
pub fn probe() {
    log::info!("==== audio-caps: capability probe (task 76 steps 1-2) ====");

    // 1a. Service reachability (safe — no positional method calls).
    reachability();

    // 1b. Device table + current routing + force-use (robust: dumpsys).
    let policy = run_dumpsys("media.audio_policy");
    log_policy_state(&policy);
    let devices = parse_devices(&policy);
    log_device_model(&devices);
    log_strategy_routing(&policy);

    // 1c. Volume table (robust: dumpsys audio).
    let audio = run_dumpsys("audio");
    log_volume_table(&audio);

    // 1d. Active patches (which device each live stream is routed to).
    log_active_patches(&run_dumpsys("media.audio_flinger"));

    // 1e. Binder reachability for the levers the refactor needs.
    //     getDevicesForAttributes (policy's own routing answer over binder).
    audio_policy_impl::probe_devices_for_attributes();
    //     AAudio granted device id (resolve AAudio-id vs policy-port-id).
    probe_aaudio_granted_device(&devices);

    log::info!("==== audio-caps: probe complete ====");
}

/// `--probe-audio-matrix`: fill the task-76 state matrix with targeted,
/// self-restoring on-device opens (step 3). Read-only beyond brief opens + a
/// restored phone-state toggle.
#[cfg(target_os = "android")]
pub fn probe_matrix() {
    log::info!("==== audio-caps: state matrix (task 76 step 3) ====");
    if let Err(e) = crate::binder::init() {
        log::warn!("audio-caps matrix: binder init failed: {e}");
        return;
    }

    // NORMAL-mode cells (the device is NORMAL at rest — confirmed by the caps
    // probe's "Phone state" line). Each cell opens + closes one stream.
    matrix_cell("OUT MEDIA NORMAL default SHARED F32 mono",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_SHARED,
        aa::CHANNEL_MONO, true, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT MEDIA NORMAL default SHARED F32 stereo",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_SHARED,
        aa::CHANNEL_STEREO, true, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT MEDIA NORMAL default SHARED I16 stereo",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_SHARED,
        aa::CHANNEL_STEREO, false, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT MEDIA NORMAL EARPIECE(port2) SHARED F32 stereo",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_SHARED,
        aa::CHANNEL_STEREO, true, vec![2], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT MEDIA NORMAL default EXCLUSIVE F32 stereo (MMAP)",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_EXCLUSIVE,
        aa::CHANNEL_STEREO, true, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT VOICE_COMMUNICATION NORMAL default SHARED F32 mono",
        aa::DIR_OUTPUT, aa::USAGE_VOICE_COMMUNICATION, aa::CONTENT_SPEECH, aa::SHARING_SHARED,
        aa::CHANNEL_MONO, true, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("IN VOICE_RECOGNITION NORMAL default SHARED F32 mono",
        aa::DIR_INPUT, 0, 0, aa::SHARING_SHARED,
        aa::CHANNEL_MONO, true, vec![], aa::INPUT_PRESET_VOICE_RECOGNITION);
    matrix_cell("IN default NORMAL default SHARED I16 mono",
        aa::DIR_INPUT, 0, 0, aa::SHARING_SHARED,
        aa::CHANNEL_MONO, false, vec![], aa::INPUT_PRESET_NONE);

    // in+out simultaneous (SHARED/SHARED) — the task-75 open question.
    let (out_rc, in_rc) = audio_impl::probe_coexist();
    log::info!("MATRIX | in+out SHARED+SHARED coexist | out_rc={out_rc} in_rc={in_rc}");

    // IN_COMMUNICATION cells — toggle phone state, run, restore to NORMAL.
    // The caps probe confirmed the device rests at NORMAL; we restore there.
    log::info!("audio-caps: entering IN_COMMUNICATION for comms-mode cells (will restore NORMAL)");
    audio_policy_impl::on_update_audio_mode(true);
    matrix_cell("OUT MEDIA IN_COMMUNICATION default SHARED F32 stereo",
        aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC, aa::SHARING_SHARED,
        aa::CHANNEL_STEREO, true, vec![], aa::INPUT_PRESET_NONE);
    matrix_cell("OUT VOICE_COMMUNICATION IN_COMMUNICATION default SHARED F32 mono",
        aa::DIR_OUTPUT, aa::USAGE_VOICE_COMMUNICATION, aa::CONTENT_SPEECH, aa::SHARING_SHARED,
        aa::CHANNEL_MONO, true, vec![], aa::INPUT_PRESET_NONE);
    audio_policy_impl::on_update_audio_mode(false);
    log::info!("audio-caps: phone state restored to NORMAL");

    log::info!("==== audio-caps: matrix complete ====");
}

#[cfg(target_os = "android")]
#[allow(clippy::too_many_arguments)]
fn matrix_cell(
    label: &str, direction: i32, usage: i32, content: i32, sharing: i32,
    mask: i32, f32_format: bool, device_ids: Vec<i32>, preset: i32,
) {
    let rc = audio_impl::probe_open(
        label, direction, usage, content, sharing, mask, f32_format, device_ids, preset,
    );
    let verdict = if rc > 0 { "WORKS".to_string() } else { format!("FAILS({rc})") };
    log::info!("MATRIX | {label} | {verdict}");
}

// ── Service reachability (safe) ──────────────────────────────────────────────

#[cfg(target_os = "android")]
fn reachability() {
    if let Err(e) = crate::binder::init() {
        log::warn!("audio-caps: binder init failed: {e}");
    }
    for name in ["media.aaudio", "media.audio_policy", "audio", "media.audio_flinger"] {
        // `service check <name>` is the cheapest registered/not check; we shell
        // it rather than bind each interface (the framework "audio" service has
        // no safe no-arg method, and a positional stub is deferred to P8).
        let out = std::process::Command::new("service")
            .args(["check", name])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                let found = s.contains("found");
                log::info!("audio-caps: service '{name}' = {}",
                    if found { "REGISTERED" } else { s.trim() });
            }
            Err(e) => log::warn!("audio-caps: service check '{name}' failed: {e}"),
        }
    }
}

// ── dumpsys plumbing ─────────────────────────────────────────────────────────

#[cfg(target_os = "android")]
fn run_dumpsys(service: &str) -> String {
    match std::process::Command::new("dumpsys").arg(service).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => { log::warn!("audio-caps: dumpsys {service} failed: {e}"); String::new() }
    }
}

#[cfg(target_os = "android")]
fn log_policy_state(dump: &str) {
    for line in dump.lines() {
        let t = line.trim();
        if t.starts_with("Phone state:")
            || t.starts_with("Force use for")
            || t.starts_with("Primary Output")
            || t.starts_with("Communication Strategy")
        {
            log::info!("audio-caps: policy {}", t);
        }
    }
}

#[cfg(target_os = "android")]
fn log_device_model(devices: &[AudioDeviceCaps]) {
    log::info!("audio-caps: --- device model ({} ports) ---", devices.len());
    for d in devices {
        log::info!(
            "audio-caps: {:?} port={} \"{}\" type={} formats={:?} rates={:?} masks={:?}",
            d.direction, d.port_id, d.name, d.type_token,
            d.formats, d.sample_rates,
            d.channel_masks.iter().map(|m| format!("0x{m:x}")).collect::<Vec<_>>(),
        );
    }
}

/// Strategy → device routing lines ("Devices: {AUDIO_DEVICE_OUT_*}") — the
/// policy's standing answer for where strategies route.
#[cfg(target_os = "android")]
fn log_strategy_routing(dump: &str) {
    let mut last_strategy = String::new();
    for line in dump.lines() {
        let t = line.trim();
        if t.starts_with("Strategy ") || t.contains("product strategy") { last_strategy = t.to_string(); }
        if let Some(dev) = t.strip_prefix("Devices:") {
            log::info!("audio-caps: routing {} -> {}", last_strategy, dev.trim());
        }
    }
}

/// Stream volume table from `dumpsys audio` — type, Min/Max, Muted, current
/// per-device index. This is the read-only volume picture (P8); the binder
/// `IAudioService` getters are deferred to the volume-write session.
#[cfg(target_os = "android")]
fn log_volume_table(dump: &str) {
    let mut in_volumes = false;
    let mut stream = String::new();
    for line in dump.lines() {
        let t = line.trim();
        if t.starts_with("Stream volumes") { in_volumes = true; log::info!("audio-caps: --- stream volumes ---"); continue; }
        if !in_volumes { continue; }
        // Section ends at the first blank-ish line after a non-stream block.
        if t.starts_with("- STREAM_") { stream = t.trim_start_matches("- ").to_string(); continue; }
        if stream.is_empty() { continue; }
        if t.starts_with("Muted:") || t.starts_with("Min:") || t.starts_with("Max:")
            || t.starts_with("Current:") || t.starts_with("Devices:")
        {
            log::info!("audio-caps: vol {} | {}", stream, t);
        }
        // A new top-level header (no leading dash, not an indented field) ends it.
        if !line.starts_with(' ') && !t.starts_with("- STREAM_") && !t.is_empty() && in_volumes && !stream.is_empty() && !t.starts_with("STREAM") {
            in_volumes = false;
        }
    }
}

/// Active audio patches from `dumpsys media.audio_flinger` — which device each
/// live stream is currently routed to.
#[cfg(target_os = "android")]
fn log_active_patches(dump: &str) {
    for line in dump.lines() {
        let t = line.trim();
        if t.contains("Patch") && (t.contains("AUDIO_DEVICE_OUT") || t.contains("AUDIO_DEVICE_IN")) {
            log::info!("audio-caps: patch {}", t);
        }
    }
}

/// Open a default MEDIA output stream and log the **granted** AAudio device id
/// (from `params_out`), then close it. Compared against the parsed policy port
/// table, this resolves the AAudio-deviceId ↔ policy-port-id namespace question
/// (the `deviceIds=[2]` ambiguity from task 75).
#[cfg(target_os = "android")]
fn probe_aaudio_granted_device(devices: &[AudioDeviceCaps]) {
    log::info!("audio-caps: --- AAudio granted-device resolution ---");
    let rc = audio_impl::probe_open(
        "granted-id MEDIA default", aa::DIR_OUTPUT, aa::USAGE_MEDIA, aa::CONTENT_MUSIC,
        aa::SHARING_SHARED, aa::CHANNEL_STEREO, true, vec![], aa::INPUT_PRESET_NONE,
    );
    log::info!(
        "audio-caps: (compare the granted_deviceIds logged above against the parsed \
         output port ids: {:?}) open_rc={rc}",
        devices.iter().filter(|d| d.direction == Direction::Output)
            .map(|d| (d.port_id, d.type_token.as_str())).collect::<Vec<_>>(),
    );
}

// ── non-android stubs ────────────────────────────────────────────────────────

#[cfg(not(target_os = "android"))]
pub fn probe() { log::warn!("audio-caps: android-only build"); }
#[cfg(not(target_os = "android"))]
pub fn probe_matrix() { log::warn!("audio-caps matrix: android-only build"); }
