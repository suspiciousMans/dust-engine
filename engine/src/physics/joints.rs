//! Joints: distance (rod / rope / spring) and pin (point) constraints between
//! two dynamic bodies, or between a body and a fixed world point.
//!
//! A joint is its own entity carrying a [`Joint`] component:
//!
//! ```ignore
//! // Pendulum: bob hangs 2 m below a fixed pivot.
//! let bob = world.spawn((Transform::from_position(Vec3::new(2.0, 8.0, 0.0)),
//!                        Collider::sphere(0.2), RigidBody::default()));
//! world.spawn((Joint::distance_to_point(bob, Vec3::new(0.0, 8.0, 0.0), 2.0),));
//! ```
//!
//! **Solver**: rods, ropes and pins are projected position-based (PBD) inside
//! the simulation step — `joint_iterations` passes before contacts, then one
//! pass after every contact iteration, with the correction fed back into the
//! velocity (`v += dx/dt`). Springs are applied as forces with semi-implicit
//! Euler before integration, with stiffness/damping clamped to the stable
//! range for the step (`omega*dt <= 1.6`, `c*dt/m <= 1`), so they cannot blow up.
//!
//! **Model limits** (bodies are translation-only, unit mass): anchors are
//! body-local offsets rotated by `Transform::rotation`, but corrections act at
//! the body centre, so joints exert no torque and a "hinge" is a point pin. A
//! static body (no `RigidBody`), a bare `Transform` entity, or a world point
//! never moves. Bodies attached to a joint never sleep. A joint whose body was
//! despawned (or has no `Transform`) is ignored. Jointed pairs do not collide
//! with each other unless [`Joint::collide_connected`] is set.

use glam::Vec3;
use hecs::Entity;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JointKind {
    /// Keeps the two anchors exactly `length` apart (rigid rod).
    Distance { length: f32 },
    /// Like `Distance` but only pulls back when stretched past `max_length`.
    Rope { max_length: f32 },
    /// Point constraint: the two anchors coincide (ball-and-socket / pin).
    Pin,
    /// Damped spring. `stiffness` N/m, `damping` N*s/m, unit-mass bodies.
    Spring { rest_length: f32, stiffness: f32, damping: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Joint {
    pub a: Entity,
    /// Second body; `None` = fixed world point stored in `anchor_b`.
    pub b: Option<Entity>,
    /// Anchor in `a`'s local space.
    pub anchor_a: Vec3,
    /// Anchor in `b`'s local space, or the world point when `b` is `None`.
    pub anchor_b: Vec3,
    pub kind: JointKind,
    pub collide_connected: bool,
}

impl Joint {
    fn make(a: Entity, b: Option<Entity>, anchor_b: Vec3, kind: JointKind) -> Self {
        Self { a, b, anchor_a: Vec3::ZERO, anchor_b, kind, collide_connected: false }
    }
    /// Rigid rod between the centres of `a` and `b`.
    pub fn distance(a: Entity, b: Entity, length: f32) -> Self {
        Self::make(a, Some(b), Vec3::ZERO, JointKind::Distance { length })
    }
    /// Rigid rod from `a` to a fixed world `point`.
    pub fn distance_to_point(a: Entity, point: Vec3, length: f32) -> Self {
        Self::make(a, None, point, JointKind::Distance { length })
    }
    pub fn rope(a: Entity, b: Entity, max_length: f32) -> Self {
        Self::make(a, Some(b), Vec3::ZERO, JointKind::Rope { max_length })
    }
    pub fn rope_to_point(a: Entity, point: Vec3, max_length: f32) -> Self {
        Self::make(a, None, point, JointKind::Rope { max_length })
    }
    /// Pins the centres of `a` and `b` together (use anchors to offset).
    pub fn pin(a: Entity, b: Entity) -> Self {
        Self::make(a, Some(b), Vec3::ZERO, JointKind::Pin)
    }
    /// Pins `a` (at its local `anchor_a`, default centre) to a world `point`.
    pub fn pin_to_point(a: Entity, point: Vec3) -> Self {
        Self::make(a, None, point, JointKind::Pin)
    }
    pub fn spring(a: Entity, b: Entity, rest_length: f32, stiffness: f32, damping: f32) -> Self {
        Self::make(a, Some(b), Vec3::ZERO, JointKind::Spring { rest_length, stiffness, damping })
    }
    pub fn spring_to_point(a: Entity, point: Vec3, rest_length: f32, stiffness: f32, damping: f32) -> Self {
        Self::make(a, None, point, JointKind::Spring { rest_length, stiffness, damping })
    }
    /// Sets the local anchors (`anchor_b` is a world point when `b` is `None`).
    pub fn with_anchors(mut self, anchor_a: Vec3, anchor_b: Vec3) -> Self {
        self.anchor_a = anchor_a;
        self.anchor_b = anchor_b;
        self
    }
    pub fn with_collide_connected(mut self, yes: bool) -> Self {
        self.collide_connected = yes;
        self
    }
}

/// Joint solver passes before contacts in [`super::step`] and the default of
/// [`super::PhysicsWorld::joint_iterations`].
pub const DEFAULT_JOINT_ITERATIONS: usize = 10;

/// Largest `omega * dt` a spring is allowed (semi-implicit Euler is stable < 2).
pub(crate) const MAX_SPRING_OMEGA_DT: f32 = 1.6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::Transform;
    use crate::physics::{Collider, PhysicsParams, PhysicsWorld, RigidBody};
    use glam::Quat;

    fn no_damp() -> PhysicsParams {
        PhysicsParams { gravity: 9.8, linear_damping: 0.0 }
    }
    fn body(w: &mut hecs::World, p: Vec3) -> Entity {
        w.spawn((Transform::from_position(p), Collider::sphere(0.1), RigidBody::default()))
    }
    fn pos(w: &hecs::World, e: Entity) -> Vec3 {
        w.get::<&Transform>(e).unwrap().position
    }
    fn vel(w: &hecs::World, e: Entity) -> Vec3 {
        w.get::<&RigidBody>(e).unwrap().velocity
    }

    #[test]
    fn pendulum_keeps_length_and_swings_at_the_right_period() {
        let mut w = hecs::World::new();
        let pivot = Vec3::new(0.0, 10.0, 0.0);
        let bob = body(&mut w, pivot + Vec3::new(2.0, 0.0, 0.0));
        w.spawn((Joint::distance_to_point(bob, pivot, 2.0),));
        let mut pw = PhysicsWorld::new(no_damp());
        let (mut worst, mut vmax, mut first_bottom, mut min_x) = (0.0f32, 0.0f32, None, 0.0f32);
        for i in 0..600 {
            pw.step_fixed(&mut w);
            let p = pos(&w, bob);
            worst = worst.max(((p - pivot).length() - 2.0).abs());
            vmax = vmax.max(vel(&w, bob).length());
            min_x = min_x.min(p.x);
            if first_bottom.is_none() && p.x <= 0.0 {
                first_bottom = Some(i as f32 / 60.0);
            }
        }
        // Length within 1% of 2 m for all 600 steps.
        assert!(worst < 0.02, "length drift {worst}");
        // Energy is not created: lowest point is 2 m below the release height.
        let vbound = (2.0f32 * 9.8 * 2.0).sqrt();
        assert!(vmax < vbound * 1.05, "vmax {vmax} vs {vbound}");
        // It swings through the bottom to the other side.
        assert!(min_x < -1.5, "min_x {min_x}");
        // Quarter period of a 90deg pendulum: sqrt(L/g)*K(sin 45deg) = 0.838 s.
        let t = first_bottom.expect("never reached the bottom");
        assert!((t - 0.838).abs() < 0.838 * 0.08, "quarter period {t}");
    }

    #[test]
    fn rod_between_two_falling_bodies_keeps_length_and_swings_about_centre() {
        let mut w = hecs::World::new();
        let a = body(&mut w, Vec3::new(-1.0, 20.0, 0.0));
        let b = body(&mut w, Vec3::new(1.0, 20.0, 0.0));
        w.get::<&mut RigidBody>(a).unwrap().velocity = Vec3::new(0.0, 3.0, 0.0);
        w.get::<&mut RigidBody>(b).unwrap().velocity = Vec3::new(0.0, -3.0, 0.0);
        w.spawn((Joint::distance(a, b, 2.0),));
        let mut pw = PhysicsWorld::new(no_damp());
        let mut worst = 0.0f32;
        for _ in 0..90 {
            pw.step_fixed(&mut w);
            worst = worst.max(((pos(&w, a) - pos(&w, b)).length() - 2.0).abs());
        }
        assert!(worst < 0.02, "{worst}");
        // Centre of mass is in free fall regardless of the rod.
        let t = 90.0 / 60.0;
        let com_y = (pos(&w, a).y + pos(&w, b).y) * 0.5;
        assert!((com_y - (20.0 - 0.5 * 9.8 * t * t)).abs() < 0.3, "com {com_y}");
        // The rod made them rotate: they are no longer both at y=com.
        assert!((pos(&w, a).y - pos(&w, b).y).abs() > 0.5);
    }

    fn crossings_period(samples: &[f32], dt: f32) -> (f32, usize) {
        // Upward zero crossings with linear interpolation.
        let mut times = Vec::new();
        for i in 1..samples.len() {
            if samples[i - 1] < 0.0 && samples[i] >= 0.0 {
                let f = -samples[i - 1] / (samples[i] - samples[i - 1]);
                times.push((i as f32 - 1.0 + f) * dt);
            }
        }
        let n = times.len();
        ((times[n - 1] - times[0]) / (n - 1) as f32, n)
    }

    #[test]
    fn spring_oscillates_at_analytic_frequency() {
        let f_hz = 2.0f32;
        let k = (2.0 * std::f32::consts::PI * f_hz).powi(2); // unit mass
        let mut w = hecs::World::new();
        let e = body(&mut w, Vec3::new(1.5, 0.0, 0.0));
        w.spawn((Joint::spring_to_point(e, Vec3::ZERO, 1.0, k, 0.0),));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        let mut xs = Vec::new();
        let mut amp = 0.0f32;
        for i in 0..600 {
            pw.step_fixed(&mut w);
            let x = pos(&w, e).x - 1.0;
            xs.push(x);
            if i > 500 {
                amp = amp.max(x.abs());
            }
        }
        let (period, n) = crossings_period(&xs, 1.0 / 60.0);
        assert!(n >= 19, "cycles {n}");
        assert!((period - 0.5).abs() < 0.5 * 0.01, "period {period}");
        // Energy preserved within 5% (amplitude 0.5 initially).
        assert!((amp - 0.5).abs() < 0.025, "amp {amp}");
    }

    #[test]
    fn two_body_spring_uses_reduced_mass() {
        // Unit masses: reduced mass 0.5, so omega = sqrt(2k). Pick k for 2 Hz.
        let k = (2.0 * std::f32::consts::PI * 2.0f32).powi(2) / 2.0;
        let mut w = hecs::World::new();
        let a = body(&mut w, Vec3::new(-0.75, 0.0, 0.0));
        let b = body(&mut w, Vec3::new(0.75, 0.0, 0.0));
        w.spawn((Joint::spring(a, b, 1.0, k, 0.0),));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        let mut sep = Vec::new();
        let mut momentum = Vec3::ZERO;
        for _ in 0..600 {
            pw.step_fixed(&mut w);
            sep.push((pos(&w, b) - pos(&w, a)).length() - 1.0);
            momentum = vel(&w, a) + vel(&w, b);
        }
        let (period, n) = crossings_period(&sep, 1.0 / 60.0);
        assert!(n >= 19 && (period - 0.5).abs() < 0.005, "period {period} n {n}");
        assert!(momentum.length() < 1e-3, "momentum {momentum:?}");
    }

    #[test]
    fn damped_spring_decays_and_stiff_spring_cannot_explode() {
        let mut w = hecs::World::new();
        let e = body(&mut w, Vec3::new(1.5, 0.0, 0.0));
        let k = 158.0f32;
        let c = 2.0 * 0.3 * k.sqrt(); // damping ratio 0.3
        w.spawn((Joint::spring_to_point(e, Vec3::ZERO, 1.0, k, c),));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        for _ in 0..180 {
            pw.step_fixed(&mut w);
        }
        let x = pos(&w, e).x - 1.0;
        assert!(x.abs() < 0.02 && vel(&w, e).length() < 0.2, "x {x}");

        let mut w = hecs::World::new();
        let e = body(&mut w, Vec3::new(2.0, 1.0, 0.0));
        w.spawn((Joint::spring_to_point(e, Vec3::ZERO, 1.0, 1.0e9, 1.0e9),));
        let mut pw = PhysicsWorld::new(no_damp());
        for _ in 0..600 {
            pw.step_fixed(&mut w);
            let p = pos(&w, e);
            assert!(p.is_finite() && p.length() < 10.0, "exploded: {p:?}");
        }
    }

    #[test]
    fn chain_hangs_without_exploding() {
        let mut w = hecs::World::new();
        let anchor = Vec3::new(0.0, 10.0, 0.0);
        let n = 12;
        let link = 0.5;
        let mut prev: Option<Entity> = None;
        let mut ids = Vec::new();
        for i in 0..n {
            let e = body(&mut w, anchor + Vec3::new((i + 1) as f32 * link, 0.0, 0.0)); // starts horizontal
            match prev {
                None => w.spawn((Joint::distance_to_point(e, anchor, link),)),
                Some(p) => w.spawn((Joint::distance(p, e, link),)),
            };
            ids.push(e);
            prev = Some(e);
        }
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        let mut worst_link = 0.0f32;
        let mut vmax = 0.0f32;
        for _ in 0..600 {
            pw.step_fixed(&mut w);
            let mut last = anchor;
            for &e in &ids {
                let p = pos(&w, e);
                assert!(p.is_finite());
                worst_link = worst_link.max(((p - last).length() - link).abs());
                last = p;
                vmax = vmax.max(vel(&w, e).length());
            }
            // Never farther from the anchor than the full chain length.
            assert!((last - anchor).length() <= n as f32 * link * 1.02);
        }
        // Per-link stretch under 10% of the link length even with a 12-link whip.
        assert!(worst_link < 0.05, "link error {worst_link}");
        assert!(vmax < 25.0, "vmax {vmax}");
    }

    #[test]
    fn rope_is_slack_then_taut() {
        let mut w = hecs::World::new();
        let pivot = Vec3::new(0.0, 10.0, 0.0);
        let e = body(&mut w, pivot - Vec3::new(0.0, 0.5, 0.0));
        w.spawn((Joint::rope_to_point(e, pivot, 2.0),));
        let mut pw = PhysicsWorld::new(no_damp());
        // Free fall while slack: after 0.2 s it has dropped by ~0.196 m, unconstrained.
        for _ in 0..12 {
            pw.step_fixed(&mut w);
        }
        let drop = 9.5 - pos(&w, e).y;
        assert!((drop - 0.5 * 9.8 * 0.04).abs() < 0.05, "drop {drop}");
        for _ in 0..300 {
            pw.step_fixed(&mut w);
            assert!((pos(&w, e) - pivot).length() < 2.03);
        }
        assert!(pos(&w, e).y < 8.2, "should hang near full length: {:?}", pos(&w, e));
    }

    #[test]
    fn pin_with_rotated_anchor_and_static_partner() {
        let mut w = hecs::World::new();
        // Body rotated +90deg about Z: local +X anchor points along world +Y.
        let e = w.spawn((
            Transform { position: Vec3::new(5.0, 5.0, 0.0), rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2), scale: Vec3::ONE },
            Collider::sphere(0.1),
            RigidBody::default(),
        ));
        w.spawn((Joint::pin_to_point(e, Vec3::new(5.0, 8.0, 0.0)).with_anchors(Vec3::new(1.0, 0.0, 0.0), Vec3::new(5.0, 8.0, 0.0)),));
        // And a pin to a bare static Transform entity.
        let post = w.spawn((Transform::from_position(Vec3::new(-5.0, 6.0, 0.0)),));
        let f = body(&mut w, Vec3::new(-5.0, 4.0, 0.0));
        w.spawn((Joint::pin(f, post),));
        let mut pw = PhysicsWorld::new(no_damp());
        for _ in 0..120 {
            pw.step_fixed(&mut w);
        }
        // Anchor (centre + 1 up) sits on the world point -> centre ~7.
        assert!((pos(&w, e) - Vec3::new(5.0, 7.0, 0.0)).length() < 0.01, "{:?}", pos(&w, e));
        assert!((pos(&w, f) - Vec3::new(-5.0, 6.0, 0.0)).length() < 0.01, "{:?}", pos(&w, f));
        assert_eq!(pos(&w, post), Vec3::new(-5.0, 6.0, 0.0));
    }

    #[test]
    fn joint_only_bodies_without_collider_move_and_dead_endpoints_are_ignored() {
        let mut w = hecs::World::new();
        let pivot = Vec3::new(0.0, 5.0, 0.0);
        let e = w.spawn((Transform::from_position(pivot + Vec3::X), RigidBody::default()));
        w.spawn((Joint::distance_to_point(e, pivot, 1.0),));
        let ghost = body(&mut w, Vec3::ZERO);
        w.spawn((Joint::distance_to_point(ghost, pivot, 1.0),));
        w.despawn(ghost).unwrap();
        let mut pw = PhysicsWorld::new(no_damp());
        for _ in 0..60 {
            pw.step_fixed(&mut w);
        }
        assert!(pos(&w, e).y < 4.9, "swung down: {:?}", pos(&w, e));
        assert!(((pos(&w, e) - pivot).length() - 1.0).abs() < 0.01);
    }

    #[test]
    fn jointed_bodies_never_sleep_and_collide_connected_flag() {
        let mut w = hecs::World::new();
        w.spawn((Transform::from_position(Vec3::new(0.0, -0.5, 0.0)), Collider::aabb(Vec3::new(50.0, 0.5, 50.0))));
        // Resting on the ground, tied to a far world point by a loose rope.
        let tied = body(&mut w, Vec3::new(0.0, 0.1, 0.0));
        w.spawn((Joint::rope_to_point(tied, Vec3::new(0.0, 0.1, 5.0), 10.0),));
        let free = body(&mut w, Vec3::new(3.0, 0.1, 0.0));
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        for _ in 0..240 {
            pw.step_fixed(&mut w);
        }
        assert!(pw.is_sleeping(free) && !pw.is_sleeping(tied));

        // Two pinned, fully overlapping spheres: only coexist if collide_connected is false.
        for (flag, expect_apart) in [(false, false), (true, true)] {
            let mut w = hecs::World::new();
            let a = body(&mut w, Vec3::ZERO);
            let b = body(&mut w, Vec3::new(0.05, 0.0, 0.0));
            w.spawn((Joint::distance(a, b, 0.05).with_collide_connected(flag),));
            let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
            for _ in 0..30 {
                pw.step_fixed(&mut w);
            }
            let d = (pos(&w, a) - pos(&w, b)).length();
            // Radii 0.1 each: contact pushes them apart (>0.05) unless ignored; the rod pulls back.
            assert_eq!(!pw.last_contacts().is_empty(), expect_apart, "flag {flag} d {d}");
        }
    }

    #[test]
    fn legacy_step_also_solves_joints() {
        let mut w = hecs::World::new();
        let pivot = Vec3::new(0.0, 5.0, 0.0);
        let e = body(&mut w, pivot + Vec3::X);
        w.spawn((Joint::distance_to_point(e, pivot, 1.0),));
        let p = no_damp();
        for _ in 0..120 {
            crate::physics::step(&mut w, 1.0 / 60.0, &p);
        }
        assert!(((pos(&w, e) - pivot).length() - 1.0).abs() < 0.02);
        assert!(pos(&w, e).y < 5.0);
    }
}
