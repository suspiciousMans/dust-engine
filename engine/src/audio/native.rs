use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

use rodio::source::{SineWave, Source};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};

use super::backend::{AudioBackend, LoopBuffer};
use super::tracks::TrackId;

/// `rodio` playback. After the device is opened, every call is
/// fallible-and-ignored: a sink that cannot be created just means a sound
/// is skipped, never a panic.
pub struct NativeBackend {
    // Must stay alive for the duration of playback — dropping it silences
    // everything, even sounds already `detach()`-ed.
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sinks: HashMap<TrackId, Sink>,
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
        let Ok(sink) = Sink::try_new(&self.handle) else { return };
        let channels = buffer.channels.max(1);
        let mut samples = buffer.samples.to_vec();
        if looped {
            // Begin mid-loop by rotating the buffer: every pass keeps the
            // same length and nothing has to be skipped while playing.
            let frames = buffer.frames().max(1);
            let start = ((start_secs.max(0.0) as f64 * buffer.sample_rate as f64) as usize) % frames;
            samples.rotate_left(start * channels as usize);
        }
        let source = rodio::buffer::SamplesBuffer::new(channels, buffer.sample_rate, samples);
        sink.set_volume(volume);
        let fade = Duration::from_secs_f32(fade_in_secs.max(0.0));
        match (looped, fade_in_secs > 0.0) {
            (true, true) => sink.append(source.repeat_infinite().fade_in(fade)),
            (true, false) => sink.append(source.repeat_infinite()),
            (false, true) => sink.append(source.fade_in(fade)),
            (false, false) => sink.append(source),
        }
        self.sinks.insert(id, sink);
    }

    fn set_track_volume(&mut self, id: TrackId, volume: f32) {
        if let Some(sink) = self.sinks.get(&id) {
            sink.set_volume(volume);
        }
    }

    fn stop_track(&mut self, id: TrackId) {
        if let Some(sink) = self.sinks.remove(&id) {
            sink.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioContext;

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
