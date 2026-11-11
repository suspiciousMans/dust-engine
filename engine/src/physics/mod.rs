use glam::Vec3;
use serde::{Deserialize, Serialize};

mod controller;
mod debug;
mod events;
mod layers;
mod narrow;
mod query;
mod register;
mod sim;

pub use controller::{CharacterController, MoveResult};
pub use events::{physics_step, physics_system, TriggerEvents};
pub use debug::{DebugColor, DebugShape, PhysicsDebug, PhysicsDebugOptions};
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
    sim::run_step(world, dt, params, 1, None).trigger_overlaps
}
