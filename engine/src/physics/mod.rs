use glam::Vec3;
use serde::{Deserialize, Serialize};

mod controller;
mod debug;
mod events;
mod joints;
mod layers;
mod narrow;
mod query;
mod register;
mod sim;

pub use controller::{CharacterController, MoveResult};
pub use events::{physics_step, physics_system, TriggerEvents};
pub use debug::{DebugColor, DebugShape, PhysicsDebug, PhysicsDebugOptions};
pub use joints::{Joint, JointKind, DEFAULT_JOINT_ITERATIONS};
pub use layers::{CollisionLayers, LayerError, LayerMask, LayerNames, PhysicsMaterial};
pub use narrow::{collide, shape_bounds, Contact, Pose};
pub use query::{Hit, PhysicsQueries, QueryFilter};
pub use register::register_physics_components;
pub use sim::{PhysicsEvent, PhysicsWorld, SleepConfig, TriggerEventKind};


/// Global tuning knobs for the simulation. Deliberately just two numbers —
/// this is meant to be an easily-editable physics engine, not a
/// feature-complete one.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PhysicsParams {
    pub gravity: f32,
    /// Fraction of velocity removed per second (crude air/ground drag).
    pub linear_damping: f32,
}

impl Default for PhysicsParams {
    fn default() -> Self {
        Self {
            gravity: 9.8,
            linear_damping: 0.15,
        }
    }
}

/// Marks an entity as simulated. Its *absence* is what makes an object with
/// a `Collider` static (collided against, but never moved) — deliberately
/// not a separate `is_dynamic` flag, so there's only one thing to keep in
/// sync rather than two.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RigidBody {
    pub velocity: Vec3,
    pub grounded: bool,
}

/// Collision shapes. All are centred on the entity's `Transform::position`.
/// * `Sphere`/`Aabb`: as before (an `Aabb` is always axis-aligned, rotation ignored).
/// * `Capsule`: swept sphere along the entity's local +Y; `half_height` is half
///   the length of the straight section (total height = `2*(half_height+radius)`).
/// * `Box`: oriented box, rotated by `Transform::rotation`.
///
/// Bodies are translation-only (no angular dynamics), so rotation is something
/// the game or editor sets; it affects collision shape, not simulation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColliderShape {
    Sphere { radius: f32 },
    Aabb { half_extents: Vec3 },
    Capsule { radius: f32, half_height: f32 },
    Box { half_extents: Vec3 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collider {
    pub shape: ColliderShape,
    /// A trigger is never physically solid — overlapping it never pushes
    /// anything out or zeroes velocity, it's only ever reported in `step`'s
    /// returned overlap list for game logic (see `Sandbox::on_trigger_entered`
    /// in the sandbox for the demo use).
    pub is_trigger: bool,
}

impl Collider {
    pub fn new(shape: ColliderShape) -> Self {
        Self { shape, is_trigger: false }
    }
    pub fn trigger(shape: ColliderShape) -> Self {
        Self { shape, is_trigger: true }
    }
    pub fn sphere(radius: f32) -> Self {
        Self::new(ColliderShape::Sphere { radius })
    }
    pub fn capsule(radius: f32, half_height: f32) -> Self {
        Self::new(ColliderShape::Capsule { radius, half_height })
    }
    pub fn aabb(half_extents: Vec3) -> Self {
        Self::new(ColliderShape::Aabb { half_extents })
    }
    pub fn oriented_box(half_extents: Vec3) -> Self {
        Self::new(ColliderShape::Box { half_extents })
    }
}

/// Advances the simulation by `dt` seconds: integrates every dynamic body
/// (gravity + damping + move), then resolves collisions by pushing dynamic
/// bodies out of whatever they overlap, except pairs involving a trigger,
/// which are reported (dynamic entity, other entity) in the returned list
/// instead of being physically resolved.
///
/// This is the original stateless entry point and keeps its original
/// behaviour (one resolution pass, no sleeping). Entities may now also carry
/// [`CollisionLayers`] and [`PhysicsMaterial`], which this honours. For events,
/// fixed timestep, sleeping and multiple solver passes use [`PhysicsWorld`].
pub fn step(world: &mut hecs::World, dt: f32, params: &PhysicsParams) -> Vec<(hecs::Entity, hecs::Entity)> {
    sim::run_step(world, dt, params, 1, DEFAULT_JOINT_ITERATIONS, None).trigger_overlaps
}

/// Compiles and runs the examples shown in GUIDE.md's Physics section, so the
/// guide cannot drift away from the real signatures.
#[cfg(test)]
mod guide_examples {
    use super::*;
    use crate::ecs::Transform;
    use glam::{Quat, Vec3};

    #[test]
    fn guide_physics_section() {
        let mut world = hecs::World::new();
        let floor = world.spawn((Transform::from_position(Vec3::new(0.0, -0.5, 0.0)), Collider::aabb(Vec3::new(20.0, 0.5, 20.0))));
        let ball = world.spawn((
            Transform::from_position(Vec3::new(0.0, 3.0, 0.0)),
            Collider::sphere(0.5),
            RigidBody::default(),
            PhysicsMaterial { friction: 0.4, restitution: 0.3 },
        ));
        let sensor = world.spawn((Transform::from_position(Vec3::new(0.0, 0.5, 0.0)), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::splat(1.0) })));
        let anchor = Vec3::new(5.0, 6.0, 0.0);
        let bob = world.spawn((Transform::from_position(anchor + Vec3::X * 2.0), Collider::sphere(0.25), RigidBody::default()));
        world.spawn((Joint::distance_to_point(bob, anchor, 2.0),));

        // Queries
        let down = world.raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 100.0, u32::MAX).expect("hit");
        assert_eq!(down.entity, ball);
        let filter = QueryFilter::default().excluding(ball);
        let hits = world.raycast_all(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 100.0, &filter.with_triggers());
        assert_eq!(hits.iter().map(|h| h.entity).collect::<Vec<_>>(), vec![sensor, floor]);
        let _ = world.box_sweep(Vec3::Y * 5.0, Vec3::splat(0.5), Quat::IDENTITY, Vec3::NEG_Y, 10.0, u32::MAX);
        let _ = world.overlap_sphere(Vec3::ZERO, 2.0, u32::MAX);

        // Stateful simulation + trigger events as a resource
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        let mut entered = false;
        for _ in 0..120 {
            for ev in pw.update(&mut world, 1.0 / 60.0) {
                entered |= ev.kind == TriggerEventKind::Enter && ev.entity == ball && ev.other == sensor;
            }
        }
        assert!(entered);
        assert!(world.get::<&Transform>(ball).unwrap().position.y > 0.0);
        let p = world.get::<&Transform>(bob).unwrap().position;
        assert!(((p - anchor).length() - 2.0).abs() < 0.05);

        // Character controller
        let player = world.spawn((Transform::from_position(Vec3::new(10.0, 2.0, 10.0)),));
        let mut cc = CharacterController::default();
        let r = cc.move_and_slide(&mut world, player, Vec3::new(0.0, -5.0, 0.0)).unwrap();
        assert!(r.grounded);
    }
}
