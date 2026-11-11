//! The interface a playback backend implements (rodio natively, an SDL2
//! mixer in the browser), plus a device-free `NullBackend` for tests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::tracks::TrackId;

/// Decoded audio for a track: interleaved `f32` samples in -1..=1.
#[derive(Clone, Debug)]
pub struct LoopBuffer {
    pub samples: Arc<[f32]>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl LoopBuffer {
    pub fn new(samples: Vec<f32>, channels: u16, sample_rate: u32) -> Self {
        Self { samples: samples.into(), channels: channels.max(1), sample_rate }
    }

    pub fn mono(samples: Vec<f32>, sample_rate: u32) -> Self {
        Self::new(samples, 1, sample_rate)
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// Length of one pass in seconds (0 for an empty or rate-less buffer).
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frames() as f64 / self.sample_rate as f64
    }

    pub fn is_empty(&self) -> bool {
        self.duration_secs() <= 0.0
    }
}

/// Mono samples plus a sample rate: `audio.add_track("drums", (samples, 44_100))`.
impl From<(Vec<f32>, u32)> for LoopBuffer {
    fn from((samples, rate): (Vec<f32>, u32)) -> Self {
        LoopBuffer::mono(samples, rate)
    }
}

/// What a playback backend must do. All gain, fade, mute and loop-clock
/// decisions are made above this trait (in `AudioEngine`); a backend only
/// plays what it is told, at the volume it is given.
pub trait AudioBackend {
    /// Decodes a sound file into a buffer.
    fn load_file(&self, path: &Path) -> anyhow::Result<LoopBuffer>;
    /// Plays a file once, fire-and-forget.
    fn play_file_once(&self, path: &Path, volume: f32) -> anyhow::Result<()>;
    /// Plays a sine blip once.
    fn play_tone(&self, frequency_hz: f32, duration_secs: f32, volume: f32);
    /// Plays mono samples once.
    fn play_oneshot(&self, samples: &[f32], sample_rate: u32, volume: f32);
    /// Starts a track. `start_secs` is where in the buffer to begin (already
    /// wrapped to the buffer length for loops); `fade_in_secs` ramps the
    /// volume up from silence. Replaces nothing: the engine stops old tracks.
    fn start_track(
        &mut self,
        id: TrackId,
        buffer: &LoopBuffer,
        looped: bool,
        start_secs: f32,
        volume: f32,
        fade_in_secs: f32,
    );
    fn set_track_volume(&mut self, id: TrackId, volume: f32);
    fn stop_track(&mut self, id: TrackId);
}

/// One thing a `NullBackend` was asked to do.
#[derive(Clone, Debug, PartialEq)]
pub enum NullEvent {
    Oneshot { frames: usize, volume: f32 },
    Tone { frequency_hz: f32, volume: f32 },
    Start { id: TrackId, looped: bool, start_secs: f32, volume: f32, fade_in_secs: f32 },
    Volume { id: TrackId, volume: f32 },
    Stop { id: TrackId },
}

/// Plays nothing and records what it was asked to. Needs no audio device.
#[derive(Debug, Default)]
pub struct NullBackend {
    pub events: Vec<NullEvent>,
    /// Currently-playing tracks and their last volume.
    pub playing: HashMap<TrackId, f32>,
}

impl NullBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn volume(&self, id: TrackId) -> Option<f32> {
        self.playing.get(&id).copied()
    }
}

impl AudioBackend for NullBackend {
    fn load_file(&self, path: &Path) -> anyhow::Result<LoopBuffer> {
        anyhow::bail!("NullBackend cannot decode {}", path.display())
    }

    fn play_file_once(&self, _path: &Path, _volume: f32) -> anyhow::Result<()> {
        Ok(())
    }

    fn play_tone(&self, _frequency_hz: f32, _duration_secs: f32, _volume: f32) {}

    fn play_oneshot(&self, _samples: &[f32], _sample_rate: u32, _volume: f32) {}

    fn start_track(&mut self, id: TrackId, buffer: &LoopBuffer, looped: bool, start_secs: f32, volume: f32, fade_in_secs: f32) {
        let _ = buffer;
        self.events.push(NullEvent::Start { id, looped, start_secs, volume, fade_in_secs });
        self.playing.insert(id, volume);
    }

    fn set_track_volume(&mut self, id: TrackId, volume: f32) {
        self.events.push(NullEvent::Volume { id, volume });
        if let Some(v) = self.playing.get_mut(&id) {
            *v = volume;
        }
    }

    fn stop_track(&mut self, id: TrackId) {
        self.events.push(NullEvent::Stop { id });
        self.playing.remove(&id);
    }
}
