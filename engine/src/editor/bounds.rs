//! Axis-aligned bounds and the "what does the viewport consider this entity's
//! shape" query used by picking, box select and snapping.

use std::sync::Arc;

use glam::{Mat4, Vec3};
use hecs::{Entity, World};

use super::hierarchy::{Primitive, PrimitiveShape};
use crate::ecs::MeshRenderer;
use crate::mesh::MeshData;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn from_center_half(c: Vec3, half: Vec3) -> Self {
        Self { min: c - half, max: c + half }
    }
    pub fn from_points(points: impl IntoIterator<Item = Vec3>) -> Option<Self> {
        let mut it = points.into_iter();
        let first = it.next()?;
        Some(it.fold(Self { min: first, max: first }, |b, p| Self { min: b.min.min(p), max: b.max.max(p) }))
    }
    pub fn of_mesh(mesh: &MeshData) -> Option<Self> {
        Self::from_points(mesh.vertices.iter().map(|v| Vec3::from_array(v.position)))
    }
    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }
    pub fn half_extents(&self) -> Vec3 {
        (self.max - self.min) * 0.5
    }
    pub fn corners(&self) -> [Vec3; 8] {
        let (a, b) = (self.min, self.max);
        [
            Vec3::new(a.x, a.y, a.z),
            Vec3::new(b.x, a.y, a.z),
            Vec3::new(a.x, b.y, a.z),
            Vec3::new(b.x, b.y, a.z),
            Vec3::new(a.x, a.y, b.z),
            Vec3::new(b.x, a.y, b.z),
            Vec3::new(a.x, b.y, b.z),
            Vec3::new(b.x, b.y, b.z),
        ]
    }
    /// Bounds of the 8 corners after `m`.
    pub fn transformed(&self, m: &Mat4) -> Aabb {
        Aabb::from_points(self.corners().map(|c| m.transform_point3(c))).unwrap()
    }
    pub fn union(&self, o: &Aabb) -> Aabb {
        Aabb { min: self.min.min(o.min), max: self.max.max(o.max) }
    }
}

/// Explicit local-space bounds for an entity (set by the mesh loader bridge,
/// e.g. from [`Aabb::of_mesh`]). `GpuMesh` keeps no CPU copy of its vertices,
/// so without this component meshes fall back to a unit cube.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalBounds(pub Aabb);

/// Optional exact pick geometry (local-space triangles). When present,
/// picking ray-casts these instead of the box.
#[derive(Clone, Debug)]
pub struct PickTriangles(pub Arc<Vec<[Vec3; 3]>>);

/// Radius of the pick sphere for entities with a transform but no mesh
/// (empties, lights, cameras).
pub const GIZMO_ICON_RADIUS: f32 = 0.3;

/// Local-space bounds used for picking / box select / snapping.
pub fn local_bounds(world: &World, e: Entity) -> Option<Aabb> {
    if let Ok(b) = world.get::<&LocalBounds>(e) {
        return Some(b.0);
    }
    if let Ok(p) = world.get::<&Primitive>(e) {
        let half = match p.0 {
            PrimitiveShape::Cube | PrimitiveShape::Sphere => Vec3::splat(0.5),
            PrimitiveShape::Plane => Vec3::new(1.0, 0.02, 1.0),
            PrimitiveShape::Capsule => Vec3::new(0.5, 1.0, 0.5),
        };
        return Some(Aabb::from_center_half(Vec3::ZERO, half));
    }
    if world.satisfies::<&MeshRenderer>(e).unwrap_or(false) {
        return Some(Aabb::from_center_half(Vec3::ZERO, Vec3::splat(0.5)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::Transform;
    use crate::mesh::primitives;

    #[test]
    fn mesh_bounds_of_cube() {
        let b = Aabb::of_mesh(&primitives::cube()).unwrap();
        assert_eq!(b.min, Vec3::splat(-0.5));
        assert_eq!(b.max, Vec3::splat(0.5));
        let p = Aabb::of_mesh(&primitives::plane()).unwrap();
        assert_eq!(p.half_extents(), Vec3::new(1.0, 0.0, 1.0));
        assert!(Aabb::of_mesh(&MeshData::default()).is_none());
    }

    #[test]
    fn transformed_rotated_box_grows() {
        let b = Aabb::from_center_half(Vec3::ZERO, Vec3::splat(1.0));
        let m = Mat4::from_rotation_y(45f32.to_radians());
        let t = b.transformed(&m);
        assert!((t.max.x - 2f32.sqrt()).abs() < 1e-5);
        assert!((t.max.y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn local_bounds_priority() {
        let mut w = World::new();
        let none = w.spawn((Transform::default(),));
        assert!(local_bounds(&w, none).is_none());
        let prim = w.spawn((Primitive(PrimitiveShape::Plane),));
        assert_eq!(local_bounds(&w, prim).unwrap().half_extents().x, 1.0);
        let explicit = w.spawn((Primitive(PrimitiveShape::Cube), LocalBounds(Aabb::from_center_half(Vec3::Y, Vec3::splat(3.0)))));
        assert_eq!(local_bounds(&w, explicit).unwrap().center(), Vec3::Y);
    }
}
