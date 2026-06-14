//! Host implementation of the `wasi:audio` draft (proposals/wasi-audio)
//! — Phase A of the consolidation. Resources wrap the SAME u32 track
//! handles the my:skiko-gfx/audio machinery ships (AudioFlinger-direct
//! ring, live-call-verified); methods delegate through the legacy trait
//! so both packages share one backend until Phase C.

use wasmtime::component::Resource;

use crate::HostState;
use crate::audio_impl as old;
use crate::wasi_audio_bindings::wasi::audio::pcm as wit;

/// Playback stream = a legacy output track handle, plus the cumulative
/// frames accepted by `write` (the `position` clock = written − buffered).
pub struct PlaybackRes {
    pub handle: u32,
    pub written: u64,
}
/// Capture stream = a legacy capture handle (shared handle space).
pub struct CaptureRes(pub u32);

fn old_config(c: &wit::StreamConfig) -> old::TrackConfig {
    old::TrackConfig {
        sample_rate: c.sample_rate,
        channel_layout: match c.channel_layout {
            wit::ChannelLayout::Mono => old::ChannelLayout::Mono,
            wit::ChannelLayout::Stereo => old::ChannelLayout::Stereo,
        },
        // The wire is f32 for both draft formats; the device path is
        // f32 — pcm-i16 needs no conversion here (format is a guest-
        // side description, the proven contract's model).
        format: old::Format::PcmF32,
        class: match c.class {
            wit::StreamClass::Media => old::StreamClass::Media,
            wit::StreamClass::VoiceCall => old::StreamClass::VoiceCall,
            wit::StreamClass::Notification => old::StreamClass::Notification,
        },
    }
}

impl wit::Host for HostState {}

impl wit::HostPlayback for HostState {
    fn open(
        &mut self,
        config: wit::StreamConfig,
    ) -> Result<Resource<PlaybackRes>, wit::AudioError> {
        // The legacy backend collapses failure causes into sentinel 0;
        // surfaced as `unavailable` (permission/config splits are a
        // backend-side refinement lane).
        let h = old::create_track(old_config(&config));
        if h == 0 {
            return Err(wit::AudioError::Unavailable);
        }
        Ok(self
            .table
            .push(PlaybackRes { handle: h, written: 0 })
            .map_err(|_| wit::AudioError::Unavailable)?)
    }

    fn write(&mut self, self_: Resource<PlaybackRes>, samples: Vec<f32>) -> u32 {
        let r = match self.table.get_mut(&self_) {
            Ok(r) => r,
            Err(_) => return 0,
        };
        let accepted = old::write_pcm_f32(r.handle, &samples);
        // `write` returns FRAMES accepted; accumulate for the position clock.
        r.written = r.written.saturating_add(accepted as u64);
        accepted
    }

    fn buffered_frames(&mut self, self_: Resource<PlaybackRes>) -> u32 {
        let h = match self.table.get(&self_) {
            Ok(r) => r.handle,
            Err(_) => return 0,
        };
        old::pending_frames(h)
    }

    fn position(&mut self, self_: Resource<PlaybackRes>) -> u64 {
        let r = match self.table.get(&self_) {
            Ok(r) => r,
            Err(_) => return 0,
        };
        // Frames consumed by the device = accepted − still-buffered. Monotonic
        // and never exceeds what was written (saturating guards the transient
        // where the ring reads slightly ahead of this thread's view).
        let pending = old::pending_frames(r.handle) as u64;
        r.written.saturating_sub(pending)
    }

    fn start(&mut self, self_: Resource<PlaybackRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.handle;
        if old::start(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn pause(&mut self, self_: Resource<PlaybackRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.handle;
        if old::pause(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn flush(&mut self, self_: Resource<PlaybackRes>) {
        let r = match self.table.get_mut(&self_) {
            Ok(r) => r,
            Err(_) => return,
        };
        let h = r.handle;
        // Dropped frames count as neither played nor pending — subtract them
        // from `written` so `position` (= written − buffered) stays continuous
        // instead of jumping forward by the discarded backlog.
        let dropped = old::pending_frames(h) as u64;
        old::flush(h);
        r.written = r.written.saturating_sub(dropped);
    }

    fn drain(&mut self, self_: Resource<PlaybackRes>) {
        if let Ok(r) = self.table.get(&self_) {
            old::drain(r.handle);
        }
    }

    fn drop(&mut self, rep: Resource<PlaybackRes>) -> wasmtime::Result<()> {
        let r = self.table.delete(rep)?;
        old::close(r.handle);
        Ok(())
    }
}

impl wit::HostCapture for HostState {
    fn open(
        &mut self,
        config: wit::StreamConfig,
    ) -> Result<Resource<CaptureRes>, wit::AudioError> {
        let h = old::open_capture(old_config(&config));
        if h == 0 {
            return Err(wit::AudioError::Unavailable);
        }
        Ok(self
            .table
            .push(CaptureRes(h))
            .map_err(|_| wit::AudioError::Unavailable)?)
    }

    fn read(&mut self, self_: Resource<CaptureRes>, max_frames: u32) -> Vec<f32> {
        let h = match self.table.get(&self_) {
            Ok(r) => r.0,
            Err(_) => return Vec::new(),
        };
        old::read_pcm_f32(h, max_frames)
    }

    fn available_frames(&mut self, self_: Resource<CaptureRes>) -> u32 {
        let h = match self.table.get(&self_) {
            Ok(r) => r.0,
            Err(_) => return 0,
        };
        old::pending_frames(h)
    }

    fn start(&mut self, self_: Resource<CaptureRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.0;
        if old::start(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn pause(&mut self, self_: Resource<CaptureRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.0;
        if old::pause(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn drop(&mut self, rep: Resource<CaptureRes>) -> wasmtime::Result<()> {
        let r = self.table.delete(rep)?;
        old::close(r.0);
        Ok(())
    }
}

pub fn add_to_linker(
    linker: &mut wasmtime::component::Linker<HostState>,
) -> wasmtime::Result<()> {
    crate::wasi_audio_bindings::AudioGuest::add_to_linker::<_, wasmtime::component::HasSelf<HostState>>(
        linker,
        |s| s,
    )
}
