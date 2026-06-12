//! Host implementation of the `wasi:audio` draft (proposals/wasi-audio)
//! — Phase A of the consolidation. Resources wrap the SAME u32 track
//! handles the my:skiko-gfx/audio machinery ships (AudioFlinger-direct
//! ring, live-call-verified); methods delegate through the legacy trait
//! so both packages share one backend until Phase C.

use wasmtime::component::Resource;

use crate::HostState;
use crate::audio_impl as old;
use crate::wasi_audio_bindings::wasi::audio::pcm as wit;

/// Playback stream = a legacy output track handle.
pub struct PlaybackRes(pub u32);
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
            .push(PlaybackRes(h))
            .map_err(|_| wit::AudioError::Unavailable)?)
    }

    fn write(&mut self, self_: Resource<PlaybackRes>, samples: Vec<f32>) -> u32 {
        let h = match self.table.get(&self_) {
            Ok(r) => r.0,
            Err(_) => return 0,
        };
        old::write_pcm_f32(h, &samples)
    }

    fn buffered_frames(&mut self, self_: Resource<PlaybackRes>) -> u32 {
        let h = match self.table.get(&self_) {
            Ok(r) => r.0,
            Err(_) => return 0,
        };
        old::pending_frames(h)
    }

    fn start(&mut self, self_: Resource<PlaybackRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.0;
        if old::start(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn pause(&mut self, self_: Resource<PlaybackRes>) -> Result<(), wit::AudioError> {
        let h = self.table.get(&self_).map_err(|_| wit::AudioError::Unavailable)?.0;
        if old::pause(h) {
            Ok(())
        } else {
            Err(wit::AudioError::Unavailable)
        }
    }

    fn drop(&mut self, rep: Resource<PlaybackRes>) -> wasmtime::Result<()> {
        let r = self.table.delete(rep)?;
        old::close(r.0);
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
