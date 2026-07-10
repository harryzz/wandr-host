//! `wandr:audio-focus/focus` host impl (wandr-arbiter-audio, M2).
//!
//! A guest calls `request(kind)` / `abandon()`; the host forwards to the
//! arbiter's `audio-focus-request <pid> <kind>` / `audio-focus-abandon <pid>`
//! socket commands. Unlike the fire-and-forget alarm/notify forwards, `request`
//! reads the arbiter's reply (`OK granted …` / `OK delayed …` / `ERR …`) and
//! maps it to a `focus-result`. The arbiter, on an owner change, delivers
//! `on-focus-changed <change>` back to this host's control socket → the
//! standalone loop calls the guest's `focus-handler.on-focus-changed` export.
//!
//! Owner identity: the host self-reports its pid (`std::process::id()` — the
//! zygote-forked child the arbiter registered), resolved to the app-id arbiter-
//! side. Mirrors `alarm_host_impl` / `notify_host_impl`.

use std::io::{Read, Write};
use crate::arbiter_sock::UnixStream;

use crate::audio_focus_host_bindings::wandr::audio_focus::focus::{
    FocusKind, FocusResult, Host,
};

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

impl FocusKind {
    fn as_wire(self) -> &'static str {
        match self {
            FocusKind::Gain                 => "gain",
            FocusKind::GainTransient        => "gain-transient",
            FocusKind::GainTransientMayDuck => "gain-transient-may-duck",
        }
    }
}

/// Fire-and-forget one line to the arbiter (abandon).
fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Send one line and read the arbiter's single-line reply (`OK …` / `ERR …`).
fn send_and_read(line: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

impl Host for crate::HostState {
    fn request(&mut self, kind: FocusKind) -> FocusResult {
        let pid = std::process::id();
        let line = format!("audio-focus-request {pid} {}\n", kind.as_wire());
        match send_and_read(&line) {
            Ok(reply) => {
                let r = reply.trim();
                log::info!("audio-focus-host: request kind={} → {r}", kind.as_wire());
                // Reply is `OK granted …` / `OK delayed …` / `ERR …`.
                if r.starts_with("OK granted") {
                    FocusResult::Granted
                } else if r.starts_with("OK delayed") {
                    FocusResult::Delayed
                } else {
                    FocusResult::Failed
                }
            }
            Err(e) => {
                log::warn!("audio-focus-host: request forward failed: {e:#} (arbiter down?)");
                FocusResult::Failed
            }
        }
    }

    fn abandon(&mut self) {
        let pid = std::process::id();
        if let Err(e) = send_oneshot(&format!("audio-focus-abandon {pid}\n")) {
            log::warn!("audio-focus-host: abandon forward failed: {e:#}");
        }
    }

    fn ring_start(&mut self) {
        forward("audio-ring-start");
    }

    fn ring_stop(&mut self) {
        forward("audio-ring-stop");
    }

    fn call_start(&mut self) {
        forward("audio-call-start");
    }

    fn call_end(&mut self) {
        forward("audio-call-end");
    }

    fn call_video(&mut self, active: bool) {
        forward(if active { "audio-call-video 1" } else { "audio-call-video 0" });
    }
}

/// Fire-and-forget `<verb> <pid>` to the arbiter (the call/ring session commands).
fn forward(verb: &str) {
    let pid = std::process::id();
    if let Err(e) = send_oneshot(&format!("{verb} {pid}\n")) {
        log::warn!("audio-focus-host: {verb} forward failed: {e:#}");
    }
}

// ── wandr:audio-focus/controls — route / volume / mute (guest-facing) ──────────
//
// The host is the applier, so it is authoritative for the `get-*` reads. Route is
// cached here (the appliers don't store it); volume reads the live policy index; mute
// reads the existing PCM-gate flags. A guest-explicit `set-route` is applied directly
// (the user tapped speaker) via the same appliers the arbiter's CommRoute uses — it
// does NOT go through the toxic setPhoneState path (see project_call_audioserver_crash).
use crate::audio_focus_host_bindings::wandr::audio_focus::controls::{
    AudioRoute, Host as ControlsHost,
};
use std::sync::atomic::{AtomicU8, Ordering};

// Last-applied route: 0=earpiece, 1=speaker, 2=bluetooth.
static ROUTE: AtomicU8 = AtomicU8::new(0);

impl ControlsHost for crate::HostState {
    fn set_route(&mut self, route: AudioRoute) {
        // bluetooth (BT_SCO) isn't wired yet → fall back to earpiece routing but record
        // the request so get-route reflects the guest's choice.
        let (code, speaker) = match route {
            AudioRoute::Earpiece  => (0u8, false),
            AudioRoute::Speaker   => (1u8, true),
            AudioRoute::Bluetooth => (2u8, false),
        };
        // Do NOT call setForceUse(COMMUNICATION) here: it re-runs
        // setOutputDevices→installPatch (the same HAL path that SIGABRTs audioserver),
        // and it has no earpiece option for the MEDIA strategy anyway. The route moves
        // inside set_comms_route, which re-routes the live shared output via a PREFERRED
        // device-role on its product strategy (setDevicesRoleForStrategy) — taking
        // effect MID-CALL with no track re-open, and never -889ing (the call output is
        // no longer deviceId-pinned). See task 97 bug #5 / project_call_audioserver_crash.
        crate::audio_impl::set_comms_route(speaker);
        ROUTE.store(code, Ordering::Relaxed);
        log::info!("audio-controls: set-route {route:?} (strategy re-route, mid-call)");
    }
    fn get_route(&mut self) -> AudioRoute {
        match ROUTE.load(Ordering::Relaxed) {
            1 => AudioRoute::Speaker,
            2 => AudioRoute::Bluetooth,
            _ => AudioRoute::Earpiece,
        }
    }
    fn set_volume(&mut self, level: f32) {
        let speaker = ROUTE.load(Ordering::Relaxed) == 1;
        crate::audio_policy_impl::set_media_volume_level(speaker, level);
    }
    fn get_volume(&mut self) -> f32 {
        let speaker = ROUTE.load(Ordering::Relaxed) == 1;
        crate::audio_policy_impl::get_media_volume_level(speaker)
    }
    fn set_mute(&mut self, muted: bool) {
        crate::audio_impl::set_app_output_muted(muted);
    }
    fn get_mute(&mut self) -> bool {
        crate::audio_impl::app_output_muted()
    }
    fn set_mic_mute(&mut self, muted: bool) {
        crate::audio_impl::set_mic_muted(muted);
    }
    fn get_mic_mute(&mut self) -> bool {
        crate::audio_impl::mic_muted()
    }
}
