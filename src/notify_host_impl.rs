//! `wandr:notify/notifier` host impl (Signal bg-receipt M3).
//!
//! A guest calls `post` / `cancel`; the host forwards to the arbiter's
//! `notify-post` / `notify-cancel` socket commands (one-shot, mirroring
//! `alarm_host_impl`). The arbiter owns the active list, surfaces it in the
//! status bar, and on a tap delivers `notification-clicked <id>` to this host's
//! control socket → the standalone loop calls the guest's `on-notification-click`
//! export.
//!
//! Owner identity = the host's own pid (`std::process::id()` — the zygote-forked
//! child the arbiter registered), resolved arbiter-side to the app-id. `title`/
//! `body` are percent-encoded so they survive the whitespace-delimited control
//! line (the arbiter decodes them; mirror of `wandr_arbiter_notify::pct_decode`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use crate::notify_host_bindings::wandr::notify::notifier::Host as NotifierHost;
use crate::notify_host_bindings::wandr::notify::notify_feed::{Host as FeedHost, Notification};

// arbiter socket: crate::arbiter_sock::arbiter_sock_path() ($WANDR_ARBITER_SOCK)

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

/// `tok=` value lookup in a whitespace-delimited line (e.g. `nid=1 app=x …`).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(key))
}

/// Conservative percent-encoding: keep `A-Za-z0-9-._`, escape the rest as `%XX`
/// (UTF-8 bytes). Mirror of the arbiter's decoder.
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

impl NotifierHost for crate::HostState {
    fn post(&mut self, id: u64, title: String, body: String) {
        let pid = std::process::id();
        let line = format!(
            "notify-post {pid} {id} {} {}\n",
            pct_encode(&title),
            pct_encode(&body),
        );
        match send_oneshot(&line) {
            Ok(()) => log::info!("notify-host: posted id={id} title={title:?}"),
            Err(e) => log::warn!("notify-host: post id={id} forward failed: {e:#} (arbiter down?)"),
        }
    }

    fn cancel(&mut self, id: u64) {
        let pid = std::process::id();
        if let Err(e) = send_oneshot(&format!("notify-cancel {pid} {id}\n")) {
            log::warn!("notify-host: cancel id={id} forward failed: {e:#}");
        }
    }
}

impl FeedHost for crate::HostState {
    /// The status bar reads the active list each frame. Answered live from the
    /// arbiter (`notify-list`) — the arbiter owns the list, so there's no host
    /// cache to keep coherent. Parses the `nid=… app=… id=… title=…` reply lines.
    fn list_active(&mut self) -> Vec<Notification> {
        let Some(reply) = arbiter_query("notify-list\n") else {
            return Vec::new();
        };
        reply
            .lines()
            .filter_map(|line| {
                let nid: u64 = field(line, "nid=")?.parse().ok()?;
                let app_id = field(line, "app=").unwrap_or("").to_string();
                let title = field(line, "title=").map(pct_decode).unwrap_or_default();
                Some(Notification { nid, app_id, title, body: String::new() })
            })
            .collect()
    }

    /// The user tapped notification `nid`; forward to the arbiter, which
    /// foregrounds the owner + delivers `on-notification-click`.
    fn click(&mut self, nid: u64) {
        if let Err(e) = send_oneshot(&format!("notify-click {nid}\n")) {
            log::warn!("notify-host: click nid={nid} forward failed: {e:#}");
        }
    }
}
