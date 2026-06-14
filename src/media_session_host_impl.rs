//! `wasi:media-session/session` + `wandr:chrome/now-playing` host impl (task 108 M2).
//!
//! The PUBLISH side (`session`, imported by the player guest): set-metadata /
//! set-playback-state / set-position / clear → forwarded to the arbiter's
//! media-session module over the line-framed control socket (one-shot, mirroring
//! `notify_host_impl` / `alarm_host_impl`). The arbiter owns the active-session
//! state.
//!
//! The READ side (`now-playing`, imported by the system chrome — the keyguard /
//! status bar): `get` / `artwork` are answered live from the arbiter
//! (`media-session-now-playing` / `media-session-artwork`), and `send-action`
//! forwards a transport tap (`media-session-action …`) which the arbiter routes
//! to the active session's `session-handler.on-action`.
//!
//! Owner identity = the host's own pid (`std::process::id()` — the zygote-forked
//! child the arbiter registered). Text fields are percent-encoded so they survive
//! the whitespace-delimited control line (mirror of `notify_host_impl`); artwork
//! bytes are base64'd (reusing `events_host_impl::b64_encode`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use crate::chrome_bindings::wandr::chrome::now_playing::{
    Action, Host as NowPlayingHost, NowPlayingInfo, PlaybackState as ChromePlaybackState,
};
use crate::events_host_impl::{b64_decode, b64_encode};
use crate::media_session_host_bindings::wasi::media_session::session::{
    Host as SessionHost, Metadata, PlaybackState, PositionState,
};

fn send_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Connect, write `line`, read the whole reply. `None` if the arbiter is
/// unreachable (the surfacer then shows nothing — fail-soft).
fn arbiter_query(line: &str) -> Option<String> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path()).ok()?;
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

/// `key=` value lookup in a whitespace-delimited line (mirror of notify_host_impl).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|t| t.strip_prefix(key))
}

/// Conservative percent-encoding: keep `A-Za-z0-9-._`, escape the rest as `%XX`.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push('%');
            let hex = |n: u8| (if n < 10 { b'0' + n } else { b'A' + (n - 10) }) as char;
            out.push(hex(b >> 4));
            out.push(hex(b & 0xf));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let unhex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (unhex(b[i + 1]), unhex(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encode a metadata field for the positional set-metadata wire. An empty field
/// must NOT become an empty token (whitespace-delimited parsing would collapse
/// it and drop the positional slot), so empty maps to the sentinel `~` — a byte
/// `pct_encode` never emits for non-empty input (a literal `~` becomes `%7E`).
fn enc_field(s: &str) -> String {
    if s.is_empty() {
        "~".to_string()
    } else {
        pct_encode(s)
    }
}

fn playback_state_wire(s: PlaybackState) -> &'static str {
    match s {
        PlaybackState::None => "none",
        PlaybackState::Paused => "paused",
        PlaybackState::Playing => "playing",
    }
}

fn parse_chrome_state(s: &str) -> ChromePlaybackState {
    match s {
        "playing" => ChromePlaybackState::Playing,
        "paused" => ChromePlaybackState::Paused,
        _ => ChromePlaybackState::None,
    }
}

/// Wire token for a transport action (shared with the arbiter parser).
fn action_wire(a: Action) -> &'static str {
    match a {
        Action::Play => "play",
        Action::Pause => "pause",
        Action::Stop => "stop",
        Action::SeekTo => "seek-to",
        Action::SeekForward => "seek-forward",
        Action::SeekBackward => "seek-backward",
        Action::PreviousTrack => "previous-track",
        Action::NextTrack => "next-track",
    }
}

// ── publish side (the player guest drives this) ──────────────────────────────
impl SessionHost for crate::HostState {
    fn set_metadata(&mut self, meta: Metadata) {
        let pid = std::process::id();
        let has_art = meta.artwork.is_some();
        let line = format!(
            "media-session-set-metadata {pid} {} {} {} {}\n",
            enc_field(&meta.title),
            enc_field(&meta.artist),
            enc_field(&meta.album),
            if has_art { 1 } else { 0 },
        );
        match send_oneshot(&line) {
            Ok(()) => log::info!("media-session: metadata {:?} by {pid}", meta.title),
            Err(e) => log::warn!("media-session: set-metadata forward failed: {e:#} (arbiter down?)"),
        }
        // Artwork on a separate line (large, base64'd) — only when present, so
        // it isn't re-sent on every metadata update with no art.
        if let Some(art) = meta.artwork {
            let aline = format!(
                "media-session-set-art {pid} {} {}\n",
                pct_encode(&art.mime),
                b64_encode(&art.data),
            );
            if let Err(e) = send_oneshot(&aline) {
                log::warn!("media-session: set-art forward failed: {e:#}");
            }
        }
    }

    fn set_playback_state(&mut self, state: PlaybackState) {
        let pid = std::process::id();
        let line = format!("media-session-set-state {pid} {}\n", playback_state_wire(state));
        if let Err(e) = send_oneshot(&line) {
            log::warn!("media-session: set-state forward failed: {e:#}");
        }
    }

    fn set_position(&mut self, pos: PositionState) {
        let pid = std::process::id();
        let line = format!(
            "media-session-set-position {pid} {} {} {}\n",
            pos.duration_s, pos.playback_rate, pos.position_s,
        );
        if let Err(e) = send_oneshot(&line) {
            log::warn!("media-session: set-position forward failed: {e:#}");
        }
    }

    fn clear(&mut self) {
        let pid = std::process::id();
        if let Err(e) = send_oneshot(&format!("media-session-clear {pid}\n")) {
            log::warn!("media-session: clear forward failed: {e:#}");
        }
    }
}

// ── read side (the system chrome renders this) ───────────────────────────────
impl NowPlayingHost for crate::HostState {
    /// The lockscreen / status bar polls this on render. Answered live from the
    /// arbiter (`media-session-now-playing`) — it owns the active-session state,
    /// so there's no host cache to keep coherent. Reply (one line) or empty:
    ///   `app=<id> title=<pct> artist=<pct> album=<pct> state=<s> dur=<f> pos=<f> art=<0|1>`
    fn get(&mut self) -> Option<NowPlayingInfo> {
        let reply = arbiter_query("media-session-now-playing\n")?;
        let line = reply.lines().next()?;
        // The arbiter reply is "OK <body>" — strip the status prefix. An EMPTY
        // body ("OK ") means no active session → None (don't render a card).
        let body = line.strip_prefix("OK").map(str::trim).unwrap_or("");
        if body.is_empty() {
            return None;
        }
        let app_id = field(body, "app=").unwrap_or("").to_string();
        let title = field(body, "title=").map(pct_decode).unwrap_or_default();
        let artist = field(body, "artist=").map(pct_decode).unwrap_or_default();
        let album = field(body, "album=").map(pct_decode).unwrap_or_default();
        let state = parse_chrome_state(field(body, "state=").unwrap_or("none"));
        let duration_s = field(body, "dur=").and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let position_s = field(body, "pos=").and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let has_artwork = field(body, "art=").map(|v| v == "1").unwrap_or(false);
        Some(NowPlayingInfo {
            app_id,
            title,
            artist,
            album,
            state,
            duration_s,
            position_s,
            has_artwork,
        })
    }

    /// Artwork bytes on demand. Reply: `<mime> <b64>` or empty. The mime is
    /// dropped — `graphics.decode-image` sniffs the format from the bytes.
    fn artwork(&mut self) -> Option<Vec<u8>> {
        let reply = arbiter_query("media-session-artwork\n")?;
        let line = reply.lines().next()?;
        // Strip the "OK " status prefix; body is "<mime> <b64>" or empty.
        let body = line.strip_prefix("OK").map(str::trim).unwrap_or("");
        let (_mime, b64) = body.split_once(' ')?;
        b64_decode(b64)
    }

    /// The user tapped a transport control. Forward to the arbiter, which routes
    /// it to the active session's `session-handler.on-action`.
    fn send_action(&mut self, act: Action, seek_time_s: Option<f64>) {
        let seek = seek_time_s.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
        let line = format!("media-session-action {} {seek}\n", action_wire(act));
        if let Err(e) = send_oneshot(&line) {
            log::warn!("media-session: send-action forward failed: {e:#}");
        }
    }
}
