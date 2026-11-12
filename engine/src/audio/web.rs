use std::path::Path;
use std::sync::{Arc, Mutex};

use sdl2::sys as sdl;

use super::backend::{AudioBackend, LoopBuffer};
use super::tracks::TrackId;

const RATE: u32 = 44_100;

/// One playing sound: mono samples at `RATE`, resampled on load.
struct Voice {
    samples: Arc<Vec<f32>>,
    pos: usize,
    volume: f32,
    looped: bool,
    /// Linear fade-in: frames played so far / total (0 total = none).
    fade_pos: usize,
    fade_total: usize,
}

struct Mixer {
    sfx: Vec<Voice>,
    tracks: Vec<(TrackId, Voice)>,
}

static MIXER: Mutex<Mixer> = Mutex::new(Mixer { sfx: Vec::new(), tracks: Vec::new() });

fn next(v: &mut Voice) -> Option<f32> {
    if v.pos >= v.samples.len() {
        if !v.looped || v.samples.is_empty() {
            return None;
        }
        v.pos = 0;
    }
    let mut gain = v.volume;
    if v.fade_pos < v.fade_total {
        gain *= v.fade_pos as f32 / v.fade_total as f32;
        v.fade_pos += 1;
    }
    let s = v.samples[v.pos] * gain;
    v.pos += 1;
    Some(s)
}

unsafe extern "C" fn callback(_: *mut std::ffi::c_void, stream: *mut u8, len: i32) {
    let out = std::slice::from_raw_parts_mut(stream as *mut f32, len as usize / 4);
    out.fill(0.0);
    let Ok(mut m) = MIXER.try_lock() else { return };
    for o in out.iter_mut() {
        let mut acc = 0.0;
        for (_, v) in m.tracks.iter_mut() {
            acc += next(v).unwrap_or(0.0);
        }
        for v in m.sfx.iter_mut() {
            acc += next(v).unwrap_or(0.0);
        }
        *o = acc.clamp(-1.0, 1.0);
    }
    m.sfx.retain(|v| v.pos < v.samples.len());
    m.tracks.retain(|(_, v)| v.looped || v.pos < v.samples.len());
}

/// Downmixes interleaved samples to mono and linearly resamples to `RATE`.
fn to_mono_rate(samples: &[f32], channels: u16, rate: u32) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let mono: Vec<f32> = samples
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect();
    if rate == RATE || mono.is_empty() {
        return mono;
    }
    let step = rate as f64 / RATE as f64;
    let n = (mono.len() as f64 / step) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let i0 = x as usize;
            let i1 = (i0 + 1).min(mono.len() - 1);
            let t = (x - i0 as f64) as f32;
            mono[i0] * (1.0 - t) + mono[i1] * t
        })
        .collect()
}

/// Minimal RIFF/WAVE reader: 8/16/24/32-bit PCM and 32-bit float.
fn decode_wav(bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
    anyhow::ensure!(bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE", "not a WAV file");
    let (mut fmt, mut channels, mut rate, mut bits) = (1u16, 1u16, RATE, 16u16);
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &bytes[i + 8..(i + 8 + size).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            let samples: Vec<f32> = match (fmt, bits) {
                (3, 32) => body.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect(),
                (_, 8) => body.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect(),
                (_, 16) => body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
                (_, 24) => body
                    .chunks_exact(3)
                    .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0)
                    .collect(),
                (_, 32) => body
                    .chunks_exact(4)
                    .map(|b| i32::from_le_bytes(b.try_into().unwrap()) as f32 / 2_147_483_648.0)
                    .collect(),
                _ => anyhow::bail!("unsupported WAV format {fmt}/{bits}"),
            };
            return Ok(to_mono_rate(&samples, channels, rate));
        }
        i += 8 + size + (size & 1);
    }
    anyhow::bail!("WAV file has no data chunk")
}

/// Browser playback: an SDL2 audio callback mixing every track and one-shot
/// at `RATE`. The mixer state is a process-wide static because the SDL
/// callback is a bare C function.
///
/// **Spatial audio here is gain/distance only.** The device is opened mono, so
/// there is no stereo image to pan: `set_track_pan`, `set_track_pitch` and
/// `track_level` keep their trait defaults (ignored / ignored / `None`) and
/// `supports_pan()` is `false`. Spatial voices still get distance, cone and
/// bus attenuation through the track volume; the mixer panel shows gains
/// without level meters. (The native `rodio` backend pans, pitches and meters.)
pub struct WebBackend;

impl WebBackend {
    /// Opens the SDL audio device; `Err` if there is none.
    pub fn open() -> anyhow::Result<Self> {
        unsafe {
            if sdl::SDL_InitSubSystem(sdl::SDL_INIT_AUDIO) != 0 {
                anyhow::bail!("SDL audio init failed");
            }
            let mut want: sdl::SDL_AudioSpec = std::mem::zeroed();
            want.freq = RATE as i32;
            want.format = sdl::AUDIO_F32LSB as u16;
            want.channels = 1;
            want.samples = 2048;
            want.callback = Some(callback);
            let mut have: sdl::SDL_AudioSpec = std::mem::zeroed();
            let dev = sdl::SDL_OpenAudioDevice(std::ptr::null(), 0, &want, &mut have, 0);
            anyhow::ensure!(dev != 0, "no audio device");
            sdl::SDL_PauseAudioDevice(dev, 0);
        }
        Ok(WebBackend)
    }

    fn push_sfx(&self, samples: Vec<f32>, volume: f32) {
        if samples.is_empty() || volume <= 0.0 {
            return;
        }
        if let Ok(mut m) = MIXER.lock() {
            // A burst of sounds shouldn't pile up without bound.
            if m.sfx.len() >= 24 {
                m.sfx.remove(0);
            }
            m.sfx.push(Voice { samples: Arc::new(samples), pos: 0, volume, looped: false, fade_pos: 0, fade_total: 0 });
        }
    }
}

impl AudioBackend for WebBackend {
    fn load_file(&self, path: &Path) -> anyhow::Result<LoopBuffer> {
        Ok(LoopBuffer::mono(decode_wav(&std::fs::read(path)?)?, RATE))
    }

    fn play_file_once(&self, path: &Path, volume: f32) -> anyhow::Result<()> {
        let samples = decode_wav(&std::fs::read(path)?)?;
        self.push_sfx(samples, volume);
        Ok(())
    }

    fn play_tone(&self, frequency_hz: f32, duration_secs: f32, volume: f32) {
        let n = (duration_secs * RATE as f32) as usize;
        let w = std::f32::consts::TAU * frequency_hz / RATE as f32;
        let samples = (0..n).map(|i| (i as f32 * w).sin() * 0.3).collect();
        self.push_sfx(samples, volume);
    }

    fn play_oneshot(&self, samples: &[f32], sample_rate: u32, volume: f32) {
        self.push_sfx(to_mono_rate(samples, 1, sample_rate), volume);
    }

    fn start_track(&mut self, id: TrackId, buffer: &LoopBuffer, looped: bool, start_secs: f32, volume: f32, fade_in_secs: f32) {
        // The web mixer runs mono at `RATE`, so convert once up front.
        let samples = to_mono_rate(&buffer.samples, buffer.channels, buffer.sample_rate);
        if samples.is_empty() {
            return;
        }
        let pos = if looped { (start_secs.max(0.0) as f64 * RATE as f64) as usize % samples.len() } else { 0 };
        let voice = Voice {
            samples: Arc::new(samples),
            pos,
            volume,
            looped,
            fade_pos: 0,
            fade_total: (fade_in_secs.max(0.0) * RATE as f32) as usize,
        };
        if let Ok(mut m) = MIXER.lock() {
            m.tracks.retain(|(i, _)| *i != id);
            m.tracks.push((id, voice));
        }
    }

    fn set_track_volume(&mut self, id: TrackId, volume: f32) {
        if let Ok(mut m) = MIXER.lock() {
            if let Some((_, v)) = m.tracks.iter_mut().find(|(i, _)| *i == id) {
                v.volume = volume;
            }
        }
    }

    fn stop_track(&mut self, id: TrackId) {
        if let Ok(mut m) = MIXER.lock() {
            m.tracks.retain(|(i, _)| *i != id);
        }
    }
}
