use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rodio::source::{SineWave, Source};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};

use super::backend::{AudioBackend, LoopBuffer};
use super::tracks::TrackId;

/// State shared between the audio thread (`PannedSource`) and the backend.
/// `f32`s are stored as bits; levels are non-negative so the bit pattern
/// orders like the value and `fetch_max` works.
struct Controls {
    left: AtomicU32,
    right: AtomicU32,
    peak: AtomicU32,
    /// Sink volume mirror, so `track_level` can report post-fader level.
    volume: AtomicU32,
}

impl Controls {
    fn new(left: f32, right: f32, volume: f32) -> Arc<Self> {
        Arc::new(Self {
            left: AtomicU32::new(left.to_bits()),
            right: AtomicU32::new(right.to_bits()),
            peak: AtomicU32::new(0),
            volume: AtomicU32::new(volume.to_bits()),
        })
    }
}

/// A buffer player that applies per-channel pan gains (left gain on even
/// output channels, right on odd) and tracks its peak output level. A mono
/// buffer is expanded to two output channels so it can actually be panned;
/// stereo and wider buffers keep their layout.
struct PannedSource {
    samples: Arc<[f32]>,
    channels: u16,
    out_channels: u16,
    sample_rate: u32,
    looped: bool,
    frame: usize,
    frames: usize,
    ch: u16,
    controls: Arc<Controls>,
    /// Peak since the last publish to `controls.peak`.
    local_peak: f32,
    since_publish: u32,
}

impl PannedSource {
    fn new(buffer: &LoopBuffer, looped: bool, start_frame: usize, controls: Arc<Controls>) -> Self {
        let channels = buffer.channels.max(1);
        let frames = buffer.frames();
        Self {
            samples: buffer.samples.clone(),
            channels,
            out_channels: channels.max(2),
            sample_rate: buffer.sample_rate.max(1),
            looped,
            frame: if looped && frames > 0 { start_frame % frames } else { 0 },
            frames,
            ch: 0,
            controls,
            local_peak: 0.0,
            since_publish: 0,
        }
    }

    fn publish(&mut self) {
        self.controls.peak.fetch_max(self.local_peak.to_bits(), Ordering::Relaxed);
        self.local_peak = 0.0;
        self.since_publish = 0;
    }
}

impl Iterator for PannedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.ch == 0 && self.frame >= self.frames {
            if !self.looped || self.frames == 0 {
                self.publish();
                return None;
            }
            self.frame = 0;
        }
        let src = if self.channels == 1 { 0 } else { self.ch as usize };
        let gain_bits = if self.ch % 2 == 0 { &self.controls.left } else { &self.controls.right };
        let gain = f32::from_bits(gain_bits.load(Ordering::Relaxed));
        let out = self.samples[self.frame * self.channels as usize + src] * gain;
        self.ch += 1;
        if self.ch >= self.out_channels {
            self.ch = 0;
            self.frame += 1;
        }
        self.local_peak = self.local_peak.max(out.abs());
        self.since_publish += 1;
        if self.since_publish >= 512 {
            self.publish();
        }
        Some(out)
    }
}

impl Source for PannedSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.out_channels
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

struct NativeTrack {
    sink: Sink,
    controls: Arc<Controls>,
}

/// `rodio` playback. After the device is opened, every call is
/// fallible-and-ignored: a sink that cannot be created just means a sound
/// is skipped, never a panic.
pub struct NativeBackend {
    // Must stay alive for the duration of playback — dropping it silences
    // everything, even sounds already `detach()`-ed.
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sinks: HashMap<TrackId, NativeTrack>,
}

impl NativeBackend {
    /// Opens the default output device; `Err` if there is none.
    pub fn open() -> anyhow::Result<Self> {
        let (stream, handle) = OutputStream::try_default()?;
        Ok(Self { _stream: stream, handle, sinks: HashMap::new() })
    }

    /// The rodio handle, for games that build their own sources.
    pub fn handle(&self) -> &OutputStreamHandle {
        &self.handle
    }
}

impl AudioBackend for NativeBackend {
    fn load_file(&self, path: &Path) -> anyhow::Result<LoopBuffer> {
        let file = File::open(path)?;
        let source = Decoder::new(BufReader::new(file))?;
        let channels = source.channels();
        let sample_rate = source.sample_rate();
        let samples: Vec<f32> = source.convert_samples().collect();
        Ok(LoopBuffer::new(samples, channels, sample_rate))
    }

    fn play_file_once(&self, path: &Path, volume: f32) -> anyhow::Result<()> {
        let file = File::open(path)?;
        let source = Decoder::new(BufReader::new(file))?;
        let sink = Sink::try_new(&self.handle)?;
        sink.set_volume(volume);
        sink.append(source);
        sink.detach();
        Ok(())
    }

    fn play_tone(&self, frequency_hz: f32, duration_secs: f32, volume: f32) {
        // `Duration::from_secs_f32` panics on negative/NaN/inf; the engine
        // sanitizes `duration_secs` before it gets here.
        let source = SineWave::new(frequency_hz)
            .take_duration(Duration::from_secs_f32(duration_secs))
            .amplify(0.3 * volume);
        if let Ok(sink) = Sink::try_new(&self.handle) {
            sink.append(source);
            sink.detach();
        }
    }

    fn play_oneshot(&self, samples: &[f32], sample_rate: u32, volume: f32) {
        let buffer = rodio::buffer::SamplesBuffer::new(1, sample_rate, samples.to_vec());
        if let Ok(sink) = Sink::try_new(&self.handle) {
            sink.set_volume(volume);
            sink.append(buffer);
            sink.detach();
        }
    }

    fn start_track(&mut self, id: TrackId, buffer: &LoopBuffer, looped: bool, start_secs: f32, volume: f32, fade_in_secs: f32) {
        self.start_track_panned(id, buffer, looped, start_secs, volume, fade_in_secs, 1.0, 1.0);
    }

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
        let Ok(sink) = Sink::try_new(&self.handle) else { return };
        // Begin mid-loop by starting the read cursor there: every pass keeps
        // the same length and nothing is skipped while playing.
        let start_frame = (start_secs.max(0.0) as f64 * buffer.sample_rate as f64) as usize;
        let controls = Controls::new(left, right, volume);
        let source = PannedSource::new(buffer, looped, start_frame, controls.clone());
        sink.set_volume(volume);
        if fade_in_secs > 0.0 {
            sink.append(source.fade_in(Duration::from_secs_f32(fade_in_secs)));
        } else {
            sink.append(source);
        }
        if let Some(old) = self.sinks.insert(id, NativeTrack { sink, controls }) {
            old.sink.stop();
        }
    }

    fn set_track_volume(&mut self, id: TrackId, volume: f32) {
        if let Some(t) = self.sinks.get(&id) {
            t.sink.set_volume(volume);
            t.controls.volume.store(volume.to_bits(), Ordering::Relaxed);
        }
    }

    fn stop_track(&mut self, id: TrackId) {
        if let Some(t) = self.sinks.remove(&id) {
            t.sink.stop();
        }
    }

    fn set_track_pan(&mut self, id: TrackId, left: f32, right: f32) {
        if let Some(t) = self.sinks.get(&id) {
            t.controls.left.store(left.to_bits(), Ordering::Relaxed);
            t.controls.right.store(right.to_bits(), Ordering::Relaxed);
        }
    }

    fn set_track_pitch(&mut self, id: TrackId, pitch: f32) {
        if let Some(t) = self.sinks.get(&id) {
            t.sink.set_speed(if pitch.is_finite() { pitch.clamp(0.1, 4.0) } else { 1.0 });
        }
    }

    /// Peak of the track's output since the last call, scaled by its volume.
    fn track_level(&self, id: TrackId) -> Option<f32> {
        let t = self.sinks.get(&id)?;
        let peak = f32::from_bits(t.controls.peak.swap(0, Ordering::Relaxed));
        let volume = f32::from_bits(t.controls.volume.load(Ordering::Relaxed));
        Some((peak * volume).clamp(0.0, 1.0))
    }

    fn supports_pan(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioContext;

    fn drain(src: &mut PannedSource, n: usize) -> Vec<f32> {
        src.by_ref().take(n).collect()
    }

    #[test]
    fn mono_buffer_is_expanded_to_two_channels_with_pan_gains() {
        let buf = LoopBuffer::mono(vec![1.0, 0.5], 44_100);
        let c = Controls::new(0.25, 0.75, 1.0);
        let mut src = PannedSource::new(&buf, false, 0, c.clone());
        assert_eq!(src.channels(), 2);
        assert_eq!(drain(&mut src, 10), vec![0.25, 0.75, 0.125, 0.375]);
        // Pan changes apply to the running source.
        let mut src = PannedSource::new(&buf, true, 0, c.clone());
        c.left.store(0.0f32.to_bits(), Ordering::Relaxed);
        c.right.store(1.0f32.to_bits(), Ordering::Relaxed);
        assert_eq!(drain(&mut src, 4), vec![0.0, 1.0, 0.0, 0.5]);
    }

    #[test]
    fn stereo_buffer_keeps_layout_and_loops_from_start_frame() {
        let buf = LoopBuffer::new(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 2, 44_100);
        let mut src = PannedSource::new(&buf, true, 4, Controls::new(1.0, 1.0, 1.0)); // 4 % 3 frames = 1
        assert_eq!(src.channels(), 2);
        assert_eq!(drain(&mut src, 6), vec![0.3, 0.4, 0.5, 0.6, 0.1, 0.2]);
    }

    #[test]
    fn peak_is_published_and_reset_on_read() {
        let buf = LoopBuffer::mono(vec![0.5; 4], 44_100);
        let c = Controls::new(1.0, 0.5, 1.0);
        let mut src = PannedSource::new(&buf, false, 0, c.clone());
        assert!(src.by_ref().count() > 0); // exhausting publishes the peak
        assert_eq!(f32::from_bits(c.peak.swap(0, Ordering::Relaxed)), 0.5);
        assert_eq!(c.peak.load(Ordering::Relaxed), 0);
    }

    /// Uses a real device when there is one and must not panic when there is
    /// not. Everything it plays is silent (gain 0 / volume 0).
    #[test]
    fn native_context_degrades_gracefully_without_a_device() {
        match AudioContext::new() {
            Err(e) => eprintln!("native audio: no device ({e}); playback not exercised"),
            Ok(mut audio) => {
                eprintln!("native audio: device opened; exercising playback silently");
                audio.add_track("a", (vec![0.1; 4410], 44_100)).set_gain(0.0);
                audio.add_track("b", (vec![0.1; 4410], 44_100)).set_gain(0.0).fade_to(0.0, 0.1);
                audio.set_music_volume(0.0);
                audio.update();
                audio.stop_track("a");
                audio.stop_music();
            }
        }
        // Stray ids on an open backend are ignored.
        if let Ok(mut b) = NativeBackend::open() {
            b.set_track_volume(TrackId(99), 0.5);
            b.stop_track(TrackId(99));
        }
    }
}
