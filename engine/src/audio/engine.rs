//! `AudioEngine`: named tracks (buses) with gain, fades, mute and one shared
//! loop clock, on top of any `AudioBackend`. This is the logic layer; the
//! backend only plays. `AudioContext` (in `mod.rs`) is this engine over the
//! platform backend.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use super::backend::{AudioBackend, LoopBuffer};
use super::mixer::{Mixer, MixerError, MixerSettings, MixerSettingsError, MUSIC_BUS, SFX_BUS};
use super::spatial::Listener;
use super::tracks::{LoopClock, TrackBank, TrackId};
use super::voices::Voices;

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

pub(super) struct Pending {
    pub(super) id: TrackId,
    pub(super) buffer: LoopBuffer,
    pub(super) fade_in_secs: f32,
}

fn sane_secs(s: f32) -> f32 {
    if s.is_finite() {
        s.max(0.0)
    } else {
        0.0
    }
}

pub struct AudioEngine<B: AudioBackend> {
    pub(super) backend: B,
    pub(super) bank: TrackBank,
    pub(super) clock: LoopClock,
    time: TimeSource,
    /// Registered but not yet started in the backend; started when the
    /// `TrackHandle` returned by `add_track` is dropped, so a chained
    /// `.set_gain(0.5)` applies before any sound plays.
    pub(super) pending: Vec<Pending>,
    pub(super) mixer: Mixer,
    /// Track name -> bus, applied whenever a track of that name is created
    /// (persisted in `MixerSettings::routes`).
    pub(super) routes: HashMap<String, String>,
    pub(super) listener: Listener,
    pub(super) voices: Voices,
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
            mixer: Mixer::new(),
            routes: HashMap::new(),
            listener: Listener::default(),
            voices: Voices::default(),
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
        self.update_voices(dt);
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
        self.bank.iter().filter(|t| !t.name.starts_with('@')).map(|t| t.name.clone()).collect()
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
                self.voices.forget(t.id);
                self.backend.stop_track(t.id);
                true
            }
            None => false,
        }
    }

    pub fn stop_all_tracks(&mut self) {
        for t in self.bank.clear() {
            self.voices.forget(t.id);
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

    pub(super) fn any_track_solo(&self) -> bool {
        self.bank.iter().any(|t| t.solo)
    }

    /// Track gain x mixer chain (bus gains, mutes, solo).
    pub(super) fn effective_volume(&self, id: TrackId) -> Option<f32> {
        let t = self.bank.by_id(id)?;
        let spatial = self.voices.gain(id).unwrap_or(1.0);
        Some(t.level() * self.mixer.effective_gain(&t.bus, t.solo, self.any_track_solo()) * spatial)
    }

    pub(super) fn push_all_volumes(&mut self) {
        let ids: Vec<TrackId> = self.bank.iter().map(|t| t.id).collect();
        for id in ids {
            self.push_volume_id(id);
        }
    }

    pub(super) fn push_volume_id(&mut self, id: TrackId) {
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
    pub(super) fn register(&mut self, name: &str, buffer: LoopBuffer, looped: bool, phase: f64, fade_in_secs: f32) -> Option<TrackId> {
        self.sync();
        if buffer.is_empty() {
            self.stop_track(name);
            return None;
        }
        let (id, replaced) = self.bank.insert(name, looped, buffer.duration_secs(), phase);
        if let Some(old) = replaced {
            self.pending.retain(|p| p.id != old.id);
            self.voices.forget(old.id);
            self.backend.stop_track(old.id);
        }
        if let Some(bus) = self.routes.get(name).filter(|b| self.mixer.has_bus(b)).cloned() {
            if let Some(t) = self.bank.get_mut(name) {
                t.bus = bus;
            }
        }
        self.pending.push(Pending { id, buffer, fade_in_secs });
        Some(id)
    }

    pub(super) fn start_pending(&mut self, id: TrackId) {
        self.sync();
        let Some(i) = self.pending.iter().position(|p| p.id == id) else { return };
        let p = self.pending.remove(i);
        let Some(track) = self.bank.by_id(id) else { return };
        let start = if track.looped { self.clock.position(track.len_secs, track.phase) } else { 0.0 };
        let looped = track.looped;
        let vol = self.effective_volume(id).unwrap_or(0.0);
        match self.voices.pan_and_pitch(id) {
            Some(((l, r), pitch)) => {
                self.backend.start_track_panned(id, &p.buffer, looped, start as f32, vol, p.fade_in_secs, l, r);
                if let Some(pitch) = pitch {
                    self.backend.set_track_pitch(id, pitch);
                }
            }
            None => self.backend.start_track(id, &p.buffer, looped, start as f32, vol, p.fade_in_secs),
        }
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

    // ---- master volumes (the `music` and `sfx` buses) ----------------------

    /// Scales every track on the `music` bus (the default bus: music, layers
    /// and added tracks). Same as `set_bus_gain("music", ..)`.
    pub fn set_music_volume(&mut self, volume: f32) {
        self.set_bus_gain(MUSIC_BUS, volume);
    }

    pub fn music_volume(&self) -> f32 {
        self.mixer.gain(MUSIC_BUS).unwrap_or(1.0)
    }

    /// Volume for `play_tone`, `play_samples` and `play_sfx_file` (0..=1).
    /// Same as `set_bus_gain("sfx", ..)`.
    pub fn set_sfx_volume(&mut self, volume: f32) {
        self.set_bus_gain(SFX_BUS, volume);
    }

    pub fn sfx_volume(&self) -> f32 {
        self.mixer.gain(SFX_BUS).unwrap_or(1.0)
    }

    // ---- mixer: buses ----------------------------------------------------

    /// Read-only view of the bus tree.
    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// Adds a bus under `parent` (`"master"`, `"sfx"`, ...).
    pub fn add_bus(&mut self, name: &str, parent: &str) -> Result<(), MixerError> {
        self.mixer.add_bus(name, parent)
    }

    /// Removes a user bus; its child buses and tracks move to its parent.
    pub fn remove_bus(&mut self, name: &str) -> Result<(), MixerError> {
        let parent = self.mixer.remove_bus(name)?;
        for t in self.bank_mut_iter() {
            if t.bus == name {
                t.bus = parent.clone();
            }
        }
        self.routes.retain(|_, b| b != name);
        self.push_all_volumes();
        Ok(())
    }

    pub fn set_bus_parent(&mut self, name: &str, parent: &str) -> Result<(), MixerError> {
        self.mixer.set_parent(name, parent)?;
        self.push_all_volumes();
        Ok(())
    }

    /// Bus gain 0..=1 (NaN ignored). False for an unknown bus.
    pub fn set_bus_gain(&mut self, name: &str, gain: f32) -> bool {
        let ok = self.mixer.set_gain(name, gain);
        if ok {
            self.push_all_volumes();
        }
        ok
    }

    pub fn set_bus_muted(&mut self, name: &str, muted: bool) -> bool {
        let ok = self.mixer.set_muted(name, muted);
        if ok {
            self.push_all_volumes();
        }
        ok
    }

    pub fn set_bus_solo(&mut self, name: &str, solo: bool) -> bool {
        let ok = self.mixer.set_solo(name, solo);
        if ok {
            self.push_all_volumes();
        }
        ok
    }

    pub fn bus_gain(&self, name: &str) -> Option<f32> {
        self.mixer.gain(name)
    }

    pub fn bus_names(&self) -> Vec<String> {
        self.mixer.buses().iter().map(|b| b.name.clone()).collect()
    }

    // ---- mixer: tracks <-> buses --------------------------------------------

    /// Routes a track through `bus` (now if it exists, and again whenever a
    /// track of that name is created later). False if the bus is unknown.
    pub fn assign_track_bus(&mut self, track: &str, bus: &str) -> bool {
        if !self.mixer.has_bus(bus) {
            return false;
        }
        self.routes.insert(track.to_string(), bus.to_string());
        let id = self.bank.get_mut(track).map(|t| {
            t.bus = bus.to_string();
            t.id
        });
        if let Some(id) = id {
            self.push_volume_id(id);
        }
        true
    }

    pub fn track_bus(&self, track: &str) -> Option<&str> {
        self.bank.get(track).map(|t| t.bus.as_str())
    }

    pub fn set_track_solo(&mut self, name: &str, solo: bool) -> bool {
        let ok = self.bank.get_mut(name).map(|t| t.solo = solo).is_some();
        if ok {
            self.push_all_volumes();
        }
        ok
    }

    pub fn track_solo(&self, name: &str) -> Option<bool> {
        self.bank.get(name).map(|t| t.solo)
    }

    /// What the backend is actually told for this track: its own gain and
    /// mute x bus chain x solo (x spatial gain for spatial tracks).
    pub fn track_effective_gain(&self, name: &str) -> Option<f32> {
        let id = self.bank.get(name)?.id;
        self.effective_volume(id)
    }

    /// The multiplier fire-and-forget sounds on `bus` get.
    pub fn bus_effective_gain(&self, bus: &str) -> f32 {
        self.mixer.effective_gain(bus, false, self.any_track_solo())
    }

    /// Peak level (0..=1) of a track since last asked, if the backend
    /// measures it (`None`: gains only).
    pub fn track_level(&self, name: &str) -> Option<f32> {
        let id = self.bank.get(name)?.id;
        self.backend.track_level(id)
    }

    // ---- mixer: persistence --------------------------------------------------

    /// Current buses (gain, mute, solo, parent) and track routing.
    pub fn mixer_settings(&self) -> MixerSettings {
        let mut s = self.mixer.to_settings();
        s.routes = self.routes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        s
    }

    /// Applies saved settings; returns warnings for anything skipped.
    pub fn apply_mixer_settings(&mut self, settings: &MixerSettings) -> Vec<String> {
        let mut warnings = self.mixer.apply_settings(settings);
        for (track, bus) in &settings.routes {
            if self.mixer.has_bus(bus) {
                self.assign_track_bus(track, bus);
            } else {
                warnings.push(format!("route `{track}`: unknown bus `{bus}`"));
            }
        }
        self.push_all_volumes();
        warnings
    }

    pub fn save_mixer_settings(&self, path: &Path) -> Result<(), MixerSettingsError> {
        self.mixer_settings().save(path)
    }

    /// Loads and applies a RON file; returns the apply warnings.
    pub fn load_mixer_settings(&mut self, path: &Path) -> Result<Vec<String>, MixerSettingsError> {
        let s = MixerSettings::load(path)?;
        Ok(self.apply_mixer_settings(&s))
    }

    fn bank_mut_iter(&mut self) -> impl Iterator<Item = &mut super::tracks::Track> {
        self.bank.iter_mut()
    }

    // ---- one-shots ------------------------------------------------------

    /// Plays a sound file once, fire-and-forget, on the `sfx` bus.
    pub fn play_sfx_file(&self, path: &Path) -> anyhow::Result<()> {
        self.backend.play_file_once(path, self.bus_effective_gain(SFX_BUS))
    }

    /// A procedural sine-wave blip — no asset file required. A bad duration
    /// (negative, NaN, infinite) is treated as zero and skipped.
    pub fn play_tone(&self, frequency_hz: f32, duration_secs: f32) {
        let secs = sane_secs(duration_secs);
        let vol = self.bus_effective_gain(SFX_BUS);
        if secs <= 0.0 || vol <= 0.0 {
            return;
        }
        self.backend.play_tone(frequency_hz, secs, vol);
    }

    /// Plays raw mono samples (-1..=1) once, on the `sfx` bus.
    pub fn play_samples(&self, samples: &[f32], sample_rate: u32) {
        self.play_samples_on(SFX_BUS, samples, sample_rate);
    }

    /// Like `play_samples`, through any bus (e.g. `"ui"`). Unknown bus: silent.
    pub fn play_samples_on(&self, bus: &str, samples: &[f32], sample_rate: u32) {
        let vol = self.bus_effective_gain(bus);
        if samples.is_empty() || vol <= 0.0 {
            return;
        }
        self.backend.play_oneshot(samples, sample_rate, vol);
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

    // ---- mixer ------------------------------------------------------------

    #[test]
    fn track_effective_gain_is_track_times_bus_chain() {
        let mut a = engine();
        a.add_bus("footsteps", "sfx").unwrap();
        a.add_track("steps", loop_of(1.0)).set_gain(0.5);
        assert_eq!(a.track_bus("steps"), Some("music")); // default bus
        assert!(a.assign_track_bus("steps", "footsteps"));
        a.set_bus_gain("footsteps", 0.5);
        a.set_bus_gain("sfx", 0.5);
        a.set_bus_gain("master", 0.8);
        // 0.5 track * 0.5 * 0.5 * 0.8
        assert!((vol(&a, "steps") - 0.1).abs() < 1e-6, "{}", vol(&a, "steps"));
        assert_eq!(a.track_effective_gain("steps"), Some(vol(&a, "steps")));
        // The music volume no longer touches a track that left the music bus.
        a.set_music_volume(0.0);
        assert!((vol(&a, "steps") - 0.1).abs() < 1e-6);
        assert!(!a.assign_track_bus("steps", "nope"));
        assert!(!a.set_bus_gain("nope", 0.5));
        assert_eq!(a.track_bus("steps"), Some("footsteps"));
    }

    #[test]
    fn bus_mute_and_solo_reach_the_backend() {
        let mut a = engine();
        a.add_track("m", loop_of(1.0));
        a.add_track("s", loop_of(1.0));
        a.assign_track_bus("s", "sfx");
        a.set_bus_muted("master", true);
        assert_eq!((vol(&a, "m"), vol(&a, "s")), (0.0, 0.0));
        a.set_bus_muted("master", false);
        assert_eq!((vol(&a, "m"), vol(&a, "s")), (1.0, 1.0));
        // Solo the sfx bus: music goes quiet, sfx stays.
        a.set_bus_solo("sfx", true);
        assert_eq!((vol(&a, "m"), vol(&a, "s")), (0.0, 1.0));
        // Mute still beats solo.
        a.set_bus_muted("sfx", true);
        assert_eq!(vol(&a, "s"), 0.0);
        a.set_bus_muted("sfx", false);
        a.set_bus_solo("sfx", false);
        // Track solo: only the soloed track plays; music_volume composes.
        a.set_track_solo("m", true);
        assert_eq!((vol(&a, "m"), vol(&a, "s")), (1.0, 0.0));
        a.set_music_volume(0.5);
        assert_eq!(vol(&a, "m"), 0.5);
        a.set_track_solo("m", false);
        assert_eq!((vol(&a, "m"), vol(&a, "s")), (0.5, 1.0));
        assert_eq!(a.track_solo("m"), Some(false));
        // Bus changes with a fade running still land on the final value.
        a.fade_track("m", 0.5, 1.0);
        a.set_bus_gain("master", 0.5);
        a.advance(1.0);
        assert!((vol(&a, "m") - 0.125).abs() < 1e-6);
    }

    #[test]
    fn one_shots_follow_the_sfx_and_ui_buses() {
        let mut a = engine();
        a.play_tone(440.0, 0.1);
        a.play_samples(&[0.1; 8], 1000);
        a.set_sfx_volume(0.5);
        a.play_tone(440.0, 0.1);
        a.set_bus_gain("master", 0.5);
        a.play_tone(440.0, 0.1);
        a.play_samples_on("ui", &[0.1; 8], 1000);
        a.set_bus_muted("sfx", true);
        a.play_tone(440.0, 0.1); // muted: never reaches the backend
        a.play_samples_on("nope", &[0.1; 8], 1000); // unknown bus: silent
        let tones: Vec<f32> = a.backend().tones.borrow().iter().map(|t| t.1).collect();
        assert_eq!(tones, [1.0, 0.5, 0.25]);
        let shots: Vec<f32> = a.backend().oneshots.borrow().iter().map(|t| t.1).collect();
        assert_eq!(shots, [1.0, 0.5]); // sfx at 1.0, then ui under master 0.5
        assert_eq!(a.sfx_volume(), 0.5);
    }

    #[test]
    fn removing_a_bus_rehomes_its_tracks_and_routes_apply_to_new_tracks() {
        let mut a = engine();
        a.add_bus("amb", "music").unwrap();
        a.set_bus_gain("amb", 0.5);
        // Route set before the track exists; applied when it is created.
        assert!(a.assign_track_bus("rain", "amb"));
        a.add_track("rain", loop_of(1.0));
        assert_eq!(a.track_bus("rain"), Some("amb"));
        assert_eq!(vol(&a, "rain"), 0.5);
        a.set_bus_parent("amb", "sfx").unwrap();
        a.set_bus_gain("sfx", 0.5);
        assert_eq!(vol(&a, "rain"), 0.25);
        assert!(a.set_bus_parent("sfx", "amb").is_err()); // cycle
        a.remove_bus("amb").unwrap();
        assert_eq!(a.track_bus("rain"), Some("sfx"));
        assert_eq!(vol(&a, "rain"), 0.5);
        assert!(a.remove_bus("music").is_err());
        // The route died with the bus: a new "rain" lands on the default bus.
        a.add_track("rain", loop_of(1.0));
        assert_eq!(a.track_bus("rain"), Some("music"));
    }

    #[test]
    fn mixer_settings_roundtrip_through_ron_and_apply_live() {
        let mut a = engine();
        a.add_bus("amb", "music").unwrap();
        a.set_bus_gain("amb", 0.25);
        a.set_bus_gain("master", 0.5);
        a.set_bus_muted("ui", true);
        a.assign_track_bus("rain", "amb");
        let text = a.mixer_settings().to_ron().unwrap();
        assert!(text.contains("amb"));

        let mut b = engine();
        b.add_track("rain", loop_of(1.0)); // exists before settings load
        let s = MixerSettings::from_ron(&text).unwrap();
        assert!(b.apply_mixer_settings(&s).is_empty());
        assert_eq!(b.bus_gain("amb"), Some(0.25));
        assert_eq!(b.track_bus("rain"), Some("amb"));
        assert_eq!(vol(&b, "rain"), 0.125); // 1 * 0.25 * 0.5 pushed to the backend
        assert_eq!(b.mixer_settings(), a.mixer_settings());

        let path = std::env::temp_dir().join(format!("jame_engine_mixer_{}.ron", std::process::id()));
        a.save_mixer_settings(&path).unwrap();
        let mut c = engine();
        assert!(c.load_mixer_settings(&path).unwrap().is_empty());
        assert_eq!(c.mixer_settings(), a.mixer_settings());
        let _ = std::fs::remove_file(&path);
        assert!(c.load_mixer_settings(&path).is_err());
    }
}
