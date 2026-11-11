//! Inspector registration for physics components.

use super::{CharacterController, Joint, Collider, ColliderShape, CollisionLayers, LayerMask, PhysicsMaterial, RigidBody};
use crate::editor::inspector::{ComponentBuilder, Registry};

fn bits_to_string(m: LayerMask) -> String {
    (0..32).filter(|i| m.0 & (1 << i) != 0).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

fn string_to_bits(s: &str, old: LayerMask) -> LayerMask {
    let mut m = 0u32;
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.parse::<u32>() {
            Ok(i) if i < 32 => m |= 1 << i,
            _ => return old, // reject malformed input, keep the old value
        }
    }
    LayerMask(m)
}

/// Registers `Collider`, `RigidBody`, `CollisionLayers`, `PhysicsMaterial`
/// and `CharacterController` with the editor inspector. [`Joint`] holds entity
/// references the inspector has no field type for, so it is registered clone-only
/// (copied by duplicate / undo, not shown or edited).
pub fn register_physics_components(reg: &mut Registry) {
    reg.register_cloneable::<Joint>();
    reg.register(
        ComponentBuilder::new("Collider", || Collider::sphere(0.5))
            .enumeration(
                "shape",
                &["Sphere", "Aabb", "Capsule", "Box"],
                |c| match c.shape {
                    ColliderShape::Sphere { .. } => 0,
                    ColliderShape::Aabb { .. } => 1,
                    ColliderShape::Capsule { .. } => 2,
                    ColliderShape::Box { .. } => 3,
                },
                |c, i| {
                    c.shape = match i {
                        0 => ColliderShape::Sphere { radius: 0.5 },
                        1 => ColliderShape::Aabb { half_extents: glam::Vec3::splat(0.5) },
                        2 => ColliderShape::Capsule { radius: 0.4, half_height: 0.5 },
                        _ => ColliderShape::Box { half_extents: glam::Vec3::splat(0.5) },
                    }
                },
            )
            .tooltip("Switching shape resets its size")
            .f32(
                "radius",
                |c| match c.shape {
                    ColliderShape::Sphere { radius } | ColliderShape::Capsule { radius, .. } => radius,
                    _ => 0.0,
                },
                |c, v| match &mut c.shape {
                    ColliderShape::Sphere { radius } | ColliderShape::Capsule { radius, .. } => *radius = v.max(0.001),
                    _ => {}
                },
            )
            .range(0.001, 100.0)
            .speed(0.01)
            .tooltip("Sphere / Capsule only")
            .f32(
                "half_height",
                |c| if let ColliderShape::Capsule { half_height, .. } = c.shape { half_height } else { 0.0 },
                |c, v| {
                    if let ColliderShape::Capsule { half_height, .. } = &mut c.shape {
                        *half_height = v.max(0.0)
                    }
                },
            )
            .range(0.0, 100.0)
            .speed(0.01)
            .tooltip("Capsule only: half the straight section")
            .vec3(
                "half_extents",
                |c| match c.shape {
                    ColliderShape::Aabb { half_extents } | ColliderShape::Box { half_extents } => half_extents,
                    _ => glam::Vec3::ZERO,
                },
                |c, v| match &mut c.shape {
                    ColliderShape::Aabb { half_extents } | ColliderShape::Box { half_extents } => *half_extents = v.max(glam::Vec3::splat(0.001)),
                    _ => {}
                },
            )
            .tooltip("Aabb / Box only")
            .bool("is_trigger", |c| c.is_trigger, |c, v| c.is_trigger = v),
    );
    reg.register(
        ComponentBuilder::new("RigidBody", RigidBody::default)
            .vec3("velocity", |b| b.velocity, |b, v| b.velocity = v)
            .bool("grounded", |b| b.grounded, |b, v| b.grounded = v),
    );
    reg.register(
        ComponentBuilder::new("CollisionLayers", CollisionLayers::default)
            .string("layer", |l| bits_to_string(l.layer), |l, s| l.layer = string_to_bits(&s, l.layer))
            .tooltip("Comma-separated layer bit indices this collider is on, e.g. 1,3")
            .string("mask", |l| bits_to_string(l.mask), |l, s| l.mask = string_to_bits(&s, l.mask))
            .tooltip("Comma-separated layer bit indices it collides with"),
    );
    reg.register(
        ComponentBuilder::new("PhysicsMaterial", PhysicsMaterial::default)
            .f32("friction", |m| m.friction, |m, v| m.friction = v)
            .range(0.0, 2.0)
            .speed(0.01)
            .f32("restitution", |m| m.restitution, |m, v| m.restitution = v)
            .range(0.0, 1.0)
            .speed(0.01),
    );
    reg.register(
        ComponentBuilder::new("CharacterController", CharacterController::default)
            .f32("radius", |c| c.radius, |c, v| c.radius = v.max(0.01))
            .range(0.01, 5.0)
            .f32("half_height", |c| c.half_height, |c, v| c.half_height = v.max(0.0))
            .range(0.0, 5.0)
            .f32("step_height", |c| c.step_height, |c, v| c.step_height = v.max(0.0))
            .range(0.0, 2.0)
            .f32("max_slope_deg", |c| c.max_slope_deg, |c, v| c.max_slope_deg = v.clamp(0.0, 89.0))
            .range(0.0, 89.0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::inspector::Value;

    #[test]
    fn registers_and_edits() {
        let mut reg = Registry::new();
        register_physics_components(&mut reg);
        for n in ["Collider", "RigidBody", "CollisionLayers", "PhysicsMaterial", "CharacterController"] {
            assert!(reg.find(n).is_some(), "{n}");
        }
        let mut w = hecs::World::new();
        let e = w.spawn((Collider::sphere(0.5), CollisionLayers::default()));
        assert!(reg.set_value(&mut w, e, "Collider", "radius", &Value::F32(2.0)));
        assert_eq!(w.get::<&Collider>(e).unwrap().shape, ColliderShape::Sphere { radius: 2.0 });
        assert!(reg.set_value(&mut w, e, "CollisionLayers", "layer", &Value::Str("1,3".into())));
        assert_eq!(w.get::<&CollisionLayers>(e).unwrap().layer, LayerMask(0b1010));
    }
}
