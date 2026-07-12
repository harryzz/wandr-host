//! Desktop (non-Android) backend for `wasi:audio/pcm` — cpal.
//!
//! The Android backend is AudioFlinger-direct (a shared cblk ring); this is its
//! desktop peer, filling the `#[cfg(not(target_os = "android"))]` bodies of the
//! `audio_impl` dispatch functions. cpal is the cross-platform I/O layer (Linux
//! ALSA→PipeWire/Pulse, Windows WASAPI, macOS CoreAudio) — so this same file is
//! the Windows/macOS backend too. On WSLg it routes through the ALSA `default`
//! PCM → PipeWire → Windows.
//!
//! Model: the WIT is a guest-driven ring with backpressure (`write` returns
//! FRAMES accepted; `buffered-frames`/`position` pace the guest). cpal is
//! callback-driven, so a host ring bridges the two: `write` fills it, the
//! output callback drains it (silence on underrun/pause); capture is the mirror.
//! `pending_frames` = ring depth, so `position = written − pending` (the A/V-sync
//! clock) works exactly as on device.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio_impl::{ChannelLayout, TrackConfig};

/// Ring depth. A backpressure/latency budget, not device state — the guest
/// writes until `buffered-frames` hits this, then waits. ~0.5 s is comfortable
/// for media playback and low enough for the current no-call use.
const BUFFER_SECS: f32 = 0.5;

struct DeskStream {
    /// Kept alive for the stream's lifetime — dropping it stops the device.
    _stream: cpal::Stream,
    /// Shared with the cpal callback thread (interleaved f32 at `channels`).
    ring: Arc<Mutex<VecDeque<f32>>>,
    /// start/pause gate honored inside the callback.
    running: Arc<AtomicBool>,
    channels: usize,
    /// Max samples the ring holds (backpressure ceiling).
    cap: usize,
}

thread_local! {
    // cpal::Stream is !Send, so the registry lives on the host thread (the
    // standalone/winit loop that services the audio WIT calls). Only the ring
    // (Arc<Mutex>) crosses into cpal's callback thread.
    static STREAMS: RefCell<HashMap<u32, DeskStream>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u32> = const { Cell::new(1) };
}

fn channels_of(cfg: &TrackConfig) -> usize {
    match cfg.channel_layout {
        ChannelLayout::Mono => 1,
        ChannelLayout::Stereo => 2,
    }
}

fn alloc_handle() -> u32 {
    NEXT.with(|n| {
        let h = n.get();
        n.set(h.wrapping_add(1).max(1));
        h
    })
}

/// PulseAudio callback period (ms). With `BufferSize::Default` the cpal pulseaudio
/// backend passes an empty `pa_buffer_attr`, so the server chooses buffering — on
/// the WSLg RDP-bridged sink that's huge, infrequent callbacks and audible
/// dropouts. `BufferSize::Fixed(n)` pins `minimum_request_length` to one period so
/// the server asks for a regular small chunk each call, and `target_length` to two
/// periods of end-to-end latency (source-verified: cpal-0.18.1
/// `make_playback_buffer_attr`). 40 ms absorbs RDP jitter while staying responsive;
/// the 0.5 s ring (`BUFFER_SECS`) covers many periods.
const PULSE_CALLBACK_MS: u32 = 40;

fn stream_config(cfg: &TrackConfig, channels: usize, host_name: &str) -> cpal::StreamConfig {
    // Fixed period only for PulseAudio — WASAPI/CoreAudio can reject `Fixed` and
    // pick sane defaults themselves, so leave those on `Default`.
    let buffer_size = if host_name.eq_ignore_ascii_case("pulseaudio") {
        let period = (cfg.sample_rate * PULSE_CALLBACK_MS / 1000).max(1);
        cpal::BufferSize::Fixed(period)
    } else {
        cpal::BufferSize::Default
    };
    cpal::StreamConfig {
        channels: channels as u16,
        // cpal 0.18: SampleRate is a plain `u32` type alias (no wrapper).
        sample_rate: cfg.sample_rate,
        buffer_size,
    }
}

/// Build an OUTPUT stream at `dev_ch` device channels, up-mixing the ring's
/// `logical`-channel audio in the callback (mono→stereo duplicates the sample).
/// The ring always holds `logical`-channel frames — write_pcm_f32 is unchanged.
fn build_output_adapting(
    device: &cpal::Device, cfg: &TrackConfig, logical: usize, dev_ch: usize,
    host_name: &str, ring: &Arc<Mutex<VecDeque<f32>>>, running: &Arc<AtomicBool>,
) -> Option<cpal::Stream> {
    let config = stream_config(cfg, dev_ch, host_name);
    let (cb_ring, cb_running) = (Arc::clone(ring), Arc::clone(running));
    device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if !cb_running.load(Ordering::Relaxed) {
                data.iter_mut().for_each(|s| *s = 0.0);
                return;
            }
            let mut r = cb_ring.lock().unwrap_or_else(|e| e.into_inner());
            for frame in data.chunks_mut(dev_ch.max(1)) {
                let mut lf = [0f32; 2];
                for c in 0..logical {
                    let v = r.pop_front().unwrap_or(0.0);
                    if c < 2 { lf[c] = v; }
                }
                for (c, out) in frame.iter_mut().enumerate() {
                    *out = if logical <= 1 { lf[0] } else { lf[c % 2] };
                }
            }
        },
        move |err| log::warn!("audio_desktop: output stream error: {err}"),
        None,
    )
    .map_err(|e| log::warn!("audio_desktop: build_output({dev_ch}ch@{}Hz) err: {e}", cfg.sample_rate))
    .ok()
}

/// Build an INPUT stream at `dev_ch` device channels, down-mixing to the ring's
/// `logical`-channel audio (stereo→mono averages the channels). The ring always
/// holds `logical`-channel frames — read_pcm_f32 is unchanged.
fn build_input_adapting(
    device: &cpal::Device, cfg: &TrackConfig, logical: usize, dev_ch: usize,
    host_name: &str, ring: &Arc<Mutex<VecDeque<f32>>>, running: &Arc<AtomicBool>, cap: usize,
) -> Option<cpal::Stream> {
    let config = stream_config(cfg, dev_ch, host_name);
    let (cb_ring, cb_running) = (Arc::clone(ring), Arc::clone(running));
    device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            if !cb_running.load(Ordering::Relaxed) {
                return;
            }
            let mut r = cb_ring.lock().unwrap_or_else(|e| e.into_inner());
            for frame in data.chunks(dev_ch.max(1)) {
                for c in 0..logical {
                    let s = if logical <= 1 {
                        frame.iter().sum::<f32>() / frame.len().max(1) as f32
                    } else {
                        frame[c.min(frame.len().saturating_sub(1))]
                    };
                    if r.len() >= cap { r.pop_front(); }
                    r.push_back(s);
                }
            }
        },
        move |err| log::warn!("audio_desktop: input stream error: {err}"),
        None,
    )
    .map_err(|e| log::warn!("audio_desktop: build_input({dev_ch}ch@{}Hz) err: {e}", cfg.sample_rate))
    .ok()
}

/// Open an output track. Returns a handle, or 0 on any failure (surfaced to the
/// guest as `audio-error::unavailable`).
pub fn create_track(cfg: TrackConfig) -> u32 {
    let channels = channels_of(&cfg);
    let cap = ((cfg.sample_rate as f32 * BUFFER_SECS) as usize).max(1) * channels;

    let host = cpal::default_host();
    log::info!("audio_desktop: cpal host = {:?}", host.id());
    let Some(device) = host.default_output_device() else {
        log::warn!("audio_desktop: no default output device");
        return 0;
    };
    log::info!("audio_desktop: default_out_cfg = {:?}", device.default_output_config());
    let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(cap)));
    let running = Arc::new(AtomicBool::new(false));

    // Try the guest's channel layout; WASAPI shared mode rejects a config that
    // doesn't match the device mix format (mono on a stereo device — the Windows
    // "peer can't hear me" bug), so fall back to the device's channels + up-mix.
    let mut res = build_output_adapting(&device, &cfg, channels, channels, host.id().name(), &ring, &running);
    if res.is_none() {
        let d = device.default_output_config().map(|c| c.channels() as usize).unwrap_or(2);
        if d != channels {
            log::info!("audio_desktop: output {channels}ch rejected — retrying at device {d}ch (up-mix)");
            res = build_output_adapting(&device, &cfg, channels, d, host.id().name(), &ring, &running);
        }
    }
    let Some(stream) = res else {
        log::warn!("audio_desktop: output track failed ({channels}ch@{}Hz)", cfg.sample_rate);
        return 0;
    };
    // cpal streams start paused-until-play on some backends; play now, gate
    // audible output via `running` (set by `start`).
    if let Err(e) = stream.play() {
        log::warn!("audio_desktop: stream.play failed: {e}");
        return 0;
    }

    let h = alloc_handle();
    STREAMS.with(|m| {
        m.borrow_mut().insert(h, DeskStream { _stream: stream, ring, running, channels, cap });
    });
    log::info!("audio_desktop: opened playback track {h} ({channels}ch @ {}Hz)", cfg.sample_rate);
    h
}

/// Append interleaved f32 up to the ring ceiling; returns FRAMES accepted
/// (the guest retries the remainder — backpressure).
pub fn write_pcm_f32(track: u32, samples: &[f32]) -> u32 {
    STREAMS.with(|m| {
        let m = m.borrow();
        let Some(st) = m.get(&track) else { return 0 };
        let mut r = st.ring.lock().unwrap_or_else(|e| e.into_inner());
        let free = st.cap.saturating_sub(r.len());
        let n = samples.len().min(free);
        r.extend(&samples[..n]);
        (n / st.channels) as u32
    })
}

/// Frames buffered (playback) / available (capture) = ring depth in frames.
pub fn pending_frames(track: u32) -> u32 {
    STREAMS.with(|m| {
        let m = m.borrow();
        let Some(st) = m.get(&track) else { return 0 };
        let len = st.ring.lock().unwrap_or_else(|e| e.into_inner()).len();
        (len / st.channels) as u32
    })
}

pub fn start(track: u32) -> bool {
    STREAMS.with(|m| {
        let m = m.borrow();
        match m.get(&track) {
            Some(st) => {
                st.running.store(true, Ordering::Relaxed);
                log::info!("audio_desktop: start(track {track}) → running=true");
                true
            }
            None => { log::warn!("audio_desktop: start(track {track}) — no such track"); false }
        }
    })
}

pub fn pause(track: u32) -> bool {
    STREAMS.with(|m| {
        let m = m.borrow();
        match m.get(&track) {
            Some(st) => { st.running.store(false, Ordering::Relaxed); true }
            None => false,
        }
    })
}

/// Discard buffered frames (playback.flush / seek).
pub fn flush(track: u32) -> bool {
    STREAMS.with(|m| {
        let m = m.borrow();
        match m.get(&track) {
            Some(st) => { st.ring.lock().unwrap_or_else(|e| e.into_inner()).clear(); true }
            None => false,
        }
    })
}

/// Play out buffered frames then stop. The ring drains naturally via the
/// callback; we just keep `running` true so it finishes. (A precise
/// drain-then-pause is a refinement; for now this plays the tail out.)
pub fn drain(track: u32) -> bool {
    STREAMS.with(|m| m.borrow().contains_key(&track))
}

pub fn close(track: u32) {
    STREAMS.with(|m| { m.borrow_mut().remove(&track); }); // drops the Stream → stops
}

/// Open an input (capture) stream — mirror of `create_track`: the callback
/// pushes mic samples into the ring; `read_pcm_f32` drains them.
pub fn open_capture(cfg: TrackConfig) -> u32 {
    let channels = channels_of(&cfg);
    let cap = ((cfg.sample_rate as f32 * BUFFER_SECS) as usize).max(1) * channels;

    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        log::warn!("audio_desktop: no default input device");
        return 0;
    };
    let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(cap)));
    let running = Arc::new(AtomicBool::new(false));

    // Same as create_track: WASAPI shared mode rejects mono on a stereo mic
    // (the Windows "peer can't hear me" bug), so fall back to the device's
    // channels and down-mix to the guest's logical layout.
    let mut res = build_input_adapting(&device, &cfg, channels, channels, host.id().name(), &ring, &running, cap);
    if res.is_none() {
        let d = device.default_input_config().map(|c| c.channels() as usize).unwrap_or(2);
        if d != channels {
            log::info!("audio_desktop: input {channels}ch rejected — retrying at device {d}ch (down-mix)");
            res = build_input_adapting(&device, &cfg, channels, d, host.id().name(), &ring, &running, cap);
        }
    }
    let Some(stream) = res else {
        log::warn!("audio_desktop: capture failed ({channels}ch@{}Hz)", cfg.sample_rate);
        return 0;
    };
    if let Err(e) = stream.play() {
        log::warn!("audio_desktop: capture stream.play failed: {e}");
        return 0;
    }

    let h = alloc_handle();
    STREAMS.with(|m| {
        m.borrow_mut().insert(h, DeskStream { _stream: stream, ring, running, channels, cap });
    });
    log::info!("audio_desktop: opened capture {h} ({channels}ch @ {}Hz)", cfg.sample_rate);
    h
}

/// Drain up to `max_frames` frames of captured audio (may be fewer / empty).
pub fn read_pcm_f32(capture: u32, max_frames: u32) -> Vec<f32> {
    STREAMS.with(|m| {
        let m = m.borrow();
        let Some(st) = m.get(&capture) else { return Vec::new() };
        let mut r = st.ring.lock().unwrap_or_else(|e| e.into_inner());
        let want = (max_frames as usize) * st.channels;
        let n = want.min(r.len());
        r.drain(..n).collect()
    })
}
