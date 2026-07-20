use std::time::Instant;
use wandr_video::{open_decoder, open_encoder, Chunk, Codec, DecoderParams, EncoderParams, Rgb24Frame};
#[test]
fn bench_1080p_software_decode() {
    for (w, h, label) in [(1280u32, 720u32, "720p"), (1920, 1080, "1080p")] {
        let mut enc = open_encoder(&EncoderParams {
            codec: Codec::Vp9, width: w, height: h, bitrate_bps: 4_000_000, framerate: 30,
        }).unwrap();
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        let mut pkts = Vec::new();
        for i in 0..60u32 {
            let bar = (i * w / 60).min(w - 1);
            for y in 0..h { for x in 0..w {
                let o = ((y * w + x) * 3) as usize;
                let on = x.abs_diff(bar) < 16;
                rgb[o] = if on {250} else {(x*255/w) as u8};
                rgb[o+1] = if on {40} else {(y*255/h) as u8};
                rgb[o+2] = 90;
            }}
            enc.encode(Rgb24Frame::new(&rgb, w, h), i % 30 == 0).unwrap();
            while let Some(p) = enc.next_packet() { pkts.push(p.data); }
        }
        let mut dec = open_decoder(&DecoderParams{codec:Codec::Vp9,width:w,height:h}).unwrap();
        let t = Instant::now();
        let mut n = 0;
        for d in &pkts {
            dec.decode(Chunk::new(d, 0)).unwrap();
            while dec.next_frame().is_some() { n += 1; }
        }
        let el = t.elapsed();
        let per_frame_ms = el.as_secs_f64() * 1000.0 / n as f64;
        let fps = n as f64 / el.as_secs_f64();
        eprintln!("{label}: {n} frames decoded in {:.0} ms = {:.2} ms/frame = {fps:.0} fps \
                   ({:.1}x realtime at 30fps)", el.as_secs_f64()*1000.0, per_frame_ms, fps/30.0);
    }
}
