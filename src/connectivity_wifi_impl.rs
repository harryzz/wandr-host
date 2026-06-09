//! `wandr:connectivity/wifi` host impl (task 90 M2 — privileged WiFi management).
//!
//! The `WifiManager` role for the privileged Settings / wifi-picker chrome. A
//! guest calls `scan` / `connect-new` / `set-enabled`; the host forwards each to
//! the arbiter's `wifi-*` relay verbs over the control socket, and the arbiter
//! relays to the single live `wandr-net` daemon (which owns the supplicant +
//! drives `IWificond`/`ISupplicant`/`IWifi`). The host is a thin proxy: it does
//! no binder itself — the daemon is the uid-system mechanism owner.
//!
//! Privilege: this interface is `add_to_linker`d ONLY for guests
//! `LoadedApp::wifi_privileged` accepts (system-install class + the
//! `wifi-control` manifest opt-in). Ordinary guests can't import it.
//!
//! Scope (M2): `scan`, `connect-new`, `set-enabled`/`is-enabled` are live (the
//! built engine half). The saved-network store + `connect(id)` / `disconnect` /
//! `forget-current` land in M3 (the arbiter WifiConfigManager) and return an
//! explicit "M3" error / no-op here until then. SSID + passphrase are base64'd on
//! the wire so they tokenise cleanly across the host→arbiter→daemon hops.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use crate::wifi_host_bindings::wandr::connectivity::wifi::{
    Host, SavedNetwork, ScanResult, SecurityKind, WifiConfig,
};

/// Connect to the arbiter, write one line, read the WHOLE reply to EOF (a scan
/// reply is multi-line). Returns the reply body, or an error if unreachable.
fn query_full(line: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(crate::arbiter_sock::arbiter_sock_path())?;
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// base64 (standard alphabet, padded) — matches the daemon's `b64_decode`.
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// base64 decode (standard alphabet) — the inverse, for the SSID in scan rows.
fn b64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let Some(v) = val(c) else { continue };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// Map the daemon's security token (`open|owe|wpa-psk|sae|wpa-eap`) to the enum.
fn parse_security(tok: &str) -> SecurityKind {
    match tok {
        "owe" => SecurityKind::Owe,
        "wpa-psk" => SecurityKind::WpaPsk,
        "sae" => SecurityKind::Sae,
        "wpa-eap" => SecurityKind::WpaEap,
        _ => SecurityKind::Open,
    }
}

impl Host for crate::HostState {
    fn set_enabled(&mut self, on: bool) {
        let n = on as u8;
        match query_full(&format!("wifi-set-enabled {n}\n")) {
            Ok(reply) => log::info!("wifi: set-enabled {on} -> {}", reply.trim()),
            Err(e) => log::warn!("wifi: set-enabled forward failed: {e:#} (arbiter down?)"),
        }
    }

    fn is_enabled(&mut self) -> bool {
        match query_full("wifi-is-enabled\n") {
            Ok(reply) => reply
                .lines()
                .find_map(|l| l.trim().strip_prefix("ok enabled="))
                .map(|v| v.trim() == "1")
                .unwrap_or(false),
            Err(e) => {
                log::warn!("wifi: is-enabled forward failed: {e:#}");
                false
            }
        }
    }

    fn scan(&mut self) -> Result<Vec<ScanResult>, String> {
        let reply = query_full("wifi-scan\n").map_err(|e| format!("arbiter unreachable: {e}"))?;
        let mut out = Vec::new();
        for ln in reply.lines() {
            let ln = ln.trim();
            if let Some(rest) = ln.strip_prefix("err ") {
                return Err(rest.to_string());
            }
            let Some(rest) = ln.strip_prefix("ap ") else { continue };
            let (mut ssid, mut bssid, mut rssi, mut freq, mut sec, mut connected) =
                (String::new(), String::new(), 0i32, 0u32, SecurityKind::Open, false);
            for kv in rest.split_whitespace() {
                if let Some(v) = kv.strip_prefix("ssid=") {
                    ssid = String::from_utf8_lossy(&b64_decode(v)).into_owned();
                } else if let Some(v) = kv.strip_prefix("bssid=") {
                    bssid = v.to_string();
                } else if let Some(v) = kv.strip_prefix("rssi=") {
                    rssi = v.parse().unwrap_or(0);
                } else if let Some(v) = kv.strip_prefix("freq=") {
                    freq = v.parse().unwrap_or(0);
                } else if let Some(v) = kv.strip_prefix("sec=") {
                    sec = parse_security(v);
                } else if let Some(v) = kv.strip_prefix("connected=") {
                    connected = v == "1";
                }
            }
            out.push(ScanResult {
                ssid,
                bssid,
                rssi,
                frequency_mhz: freq,
                security: sec,
                connected,
            });
        }
        Ok(out)
    }

    fn connect_new(&mut self, cfg: WifiConfig) -> Result<u32, String> {
        let psk = cfg.passphrase.unwrap_or_default();
        let line = format!(
            "wifi-connect {} {}\n",
            b64_encode(cfg.ssid.as_bytes()),
            b64_encode(psk.as_bytes()),
        );
        let reply = query_full(&line).map_err(|e| format!("arbiter unreachable: {e}"))?;
        let r = reply.trim();
        if r.starts_with("ok") {
            // The persisted saved-network id is assigned by the M3 WifiConfigManager;
            // until then connect-new associates but does not persist, so there is no
            // real handle to return. 0 = "connected, not yet saved".
            log::info!("wifi: connect-new {:?} -> {r}", cfg.ssid);
            Ok(0)
        } else {
            Err(r.trim_start_matches("err ").to_string())
        }
    }

    // ── M3 (saved-network store / WifiConfigManager) — not yet wired ──────────
    fn list_saved(&mut self) -> Vec<SavedNetwork> {
        Vec::new()
    }

    fn add_network(&mut self, _cfg: WifiConfig) -> Result<u32, String> {
        Err("saved-network store is M3 (use connect-new for now)".into())
    }

    fn update_network(&mut self, _id: u32, _cfg: WifiConfig) -> Result<(), String> {
        Err("saved-network store is M3".into())
    }

    fn remove_network(&mut self, id: u32) {
        log::info!("wifi: remove-network {id} ignored (saved-network store is M3)");
    }

    fn set_auto_connect(&mut self, id: u32, on: bool) {
        log::info!("wifi: set-auto-connect {id}={on} ignored (saved-network store is M3)");
    }

    fn connect(&mut self, _id: u32) -> Result<(), String> {
        Err("connect-by-saved-id is M3 (use connect-new)".into())
    }

    fn disconnect(&mut self) {
        log::info!("wifi: disconnect ignored (M3 — needs a daemon disconnect verb)");
    }

    fn forget_current(&mut self) {
        log::info!("wifi: forget-current ignored (M3 — saved-network store)");
    }
}
