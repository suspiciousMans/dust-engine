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

    // ---- optional spatial / metering capabilities ------------------------
    //
    // Additive, default-bodied methods: a backend that cannot do these keeps
    // working (spatial voices then get distance gain only) and existing
    // implementors need no change.

    /// Starts a track that will be panned: like `start_track`, plus initial
    /// per-channel gains. A backend that can pan should override this so a
    /// mono source is routed to two output channels from the first sample.
    /// The default starts the track and calls `set_track_pan`.
    #[allow(clippy::too_many_arguments)]
    fn start_track_panned(
        &mut self,
        id: TrackId,
        buffer: &LoopBuffer,
        looped: bool,
        start_secs: f32,
        volume: f32,
        fade_in_secs: f32,
        left: f32,
        right: f32,
    ) {
        self.start_track(id, buffer, looped, start_secs, volume, fade_in_secs);
        self.set_track_pan(id, left, right);
    }

    /// Per-channel gain multipliers for a track started with
    /// `start_track_panned` (`(1, 1)` = unpanned; the engine passes the
    /// equal-power pair from `spatial::pan_gains`, `left^2 + right^2 == 1`).
    /// Default: ignored ("gain/distance only").
    fn set_track_pan(&mut self, _id: TrackId, _left: f32, _right: f32) {}

    /// Playback-rate multiplier (doppler). 1 = normal. Default: ignored.
    fn set_track_pitch(&mut self, _id: TrackId, _pitch: f32) {}

    /// Peak output level (0..=1) of a track since the last call, if this
    /// backend can measure it. Default `None`: the mixer UI then shows gains
    /// only.
    fn track_level(&self, _id: TrackId) -> Option<f32> {
        None
    }

    /// Whether `set_track_pan` does anything on this backend.
    fn supports_pan(&self) -> bool {
        false
    }
}

/// One thing a `NullBackend` was asked to do.
#[derive(Clone, Debug, PartialEq)]
pub enum NullEvent {
    Oneshot { frames: usize, volume: f32 },
    Tone { frequency_hz: f32, volume: f32 },
    Start { id: TrackId, looped: bool, start_secs: f32, volume: f32, fade_in_secs: f32 },
    Volume { id: TrackId, volume: f32 },
    Stop { id: TrackId },
    Pan { id: TrackId, left: f32, right: f32 },
    Pitch { id: TrackId, pitch: f32 },
}

/// Plays nothing and records what it was asked to. Needs no audio device.
#[derive(Debug, Default)]
pub struct NullBackend {
    pub events: Vec<NullEvent>,
    /// Currently-playing tracks and their last volume.
    pub playing: HashMap<TrackId, f32>,
    /// Last pan (left, right) per playing track.
    pub pans: HashMap<TrackId, (f32, f32)>,
    /// Last pitch per playing track.
    pub pitches: HashMap<TrackId, f32>,
    /// Levels `track_level` reports; set from tests to drive meters.
    pub levels: HashMap<TrackId, f32>,
    /// Fire-and-forget one-shots, in call order: (frames, volume).
    pub oneshots: std::cell::RefCell<Vec<(usize, f32)>>,
    /// `play_tone` calls, in order: (frequency_hz, volume).
    pub tones: std::cell::RefCell<Vec<(f32, f32)>>,
}

impl NullBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn volume(&self, id: TrackId) -> Option<f32> {
        self.playing.get(&id).copied()
    }

    pub fn pan(&self, id: TrackId) -> Option<(f32, f32)> {
        self.pans.get(&id).copied()
    }

    pub fn pitch(&self, id: TrackId) -> Option<f32> {
        self.pitches.get(&id).copied()
    }
}

impl AudioBackend for NullBackend {
    fn load_file(&self, path: &Path) -> anyhow::Result<LoopBuffer> {
        anyhow::bail!("NullBackend cannot decode {}", path.display())
    }

    fn play_file_once(&self, _path: &Path, _volume: f32) -> anyhow::Result<()> {
        Ok(())
    }

    fn play_tone(&self, frequency_hz: f32, _duration_secs: f32, volume: f32) {
        self.tones.borrow_mut().push((frequency_hz, volume));
    }

    fn play_oneshot(&self, samples: &[f32], _sample_rate: u32, volume: f32) {
        self.oneshots.borrow_mut().push((samples.len(), volume));
    }

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
        self.pans.remove(&id);
        self.pitches.remove(&id);
    }

    fn set_track_pan(&mut self, id: TrackId, left: f32, right: f32) {
        self.events.push(NullEvent::Pan { id, left, right });
        self.pans.insert(id, (left, right));
    }

    fn set_track_pitch(&mut self, id: TrackId, pitch: f32) {
        self.events.push(NullEvent::Pitch { id, pitch });
        self.pitches.insert(id, pitch);
    }

    fn track_level(&self, id: TrackId) -> Option<f32> {
        self.levels.get(&id).copied()
    }

    fn supports_pan(&self) -> bool {
        true
    }
}
