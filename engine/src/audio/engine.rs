//! `AudioEngine`: named tracks (buses) with gain, fades, mute and one shared
//! loop clock, on top of any `AudioBackend`. This is the logic layer; the
//! backend only plays. `AudioContext` (in `mod.rs`) is this engine over the
//! platform backend.

use std::path::Path;
use std::time::Instant;

use super::backend::{AudioBackend, LoopBuffer};
use super::tracks::{LoopClock, TrackBank, TrackId};

/// Name of the track `play_music_*` uses.
pub const MUSIC_TRACK: &str = "music";
/// Name of the track `play_music_layer_samples` uses.
pub const LAYER_TRACK: &str = "layer";

enum TimeSource {
    /// Real elapsed time since the last `update`/`sync`.
    Wall(Instant),
    /// Only `advance(dt)` moves time (tests).
    Manual,
}

struct Pending {
    id: TrackId,
    buffer: LoopBuffer,
    fade_in_secs: f32,
}

fn sane_secs(s: f32) -> f32 {
    if s.is_finite() {
        s.max(0.0)
    } else {
        0.0
    }
}

pub struct AudioEngine<B: AudioBackend> {
    backend: B,
    bank: TrackBank,
    clock: LoopClock,
    time: TimeSource,
    /// Registered but not yet started in the backend; started when the
    /// `TrackHandle` returned by `add_track` is dropped, so a chained
    /// `.set_gain(0.5)` applies before any sound plays.
    pending: Vec<Pending>,
    music_volume: f32,
    sfx_volume: f32,
}

impl<B: AudioBackend> AudioEngine<B> {
    /// Uses real time for the loop clock and fades; call `update()` once a frame.
    pub fn with_backend(backend: B) -> Self {
        Self::build(backend, TimeSource::Wall(Instant::now()))
    }

    /// Time only moves through `advance(dt)`. For tests.
    pub fn with_manual_time(backend: B) -> Self {
        Self::build(backend, TimeSource::Manual)
    }

    fn build(backend: B, time: TimeSource) -> Self {
        Self {
            backend,
            bank: TrackBank::new(),
            clock: LoopClock::new(),
            time,
            pending: Vec::new(),
            music_volume: 1.0,
            sfx_volume: 1.0,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    // ---- time -----------------------------------------------------------

    /// Moves the loop clock and all fades forward by real elapsed time (a
    /// no-op with manual time). Call once per frame so fades progress.
    pub fn update(&mut self) {
        if let TimeSource::Wall(last) = &mut self.time {
            let now = Instant::now();
            let dt = now.duration_since(*last).as_secs_f64();
            *last = now;
            self.advance(dt);
        }
    }

    /// Moves the loop clock and all fades forward by `dt` seconds.
    pub fn advance(&mut self, dt: f64) {
        self.clock.advance(dt);
        for id in self.bank.tick(dt) {
            self.push_volume_id(id);
        }
    }

    /// Seconds on the shared loop clock.
    pub fn loop_clock_secs(&self) -> f64 {
        self.clock.now()
    }

    /// Where a looping track currently is inside its loop, in seconds.
    pub fn track_position(&self, name: &str) -> Option<f64> {
        let t = self.bank.get(name)?;
        Some(if t.looped { self.clock.position(t.len_secs, t.phase) } else { 0.0 })
    }

    // ---- tracks ---------------------------------------------------------

    /// Adds (or replaces) a looping track that joins the shared loop clock:
    /// it plays whatever part of its loop the clock is at, so it is in time
    /// with every other track. Starts at gain 1; chain to change it:
    /// `audio.add_track("drums", source).set_gain(0.5);`
    pub fn add_track(&mut self, name: &str, source: impl Into<LoopBuffer>) -> TrackHandle<'_, B> {
        self.add_track_with_offset(name, source, 0.0)
    }

    /// Like `add_track`, with a phase `offset_secs` against the shared clock.
    /// Equal-length tracks added with the same offset are always in time.
    pub fn add_track_with_offset(&mut self, name: &str, source: impl Into<LoopBuffer>, offset_secs: f32) -> TrackHandle<'_, B> {
        let id = self.register(name, source.into(), true, f64::from(sane_secs(offset_secs)), 0.0);
        TrackHandle { engine: self, name: name.to_string(), id }
    }

    /// `add_track_with_offset` with the arguments in the roadmap's order.
    pub fn play_loop(&mut self, name: &str, samples: Vec<f32>, sample_rate: u32, offset_secs: f32) -> TrackHandle<'_, B> {
        self.add_track_with_offset(name, (samples, sample_rate), offset_secs)
    }

    /// Decodes `path` and adds it as a looping track.
    pub fn add_track_file(&mut self, name: &str, path: &Path) -> anyhow::Result<TrackHandle<'_, B>> {
        let buffer = self.backend.load_file(path)?;
        Ok(self.add_track(name, buffer))
    }

    pub fn has_track(&self, name: &str) -> bool {
        self.bank.get(name).is_some()
    }

    pub fn track_names(&self) -> Vec<String> {
        self.bank.iter().map(|t| t.name.clone()).collect()
    }

    /// Current gain (mid-fade value), before mute and music volume.
    pub fn track_gain(&self, name: &str) -> Option<f32> {
        self.bank.get(name).map(|t| t.gain.value())
    }

    pub fn track_muted(&self, name: &str) -> Option<bool> {
        self.bank.get(name).map(|t| t.muted)
    }

    /// Sets a track's gain (0..=1) at once. Returns false if no such track.
    pub fn set_track_gain(&mut self, name: &str, gain: f32) -> bool {
        self.edit_track(name, |t| t.gain.set(gain))
    }

    /// Fades a track's gain to `gain` over `secs`; progresses with `update()`.
    pub fn fade_track(&mut self, name: &str, gain: f32, secs: f32) -> bool {
        self.edit_track(name, |t| t.gain.fade_to(gain, secs))
    }

    pub fn set_track_muted(&mut self, name: &str, muted: bool) -> bool {
        self.edit_track(name, |t| t.muted = muted)
    }

    /// Stops and removes a track. Returns false if there was none.
    pub fn stop_track(&mut self, name: &str) -> bool {
        match self.bank.remove(name) {
            Some(t) => {
                self.pending.retain(|p| p.id != t.id);
                self.backend.stop_track(t.id);
                true
            }
            None => false,
        }
    }

    pub fn stop_all_tracks(&mut self) {
        for t in self.bank.clear() {
            self.backend.stop_track(t.id);
        }
        self.pending.clear();
    }

    fn edit_track(&mut self, name: &str, f: impl FnOnce(&mut super::tracks::Track)) -> bool {
        let Some(t) = self.bank.get_mut(name) else { return false };
        f(t);
        let id = t.id;
        self.push_volume_id(id);
        true
    }

    fn effective_volume(&self, id: TrackId) -> Option<f32> {
        self.bank.by_id(id).map(|t| t.level() * self.music_volume)
    }

    fn push_volume_id(&mut self, id: TrackId) {
        if self.pending.iter().any(|p| p.id == id) {
            return; // not started yet; it starts at the then-current volume
        }
        if let Some(v) = self.effective_volume(id) {
            self.backend.set_track_volume(id, v);
        }
    }

    fn sync(&mut self) {
        // Only the clock/fades; same as `update` (kept separate for clarity).
        self.update();
    }

    /// Registers a track (stopping any with the same name) and queues its
    /// backend start. Returns None for an empty buffer (the old track is
    /// still removed).
    fn register(&mut self, name: &str, buffer: LoopBuffer, looped: bool, phase: f64, fade_in_secs: f32) -> Option<TrackId> {
        self.sync();
        if buffer.is_empty() {
            self.stop_track(name);
            return None;
        }
        let (id, replaced) = self.bank.insert(name, looped, buffer.duration_secs(), phase);
        if let Some(old) = replaced {
            self.pending.retain(|p| p.id != old.id);
            self.backend.stop_track(old.id);
        }
        self.pending.push(Pending { id, buffer, fade_in_secs });
        Some(id)
    }

    fn start_pending(&mut self, id: TrackId) {
        self.sync();
        let Some(i) = self.pending.iter().position(|p| p.id == id) else { return };
        let p = self.pending.remove(i);
        let Some(track) = self.bank.by_id(id) else { return };
        let start = if track.looped { self.clock.position(track.len_secs, track.phase) } else { 0.0 };
        let (looped, vol) = (track.looped, track.level() * self.music_volume);
        self.backend.start_track(id, &p.buffer, looped, start as f32, vol, p.fade_in_secs);
    }

    /// Registers, sets the initial gain, and starts right away (legacy path).
    fn start_now(&mut self, name: &str, buffer: LoopBuffer, looped: bool, position_secs: f32, fade_in: f32, gain: f32) {
        let phase = self.clock.phase_for_position(f64::from(sane_secs(position_secs)));
        if let Some(id) = self.register(name, buffer, looped, phase, fade_in) {
            if let Some(t) = self.bank.get_mut(name) {
                t.gain.set(gain);
            }
            self.start_pending(id);
        }
    }

    // ---- master volumes -------------------------------------------------

    /// Scales every track (music, layers, added tracks).
    pub fn set_music_volume(&mut self, volume: f32) {
        if volume.is_finite() {
            self.music_volume = volume.clamp(0.0, 1.0);
        }
        let ids: Vec<TrackId> = self.bank.iter().map(|t| t.id).collect();
        for id in ids {
            self.push_volume_id(id);
        }
    }

    pub fn music_volume(&self) -> f32 {
        self.music_volume
    }

    /// Volume for `play_tone`, `play_samples` and `play_sfx_file` (0..=1).
    pub fn set_sfx_volume(&mut self, volume: f32) {
        if volume.is_finite() {
            self.sfx_volume = volume.clamp(0.0, 1.0);
        }
    }

    pub fn sfx_volume(&self) -> f32 {
        self.sfx_volume
    }

    // ---- one-shots ------------------------------------------------------

    /// Plays a sound file once, fire-and-forget.
    pub fn play_sfx_file(&self, path: &Path) -> anyhow::Result<()> {
        self.backend.play_file_once(path, self.sfx_volume)
    }

    /// A procedural sine-wave blip — no asset file required. A bad duration
    /// (negative, NaN, infinite) is treated as zero and skipped.
    pub fn play_tone(&self, frequency_hz: f32, duration_secs: f32) {
        let secs = sane_secs(duration_secs);
        if secs <= 0.0 || self.sfx_volume <= 0.0 {
            return;
        }
        self.backend.play_tone(frequency_hz, secs, self.sfx_volume);
    }

    /// Plays raw mono samples (-1..=1) once, at the sfx volume.
    pub fn play_samples(&self, samples: &[f32], sample_rate: u32) {
        if samples.is_empty() || self.sfx_volume <= 0.0 {
            return;
        }
        self.backend.play_oneshot(samples, sample_rate, self.sfx_volume);
    }

    // ---- legacy music / layer API (now two named tracks) -----------------

    /// Plays (and replaces) the "music" track from a file.
    pub fn play_music_file(&mut self, path: &Path, looped: bool) -> anyhow::Result<()> {
        let buffer = self.backend.load_file(path)?;
        self.start_now(MUSIC_TRACK, buffer, looped, 0.0, 0.0, 1.0);
        Ok(())
    }

    /// Loops raw mono samples as the "music" track, replacing it. Starts
    /// `offset_secs` into the loop and fades in over 0.6s.
    pub fn play_music_samples(&mut self, samples: Vec<f32>, sample_rate: u32, offset_secs: f32) {
        if samples.is_empty() {
            self.stop_music();
            return;
        }
        self.start_now(MUSIC_TRACK, LoopBuffer::mono(samples, sample_rate), true, offset_secs, 0.6, 1.0);
    }

    /// Stops the music, the layer and every added track.
    pub fn stop_music(&mut self) {
        self.stop_all_tracks();
    }

    /// Loops a second track ("layer") over the music, silent until
    /// `set_music_layer_gain` raises it. `offset_secs` is the position in the
    /// loop to start at; give it the same value the music would be at.
    pub fn play_music_layer_samples(&mut self, samples: Vec<f32>, sample_rate: u32, offset_secs: f32) {
        self.stop_track(LAYER_TRACK);
        if samples.is_empty() {
            return;
        }
        self.start_now(LAYER_TRACK, LoopBuffer::mono(samples, sample_rate), true, offset_secs, 0.0, 0.0);
    }

    /// How loud the layer is, 0..=1 (scaled by the music volume).
    pub fn set_music_layer_gain(&mut self, gain: f32) {
        self.set_track_gain(LAYER_TRACK, gain);
    }
}

/// Chainable edits to a track just added with `add_track`. The track starts
/// playing when this is dropped (end of the statement), after the edits.
pub struct TrackHandle<'a, B: AudioBackend> {
    engine: &'a mut AudioEngine<B>,
    name: String,
    id: Option<TrackId>,
}

impl<B: AudioBackend> TrackHandle<'_, B> {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sets the gain (0..=1) at once.
    pub fn set_gain(&mut self, gain: f32) -> &mut Self {
        self.engine.set_track_gain(&self.name, gain);
        self
    }

    /// Fades the gain to `gain` over `secs` (progresses with `update()`).
    pub fn fade_to(&mut self, gain: f32, secs: f32) -> &mut Self {
        self.engine.fade_track(&self.name, gain, secs);
        self
    }

    pub fn set_muted(&mut self, muted: bool) -> &mut Self {
        self.engine.set_track_muted(&self.name, muted);
        self
    }
}

impl<B: AudioBackend> Drop for TrackHandle<'_, B> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.engine.start_pending(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::{NullBackend, NullEvent};

    fn engine() -> AudioEngine<NullBackend> {
        AudioEngine::with_manual_time(NullBackend::new())
    }

    fn loop_of(secs: f32) -> (Vec<f32>, u32) {
        (vec![0.1; (secs * 1000.0) as usize], 1000)
    }

    fn vol(a: &AudioEngine<NullBackend>, name: &str) -> f32 {
        let id = a.bank.get(name).unwrap().id;
        a.backend().volume(id).unwrap()
    }

    fn start_of(a: &AudioEngine<NullBackend>, name: &str) -> f32 {
        let id = a.bank.get(name).unwrap().id;
        a.backend()
            .events
            .iter()
            .find_map(|e| match e {
                NullEvent::Start { id: i, start_secs, .. } if *i == id => Some(*start_secs),
                _ => None,
            })
            .unwrap()
    }

    /// Exit criterion: three tracks, independent gains, one shared clock; the
    /// third layer is one line.
    #[test]
    fn three_tracks_independent_gains_shared_clock() {
        let mut a = engine();
        a.add_track("bass", loop_of(2.0)).set_gain(1.0);
        a.add_track("pads", loop_of(2.0)).set_gain(0.25);
        a.advance(0.7);
        a.add_track("drums", loop_of(2.0)).set_gain(0.5); // the one-line third layer

        assert_eq!(a.track_names(), ["bass", "pads", "drums"]);
        assert_eq!((vol(&a, "bass"), vol(&a, "pads"), vol(&a, "drums")), (1.0, 0.25, 0.5));

        // The late track joined mid-loop at the shared position, not at 0.
        assert!((start_of(&a, "drums") - 0.7).abs() < 1e-5);
        assert_eq!(start_of(&a, "bass"), 0.0);

        // Gains are independent: changing one leaves the others alone.
        a.set_track_gain("pads", 0.9);
        assert_eq!((vol(&a, "bass"), vol(&a, "pads"), vol(&a, "drums")), (1.0, 0.9, 0.5));

        // All three are at the same loop position after any amount of time.
        for _ in 0..500 {
            a.advance(0.0371);
        }
        let p = a.track_position("bass").unwrap();
        assert_eq!(a.track_position("pads"), Some(p));
        assert_eq!(a.track_position("drums"), Some(p));
        let expected = (0.7 + 500.0 * 0.0371f64).rem_euclid(2.0);
        assert!((p - expected).abs() < 1e-6, "{p} vs {expected}");
    }

    #[test]
    fn same_offset_equal_length_tracks_stay_aligned_regardless_of_start_time() {
        let mut a = engine();
        a.add_track_with_offset("a", loop_of(1.5), 0.4);
        a.advance(3.3);
        a.add_track_with_offset("b", loop_of(1.5), 0.4);
        // Started 3.3s apart, same offset: same position now and forever.
        for _ in 0..200 {
            assert_eq!(a.track_position("a"), a.track_position("b"));
            a.advance(0.31);
        }
        assert!((start_of(&a, "b") - ((3.3f32 + 0.4) % 1.5)).abs() < 1e-5);
    }

    #[test]
    fn music_volume_scales_all_tracks_and_mute_composes() {
        let mut a = engine();
        a.add_track("a", loop_of(1.0)).set_gain(0.5);
        a.add_track("b", loop_of(1.0)).set_gain(1.0);
        a.set_music_volume(0.5);
        assert_eq!((vol(&a, "a"), vol(&a, "b")), (0.25, 0.5));
        a.set_track_muted("a", true);
        assert_eq!(vol(&a, "a"), 0.0);
        assert_eq!(a.track_gain("a"), Some(0.5)); // gain remembered
        a.set_track_muted("a", false);
        assert_eq!(vol(&a, "a"), 0.25);
        a.set_music_volume(f32::NAN); // ignored
        assert_eq!(a.music_volume(), 0.5);
    }

    #[test]
    fn fade_progresses_with_advance_and_reaches_target() {
        let mut a = engine();
        a.add_track("a", loop_of(1.0)).set_gain(0.0).fade_to(1.0, 2.0);
        assert_eq!(vol(&a, "a"), 0.0);
        a.advance(0.5);
        assert!((vol(&a, "a") - 0.25).abs() < 1e-6);
        a.advance(1.5);
        assert_eq!(vol(&a, "a"), 1.0);
        let n = a.backend().events.len();
        a.advance(1.0); // fade finished: no further backend traffic
        assert_eq!(a.backend().events.len(), n);
    }

    #[test]
    fn track_starts_at_chained_gain_not_full_volume() {
        let mut a = engine();
        a.add_track("quiet", loop_of(1.0)).set_gain(0.2);
        let starts: Vec<f32> = a
            .backend()
            .events
            .iter()
            .filter_map(|e| if let NullEvent::Start { volume, .. } = e { Some(*volume) } else { None })
            .collect();
        assert_eq!(starts, [0.2]);
    }

    #[test]
    fn same_name_replaces_and_stop_removes() {
        let mut a = engine();
        a.add_track("x", loop_of(1.0));
        let first = a.bank.get("x").unwrap().id;
        a.add_track("x", loop_of(1.0));
        assert!(a.backend().volume(first).is_none());
        assert_eq!(a.backend().playing.len(), 1);
        assert!(a.stop_track("x"));
        assert!(!a.stop_track("x"));
        assert!(a.backend().playing.is_empty());
        assert!(!a.set_track_gain("x", 0.5));
    }

    #[test]
    fn empty_source_adds_nothing_and_does_not_panic() {
        let mut a = engine();
        a.add_track("e", (Vec::new(), 44_100)).set_gain(0.5);
        a.add_track("z", (vec![0.0; 10], 0)).set_gain(0.5);
        assert!(a.track_names().is_empty());
        assert!(a.backend().playing.is_empty());
    }

    #[test]
    fn legacy_music_and_layer_are_tracks_with_old_semantics() {
        let mut a = engine();
        a.play_music_samples(vec![0.1; 4000], 1000, 1.5);
        assert_eq!(a.track_names(), ["music"]);
        assert!((start_of(&a, "music") - 1.5).abs() < 1e-5);
        assert_eq!(vol(&a, "music"), 1.0);

        a.advance(1.0);
        a.play_music_layer_samples(vec![0.1; 4000], 1000, 2.5); // game clock is now 2.5
        assert_eq!(vol(&a, "layer"), 0.0); // silent until raised
        assert!((start_of(&a, "layer") - 2.5).abs() < 1e-5);
        assert_eq!(a.track_position("music"), a.track_position("layer"));

        a.set_music_volume(0.5);
        a.set_music_layer_gain(0.8);
        assert_eq!(vol(&a, "layer"), 0.4);
        assert_eq!(vol(&a, "music"), 0.5);

        // Fade-in on the music start, none on the layer.
        let fades: Vec<f32> = a
            .backend()
            .events
            .iter()
            .filter_map(|e| if let NullEvent::Start { fade_in_secs, .. } = e { Some(*fade_in_secs) } else { None })
            .collect();
        assert_eq!(fades, [0.6, 0.0]);

        a.stop_music();
        assert!(a.backend().playing.is_empty());
        assert!(a.track_names().is_empty());
    }

    #[test]
    fn bad_inputs_do_not_panic() {
        let mut a = engine();
        a.play_music_samples(vec![0.1; 100], 1000, f32::NAN);
        a.play_music_layer_samples(vec![0.1; 100], 1000, f32::INFINITY);
        a.play_music_layer_samples(vec![0.1; 100], 1000, -5.0);
        a.add_track_with_offset("t", loop_of(1.0), f32::NEG_INFINITY).set_gain(f32::NAN).fade_to(f32::NAN, f32::NAN);
        a.play_tone(440.0, f32::NAN);
        a.play_tone(440.0, -1.0);
        a.set_sfx_volume(f32::NAN);
        a.advance(f64::NAN);
        a.advance(-3.0);
        assert!(a.has_track("t"));
    }

    #[test]
    fn non_looped_file_style_track_starts_at_zero() {
        let mut a = engine();
        a.advance(5.0);
        a.start_now(MUSIC_TRACK, LoopBuffer::mono(vec![0.0; 3000], 1000), false, 0.0, 0.0, 1.0);
        assert_eq!(start_of(&a, "music"), 0.0);
        assert_eq!(a.track_position("music"), Some(0.0));
    }
}
