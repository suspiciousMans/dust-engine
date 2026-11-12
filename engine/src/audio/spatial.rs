//! Pure 3D-audio math: no device, no engine state, no history.
//!
//! [`spatialize`] turns a [`Listener`] and an [`Emitter`] into
//! [`SpatialParams`]: a distance-attenuated gain, a stereo pan, and an
//! optional doppler pitch. It is a pure function of its two arguments, so it
//! has no hysteresis: the same inputs always give bit-identical outputs no
//! matter what was evaluated before.
//!
//! Conventions (right-handed, like the rest of the engine): the listener
//! looks down `forward` (default `-Z`), `up` defaults to `+Y`, and its right
//! hand is `forward x up` (`+X` by default).
//!
//! * **Distance model** (same formulas as OpenAL / Web Audio), with the
//!   distance clamped to `[min_distance, max_distance]`, `rf` = `rolloff_factor`:
//!   * `Inverse`:     `min / (min + rf * (d - min))`
//!   * `Linear`:      `1 - rf * (d - min) / (max - min)` (clamped to 0..=1)
//!   * `Exponential`: `(d / min) ^ -rf`
//!
//!   Because the distance is clamped, `Inverse` and `Exponential` hold their
//!   `max_distance` gain beyond it (they do not cut to silence); `Linear`
//!   reaches 0 at `max_distance` when `rf = 1`.
//! * **Pan** is the sine of the azimuth: `dot(direction_to_emitter, right)`,
//!   in -1 (hard left) ..= 1 (hard right). A source directly behind the
//!   listener pans to the centre, and a source behind-right pans exactly like
//!   its mirror image in front (front/back confusion is inherent to
//!   stereo). [`SpatialParams::behind`] reports how far behind the listener
//!   the source is (0 = in front or beside, 1 = straight behind), and
//!   `Emitter::rear_attenuation` optionally darkens rear sources by lowering
//!   their gain (off by default).
//! * [`pan_gains`] converts a pan into **equal-power** channel gains
//!   `(cos t, sin t)` with `t = (pan + 1) * pi / 4`, so `l^2 + r^2 == 1`.
//! * **Doppler** is `(c + df * v_listener) / (c + df * v_emitter)` where both
//!   speeds are projected on the listener->emitter direction (positive =
//!   toward the emitter for the listener, away from the listener for the
//!   emitter). `doppler_pitch` is `None` unless the emitter has a velocity.

use glam::Vec3;

/// Speed of sound used for doppler, metres per second.
pub const SPEED_OF_SOUND: f32 = 343.0;

/// Where the ears are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Listener {
    pub position: Vec3,
    /// Direction the listener faces (need not be normalised).
    pub forward: Vec3,
    pub up: Vec3,
    /// Only used for doppler; `Vec3::ZERO` if you do not track it.
    pub velocity: Vec3,
}

impl Default for Listener {
    fn default() -> Self {
        Self { position: Vec3::ZERO, forward: Vec3::NEG_Z, up: Vec3::Y, velocity: Vec3::ZERO }
    }
}

impl Listener {
    pub fn at(position: Vec3) -> Self {
        Self { position, ..Self::default() }
    }

    /// A listener whose orientation is a rotation applied to the default
    /// frame (`-Z` forward, `+Y` up), e.g. a camera `Transform::rotation`.
    pub fn from_rotation(position: Vec3, rotation: glam::Quat) -> Self {
        Self { position, forward: rotation * Vec3::NEG_Z, up: rotation * Vec3::Y, velocity: Vec3::ZERO }
    }

    /// Orthonormal `(forward, right, up)`; falls back to the default frame
    /// for a zero / non-finite forward and to any perpendicular for a
    /// degenerate `up` (zero, or parallel to `forward`).
    pub fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let f = self.forward.try_normalize().filter(|v| v.is_finite()).unwrap_or(Vec3::NEG_Z);
        let mut right = f.cross(self.up);
        if !(right.is_finite() && right.length_squared() > 1e-12) {
            let alt = if f.y.abs() < 0.99 { Vec3::Y } else { Vec3::X };
            right = f.cross(alt);
        }
        let right = right.normalize();
        let up = right.cross(f);
        (f, right, up)
    }
}

/// How gain falls off with distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rolloff {
    #[default]
    Inverse,
    Linear,
    Exponential,
}

/// A directional emitter: full gain inside `inner_angle`, `outer_gain`
/// outside `outer_angle`, linear in angle between. Angles are *full* cone
/// angles in radians, measured around `Emitter::direction`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cone {
    pub inner_angle: f32,
    pub outer_angle: f32,
    /// Gain outside the outer cone, 0..=1.
    pub outer_gain: f32,
}

impl Cone {
    pub fn new(inner_angle: f32, outer_angle: f32, outer_gain: f32) -> Self {
        Self { inner_angle, outer_angle, outer_gain }
    }

    /// Gain for a listener at `angle` radians off the emitter's axis.
    pub fn gain(&self, angle: f32) -> f32 {
        let outer_gain = if self.outer_gain.is_finite() { self.outer_gain.clamp(0.0, 1.0) } else { 1.0 };
        let inner = (self.inner_angle.max(0.0) * 0.5).min(std::f32::consts::PI);
        let outer = (self.outer_angle.max(0.0) * 0.5).clamp(inner, std::f32::consts::PI);
        if angle <= inner {
            1.0
        } else if angle >= outer {
            outer_gain
        } else {
            let t = (angle - inner) / (outer - inner);
            1.0 + (outer_gain - 1.0) * t
        }
    }
}

/// A sound source in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Emitter {
    pub position: Vec3,
    /// `None`: velocity is not tracked and no doppler is produced.
    /// `Some(Vec3::ZERO)`: stationary, but the listener's motion still shifts pitch.
    pub velocity: Option<Vec3>,
    /// At or inside this distance the gain is 1.
    pub min_distance: f32,
    /// The distance model stops changing beyond this.
    pub max_distance: f32,
    pub rolloff: Rolloff,
    pub rolloff_factor: f32,
    /// Axis of the cone (only used when `cone` is `Some`).
    pub direction: Vec3,
    pub cone: Option<Cone>,
    /// 0 disables doppler, 1 is physical.
    pub doppler_factor: f32,
    /// 0..=1: gain lost for a source straight behind the listener
    /// (scaled by [`SpatialParams::behind`]). 0 = off.
    pub rear_attenuation: f32,
}

impl Default for Emitter {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            velocity: None,
            min_distance: 1.0,
            max_distance: 50.0,
            rolloff: Rolloff::Inverse,
            rolloff_factor: 1.0,
            direction: Vec3::NEG_Z,
            cone: None,
            doppler_factor: 1.0,
            rear_attenuation: 0.0,
        }
    }
}

impl Emitter {
    pub fn at(position: Vec3) -> Self {
        Self { position, ..Self::default() }
    }

    pub fn with_distances(mut self, min: f32, max: f32) -> Self {
        self.min_distance = min;
        self.max_distance = max;
        self
    }

    pub fn with_rolloff(mut self, rolloff: Rolloff, factor: f32) -> Self {
        self.rolloff = rolloff;
        self.rolloff_factor = factor;
        self
    }

    pub fn with_velocity(mut self, velocity: Vec3) -> Self {
        self.velocity = Some(velocity);
        self
    }

    pub fn with_cone(mut self, direction: Vec3, cone: Cone) -> Self {
        self.direction = direction;
        self.cone = Some(cone);
        self
    }
}

/// What a listener hears of one emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialParams {
    /// Distance and cone attenuation, 0..=1 (before any track/bus gain).
    pub gain: f32,
    /// -1 (left) ..= 1 (right).
    pub pan: f32,
    /// Playback-rate multiplier; `None` if the emitter has no velocity or
    /// doppler is off.
    pub doppler_pitch: Option<f32>,
    /// Straight-line distance listener -> emitter.
    pub distance: f32,
    /// 0 = in front of or beside the listener .. 1 = straight behind.
    pub behind: f32,
}

impl SpatialParams {
    /// Silence at the centre: used for non-finite input.
    pub const SILENT: SpatialParams = SpatialParams { gain: 0.0, pan: 0.0, doppler_pitch: None, distance: 0.0, behind: 0.0 };

    /// Equal-power `(left, right)` channel gains for this pan, already
    /// multiplied by nothing else (apply `gain` separately).
    pub fn channel_gains(&self) -> (f32, f32) {
        pan_gains(self.pan)
    }
}

/// Equal-power pan law: `pan` in -1..=1 -> `(left, right)` with
/// `left^2 + right^2 == 1`. Centre is `(0.7071, 0.7071)`.
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    let t = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
    (t.cos(), t.sin())
}

/// Gain of the distance model alone (see module docs).
pub fn distance_gain(rolloff: Rolloff, rolloff_factor: f32, min: f32, max: f32, distance: f32) -> f32 {
    let min = if min.is_finite() { min.max(1e-4) } else { 1.0 };
    let max = if max.is_finite() { max.max(min) } else { min };
    let rf = if rolloff_factor.is_finite() { rolloff_factor.max(0.0) } else { 1.0 };
    let d = distance.clamp(min, max);
    let g = match rolloff {
        Rolloff::Inverse => min / (min + rf * (d - min)),
        Rolloff::Linear => {
            if max > min {
                1.0 - rf * (d - min) / (max - min)
            } else {
                1.0
            }
        }
        Rolloff::Exponential => (d / min).powf(-rf),
    };
    g.clamp(0.0, 1.0)
}

/// Computes what `listener` hears of `emitter`. Pure and deterministic.
/// Non-finite positions give [`SpatialParams::SILENT`].
pub fn spatialize(listener: &Listener, emitter: &Emitter) -> SpatialParams {
    if !(listener.position.is_finite() && emitter.position.is_finite()) {
        return SpatialParams::SILENT;
    }
    let offset = emitter.position - listener.position;
    let distance = offset.length();
    let (forward, right, _up) = listener.basis();
    // Unit vector listener -> emitter; undefined at distance 0 (then: centre).
    let dir = if distance > 1e-6 { offset / distance } else { Vec3::ZERO };

    let mut gain = distance_gain(emitter.rolloff, emitter.rolloff_factor, emitter.min_distance, emitter.max_distance, distance);

    if let Some(cone) = &emitter.cone {
        if let Some(axis) = emitter.direction.try_normalize().filter(|a| a.is_finite()) {
            if distance > 1e-6 {
                // The listener sits along (listener - emitter) from the emitter.
                let angle = axis.dot(-dir).clamp(-1.0, 1.0).acos();
                gain *= cone.gain(angle);
            }
        }
    }

    let pan = dir.dot(right).clamp(-1.0, 1.0);
    let behind = (-dir.dot(forward)).max(0.0).min(1.0);
    if emitter.rear_attenuation.is_finite() && emitter.rear_attenuation > 0.0 {
        gain *= 1.0 - emitter.rear_attenuation.min(1.0) * behind;
    }

    let doppler_pitch = emitter.velocity.and_then(|v| doppler(listener, v, dir, emitter.doppler_factor));

    SpatialParams { gain: gain.clamp(0.0, 1.0), pan, doppler_pitch, distance, behind }
}

fn doppler(listener: &Listener, emitter_velocity: Vec3, dir: Vec3, factor: f32) -> Option<f32> {
    if !(factor.is_finite() && factor > 0.0) || !emitter_velocity.is_finite() || !listener.velocity.is_finite() {
        return None;
    }
    let c = SPEED_OF_SOUND;
    // Speeds along listener->emitter, capped below c/factor so the ratio stays finite.
    let cap = c / factor;
    let vl = listener.velocity.dot(dir).clamp(-cap * 0.9, cap * 0.9);
    let vs = emitter_velocity.dot(dir).clamp(-cap * 0.9, cap * 0.9);
    let pitch = (c + factor * vl) / (c + factor * vs);
    Some(pitch.clamp(0.25, 4.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "{a} vs {b}");
    }

    fn at(x: f32, y: f32, z: f32) -> Emitter {
        Emitter::at(Vec3::new(x, y, z))
    }

    #[test]
    fn inverse_linear_exponential_hand_values() {
        // 3-4-5 triangle: distance 5.
        let l = Listener::default();
        let p = spatialize(&l, &at(3.0, 0.0, -4.0));
        close(p.distance, 5.0);
        close(p.gain, 0.2); // 1 / (1 + 1*(5-1))
        close(p.pan, 0.6); // 3/5
        assert_eq!(p.doppler_pitch, None);
        close(p.behind, 0.0);

        let lin = at(3.0, 0.0, -4.0).with_distances(1.0, 11.0).with_rolloff(Rolloff::Linear, 1.0);
        close(spatialize(&l, &lin).gain, 0.6); // 1 - 4/10
        let lin_half = lin.with_rolloff(Rolloff::Linear, 0.5);
        close(spatialize(&l, &lin_half).gain, 0.8);

        let exp = at(3.0, 0.0, -4.0).with_rolloff(Rolloff::Exponential, 2.0);
        close(spatialize(&l, &exp).gain, 0.04); // 5^-2

        let inv2 = at(3.0, 0.0, -4.0).with_rolloff(Rolloff::Inverse, 2.0);
        close(spatialize(&l, &inv2).gain, 1.0 / 9.0); // 1/(1+2*4)
    }

    #[test]
    fn inside_min_distance_is_full_and_beyond_max_is_clamped() {
        let l = Listener::default();
        let e = at(0.0, 0.0, -0.5);
        close(spatialize(&l, &e).gain, 1.0);
        close(spatialize(&l, &at(0.0, 0.0, -1.0)).gain, 1.0);
        // Inverse holds its max-distance gain (1/(1+49) = 0.02) beyond max.
        close(spatialize(&l, &at(0.0, 0.0, -50.0)).gain, 0.02);
        close(spatialize(&l, &at(0.0, 0.0, -5000.0)).gain, 0.02);
        // Linear is exactly silent at and beyond max.
        let lin = at(0.0, 0.0, -20.0).with_distances(2.0, 20.0).with_rolloff(Rolloff::Linear, 1.0);
        assert_eq!(spatialize(&l, &lin).gain, 0.0);
        close(spatialize(&l, &at(0.0, 0.0, -11.0).with_distances(2.0, 20.0).with_rolloff(Rolloff::Linear, 1.0)).gain, 0.5);
    }

    #[test]
    fn equal_power_pan_law() {
        let (l, r) = pan_gains(0.0);
        close(l, std::f32::consts::FRAC_1_SQRT_2);
        close(r, std::f32::consts::FRAC_1_SQRT_2);
        assert!(pan_gains(-1.0).1.abs() < 1e-6);
        close(pan_gains(-1.0).0, 1.0);
        close(pan_gains(1.0).1, 1.0);
        assert!(pan_gains(1.0).0.abs() < 1e-6);
        // pan 0.6 -> t = 72 degrees.
        let (l, r) = pan_gains(0.6);
        close(l, 0.309_017);
        close(r, 0.951_057);
        for i in -20..=20 {
            let (l, r) = pan_gains(i as f32 / 20.0);
            close(l * l + r * r, 1.0);
        }
        // Out-of-range and NaN are tamed.
        assert_eq!(pan_gains(5.0), pan_gains(1.0));
        assert_eq!(pan_gains(f32::NAN), pan_gains(0.0));
        // Via SpatialParams.
        let p = spatialize(&Listener::default(), &at(3.0, 0.0, -4.0));
        let (l, r) = p.channel_gains();
        close(l, 0.309_017);
        close(r, 0.951_057);
    }

    #[test]
    fn pan_follows_listener_orientation() {
        // Emitter on +X. Facing -Z it is to the right; facing +X it is
        // ahead (centre); facing +Z it is to the left; facing -X, behind.
        let e = at(10.0, 0.0, 0.0);
        let mut l = Listener::default();
        close(spatialize(&l, &e).pan, 1.0);
        l.forward = Vec3::X;
        close(spatialize(&l, &e).pan, 0.0);
        l.forward = Vec3::Z;
        close(spatialize(&l, &e).pan, -1.0);
        // from_rotation: a 90 degree yaw left (about +Y) turns -Z into -X.
        let l = Listener::from_rotation(Vec3::ZERO, glam::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        close(l.forward.x, -1.0);
        close(spatialize(&l, &e).pan, 0.0); // +X is now directly behind
        close(spatialize(&l, &e).behind, 1.0);
        // Rolling the listener (up tilted) moves the right hand with it.
        let rolled = Listener { up: Vec3::X, ..Listener::default() }; // right = (-Z) x X = -Y
        close(spatialize(&rolled, &at(0.0, -10.0, 0.0)).pan, 1.0);
        close(spatialize(&rolled, &at(10.0, 0.0, 0.0)).pan, 0.0);
    }

    #[test]
    fn behind_listener() {
        let l = Listener::default();
        // Straight behind: centre, behind = 1.
        let p = spatialize(&l, &at(0.0, 0.0, 5.0));
        close(p.pan, 0.0);
        close(p.behind, 1.0);
        // Behind-right (3,0,4): pans exactly like its front mirror (3,0,-4).
        let back = spatialize(&l, &at(3.0, 0.0, 4.0));
        let front = spatialize(&l, &at(3.0, 0.0, -4.0));
        close(back.pan, front.pan);
        close(back.gain, front.gain);
        close(back.behind, 0.8);
        close(front.behind, 0.0);
        // Beside: not behind.
        close(spatialize(&l, &at(5.0, 0.0, 0.0)).behind, 0.0);
        // Rear attenuation: gain * (1 - 0.5 * behind); distance 5 -> 0.2 * 0.6.
        let mut e = at(3.0, 0.0, 4.0);
        e.rear_attenuation = 0.5;
        close(spatialize(&l, &e).gain, 0.2 * (1.0 - 0.5 * 0.8));
        e.position = Vec3::new(3.0, 0.0, -4.0);
        close(spatialize(&l, &e).gain, 0.2); // front untouched
    }

    #[test]
    fn cone_attenuation_hand_values() {
        // Emitter 10 m ahead of the listener (at -Z), listener sits along +Z from it.
        let l = Listener::default();
        let cone = Cone::new(std::f32::consts::FRAC_PI_2, std::f32::consts::PI, 0.2); // 90 / 180 degrees full
        let base = at(0.0, 0.0, -10.0).with_distances(1.0, 1000.0);
        let g0 = spatialize(&l, &base).gain;
        // Facing the listener (axis +Z): inside the inner cone.
        let facing = base.with_cone(Vec3::Z, cone);
        close(spatialize(&l, &facing).gain, g0);
        // Facing away (axis -Z): 180 degrees off -> outer gain.
        let away = base.with_cone(Vec3::NEG_Z, cone);
        close(spatialize(&l, &away).gain, g0 * 0.2);
        // 90 degrees off axis: exactly the outer half-angle -> outer gain.
        let side = base.with_cone(Vec3::X, cone);
        close(spatialize(&l, &side).gain, g0 * 0.2);
        // 67.5 degrees off axis: halfway between 45 and 90 -> 1 + (0.2-1)/2 = 0.6.
        let a = 67.5f32.to_radians();
        let mid = base.with_cone(Vec3::new(a.sin(), 0.0, a.cos()), cone);
        close(spatialize(&l, &mid).gain, g0 * 0.6);
        // Cone.gain directly.
        close(cone.gain(0.0), 1.0);
        close(cone.gain(45f32.to_radians()), 1.0);
        close(cone.gain(10.0), 0.2);
    }

    #[test]
    fn doppler_hand_values() {
        let l = Listener::default();
        // Emitter 100 m ahead, flying toward the listener at 34.3 m/s (= c/10).
        let e = at(0.0, 0.0, -100.0).with_velocity(Vec3::new(0.0, 0.0, 34.3));
        close(spatialize(&l, &e).doppler_pitch.unwrap(), 343.0 / (343.0 - 34.3)); // 1.1111
        // Receding at the same speed: 343 / 377.3.
        let e = at(0.0, 0.0, -100.0).with_velocity(Vec3::new(0.0, 0.0, -34.3));
        close(spatialize(&l, &e).doppler_pitch.unwrap(), 343.0 / 377.3);
        // Stationary emitter, listener approaching at 34.3 m/s: 377.3 / 343 = 1.1.
        let l2 = Listener { velocity: Vec3::new(0.0, 0.0, -34.3), ..Listener::default() };
        let e = at(0.0, 0.0, -100.0).with_velocity(Vec3::ZERO);
        close(spatialize(&l2, &e).doppler_pitch.unwrap(), 1.1);
        // Sideways motion (perpendicular to line of sight): no shift.
        let e = at(0.0, 0.0, -100.0).with_velocity(Vec3::new(50.0, 0.0, 0.0));
        close(spatialize(&l, &e).doppler_pitch.unwrap(), 1.0);
        // factor 0 and no velocity: None.
        let mut e = at(0.0, 0.0, -100.0).with_velocity(Vec3::Z);
        e.doppler_factor = 0.0;
        assert_eq!(spatialize(&l, &e).doppler_pitch, None);
        assert_eq!(spatialize(&l, &at(0.0, 0.0, -100.0)).doppler_pitch, None);
        // Supersonic is capped, not infinite or negative.
        let e = at(0.0, 0.0, -100.0).with_velocity(Vec3::new(0.0, 0.0, 10_000.0));
        let p = spatialize(&l, &e).doppler_pitch.unwrap();
        assert!(p.is_finite() && (0.25..=4.0).contains(&p), "{p}");
    }

    #[test]
    fn degenerate_inputs_never_nan() {
        let l = Listener::default();
        // Emitter exactly on the listener: full gain, centred.
        let p = spatialize(&l, &Emitter::default());
        assert_eq!((p.gain, p.pan, p.distance), (1.0, 0.0, 0.0));
        // Zero forward, up parallel to forward, zero up.
        for bad in [
            Listener { forward: Vec3::ZERO, ..l },
            Listener { up: Vec3::NEG_Z, ..l },
            Listener { up: Vec3::ZERO, ..l },
            Listener { forward: Vec3::new(f32::NAN, 0.0, 0.0), ..l },
        ] {
            let p = spatialize(&bad, &at(3.0, 0.0, -4.0));
            assert!(p.gain.is_finite() && p.pan.is_finite() && (-1.0..=1.0).contains(&p.pan), "{p:?}");
        }
        // Non-finite positions are silent.
        assert_eq!(spatialize(&l, &at(f32::NAN, 0.0, 0.0)), SpatialParams::SILENT);
        assert_eq!(spatialize(&Listener::at(Vec3::splat(f32::INFINITY)), &at(0.0, 0.0, 0.0)), SpatialParams::SILENT);
        // Nonsense distances/factors.
        let mut e = at(0.0, 0.0, -5.0);
        e.min_distance = 0.0;
        e.max_distance = -1.0;
        e.rolloff_factor = f32::NAN;
        let g = spatialize(&l, &e).gain;
        assert!(g.is_finite() && (0.0..=1.0).contains(&g));
        let mut e = at(0.0, 0.0, -5.0).with_rolloff(Rolloff::Linear, 1.0);
        e.min_distance = 2.0;
        e.max_distance = 2.0; // zero-width range
        assert!(spatialize(&l, &e).gain.is_finite());
    }

    #[test]
    fn result_has_no_hysteresis() {
        // The result depends only on the arguments, not on evaluation order.
        let l = Listener::default();
        let e = at(2.0, 1.0, -7.0).with_velocity(Vec3::new(1.0, 0.0, 2.0));
        let first = spatialize(&l, &e);
        for i in 0..200 {
            let noise = at(i as f32 * 0.37 - 30.0, (i % 7) as f32, -(i as f32));
            let _ = spatialize(&Listener::at(Vec3::splat(i as f32)), &noise);
            assert_eq!(spatialize(&l, &e), first);
        }
        // Crossing the min-distance boundary back and forth gives the same value each way.
        let near = at(0.0, 0.0, -1.0);
        let far = at(0.0, 0.0, -1.0 - 1e-3);
        let (a, b) = (spatialize(&l, &near), spatialize(&l, &far));
        for _ in 0..10 {
            assert_eq!(spatialize(&l, &near), a);
            assert_eq!(spatialize(&l, &far), b);
        }
        assert!(b.gain < a.gain);
    }

    #[test]
    fn gain_is_monotonic_in_distance_for_every_model() {
        let l = Listener::default();
        for rolloff in [Rolloff::Inverse, Rolloff::Linear, Rolloff::Exponential] {
            let mut last = 2.0;
            for i in 0..200 {
                let e = at(0.0, 0.0, -(i as f32) * 0.5).with_distances(1.0, 60.0).with_rolloff(rolloff, 1.0);
                let g = spatialize(&l, &e).gain;
                assert!(g <= last + 1e-6, "{rolloff:?} at {i}: {g} > {last}");
                last = g;
            }
        }
    }
}
