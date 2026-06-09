//! Per-host control socket for arbiter→guest event delivery
//! (task 47 step 3a).
//!
//! The outbound half of the IME protocol (guest→arbiter editor-focus
//! events) was step 2 — `ime_host_impl.rs` opens a fresh UNIX socket
//! per call. This module is the INBOUND half: each wandr-host child
//! binds `/data/local/tmp/wandr-host-<pid>.sock` and the arbiter
//! pushes events down it when an IME app calls e.g. `ime-send-key-event`.
//!
//! Architecture (matches the existing InputFlinger drain pattern):
//!
//!   accept thread (background)         render loop (main thread)
//!   ───────────────────────             ───────────────────────────
//!   listener.accept()                   ── per frame ──
//!   read lines                          drain_queue() → Vec<InboundEvent>
//!   parse                               for each event:
//!   queue.push_back(event)                dispatch_key_v2(skiko, store, …)
//!                                       (re-uses task 33 step 3 dispatch)
//!
//! wasmtime's `Store` is `!Send` — only the render-loop thread can
//! call into the wasm guest. The accept thread JUST parses + queues;
//! the render loop drains + dispatches.
//!
//! Wire format (one line per event, ASCII):
//!
//!   key-event <code-point> <key-id> <down|up>
//!     Synthesize a key event into the focused editor (step 3a). Sent
//!     to whichever wandr-host owns the focused-editor pid.
//!
//!   editor-attached <input-type> <hint-underscored> <initial-text-underscored> <sel-start> <sel-end>
//!     (task 49 step 1a). Sent to whichever wandr-host owns the
//!     ACTIVE IME's pid when an editor focuses. `hint-underscored` and
//!     `initial-text-underscored` are space→`_` escaped (matches the
//!     existing `attach-editor` CLI convention); use `-` for the empty
//!     string. `input-type` is one of the bare enum tags
//!     `text`/`number`/`phone`/`email`/`url`/`password`/`multiline-text`.
//!
//!   editor-detached
//!     (task 49 step 1a). Sent to the IME's host when the focused
//!     editor loses focus.
//!
//! Future extensions (commit-text / composing / set-selection / etc.)
//! land alongside as additional verbs.
//!
//! Per-host socket path is derived from getpid() — the arbiter knows
//! the focused-app's pid (it's in `EditorFocus`) and the IME pid
//! (it's in `ActiveIme`), so it addresses
//! `/data/local/tmp/wandr-host-<pid>.sock` directly. No registration
//! handshake needed.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

/// Editor metadata delivered to the IME on focus. Mirrors the WIT
/// `wandr:ime/editor-info` record exactly. Task 49 step 1a.
#[derive(Clone, Debug, Default)]
pub struct EditorInfo {
    /// One of the `wandr:ime/input-type` enum tags as a string:
    /// `text`, `number`, `phone`, `email`, `url`, `password`,
    /// `multiline-text`. The IME-side bindings convert to the WIT
    /// enum value before calling the guest.
    pub input_type: String,
    pub hint: String,
    pub initial_text: String,
    pub initial_selection_start: u32,
    pub initial_selection_end: u32,
}

/// One incoming event. Stored in the queue between the accept thread
/// and the render-loop drain.
#[derive(Clone, Debug)]
pub enum InboundEvent {
    /// Synthesize a key event into the focused guest. `action` is
    /// 0=down, 1=up (matches `dispatch_key_v2`'s `kind` byte).
    KeyEvent { code_point: u32, key_id: u32, action: u8 },

    /// Task 49 step 1a — IME-bound notification: an editor focused.
    /// The render loop calls the IME guest's exported
    /// `wandr:ime/ime.on-editor-attached(info)` via the host bindgen
    /// added in step 1b. Hosts that aren't IMEs (i.e. running
    /// without `--standalone-overlay`) log + drop this — see the
    /// drain code in `standalone.rs`.
    EditorAttached { info: EditorInfo },

    /// Task 49 step 1a — paired with `EditorAttached`. Calls the
    /// IME's `wandr:ime/ime.on-editor-detached()` export.
    EditorDetached,

    /// Task 68 — the arbiter tells THIS (foreground/editor) host how much of
    /// its surface the soft keyboard now occludes, in physical px. The render
    /// loop adds it to the base bottom inset (set_insets + on_resize) so the
    /// guest re-lays-out its bottom content above the keyboard. `0` = keyboard
    /// hidden → restore the base inset.
    KeyboardInset { px: u32 },

    /// Task 71 (WMS-authority step) — the arbiter made this surface visible
    /// (newly foregrounded, or an overlay re-engaged) and explicitly asks for a
    /// fresh frame. Decouples "you are visible, repaint" from the async role
    /// signal: relying on the SIGUSR2 role flip alone left a re-shown surface
    /// present-but-empty (the on-demand render gate had nothing to invalidate).
    /// The drain already marks the frame dirty, which forces the repaint.
    Present,

    /// Task 73 (modular WM) — the arbiter (wandr-arbiter-wm) is the source of
    /// this surface's window geometry: chrome insets, soft-keyboard occlusion,
    /// and orientation, computed once and pushed as data. The host is a dumb
    /// applier (it still runs its own dihedral skia matrix to render). Fields
    /// use sentinels so the arbiter can move policy in stages without a wire
    /// change: `inset_top`/`inset_bottom` = `0xFFFF` means "keep my env-sourced
    /// inset"; `orient` = `255` means "keep my own orientation". `keyboard_px`
    /// is always authoritative (0 = no keyboard). Subsumes `KeyboardInset`.
    Geometry { inset_top: u32, inset_bottom: u32, keyboard_px: u32, orient: u32 },

    /// Arbiter Inc. 3c — a scheduled alarm fired. The arbiter's alarm module
    /// delivers `alarm-fired <id>` to this surface's control socket; the
    /// standalone drain calls the guest's `wandr:alarm/alarm-handler.on-alarm(id)`
    /// export (if the guest exports it).
    AlarmFired { id: u64 },

    /// Signal bg-receipt M3 — the user tapped this app's notification. The
    /// arbiter's notify module delivers `notification-clicked <id>` to this
    /// surface's control socket (and foregrounds the app); the standalone drain
    /// calls the guest's `wandr:notify/notify-handler.on-notification-click(id)`
    /// export (if the guest exports it).
    NotificationClicked { id: u64 },

    /// Task 90 event bus — the arbiter fanned an event on a topic this guest
    /// subscribed to. Delivered as `event <topic> <base64-payload>` on the control
    /// socket; the standalone drain calls the guest's
    /// `wandr:events/incoming-handler.handle(msg)` export (if it exports it).
    Event { topic: String, data: Vec<u8> },

    /// PowerManager (wandr-arbiter-power) — the arbiter decided the doze state and
    /// pushed `doze <cadence-ms>` to this host. `cadence_ms = 0` means resume
    /// normal pacing; `>0` means slow the render/bg-tick loop to that coarse
    /// cadence while the screen is off. The host is a dumb applier.
    Doze { cadence_ms: u64 },

    /// wandr-arbiter-audio (M2) — the audio-focus arbiter changed this guest's
    /// focus and pushed `on-focus-changed <change>`. `change` is the wire code
    /// 0=loss, 1=loss-transient, 2=duck, 3=gain (matching the
    /// `wandr:audio-focus/focus-handler.focus-change` enum order); the standalone
    /// drain calls the guest's `on-focus-changed` export (inert if not exported).
    FocusChanged { change: u32 },

    /// wandr-arbiter-audio (M3) — the arbiter started/ended a comms session on
    /// this host and pushed `audio-policy set-mode <comm|normal>`. The host (the
    /// call owner, which holds the binder connection) applies it globally via
    /// `audio_policy_impl::on_update_audio_mode` (mirrors `AudioService
    /// .onUpdateAudioMode`: setPhoneState + volume re-apply). "arbiter decides, host applies."
    CommMode { comm: bool },

    /// wandr-arbiter-audio (M3) — the arbiter changed the call routing and pushed
    /// `audio-policy set-route <speaker|earpiece>`. The host applies it via
    /// `audio_policy_impl::set_route` (setForceUse COMMUNICATION).
    CommRoute { speaker: bool },

    /// wandr-arbiter-audio (P8) — the arbiter decided a volume step and pushed
    /// `audio-policy volume <up|down> <speaker|earpiece>` to the chosen owner
    /// host, which applies it via `audio_policy_impl::adjust_volume_on`.
    VolumeAdjust { up: bool, speaker: bool },

    /// wandr-arbiter-audio (P8) — the arbiter decided an output mute change and
    /// pushed `audio-policy mute <on|off> <speaker|earpiece>` to the owner host.
    MuteSet { muted: bool, speaker: bool },

    /// wandr-arbiter-audio (P8) — per-app output mute: the arbiter pushed
    /// `audio-policy app-mute <on|off>` to this app's host, which gates its PCM
    /// write path (silence). Orthogonal to the global policy mute.
    AppMute { muted: bool },

    /// wandr-arbiter-audio (P8) — mic-mute / input-disable: the arbiter pushed
    /// `audio-policy mic-mute <on|off>` to the owner host, which gates its
    /// capture read path (returns silence). Dormant until capture is opened.
    MicMute { muted: bool },

    /// wandr-arbiter-audio Ringer — the arbiter pushed `ringtone start|stop` for an
    /// incoming call. The owner host plays/stops a generated ringtone over AAudio
    /// (`ringer_impl`). `start=false` ⇒ stop.
    Ringtone { start: bool },

    /// wandr-arbiter-audio Ringer — the arbiter pushed `haptics ring-start|ring-stop`.
    /// The host runs/stops a repeating ring-vibrate over the vibrator HAL.
    RingVibrate { start: bool },
}

/// Sentinel: a `geometry` inset field the host should leave at its current
/// (env-sourced) value. Mirrors `wandr_arbiter_core::INSET_HOST_OWNED`.
pub const GEOM_INSET_KEEP: u32 = 0xFFFF;

/// Sentinel: a `geometry` orient field the host should ignore (it keeps its
/// own rotation authority). Mirrors `wandr_arbiter_core::ORIENT_HOST_OWNED`.
pub const GEOM_ORIENT_KEEP: u32 = 255;

fn queue() -> &'static Mutex<VecDeque<InboundEvent>> {
    static Q: OnceLock<Mutex<VecDeque<InboundEvent>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Drain all queued events. Called once per frame by the render loop.
/// Returns an empty Vec when no events are queued.
pub fn drain_queue() -> Vec<InboundEvent> {
    match queue().lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_)    => Vec::new(),
    }
}

/// Bind the per-host socket + spawn the accept thread. Returns the
/// socket path for logging. Called by `standalone::run_with_engine`
/// AFTER fork (so each forked child binds its own pid-named socket).
pub fn spawn_listener() -> Result<String> {
    let path = format!("/data/local/tmp/wandr-host-{}.sock", std::process::id());
    if Path::new(&path).exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("UnixListener::bind {path}"))?;

    // 0o666 — same as the arbiter's socket. The arbiter is root in
    // the dev path so perms aren't load-bearing; on a sepolicy'd
    // production build the path + SELinux context matter more.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));

    let path_clone = path.clone();
    std::thread::Builder::new()
        .name("wandr-host-ime-inbound".into())
        .spawn(move || {
            loop {
                let (stream, _addr) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::warn!("ime-inbound: accept failed: {e}");
                        continue;
                    }
                };
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    parse_and_queue(&line);
                }
            }
        })
        .with_context(|| "spawn wandr-host-ime-inbound thread")?;

    Ok(path_clone)
}

/// Parse one wire-format line and push the matching event onto the
/// queue. Silently drops malformed lines (with a warn log) so a buggy
/// or hostile arbiter can't crash the guest.
fn parse_and_queue(line: &str) {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("key-event ") {
        // <code-point> <key-id> <down|up>
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() != 3 {
            log::warn!("ime-inbound: malformed key-event: {line:?}");
            return;
        }
        let Ok(code_point) = parts[0].parse::<u32>() else {
            log::warn!("ime-inbound: bad code-point in {line:?}");
            return;
        };
        let Ok(key_id) = parts[1].parse::<u32>() else {
            log::warn!("ime-inbound: bad key-id in {line:?}");
            return;
        };
        let action: u8 = match parts[2] {
            "down" => 0,
            "up"   => 1,
            other  => {
                log::warn!("ime-inbound: bad action {other:?} in {line:?}");
                return;
            }
        };
        if let Ok(mut q) = queue().lock() {
            q.push_back(InboundEvent::KeyEvent { code_point, key_id, action });
        }
    } else if let Some(rest) = line.strip_prefix("editor-attached ") {
        // <input-type> <hint-underscored> <initial-text-underscored> <sel-start> <sel-end>
        // `-` decodes to empty string. Spaces in hint/text are
        // `_`-escaped per the established attach-editor CLI convention.
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() != 5 {
            log::warn!("ime-inbound: malformed editor-attached: {line:?}");
            return;
        }
        let input_type = parts[0].to_string();
        let hint         = unescape_underscores(parts[1]);
        let initial_text = unescape_underscores(parts[2]);
        let Ok(sel_start) = parts[3].parse::<u32>() else {
            log::warn!("ime-inbound: bad sel-start in {line:?}");
            return;
        };
        let Ok(sel_end) = parts[4].parse::<u32>() else {
            log::warn!("ime-inbound: bad sel-end in {line:?}");
            return;
        };
        let info = EditorInfo {
            input_type,
            hint,
            initial_text,
            initial_selection_start: sel_start,
            initial_selection_end:   sel_end,
        };
        if let Ok(mut q) = queue().lock() {
            q.push_back(InboundEvent::EditorAttached { info });
        }
    } else if line == "editor-detached" {
        if let Ok(mut q) = queue().lock() {
            q.push_back(InboundEvent::EditorDetached);
        }
    } else if let Some(rest) = line.strip_prefix("keyboard-inset ") {
        // <px> — physical px the soft keyboard occludes (0 = hidden). Task 68.
        match rest.trim().parse::<u32>() {
            Ok(px) => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::KeyboardInset { px });
                }
            },
            Err(_) => log::warn!("ime-inbound: bad keyboard-inset px in {line:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("geometry ") {
        // <inset_top> <inset_bottom> <keyboard_px> <orient>. Task 73.
        // Sentinels: inset 0xFFFF = keep; orient 255 = keep. The applier in
        // standalone.rs reads them.
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() != 4 {
            log::warn!("ime-inbound: malformed geometry: {line:?}");
            return;
        }
        let (Ok(inset_top), Ok(inset_bottom), Ok(keyboard_px), Ok(orient)) = (
            parts[0].parse::<u32>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
            parts[3].parse::<u32>(),
        ) else {
            log::warn!("ime-inbound: bad geometry field in {line:?}");
            return;
        };
        if let Ok(mut q) = queue().lock() {
            q.push_back(InboundEvent::Geometry { inset_top, inset_bottom, keyboard_px, orient });
        }
    } else if let Some(rest) = line.strip_prefix("alarm-fired ") {
        // Arbiter Inc. 3c — a scheduled alarm fired; call the guest's on-alarm.
        match rest.trim().parse::<u64>() {
            Ok(id) => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::AlarmFired { id });
                }
            }
            Err(_) => log::warn!("ime-inbound: bad alarm-fired id in {line:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("notification-clicked ") {
        // Signal bg-receipt M3 — the user tapped this app's notification; call
        // the guest's on-notification-click.
        match rest.trim().parse::<u64>() {
            Ok(id) => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::NotificationClicked { id });
                }
            }
            Err(_) => log::warn!("ime-inbound: bad notification-clicked id in {line:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("event ") {
        // Task 90 event bus — `event <topic> <base64-payload>`. Decode + enqueue;
        // the standalone drain calls the guest's wandr:events/incoming-handler.
        let mut it = rest.trim().splitn(2, ' ');
        if let Some(topic) = it.next() {
            let payload_b64 = it.next().unwrap_or("");
            match crate::events_host_impl::b64_decode(payload_b64) {
                Some(data) => {
                    if let Ok(mut q) = queue().lock() {
                        q.push_back(InboundEvent::Event { topic: topic.to_string(), data });
                    }
                }
                None => log::warn!("ime-inbound: bad base64 payload in event line {line:?}"),
            }
        }
    } else if let Some(rest) = line.strip_prefix("doze ") {
        // PowerManager — arbiter-decided doze cadence (ms; 0 = resume normal).
        match rest.trim().parse::<u64>() {
            Ok(cadence_ms) => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::Doze { cadence_ms });
                }
            }
            Err(_) => log::warn!("ime-inbound: bad doze cadence in {line:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("input-suppress ") {
        // Task 79 — proximity screen-off: drop touch while the panel is blanked.
        // Applied directly (the gate is a process-global atomic in `input`, read
        // on the render thread), so no InboundEvent queue plumbing is needed.
        match rest.trim() {
            "1" => crate::input::set_touch_suppressed(true),
            "0" => crate::input::set_touch_suppressed(false),
            other => log::warn!("ime-inbound: bad input-suppress arg {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("play-tone") {
        // arbiter-audio → host applier: play a sine tone (test / warm-up). Runs for
        // ~ms, so spawn it off the control thread. `play-tone [ms] [hz] [vol0-1]`.
        let toks: Vec<&str> = rest.split_whitespace().collect();
        let ms = toks.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1500);
        let hz = toks.get(1).and_then(|s| s.parse::<f32>().ok()).unwrap_or(440.0);
        let vol = toks.get(2).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.5);
        log::info!("ime-inbound: play-tone {ms}ms {hz}Hz vol={vol}");
        std::thread::spawn(move || crate::audio_impl::play_tone(ms, hz, vol));
    } else if let Some(rest) = line.strip_prefix("on-focus-changed ") {
        // wandr-arbiter-audio M2 — audio-focus change; call the guest's
        // on-focus-changed. Map the wire token to the focus-change enum order.
        let change = match rest.trim() {
            "loss"           => Some(0u32),
            "loss-transient" => Some(1),
            "duck"           => Some(2),
            "gain"           => Some(3),
            _                => None,
        };
        match change {
            Some(change) => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::FocusChanged { change });
                }
            }
            None => log::warn!("ime-inbound: bad on-focus-changed token in {line:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy set-mode ") {
        // wandr-arbiter-audio M3 — comms session mode (the call owner applies it).
        match rest.trim() {
            "comm"   => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::CommMode { comm: true }); } }
            "normal" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::CommMode { comm: false }); } }
            other    => log::warn!("ime-inbound: bad audio-policy set-mode {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy set-route ") {
        // wandr-arbiter-audio M3 — comms routing (speaker/earpiece).
        match rest.trim() {
            "speaker"  => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::CommRoute { speaker: true }); } }
            "earpiece" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::CommRoute { speaker: false }); } }
            other      => log::warn!("ime-inbound: bad audio-policy set-route {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy volume ") {
        // wandr-arbiter-audio P8 — arbiter-decided volume step: "<up|down> <speaker|earpiece>".
        let t: Vec<&str> = rest.split_whitespace().collect();
        match (t.first().copied(), t.get(1).copied()) {
            (Some(dir), Some(dev)) if (dir == "up" || dir == "down") && (dev == "speaker" || dev == "earpiece") => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::VolumeAdjust { up: dir == "up", speaker: dev == "speaker" });
                }
            }
            _ => log::warn!("ime-inbound: bad audio-policy volume {rest:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy mute ") {
        // wandr-arbiter-audio P8 — output mute: "<on|off> <speaker|earpiece>".
        let t: Vec<&str> = rest.split_whitespace().collect();
        match (t.first().copied(), t.get(1).copied()) {
            (Some(st), Some(dev)) if (st == "on" || st == "off") && (dev == "speaker" || dev == "earpiece") => {
                if let Ok(mut q) = queue().lock() {
                    q.push_back(InboundEvent::MuteSet { muted: st == "on", speaker: dev == "speaker" });
                }
            }
            _ => log::warn!("ime-inbound: bad audio-policy mute {rest:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy app-mute ") {
        // wandr-arbiter-audio P8 — per-app output mute: "<on|off>".
        match rest.trim() {
            "on"  => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::AppMute { muted: true }); } }
            "off" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::AppMute { muted: false }); } }
            other => log::warn!("ime-inbound: bad audio-policy app-mute {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("audio-policy mic-mute ") {
        // wandr-arbiter-audio P8 — mic-mute / input-disable: "<on|off>".
        match rest.trim() {
            "on"  => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::MicMute { muted: true }); } }
            "off" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::MicMute { muted: false }); } }
            other => log::warn!("ime-inbound: bad audio-policy mic-mute {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("ringtone ") {
        // wandr-arbiter-audio Ringer — incoming-call ringtone.
        match rest.trim() {
            "start" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::Ringtone { start: true }); } }
            "stop"  => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::Ringtone { start: false }); } }
            other   => log::warn!("ime-inbound: bad ringtone {other:?}"),
        }
    } else if let Some(rest) = line.strip_prefix("haptics ") {
        // wandr-arbiter-audio Ringer — incoming-call vibrate.
        match rest.trim() {
            "ring-start" => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::RingVibrate { start: true }); } }
            "ring-stop"  => { if let Ok(mut q) = queue().lock() { q.push_back(InboundEvent::RingVibrate { start: false }); } }
            other        => log::warn!("ime-inbound: bad haptics {other:?}"),
        }
    } else if line == "present" {
        // Task 71 — arbiter-driven "you are visible, repaint now". The drain
        // marks the frame dirty, forcing a full repaint into the shown surface.
        if let Ok(mut q) = queue().lock() {
            q.push_back(InboundEvent::Present);
        }
    } else if !line.is_empty() {
        // Unknown verb — log so we can spot a protocol-skew between
        // arbiter + host versions. Don't crash.
        log::warn!("ime-inbound: unknown verb in {line:?}");
    }
}

/// Reverse of the attach-editor CLI's space-escape: `-` → empty,
/// other chars passthrough with `_` → space. Symmetric with the
/// arbiter's `escape_underscores` in main.rs.
fn unescape_underscores(s: &str) -> String {
    if s == "-" {
        return String::new();
    }
    s.replace('_', " ")
}
