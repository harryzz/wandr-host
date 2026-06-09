//! `wandr:events/producer` host impl — the publish side of the wandr event bus
//! (task 90). A guest calls `producer.publish(msg)`; the host forwards
//! `(topic, data)` to the arbiter's generic `evt-publish`, which stores the
//! topic's retained value and fans it to every subscribed guest's
//! `incoming-handler.handle` (the push side, called from `standalone.rs`).
//!
//! In-process broker: the arbiter is the broker (no `client`/connection — that's
//! why this is `wandr:events`, not the resource-based `wasi:messaging` proposal,
//! though the vocabulary matches for forward-compat). Subscription is NOT a WIT
//! call: wandr reads a guest's `package.toml` `[events] subscribe = [...]` and
//! registers it with the arbiter (`evt-subscribe`); see `standalone.rs`.

use std::io::Write;
use std::os::unix::net::UnixStream;

use crate::events_host_bindings::wandr::events::producer::Host;
use crate::events_host_bindings::wandr::events::types::Message;

/// Forward one line to the arbiter (fire-and-forget; the arbiter is the broker).
fn arbiter_oneshot(line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// base64 (standard alphabet, padded) — the host↔arbiter socket is line-framed
/// text, so an opaque `list<u8>` payload is base64'd on the wire.
pub fn b64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Decode base64 (ignores padding/whitespace). Returns `None` on a bad char.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut bits = 0u32;
    let mut nbits = 0;
    let mut out = Vec::new();
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        bits = (bits << 6) | val(c)?;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

impl crate::events_host_bindings::wandr::events::types::Host for crate::HostState {}

impl Host for crate::HostState {
    fn publish(&mut self, msg: Message) {
        // Guest-originated broadcast → the arbiter's generic bus. Fire-and-forget
        // (the arbiter is the authority; if it's down the event just isn't fanned).
        let line = format!("evt-publish {} {}\n", msg.topic, b64_encode(&msg.data));
        if let Err(e) = arbiter_oneshot(&line) {
            log::warn!("events: producer.publish({}) forward failed: {e:#} (arbiter down?)", msg.topic);
        }
    }
}
