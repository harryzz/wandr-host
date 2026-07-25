//! GStreamer decode backend, through the public `CodecBackend`/`Decoder` seam.
//! Feeds the committed cros-codecs test-25fps H.264 Annex-B clip (250 frames) via
//! `open_decoder_with`, forcing the `gstreamer-sw` or `gstreamer-hw` lane, and
//! asserts the whole clip decodes at the right resolution.
//!
//! Needs the `gstreamer` feature + a system GStreamer. The SW test needs libav
//! (`avdec_h264`); the HW test needs a VA/D3D11/VT decoder and SKIPS where none
//! exists (so it's a no-op on WSL/CI but a real check on a GPU box like popos).
#![cfg(feature = "gstreamer")]

use std::time::Duration;

use wandr_video::{open_decoder_with, Chunk, Codec, DecoderParams, FrameLocation, Preferences};

static CLIP: &[u8] =
    include_bytes!("../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");

/// Split an Annex-B stream into access units (one coded picture + its preceding
/// SPS/PPS/SEI). Same logic as tests/d3d11_hw.rs.
fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut sc = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            sc.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    sc.push(data.len());

    let mut aus: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_has_vcl = false;
    for w in sc.windows(2) {
        let nal = &data[w[0]..w[1]];
        let ntype = nal[3] & 0x1f;
        let is_vcl = ntype == 1 || ntype == 5;
        let first_mb_zero = is_vcl && (nal[4] & 0x80) != 0;
        if is_vcl && first_mb_zero && cur_has_vcl {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        cur.extend_from_slice(nal);
        if is_vcl {
            cur_has_vcl = true;
        }
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

/// Decode the whole clip through one forced backend lane. Returns (frames, dims),
/// or None if that lane can't open here (no such decoder — used to SKIP the HW
/// test on boxes without a hardware decoder).
fn decode_clip(prefs: Preferences) -> Option<(usize, (u32, u32))> {
    let aus = split_access_units(CLIP);
    assert!(aus.len() >= 240, "fixture split into {} AUs", aus.len());

    let params = DecoderParams { codec: Codec::H264, width: 320, height: 240 };
    let mut dec = match open_decoder_with(&params, prefs) {
        Ok(d) => d,
        Err(_) => return None,
    };

    let mut frames = 0usize;
    let mut dims = (0u32, 0u32);
    for (i, au) in aus.iter().enumerate() {
        dec.decode(Chunk::new(au, i as i64 * 40_000)).expect("decode AU");
        while let Some(f) = dec.next_frame() {
            frames += 1;
            dims = f.dimensions();
        }
    }
    dec.flush().expect("flush");
    // The block-draining flush() should have captured the tail; a short poll mops
    // up anything still trickling to the appsink.
    let mut idle = 0;
    while idle < 200 {
        if let Some(f) = dec.next_frame() {
            frames += 1;
            dims = f.dimensions();
            idle = 0;
        } else {
            std::thread::sleep(Duration::from_millis(5));
            idle += 1;
        }
    }
    Some((frames, dims))
}

#[test]
fn gstreamer_sw_decodes_test25fps() {
    let prefs = Preferences { no_hardware: true, backend: Some("gstreamer-sw"), ..Default::default() };
    let (frames, dims) = decode_clip(prefs).expect("gstreamer-sw must open (need avdec_h264)");
    eprintln!("gstreamer-sw: {frames} frames at {dims:?}");
    assert_eq!(dims, (320, 240));
    assert!(frames >= 250, "gstreamer-sw decoded only {frames}/250");
}

/// FAMILY pin: `backend: Some("gstreamer")` (no `-hw`/`-sw` suffix) must resolve
/// to a gstreamer lane chosen by the accel preference — `no_hardware` → the sw
/// lane. Before family matching landed, `"gstreamer"` matched no backend NAME
/// (they are `gstreamer-hw`/`gstreamer-sw`) so `open_decoder_with` errored. This
/// is the exact path the `s` key drives (`WANDR_VIDEO_BACKEND=gstreamer` + sw pref).
#[test]
fn gstreamer_family_sw_resolves() {
    let prefs = Preferences { no_hardware: true, backend: Some("gstreamer"), ..Default::default() };
    let (frames, dims) = decode_clip(prefs).expect("family 'gstreamer'+no_hw must resolve to gstreamer-sw");
    eprintln!("gstreamer family (no_hw): {frames} frames at {dims:?}");
    assert_eq!(dims, (320, 240));
    assert!(frames >= 250, "family sw lane decoded only {frames}/250");
}

/// FAMILY pin with a HW preference (the `h` key path). Resolves to gstreamer-hw
/// where a HW H.264 decoder exists, else SKIPs — matching the hw test's contract.
#[test]
fn gstreamer_family_hw_resolves() {
    let prefs =
        Preferences { require_hardware: true, backend: Some("gstreamer"), ..Default::default() };
    match decode_clip(prefs) {
        None => eprintln!("SKIP gstreamer family+hw: no hardware H.264 decoder on this box"),
        Some((frames, dims)) => {
            eprintln!("gstreamer family (hw): {frames} frames at {dims:?}");
            assert_eq!(dims, (320, 240));
            assert!(frames >= 250, "family hw lane decoded only {frames}/250");
        }
    }
}

#[test]
fn gstreamer_hw_decodes_test25fps() {
    // Force the HW lane. If no hardware H.264 decoder exists here, SKIP (no-op).
    let prefs = Preferences {
        require_hardware: true,
        backend: Some("gstreamer-hw"),
        ..Default::default()
    };
    match decode_clip(prefs) {
        None => eprintln!("SKIP gstreamer-hw: no hardware H.264 decoder on this box"),
        Some((frames, dims)) => {
            eprintln!("gstreamer-hw: {frames} frames at {dims:?}");
            assert_eq!(dims, (320, 240));
            assert!(frames >= 250, "gstreamer-hw decoded only {frames}/250");
        }
    }
}

/// Zero-copy GPU output: the HW lane hands back dma-buf `Frame::gpu` frames that
/// carry the NV12 fourcc + DRM modifier + planes the host compositor imports. Needs
/// a HW decoder AND `WANDR_VIDEO_GST_GPU=1` (opt-in), so it SKIPS elsewhere.
#[test]
fn gstreamer_hw_gpu_dmabuf() {
    if std::env::var("WANDR_VIDEO_GST_GPU").map(|v| v == "0").unwrap_or(true) {
        eprintln!("SKIP gstreamer-hw GPU: set WANDR_VIDEO_GST_GPU=1 (+ HW box) to run");
        return;
    }
    let aus = split_access_units(CLIP);
    let params = DecoderParams { codec: Codec::H264, width: 320, height: 240 };
    let prefs = Preferences {
        require_hardware: true,
        backend: Some("gstreamer-hw"),
        ..Default::default()
    };
    let mut dec = match open_decoder_with(&params, prefs) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("SKIP gstreamer-hw GPU: no hardware decoder");
            return;
        }
    };

    const NV12: u32 = b'N' as u32 | (b'V' as u32) << 8 | (b'1' as u32) << 16 | (b'2' as u32) << 24;
    let mut gpu_frames = 0usize;
    let mut meta: Option<(u32, u64, usize, (u32, u32))> = None;

    let mut drain = |dec: &mut Box<dyn wandr_video::Decoder>,
                     gpu_frames: &mut usize,
                     meta: &mut Option<(u32, u64, usize, (u32, u32))>| {
        while let Some(f) = dec.next_frame() {
            assert_eq!(f.location(), FrameLocation::Gpu, "GPU lane must emit Frame::gpu");
            let dims = f.dimensions();
            *gpu_frames += 1;
            if meta.is_none() {
                let gf = f.into_gpu().unwrap_or_else(|_| panic!("into_gpu"));
                *meta = Some((gf.fourcc, gf.modifier, gf.planes.len(), (gf.width, gf.height)));
            }
        }
    };

    for (i, au) in aus.iter().enumerate() {
        dec.decode(Chunk::new(au, i as i64 * 40_000)).expect("decode");
        drain(&mut dec, &mut gpu_frames, &mut meta);
    }
    dec.flush().expect("flush");
    let mut idle = 0;
    while idle < 200 {
        let before = gpu_frames;
        drain(&mut dec, &mut gpu_frames, &mut meta);
        if gpu_frames == before {
            std::thread::sleep(Duration::from_millis(5));
            idle += 1;
        } else {
            idle = 0;
        }
    }

    let (fourcc, modifier, planes, dims) = match meta {
        Some(m) => m,
        None => {
            // e.g. an old Intel i965 GPU whose VA plugin doesn't advertise dma-buf
            // export (not-negotiated). The code path is exercised on capable HW.
            eprintln!("SKIP gstreamer-hw GPU: this GPU/driver produced no dma-buf frames");
            return;
        }
    };
    eprintln!(
        "gstreamer-hw GPU: {gpu_frames} dma-buf frames, fourcc={fourcc:#x} modifier={modifier:#x} planes={planes} dims={dims:?}"
    );
    assert_eq!(fourcc, NV12, "expected NV12 dma-buf");
    assert_eq!(planes, 2, "NV12 has 2 planes (Y + interleaved UV)");
    assert_eq!(dims, (320, 240));
    assert!(gpu_frames >= 240, "GPU lane produced only {gpu_frames} frames");
}
