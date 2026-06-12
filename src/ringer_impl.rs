//! Host-side ringer appliers — the playback half of `wandr-arbiter-audio`'s Ringer.
//!
//! The arbiter decides (ringer mode) and pushes `ringtone start|stop` /
//! `haptics ring-start|ring-stop` to the call owner's host (this process). Here we
//! apply them: a generated ringtone loop over AAudio ([`crate::audio_impl`]) and a
//! repeating buzz over the vibrator HAL ([`crate::haptics_impl`]). Each runs on its
//! own thread gated by an atomic, joined on stop so a second ring can't overlap.
//!
//! Non-Android / no audio: `create_track` returns 0 and the thread exits cleanly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::audio_impl::{ChannelLayout, Format, StreamClass, TrackConfig};

const SAMPLE_RATE: u32 = 48_000;
/// Write granularity — 20 ms of stereo frames (matches the AAudio burst pacing).
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize / 1000 * 20) * 2;

static RINGING: AtomicBool = AtomicBool::new(false);
static RING_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static VIBRATING: AtomicBool = AtomicBool::new(false);
static VIB_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Start the ringtone loop (idempotent while already ringing).
pub fn ringtone_start() {
    let mut g = RING_THREAD.lock().unwrap();
    if g.is_some() {
        return;
    }
    RINGING.store(true, Ordering::SeqCst);
    *g = Some(thread::spawn(ring_loop));
}

/// Stop + join the ringtone thread (closes the AAudio track).
pub fn ringtone_stop() {
    RINGING.store(false, Ordering::SeqCst);
    if let Some(h) = RING_THREAD.lock().unwrap().take() {
        let _ = h.join();
    }
}

/// Start the repeating ring-vibrate (idempotent while already vibrating).
pub fn vibrate_start() {
    let mut g = VIB_THREAD.lock().unwrap();
    if g.is_some() {
        return;
    }
    VIBRATING.store(true, Ordering::SeqCst);
    *g = Some(thread::spawn(vibrate_loop));
}

/// Stop + join the vibrate thread.
pub fn vibrate_stop() {
    VIBRATING.store(false, Ordering::SeqCst);
    if let Some(h) = VIB_THREAD.lock().unwrap().take() {
        let _ = h.join();
    }
}

fn ring_loop() {
    // This device's MMAP output is stereo-only (see repros/call-live).
    let cfg = TrackConfig {
        sample_rate: SAMPLE_RATE,
        channel_layout: ChannelLayout::Stereo,
        format: Format::PcmF32,
        // Ignored on the routed path below (create_track_routed forces
        // Route::Ringtone → loudspeaker); set for record completeness.
        class: StreamClass::Notification,
    };
    // The ringtone is a fixed-intent route — the loud speaker, never the
    // earpiece (which the classless guest `create_track` defaults to). Express
    // that intent directly instead of inheriting the comms route.
    let track = crate::audio_impl::create_track_routed(cfg, crate::audio_routing::Route::Ringtone);
    if track == 0 {
        RINGING.store(false, Ordering::SeqCst);
        return;
    }
    let cycle = ring_cycle(); // ~2 s of stereo PCM, looped
    let mut pos = 0usize;

    // Prime the ring before start (write-then-start), then start.
    while pos < cycle.len() {
        let end = (pos + CHUNK_SAMPLES).min(cycle.len());
        let wrote = crate::audio_impl::write_pcm_f32(track, &cycle[pos..end]) as usize;
        if wrote == 0 {
            break; // ring full — primed
        }
        pos += wrote * 2; // frames → interleaved stereo samples
    }
    crate::audio_impl::start(track);

    while RINGING.load(Ordering::SeqCst) {
        if pos >= cycle.len() {
            pos = 0; // loop the cadence
        }
        let end = (pos + CHUNK_SAMPLES).min(cycle.len());
        let wrote = crate::audio_impl::write_pcm_f32(track, &cycle[pos..end]) as usize;
        if wrote == 0 {
            thread::sleep(Duration::from_millis(15)); // ring full — let it drain
            continue;
        }
        pos += wrote * 2;
    }
    crate::audio_impl::close(track);
}

/// One ~2 s ring cadence as interleaved-stereo f32: two ~0.4 s dual-tone pulses
/// ("ring-ring") then ~1 s of silence — recognizably a phone ring, looped.
fn ring_cycle() -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let total = (sr * 2.0) as usize; // 2 s of frames
    let mut out = Vec::with_capacity(total * 2);
    // A pleasant dual tone (a warble) — two musical-ish frequencies summed.
    let (f1, f2) = (587.33_f32, 880.0_f32); // D5 + A5
    // Pulse windows (seconds): on, off, on, off.
    let on = |t: f32| (t < 0.4) || (t >= 0.6 && t < 1.0);
    for i in 0..total {
        let t = i as f32 / sr;
        let s = if on(t) {
            // Short attack/release per pulse-edge to avoid clicks.
            let local = if t < 0.4 { t } else { t - 0.6 };
            let env = (local * 25.0).min(1.0).min(((0.4 - local) * 25.0).max(0.0));
            let tone = (std::f32::consts::TAU * f1 * t).sin()
                + (std::f32::consts::TAU * f2 * t).sin();
            0.3 * env * tone * 0.5
        } else {
            0.0
        };
        out.push(s); // L
        out.push(s); // R
    }
    out
}

fn vibrate_loop() {
    // Buzz ~0.5 s, pause ~1.2 s — a ring-like haptic cadence.
    while VIBRATING.load(Ordering::SeqCst) {
        crate::haptics_impl::vibrate_ms(500);
        // Sleep in small slices so `stop` is responsive.
        for _ in 0..17 {
            if !VIBRATING.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
