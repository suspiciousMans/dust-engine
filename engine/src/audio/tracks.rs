//! Backend-independent audio-track logic: the track registry, gain fading,
//! and the shared loop clock. Nothing here touches an audio device, so it is
//! unit-tested directly; `AudioEngine` (see `engine.rs`) drives a real or null
//! backend from these types.

/// Identifies one started track to a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u32);

/// A track's gain with an optional linear fade toward a target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainFader {
    value: f32,
    target: f32,
    /// Gain units per second while fading (0 when idle).
    rate: f32,
}

fn sane_gain(g: f32) -> Option<f32> {
    g.is_finite().then(|| g.clamp(0.0, 1.0))
}

impl GainFader {
    pub fn new(value: f32) -> Self {
        let v = sane_gain(value).unwrap_or(1.0);
        Self { value: v, target: v, rate: 0.0 }
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn is_fading(&self) -> bool {
        self.value != self.target
    }

    /// Jumps to `gain` (clamped to 0..=1) and cancels any fade. NaN/inf is ignored.
    pub fn set(&mut self, gain: f32) {
        if let Some(g) = sane_gain(gain) {
            self.value = g;
            self.target = g;
            self.rate = 0.0;
        }
    }

    /// Moves linearly to `gain` over `secs`. `secs <= 0` jumps immediately.
    pub fn fade_to(&mut self, gain: f32, secs: f32) {
        let Some(g) = sane_gain(gain) else { return };
        if !secs.is_finite() || secs <= 0.0 {
            self.set(g);
            return;
        }
        self.target = g;
        self.rate = (g - self.value).abs() / secs;
        if self.rate == 0.0 {
            self.value = g;
        }
    }

    /// Advances the fade by `dt` seconds; returns true if the value changed.
    pub fn tick(&mut self, dt: f64) -> bool {
        if !self.is_fading() || !dt.is_finite() || dt <= 0.0 {
            return false;
        }
        let step = (self.rate as f64 * dt) as f32;
        let before = self.value;
        if (self.target - self.value).abs() <= step {
            self.value = self.target;
            self.rate = 0.0;
        } else if self.target > self.value {
            self.value += step;
        } else {
            self.value -= step;
        }
        self.value != before
    }
}

/// The one clock every looping track reads. A track with phase `p` is at
/// position `(now + p) mod len`, so two equal-length tracks with the same
/// phase are at the same position no matter when each was started.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoopClock {
    t: f64,
}

impl LoopClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seconds elapsed on the shared clock.
    pub fn now(&self) -> f64 {
        self.t
    }

    /// Advances by `dt` seconds. Negative or non-finite steps are ignored.
    pub fn advance(&mut self, dt: f64) {
        if dt.is_finite() && dt > 0.0 {
            self.t += dt;
        }
    }

    /// Position in seconds inside a loop of `len_secs` for a given phase.
    pub fn position(&self, len_secs: f64, phase: f64) -> f64 {
        if !(len_secs.is_finite() && len_secs > 0.0) {
            return 0.0;
        }
        (self.t + phase).rem_euclid(len_secs)
    }

    /// How many times a loop of `len_secs` with `phase` has wrapped so far.
    pub fn loops_completed(&self, len_secs: f64, phase: f64) -> u64 {
        if !(len_secs.is_finite() && len_secs > 0.0) {
            return 0;
        }
        ((self.t + phase) / len_secs).floor().max(0.0) as u64
    }

    /// The phase that puts a track at `position_secs` right now.
    pub fn phase_for_position(&self, position_secs: f64) -> f64 {
        position_secs - self.t
    }
}

/// One named track in the registry.
#[derive(Clone, Debug)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub gain: GainFader,
    pub muted: bool,
    pub looped: bool,
    pub len_secs: f64,
    /// Offset against the shared loop clock (see `LoopClock`).
    pub phase: f64,
    /// Mixer bus this track plays through (see `mixer`).
    pub bus: String,
    /// Solo flag (see `Mixer::effective_gain`).
    pub solo: bool,
}

impl Track {
    /// Gain after mute, before the master music volume.
    pub fn level(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.gain.value()
        }
    }
}

/// Named tracks, in insertion order.
#[derive(Debug, Default)]
pub struct TrackBank {
    tracks: Vec<Track>,
    next_id: u32,
}

impl TrackBank {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a track under `name` at full gain. An existing track with
    /// that name is removed and returned so the caller can stop it.
    pub fn insert(&mut self, name: &str, looped: bool, len_secs: f64, phase: f64) -> (TrackId, Option<Track>) {
        let replaced = self.remove(name);
        let id = TrackId(self.next_id);
        self.next_id += 1;
        self.tracks.push(Track {
            id,
            name: name.to_string(),
            gain: GainFader::new(1.0),
            muted: false,
            looped,
            len_secs,
            phase,
            bus: super::mixer::MUSIC_BUS.to_string(),
            solo: false,
        });
        (id, replaced)
    }

    pub fn get(&self, name: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.name == name)
    }

    pub fn by_id(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn remove(&mut self, name: &str) -> Option<Track> {
        let i = self.tracks.iter().position(|t| t.name == name)?;
        Some(self.tracks.remove(i))
    }

    pub fn clear(&mut self) -> Vec<Track> {
        std::mem::take(&mut self.tracks)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Track> {
        self.tracks.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Advances every fade; returns the ids whose gain changed.
    pub fn tick(&mut self, dt: f64) -> Vec<TrackId> {
        self.tracks.iter_mut().filter_map(|t| t.gain.tick(dt).then_some(t.id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_clamps_and_ignores_nan() {
        let mut g = GainFader::new(0.5);
        g.set(2.0);
        assert_eq!(g.value(), 1.0);
        g.set(-1.0);
        assert_eq!(g.value(), 0.0);
        g.set(f32::NAN);
        assert_eq!(g.value(), 0.0);
        g.fade_to(f32::INFINITY, 1.0);
        assert!(!g.is_fading());
    }

    #[test]
    fn fader_fades_linearly_and_lands_exactly() {
        let mut g = GainFader::new(0.0);
        g.fade_to(1.0, 2.0);
        assert!(g.is_fading());
        assert!(g.tick(0.5));
        assert!((g.value() - 0.25).abs() < 1e-6);
        g.tick(0.5);
        assert!((g.value() - 0.5).abs() < 1e-6);
        g.tick(10.0);
        assert_eq!(g.value(), 1.0);
        assert!(!g.is_fading());
        assert!(!g.tick(1.0));
        // Downward, and zero-length fade is a jump.
        g.fade_to(0.25, 1.0);
        g.tick(0.5);
        assert!((g.value() - 0.625).abs() < 1e-6);
        g.fade_to(0.9, 0.0);
        assert_eq!(g.value(), 0.9);
    }

    #[test]
    fn set_cancels_fade() {
        let mut g = GainFader::new(0.0);
        g.fade_to(1.0, 1.0);
        g.set(0.3);
        assert!(!g.tick(1.0));
        assert_eq!(g.value(), 0.3);
    }

    #[test]
    fn clock_position_wraps_and_ignores_bad_steps() {
        let mut c = LoopClock::new();
        c.advance(f64::NAN);
        c.advance(-1.0);
        assert_eq!(c.now(), 0.0);
        c.advance(2.5);
        assert!((c.position(2.0, 0.0) - 0.5).abs() < 1e-9);
        assert!((c.position(2.0, 1.0) - 1.5).abs() < 1e-9);
        assert!((c.position(2.0, -3.0) - 1.5).abs() < 1e-9);
        assert_eq!(c.position(0.0, 0.0), 0.0);
        assert_eq!(c.loops_completed(2.0, 0.0), 1);
    }

    #[test]
    fn equal_length_loops_with_same_phase_stay_aligned_over_many_loops() {
        let len = 1.75;
        let mut clock = LoopClock::new();
        clock.advance(0.3);
        // Track A "started" at t=0.3, track B 4.1s later, both phase 0.25.
        let phase = 0.25;
        for _ in 0..1000 {
            clock.advance(0.0173);
            let a = clock.position(len, phase);
            let b = clock.position(len, phase);
            assert_eq!(a, b);
        }
        assert!(clock.loops_completed(len, phase) >= 9);
    }

    #[test]
    fn bank_insert_replace_remove_tick() {
        let mut bank = TrackBank::new();
        let (a, none) = bank.insert("a", true, 2.0, 0.0);
        assert!(none.is_none());
        let (b, _) = bank.insert("b", true, 2.0, 0.0);
        assert_ne!(a, b);
        let (a2, replaced) = bank.insert("a", true, 3.0, 0.0);
        assert_eq!(replaced.unwrap().id, a);
        assert_ne!(a2, a);
        assert_eq!(bank.len(), 2);
        assert_eq!(bank.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        bank.get_mut("b").unwrap().gain.fade_to(0.0, 1.0);
        assert_eq!(bank.tick(0.25), vec![b]);
        assert!(bank.remove("b").is_some());
        assert!(bank.remove("b").is_none());
        assert_eq!(bank.clear().len(), 1);
        assert!(bank.is_empty());
    }

    #[test]
    fn muted_track_level_is_zero_but_gain_is_kept() {
        let mut bank = TrackBank::new();
        bank.insert("a", true, 1.0, 0.0);
        let t = bank.get_mut("a").unwrap();
        t.gain.set(0.7);
        t.muted = true;
        assert_eq!(t.level(), 0.0);
        t.muted = false;
        assert_eq!(t.level(), 0.7);
    }
}
