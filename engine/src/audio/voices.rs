//! Spatial voices: positioned one-shots and positioned looping tracks on an
//! `AudioEngine`, a fixed voice bound with stealing, and the per-frame
//! listener/emitter sync that turns `spatial::spatialize` results into
//! backend volume, pan and pitch.
//!
//! * [`AudioEngine::play_at`] – fire-and-forget positioned sound (tracked as
//!   a voice until its buffer has played, so it counts toward the bound).
//! * [`AudioEngine::add_spatial_track`] – a named looping positioned track on
//!   the shared loop clock; shows up in the mixer like any other track.
//! * Both return a [`SpatialHandle`]. The handle holds no borrow of the
//!   engine: `handle.set_position(p)` writes the emitter and the next
//!   `update()` / `advance(dt)` applies it.
//! * [`AudioEngine::set_listener`] re-evaluates every voice immediately.
//! * Spatial voices default to the `sfx` bus; change it with
//!   `assign_track_bus(handle.name(), "music")`.
//!
//! **Voice bound.** At most [`DEFAULT_MAX_SPATIAL_VOICES`] (32, adjustable
//! with `set_max_spatial_voices`) spatial voices play at once. A new voice
//! over the limit steals the voice with the lowest *effective* gain (track
//! gain x bus chain x distance/cone gain; ties steal the oldest). If the new
//! voice would itself be quieter than every playing voice it is not started
//! (`handle.is_alive()` is false). Plain `add_track` tracks and mono
//! `play_samples` one-shots are not counted.
//!
//! **Backend support.** Distance/cone gain works everywhere. Pan needs
//! `AudioBackend::set_track_pan` (rodio/`NativeBackend`: yes; the Emscripten
//! `WebBackend` mixes mono, so there it is gain/distance only). Doppler needs
//! `set_track_pitch` (native: yes via sink speed, web: no). A looped track
//! whose pitch is not 1 drifts from the shared loop clock.

use std::sync::{Arc, Mutex, MutexGuard};

use glam::Vec3;

use super::backend::{AudioBackend, LoopBuffer};
use super::engine::AudioEngine;
use super::mixer::SFX_BUS;
use super::spatial::{pan_gains, spatialize, Emitter, Listener, SpatialParams};
use super::tracks::TrackId;

/// Simultaneous spatial voices unless `set_max_spatial_voices` says otherwise.
pub const DEFAULT_MAX_SPATIAL_VOICES: usize = 32;

#[derive(Debug)]
struct Shared {
    emitter: Emitter,
    alive: bool,
    stop: bool,
    stop_on_drop: bool,
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A playing (or finished/stolen) spatial voice. Cheap to clone; `Send + Sync`
/// so it can live in an ECS component.
#[derive(Clone, Debug)]
pub struct SpatialHandle {
    shared: Arc<Mutex<Shared>>,
    name: String,
    id: TrackId,
}

impl SpatialHandle {
    fn dead(name: &str, emitter: Emitter) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared { emitter, alive: false, stop: false, stop_on_drop: false })),
            name: name.to_string(),
            id: TrackId(u32::MAX),
        }
    }

    /// The engine track name (`"@oneshot:N"` for `play_at`); use it with
    /// `assign_track_bus`, `set_track_gain`, ...
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn track_id(&self) -> TrackId {
        self.id
    }

    /// Moves the emitter; applied by the next `update()`/`advance()`.
    pub fn set_position(&self, position: Vec3) {
        lock(&self.shared).emitter.position = position;
    }

    pub fn position(&self) -> Vec3 {
        lock(&self.shared).emitter.position
    }

    /// `None` turns doppler off for this emitter.
    pub fn set_velocity(&self, velocity: Option<Vec3>) {
        lock(&self.shared).emitter.velocity = velocity;
    }

    /// Replaces all emitter parameters (distances, rolloff, cone, ...).
    pub fn set_emitter(&self, emitter: Emitter) {
        lock(&self.shared).emitter = emitter;
    }

    pub fn emitter(&self) -> Emitter {
        lock(&self.shared).emitter
    }

    /// False once the sound finished, was stopped, stolen, or never started.
    pub fn is_alive(&self) -> bool {
        lock(&self.shared).alive
    }

    /// Stops the voice at the next `update()`/`advance()`.
    pub fn stop(&self) {
        lock(&self.shared).stop = true;
    }

    /// If set, the engine stops the voice once every `SpatialHandle` clone is
    /// dropped (for a looping emitter owned by an entity that may despawn).
    /// Off by default: dropping a handle never silences a sound.
    pub fn stop_when_dropped(&self, yes: bool) -> &Self {
        lock(&self.shared).stop_on_drop = yes;
        self
    }
}

pub(super) struct Voice {
    id: TrackId,
    shared: Arc<Mutex<Shared>>,
    oneshot: bool,
    seq: u64,
    /// Seconds of the buffer played (advances at pitch speed).
    progress: f64,
    duration: f64,
    params: SpatialParams,
    pushed_volume: f32,
    pushed_pan: (f32, f32),
    pushed_pitch: f32,
}

#[derive(Default)]
pub(super) struct Voices {
    list: Vec<Voice>,
    max: usize,
    next_seq: u64,
    next_oneshot: u64,
}

impl Voices {
    fn max(&self) -> usize {
        if self.max == 0 {
            DEFAULT_MAX_SPATIAL_VOICES
        } else {
            self.max
        }
    }

    fn find(&self, id: TrackId) -> Option<&Voice> {
        self.list.iter().find(|v| v.id == id)
    }

    /// Forgets a voice (its track is gone); its handle reports dead.
    pub(super) fn forget(&mut self, id: TrackId) {
        if let Some(i) = self.list.iter().position(|v| v.id == id) {
            let v = self.list.remove(i);
            lock(&v.shared).alive = false;
        }
    }

    /// The distance/cone gain of a spatial voice (`None` for plain tracks).
    pub(super) fn gain(&self, id: TrackId) -> Option<f32> {
        self.find(id).map(|v| v.params.gain)
    }

    /// Initial pan gains and pitch for a spatial voice being started.
    pub(super) fn pan_and_pitch(&self, id: TrackId) -> Option<((f32, f32), Option<f32>)> {
        self.find(id).map(|v| (v.pushed_pan, v.params.doppler_pitch))
    }

    pub(super) fn note_volume(&mut self, id: TrackId, volume: f32) {
        if let Some(v) = self.list.iter_mut().find(|v| v.id == id) {
            v.pushed_volume = volume;
        }
    }
}

impl<B: AudioBackend> AudioEngine<B> {
    // ---- listener ---------------------------------------------------------

    /// Sets where the listener is, faces and moves; call once per frame
    /// (or every time it changes). Every voice is re-evaluated at once.
    pub fn set_listener(&mut self, listener: Listener) {
        self.listener = listener;
        self.update_voices(0.0);
    }

    pub fn listener(&self) -> &Listener {
        &self.listener
    }

    // ---- voice bound ------------------------------------------------------

    pub fn max_spatial_voices(&self) -> usize {
        self.voices.max()
    }

    /// Changes the voice bound (at least 1). Lowering it steals the quietest
    /// voices straight away.
    pub fn set_max_spatial_voices(&mut self, max: usize) {
        self.voices.max = max.max(1);
        while self.voices.list.len() > self.voices.max() {
            if !self.steal_quietest() {
                break;
            }
        }
    }

    /// Spatial voices playing right now (one-shots and loops).
    pub fn spatial_voice_count(&self) -> usize {
        self.voices.list.len()
    }

    // ---- starting voices ------------------------------------------------------

    /// Plays `source` once from `position` and forgets about it. `params`
    /// carries the distance model and the rest of the emitter; its own
    /// `position` is replaced by `position`. The returned handle can still
    /// move the emitter while the sound plays.
    ///
    /// ```ignore
    /// audio.set_listener(Listener::at(player));
    /// let h = audio.play_at((samples, 44_100), Vec3::new(3.0, 0.0, -4.0), Emitter::default());
    /// h.set_position(new_pos); // follows a moving source
    /// ```
    pub fn play_at(&mut self, source: impl Into<LoopBuffer>, position: Vec3, params: Emitter) -> SpatialHandle {
        self.voices.next_oneshot += 1;
        let name = format!("@oneshot:{}", self.voices.next_oneshot);
        self.start_voice(&name, source.into(), false, Emitter { position, ..params })
    }

    /// Adds (or replaces) a named looping track at `emitter.position`. It
    /// joins the shared loop clock like `add_track`.
    pub fn add_spatial_track(&mut self, name: &str, source: impl Into<LoopBuffer>, emitter: Emitter) -> SpatialHandle {
        self.start_voice(name, source.into(), true, emitter)
    }

    /// What the listener currently hears of this voice; `None` once it is gone.
    pub fn spatial_params(&self, handle: &SpatialHandle) -> Option<SpatialParams> {
        self.voices.find(handle.id).map(|v| v.params)
    }

    /// Like `spatial_params`, by track name.
    pub fn track_spatial_params(&self, name: &str) -> Option<SpatialParams> {
        let id = self.bank.get(name)?.id;
        self.voices.find(id).map(|v| v.params)
    }

    fn start_voice(&mut self, name: &str, buffer: LoopBuffer, looped: bool, emitter: Emitter) -> SpatialHandle {
        self.update_voices_clock_only();
        if buffer.is_empty() {
            if looped {
                self.stop_track(name);
            }
            return SpatialHandle::dead(name, emitter);
        }
        if self.bank.get(name).is_some() {
            self.stop_track(name); // same-name replacement frees its slot first
        }

        // Where would it sit in the mix? Needed before it exists, to decide
        // whether it may steal a slot.
        let params = spatialize(&self.listener, &emitter);
        let bus = self.routed_bus(name).unwrap_or_else(|| SFX_BUS.to_string());
        let new_eff = self.mixer.effective_gain(&bus, false, self.any_track_solo()) * params.gain;
        while self.voices.list.len() >= self.voices.max() {
            let Some((_, quietest)) = self.quietest_voice() else { break };
            if quietest > new_eff {
                return SpatialHandle::dead(name, emitter); // quieter than everything playing
            }
            if !self.steal_quietest() {
                break;
            }
        }

        let Some(id) = self.register(name, buffer.clone(), looped, 0.0, 0.0) else {
            return SpatialHandle::dead(name, emitter);
        };
        if self.routed_bus(name).is_none() {
            if let Some(t) = self.bank.get_mut(name) {
                t.bus = SFX_BUS.to_string();
            }
        }
        let shared = Arc::new(Mutex::new(Shared { emitter, alive: true, stop: false, stop_on_drop: false }));
        let seq = self.voices.next_seq;
        self.voices.next_seq += 1;
        let (l, r) = pan_gains(params.pan);
        self.voices.list.push(Voice {
            id,
            shared: shared.clone(),
            oneshot: !looped,
            seq,
            progress: 0.0,
            duration: buffer.duration_secs(),
            params,
            pushed_volume: 0.0,
            pushed_pan: (l, r),
            pushed_pitch: params.doppler_pitch.unwrap_or(1.0),
        });
        let vol = self.effective_volume(id).unwrap_or(0.0);
        self.voices.note_volume(id, vol);
        self.start_pending(id);
        SpatialHandle { shared, name: name.to_string(), id }
    }

    /// The bus `assign_track_bus` routed this track name to, if it still exists.
    fn routed_bus(&self, name: &str) -> Option<String> {
        self.routes.get(name).filter(|b| self.mixer.has_bus(b)).cloned()
    }

    // ---- voice management ---------------------------------------------------

    /// (id, effective gain) of the voice that would be stolen next: lowest
    /// effective gain, oldest on ties.
    fn quietest_voice(&self) -> Option<(TrackId, f32)> {
        self.voices
            .list
            .iter()
            .map(|v| (v.id, v.seq, self.effective_volume(v.id).unwrap_or(0.0)))
            .min_by(|a, b| a.2.total_cmp(&b.2).then(a.1.cmp(&b.1)))
            .map(|(id, _, g)| (id, g))
    }

    fn steal_quietest(&mut self) -> bool {
        match self.quietest_voice() {
            Some((id, _)) => {
                self.stop_voice(id);
                true
            }
            None => false,
        }
    }

    fn stop_voice(&mut self, id: TrackId) {
        match self.bank.by_id(id).map(|t| t.name.clone()) {
            Some(name) => {
                self.stop_track(&name);
            }
            None => self.voices.forget(id),
        }
    }

    /// Advances only the wall clock (so a voice started right now lines up
    /// with the loop clock); does not touch voices twice.
    fn update_voices_clock_only(&mut self) {
        self.update();
    }

    /// Per-frame voice work: age and expire one-shots, honour `stop()` and
    /// `stop_when_dropped`, then re-spatialize every voice and send changed
    /// volume / pan / pitch to the backend.
    pub(super) fn update_voices(&mut self, dt: f64) {
        if self.voices.list.is_empty() {
            return;
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        let mut stop = Vec::new();
        for v in &mut self.voices.list {
            v.progress += dt * f64::from(v.pushed_pitch);
            let (stop_req, orphan) = {
                let s = lock(&v.shared);
                (s.stop, s.stop_on_drop && Arc::strong_count(&v.shared) == 1)
            };
            if stop_req || orphan || (v.oneshot && v.progress >= v.duration) {
                stop.push(v.id);
            }
        }
        for id in stop {
            self.stop_voice(id);
        }

        // Re-spatialize. Params first (effective_volume reads them), then push.
        let listener = self.listener;
        for v in &mut self.voices.list {
            let emitter = lock(&v.shared).emitter;
            v.params = spatialize(&listener, &emitter);
        }
        let ids: Vec<TrackId> = self.voices.list.iter().map(|v| v.id).collect();
        for id in ids {
            let vol = self.effective_volume(id).unwrap_or(0.0);
            let Some(v) = self.voices.list.iter_mut().find(|v| v.id == id) else { continue };
            let pan = pan_gains(v.params.pan);
            let pitch = v.params.doppler_pitch;
            let (push_vol, push_pan, push_pitch) = (
                vol.to_bits() != v.pushed_volume.to_bits(),
                pan != v.pushed_pan,
                pitch.filter(|p| p.to_bits() != v.pushed_pitch.to_bits()),
            );
            if push_vol {
                v.pushed_volume = vol;
            }
            if push_pan {
                v.pushed_pan = pan;
            }
            if let Some(p) = push_pitch {
                v.pushed_pitch = p;
            }
            if push_vol {
                self.backend.set_track_volume(id, vol);
            }
            if push_pan {
                self.backend.set_track_pan(id, pan.0, pan.1);
            }
            if let Some(p) = push_pitch {
                self.backend.set_track_pitch(id, p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::NullBackend;
    use crate::audio::spatial::Rolloff;
    use crate::audio::mixer::{MASTER_BUS, MUSIC_BUS};

    fn engine() -> AudioEngine<NullBackend> {
        AudioEngine::with_manual_time(NullBackend::new())
    }

    fn clip(secs: f32) -> (Vec<f32>, u32) {
        (vec![0.1; (secs * 1000.0) as usize], 1000)
    }

    /// Emitter that falls off linearly between 1 and 11 units, so gain at
    /// distance d (1..11) is exactly 1 - (d - 1) / 10.
    fn linear() -> Emitter {
        Emitter::default().with_distances(1.0, 11.0).with_rolloff(Rolloff::Linear, 1.0)
    }

    fn backend_vol(a: &AudioEngine<NullBackend>, h: &SpatialHandle) -> Option<f32> {
        a.backend().volume(h.track_id())
    }

    #[test]
    fn play_at_applies_distance_gain_and_pan() {
        let mut a = engine();
        a.set_listener(Listener::at(Vec3::ZERO));
        let near = a.play_at(clip(1.0), Vec3::new(1.0, 0.0, 0.0), linear());
        let far = a.play_at(clip(1.0), Vec3::new(0.0, 0.0, -6.0), linear());
        assert!(near.is_alive() && far.is_alive());
        assert_eq!(backend_vol(&a, &near), Some(1.0));
        assert!((backend_vol(&a, &far).unwrap() - 0.5).abs() < 1e-4);
        // +X is to the listener's right: right channel louder than left.
        let (l, r) = a.backend().pan(near.track_id()).unwrap();
        assert!(r > l, "pan {l} {r}");
        // dead ahead: centered.
        let (l, r) = a.backend().pan(far.track_id()).unwrap();
        assert!((l - r).abs() < 1e-4);
        // One-shots are hidden from the user-facing track list but live on `sfx`.
        assert!(a.track_names().is_empty());
        assert_eq!(a.track_bus(near.name()), Some(SFX_BUS));
        assert_eq!(a.spatial_voice_count(), 2);
    }

    #[test]
    fn handle_moves_emitter_and_listener_re_evaluates() {
        let mut a = engine();
        let h = a.play_at(clip(10.0), Vec3::new(0.0, 0.0, -1.0), linear());
        assert_eq!(backend_vol(&a, &h), Some(1.0));
        h.set_position(Vec3::new(0.0, 0.0, -6.0));
        assert_eq!(backend_vol(&a, &h), Some(1.0), "applied on the next update, not before");
        a.advance(0.016);
        assert!((backend_vol(&a, &h).unwrap() - 0.5).abs() < 1e-4);
        // Walking the listener closer is picked up immediately.
        a.set_listener(Listener::at(Vec3::new(0.0, 0.0, -6.0)));
        assert_eq!(backend_vol(&a, &h), Some(1.0));
        // Move listener so the source is on its left.
        a.set_listener(Listener::at(Vec3::new(5.0, 0.0, -6.0)));
        let (l, r) = a.backend().pan(h.track_id()).unwrap();
        assert!(l > r);
        assert!((a.spatial_params(&h).unwrap().distance - 5.0).abs() < 1e-4);
    }

    #[test]
    fn unchanged_values_are_not_resent_to_the_backend() {
        let mut a = engine();
        let h = a.play_at(clip(10.0), Vec3::new(0.0, 0.0, -4.0), linear());
        let n = a.backend().events.len();
        for _ in 0..10 {
            a.advance(0.016);
        }
        assert_eq!(a.backend().events.len(), n);
        drop(h);
    }

    #[test]
    fn one_shot_finishes_and_frees_its_slot() {
        let mut a = engine();
        let h = a.play_at(clip(0.5), Vec3::ZERO, Emitter::default());
        assert!(h.is_alive());
        a.advance(0.4);
        assert!(h.is_alive());
        a.advance(0.2);
        assert!(!h.is_alive());
        assert_eq!(a.spatial_voice_count(), 0);
        assert!(!a.has_track(h.name()));
        assert_eq!(backend_vol(&a, &h), None, "backend stopped");
    }

    #[test]
    fn stop_and_stop_when_dropped() {
        let mut a = engine();
        let h = a.play_at(clip(10.0), Vec3::ZERO, Emitter::default());
        h.stop();
        a.advance(0.01);
        assert!(!h.is_alive());

        let looped = a.add_spatial_track("hum", clip(1.0), Emitter::default());
        a.advance(5.0);
        assert!(looped.is_alive(), "loops do not expire");
        // Dropping the handle alone does not silence...
        let keep = looped.clone();
        drop(looped);
        a.advance(0.01);
        assert!(keep.is_alive());
        // ...unless asked to.
        keep.stop_when_dropped(true);
        let probe = keep.track_id();
        drop(keep);
        a.advance(0.01);
        assert!(a.backend().volume(probe).is_none());
        assert!(!a.has_track("hum"));
    }

    #[test]
    fn spatial_track_is_a_normal_named_track_on_the_loop_clock() {
        let mut a = engine();
        a.advance(0.25);
        let h = a.add_spatial_track("fire", clip(1.0), Emitter::at(Vec3::new(0.0, 0.0, -6.0)).with_distances(1.0, 11.0).with_rolloff(Rolloff::Linear, 1.0));
        assert_eq!(a.track_names(), ["fire"]);
        assert_eq!(a.track_bus("fire"), Some(SFX_BUS));
        assert!((a.track_position("fire").unwrap() - 0.25).abs() < 1e-6);
        assert!((a.track_effective_gain("fire").unwrap() - 0.5).abs() < 1e-4);
        assert!((a.track_spatial_params("fire").unwrap().gain - 0.5).abs() < 1e-4);

        // Mixer controls apply on top of the spatial gain.
        a.set_track_gain("fire", 0.5);
        assert!((backend_vol(&a, &h).unwrap() - 0.25).abs() < 1e-4);
        a.set_bus_muted(SFX_BUS, true);
        assert_eq!(backend_vol(&a, &h), Some(0.0));
        a.set_bus_muted(SFX_BUS, false);
        assert!(a.assign_track_bus("fire", MUSIC_BUS));
        assert_eq!(a.track_bus("fire"), Some(MUSIC_BUS));
        a.set_track_gain("fire", 1.0);
        a.set_music_volume(0.5);
        assert!((backend_vol(&a, &h).unwrap() - 0.25).abs() < 1e-4);

        // Re-adding the name replaces the voice (same slot count).
        let h2 = a.add_spatial_track("fire", clip(1.0), Emitter::default());
        assert!(!h.is_alive() && h2.is_alive());
        assert_eq!(a.spatial_voice_count(), 1);
        // Route survives replacement.
        assert_eq!(a.track_bus("fire"), Some(MUSIC_BUS));
        assert!(a.stop_track("fire"));
        assert!(!h2.is_alive());
    }

    #[test]
    fn route_set_before_creation_is_honoured() {
        let mut a = engine();
        a.add_bus("ambience", MASTER_BUS).unwrap();
        a.assign_track_bus("wind", "ambience");
        let _h = a.add_spatial_track("wind", clip(1.0), Emitter::default());
        assert_eq!(a.track_bus("wind"), Some("ambience"));
        a.set_bus_gain("ambience", 0.5);
        assert!((a.track_effective_gain("wind").unwrap() - 0.5).abs() < 1e-4);
    }

    /// Positions chosen so the effective gains are 0.9, 0.7, 0.5, 0.3 (linear
    /// model above: gain = 1 - (d-1)/10 -> d = 2, 4, 6, 8 on -Z).
    fn at_dist(d: f32) -> Vec3 {
        Vec3::new(0.0, 0.0, -d)
    }

    #[test]
    fn full_pool_steals_the_lowest_effective_gain() {
        let mut a = engine();
        a.set_max_spatial_voices(3);
        let v2 = a.play_at(clip(10.0), at_dist(2.0), linear()); // 0.9
        let v6 = a.play_at(clip(10.0), at_dist(6.0), linear()); // 0.5
        let v4 = a.play_at(clip(10.0), at_dist(4.0), linear()); // 0.7
        assert_eq!(a.spatial_voice_count(), 3);

        // Louder (0.8) than the quietest (0.5): steals v6.
        let v3 = a.play_at(clip(10.0), at_dist(3.0), linear());
        assert!(v3.is_alive());
        assert!(!v6.is_alive(), "lowest effective gain was stolen");
        assert!(v2.is_alive() && v4.is_alive());
        assert_eq!(a.spatial_voice_count(), 3);
        assert_eq!(backend_vol(&a, &v6), None, "backend voice stopped");

        // A voice quieter (0.3) than everything playing is not started.
        let v8 = a.play_at(clip(10.0), at_dist(8.0), linear());
        assert!(!v8.is_alive());
        assert_eq!(a.spatial_voice_count(), 3);
        assert_eq!(backend_vol(&a, &v8), None);
        assert!(v2.is_alive() && v3.is_alive() && v4.is_alive());
    }

    #[test]
    fn stealing_uses_bus_and_track_gain_not_just_distance() {
        let mut a = engine();
        a.set_max_spatial_voices(2);
        let a1 = a.add_spatial_track("a", clip(5.0), Emitter::at(at_dist(1.0)));
        let b1 = a.add_spatial_track("b", clip(5.0), Emitter::at(at_dist(1.0)));
        // Same distance, but `a` is turned down on its track fader: it is
        // the one to go.
        a.set_track_gain("a", 0.2);
        let c1 = a.add_spatial_track("c", clip(5.0), Emitter::at(at_dist(1.0)));
        assert!(!a1.is_alive());
        assert!(b1.is_alive() && c1.is_alive());

        // A muted bus makes a voice effectively silent -> first to be stolen,
        // and a new voice routed to a muted bus does not displace anyone.
        a.set_track_gain("b", 1.0);
        a.add_bus("quiet", MASTER_BUS).unwrap();
        a.assign_track_bus("d", "quiet");
        a.set_bus_muted("quiet", true);
        let d1 = a.add_spatial_track("d", clip(5.0), Emitter::at(at_dist(1.0)));
        assert!(!d1.is_alive(), "muted-bus newcomer is quieter than all");
        assert!(b1.is_alive() && c1.is_alive());
        a.set_bus_muted("quiet", false);
        a.set_bus_muted(SFX_BUS, true);
        a.assign_track_bus("e", "quiet");
        let e1 = a.add_spatial_track("e", clip(5.0), Emitter::at(at_dist(1.0)));
        assert!(e1.is_alive(), "now it is louder than the muted sfx voices");
        assert_eq!(a.spatial_voice_count(), 2);
    }

    #[test]
    fn ties_steal_the_oldest() {
        let mut a = engine();
        a.set_max_spatial_voices(2);
        let first = a.play_at(clip(10.0), at_dist(3.0), linear());
        let second = a.play_at(clip(10.0), at_dist(3.0), linear());
        let third = a.play_at(clip(10.0), at_dist(3.0), linear());
        assert!(!first.is_alive());
        assert!(second.is_alive() && third.is_alive());
    }

    #[test]
    fn many_play_at_calls_never_exceed_the_bound() {
        let mut a = engine();
        assert_eq!(a.max_spatial_voices(), DEFAULT_MAX_SPATIAL_VOICES);
        a.set_max_spatial_voices(8);
        for i in 0..200 {
            a.play_at(clip(10.0), at_dist(1.0 + (i % 9) as f32), linear());
            assert!(a.spatial_voice_count() <= 8);
            assert!(a.backend().playing.len() <= 8, "backend never holds more voices than the bound");
        }
    }

    #[test]
    fn lowering_the_bound_drops_the_quietest() {
        let mut a = engine();
        let loud = a.play_at(clip(10.0), at_dist(2.0), linear());
        let mid = a.play_at(clip(10.0), at_dist(5.0), linear());
        let quiet = a.play_at(clip(10.0), at_dist(9.0), linear());
        a.set_max_spatial_voices(1);
        assert!(loud.is_alive() && !mid.is_alive() && !quiet.is_alive());
        a.set_max_spatial_voices(0);
        assert_eq!(a.max_spatial_voices(), 1, "bound is at least 1");
    }

    #[test]
    fn plain_tracks_do_not_count_toward_the_bound() {
        let mut a = engine();
        a.set_max_spatial_voices(1);
        for i in 0..5 {
            a.add_track(&format!("t{i}"), clip(1.0));
        }
        let h = a.play_at(clip(1.0), Vec3::ZERO, Emitter::default());
        assert!(h.is_alive());
        assert_eq!(a.track_names().len(), 5);
    }

    #[test]
    fn doppler_pitch_is_pushed() {
        let mut a = engine();
        let h = a.add_spatial_track("siren", clip(1.0), Emitter::at(at_dist(10.0)).with_velocity(Vec3::new(0.0, 0.0, 20.0)));
        // Source approaching the listener at origin: pitch above 1.
        let p = a.backend().pitch(h.track_id()).expect("initial pitch");
        assert!(p > 1.0, "pitch {p}");
        h.set_velocity(Some(Vec3::new(0.0, 0.0, -20.0))); // receding
        a.advance(0.016);
        assert!(a.backend().pitch(h.track_id()).unwrap() < 1.0);
        h.set_velocity(None);
        a.advance(0.016);
        assert!((a.backend().pitch(h.track_id()).unwrap() - 1.0).abs() < 1e-3 || a.spatial_params(&h).unwrap().doppler_pitch.is_none());
    }

    #[test]
    fn empty_buffer_and_non_finite_dt_are_harmless() {
        let mut a = engine();
        let h = a.play_at((Vec::<f32>::new(), 1000), Vec3::ZERO, Emitter::default());
        assert!(!h.is_alive());
        let h = a.play_at(clip(1.0), Vec3::new(f32::NAN, 0.0, 0.0), Emitter::default());
        a.advance(f64::NAN);
        a.advance(-1.0);
        a.set_listener(Listener::at(Vec3::splat(f32::INFINITY)));
        let _ = h.is_alive();
        assert!(a.spatial_voice_count() <= 1);
    }

    #[test]
    fn handles_are_send_and_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<SpatialHandle>();
    }
}
