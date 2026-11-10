//! Local <-> world transform helpers.
//!
//! In this engine `Transform` is **local** (relative to the `Parent`, or world
//! for roots). Nothing in the renderer propagates it yet, so editor code that
//! wants "where is this thing in the world" or "put this thing at a world
//! pose" goes through these helpers.

use glam::{Mat4, Quat, Vec3};
use hecs::{Entity, World};

use super::hierarchy::parent_of;
use crate::ecs::Transform;

fn local(world: &World, e: Entity) -> Transform {
    world.get::<&Transform>(e).map(|t| *t).unwrap_or_default()
}

/// Parent chain composed root-first. Guarded against cyclic data.
pub fn parent_world_matrix(world: &World, e: Entity) -> Mat4 {
    let mut chain = Vec::new();
    let mut cur = parent_of(world, e);
    while let Some(p) = cur {
        chain.push(p);
        cur = parent_of(world, p);
        if chain.len() > 10_000 {
            break;
        }
    }
    chain.iter().rev().fold(Mat4::IDENTITY, |m, p| m * local(world, *p).matrix())
}

pub fn world_matrix(world: &World, e: Entity) -> Mat4 {
    parent_world_matrix(world, e) * local(world, e).matrix()
}

pub fn world_transform(world: &World, e: Entity) -> Transform {
    let (scale, rotation, position) = world_matrix(world, e).to_scale_rotation_translation();
    Transform { position, rotation, scale }
}

/// Local transform that yields `target` in the world under `parent_world`.
/// Exact unless the parent has non-uniform scale and `target` is rotated
/// relative to it (shear can't be represented by TRS).
pub fn local_for_world(parent_world: &Mat4, target: &Transform) -> Transform {
    let m = parent_world.inverse() * target.matrix();
    let (scale, rotation, position) = m.to_scale_rotation_translation();
    Transform { position, rotation: rotation.normalize(), scale }
}

/// Sets the entity's local transform so its world transform becomes `target`.
/// Returns false if the entity has no `Transform`.
pub fn set_world_transform(world: &mut World, e: Entity, target: &Transform) -> bool {
    let pw = parent_world_matrix(world, e);
    let new = local_for_world(&pw, target);
    match world.get::<&mut Transform>(e) {
        Ok(mut t) => {
            *t = new;
            true
        }
        Err(_) => false,
    }
}

pub fn world_position(world: &World, e: Entity) -> Vec3 {
    world_matrix(world, e).w_axis.truncate()
}

pub fn world_rotation(world: &World, e: Entity) -> Quat {
    world_transform(world, e).rotation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::hierarchy::reparent;

    fn tr(p: Vec3, r: Quat, s: Vec3) -> Transform {
        Transform { position: p, rotation: r, scale: s }
    }

    #[test]
    fn root_world_equals_local() {
        let mut w = World::new();
        let e = w.spawn((Transform::from_position(Vec3::new(1.0, 2.0, 3.0)),));
        assert_eq!(world_position(&w, e), Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn child_composes_with_parent() {
        let mut w = World::new();
        let p = w.spawn((tr(Vec3::new(10.0, 0.0, 0.0), Quat::from_rotation_y(90f32.to_radians()), Vec3::splat(2.0)),));
        let c = w.spawn((Transform::from_position(Vec3::X),));
        reparent(&mut w, c, Some(p), None).unwrap();
        // local +X, scaled x2, rotated 90deg about Y => -Z*2, plus parent offset
        let wp = world_position(&w, c);
        assert!((wp - Vec3::new(10.0, 0.0, -2.0)).length() < 1e-4, "{wp:?}");
    }

    #[test]
    fn set_world_round_trips_through_parent() {
        let mut w = World::new();
        let g = w.spawn((tr(Vec3::new(1.0, 1.0, 1.0), Quat::from_rotation_z(0.5), Vec3::splat(3.0)),));
        let p = w.spawn((tr(Vec3::new(0.0, 2.0, 0.0), Quat::from_rotation_x(0.3), Vec3::ONE),));
        let c = w.spawn((Transform::default(),));
        reparent(&mut w, p, Some(g), None).unwrap();
        reparent(&mut w, c, Some(p), None).unwrap();
        let target = tr(Vec3::new(5.0, -1.0, 2.0), Quat::from_rotation_y(1.0), Vec3::splat(1.5));
        assert!(set_world_transform(&mut w, c, &target));
        let got = world_transform(&w, c);
        assert!((got.position - target.position).length() < 1e-4);
        assert!(got.rotation.angle_between(target.rotation) < 1e-4);
        assert!((got.scale - target.scale).length() < 1e-4);
    }

    #[test]
    fn missing_transform_is_reported() {
        let mut w = World::new();
        let e = w.spawn(());
        assert!(!set_world_transform(&mut w, e, &Transform::default()));
    }
}
