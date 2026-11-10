//! Spatial queries: raycast, shape sweeps (sphere/capsule/any shape) and
//! overlap tests, as methods on `hecs::World` via [`PhysicsQueries`].
//!
//! All queries read `Transform` + `Collider` (+ optional `CollisionLayers`).
//! Triggers are skipped unless `QueryFilter::include_triggers` is set.
//! `layer_mask` arguments are tested against the collider's *layer* bits.

use glam::{Quat, Vec3};
use hecs::{Entity, World};

use super::layers::{CollisionLayers, LayerMask};
use super::narrow::{collide, shape_bounds, Pose};
use super::{Collider, ColliderShape};
use crate::ecs::Transform;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub entity: Entity,
    /// World-space point of impact.
    pub point: Vec3,
    /// Surface normal at the hit, facing back toward the query.
    pub normal: Vec3,
    /// Distance travelled along the (unit) direction before impact.
    pub distance: f32,
    /// The query shape/origin already overlapped the collider at distance 0.
    pub started_inside: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct QueryFilter {
    pub mask: LayerMask,
    pub exclude: Option<Entity>,
    pub include_triggers: bool,
}

impl Default for QueryFilter {
    fn default() -> Self {
        Self { mask: LayerMask::ALL, exclude: None, include_triggers: false }
    }
}

impl QueryFilter {
    pub fn mask(mask: u32) -> Self {
        Self { mask: LayerMask(mask), ..Default::default() }
    }
    pub fn excluding(mut self, e: Entity) -> Self {
        self.exclude = Some(e);
        self
    }
    pub fn with_triggers(mut self) -> Self {
        self.include_triggers = true;
        self
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Cand {
    pub entity: Entity,
    pub shape: ColliderShape,
    pub pose: Pose,
}

pub(crate) fn candidates(world: &World, filter: &QueryFilter) -> Vec<Cand> {
    let mut out: Vec<Cand> = world
        .query::<(&Transform, &Collider, Option<&CollisionLayers>)>()
        .iter()
        .filter(|(e, (_, c, layers))| {
            Some(*e) != filter.exclude
                && (filter.include_triggers || !c.is_trigger)
                && layers.map_or(LayerMask::bit(0), |l| l.layer).intersects(filter.mask)
        })
        .map(|(entity, (t, c, _))| Cand { entity, shape: c.shape, pose: Pose::from(t) })
        .collect();
    // hecs iteration order is archetype-dependent; make results order-stable.
    out.sort_by_key(|c| c.entity.to_bits());
    out
}

// ----------------------------------------------------------------- rays

fn ray_sphere(o: Vec3, d: Vec3, center: Vec3, r: f32) -> Option<f32> {
    let m = o - center;
    let b = m.dot(d);
    let c = m.dot(m) - r * r;
    if c > 0.0 && b > 0.0 {
        return None;
    }
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    Some((-b - disc.sqrt()).max(0.0))
}

/// Ray vs capsule whose axis is local Y (segment y in [-hh, hh]); returns (t, local normal).
fn ray_capsule_local(o: Vec3, d: Vec3, r: f32, hh: f32) -> Option<(f32, Vec3)> {
    let closest = Vec3::new(0.0, o.y.clamp(-hh, hh), 0.0);
    if (o - closest).length_squared() <= r * r {
        return Some((0.0, -d));
    }
    let mut best: Option<(f32, Vec3)> = None;
    let mut consider = |t: f32, n: Vec3| {
        if t >= 0.0 && best.map_or(true, |(bt, _)| t < bt) {
            best = Some((t, n));
        }
    };
    if hh > 0.0 {
        let a = d.x * d.x + d.z * d.z;
        if a > 1e-12 {
            let b = o.x * d.x + o.z * d.z;
            let c = o.x * o.x + o.z * o.z - r * r;
            let disc = b * b - a * c;
            if disc >= 0.0 {
                let t = (-b - disc.sqrt()) / a;
                let y = o.y + t * d.y;
                if t >= 0.0 && y.abs() <= hh {
                    consider(t, Vec3::new(o.x + t * d.x, 0.0, o.z + t * d.z) / r);
                }
            }
        }
    }
    for sy in [-hh, hh] {
        let c = Vec3::new(0.0, sy, 0.0);
        if let Some(t) = ray_sphere(o, d, c, r) {
            let p = o + d * t;
            // Only the outward hemisphere belongs to this cap.
            if (sy >= 0.0 && p.y >= sy) || (sy <= 0.0 && p.y <= sy) {
                consider(t, (p - c) / r);
            }
        }
    }
    best
}

fn ray_box_local(o: Vec3, d: Vec3, h: Vec3) -> Option<(f32, Vec3)> {
    if o.abs().cmple(h).all() {
        return Some((0.0, -d));
    }
    let (mut tmin, mut tmax) = (0.0f32, f32::MAX);
    let mut normal = Vec3::ZERO;
    for i in 0..3 {
        if d[i].abs() < 1e-9 {
            if o[i].abs() > h[i] {
                return None;
            }
        } else {
            let inv = 1.0 / d[i];
            let (mut t1, mut t2) = ((-h[i] - o[i]) * inv, (h[i] - o[i]) * inv);
            let mut sign = -1.0;
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
                sign = 1.0;
            }
            if t1 > tmin {
                tmin = t1;
                normal = Vec3::ZERO;
                normal[i] = sign;
            }
            tmax = tmax.min(t2);
            if tmin > tmax {
                return None;
            }
        }
    }
    Some((tmin, normal))
}

/// Ray vs one posed shape. `dir` must be unit length.
pub fn ray_shape(shape: ColliderShape, pose: Pose, origin: Vec3, dir: Vec3) -> Option<(f32, Vec3)> {
    match shape {
        ColliderShape::Sphere { radius } => {
            let t = ray_sphere(origin, dir, pose.position, radius)?;
            let p = origin + dir * t;
            let n = if t == 0.0 { -dir } else { (p - pose.position) / radius };
            Some((t, n))
        }
        ColliderShape::Aabb { half_extents } => ray_box_local(origin - pose.position, dir, half_extents),
        ColliderShape::Box { half_extents } => {
            let inv = pose.rotation.inverse();
            let (t, n) = ray_box_local(inv * (origin - pose.position), inv * dir, half_extents)?;
            Some((t, if t == 0.0 { -dir } else { pose.rotation * n }))
        }
        ColliderShape::Capsule { radius, half_height } => {
            let inv = pose.rotation.inverse();
            let (t, n) = ray_capsule_local(inv * (origin - pose.position), inv * dir, radius, half_height)?;
            Some((t, if t == 0.0 { -dir } else { pose.rotation * n }))
        }
    }
}

// ------------------------------------------------------------ shape casts

fn min_extent(shape: ColliderShape) -> f32 {
    match shape {
        ColliderShape::Sphere { radius } | ColliderShape::Capsule { radius, .. } => radius,
        ColliderShape::Aabb { half_extents } | ColliderShape::Box { half_extents } => {
            half_extents.min_element()
        }
    }
}

fn deepest(cands: &[Cand], shape: ColliderShape, pose: Pose) -> Option<(Entity, super::narrow::Contact)> {
    let mut best: Option<(Entity, super::narrow::Contact)> = None;
    for c in cands {
        if let Some(contact) = collide(shape, pose, c.shape, c.pose) {
            if best.map_or(true, |(_, b)| contact.depth > b.depth) {
                best = Some((c.entity, contact));
            }
        }
    }
    best
}

pub(crate) fn cast_against(cands: &[Cand], shape: ColliderShape, pose: Pose, dir: Vec3, max: f32) -> Option<Hit> {
    let dir = dir.try_normalize()?;
    if let Some((entity, c)) = deepest(cands, shape, pose) {
        return Some(Hit { entity, point: c.point, normal: c.normal, distance: 0.0, started_inside: true });
    }
    // Cull to colliders touching the swept volume.
    let (lo0, hi0) = shape_bounds(shape, pose);
    let end = dir * max;
    let (lo, hi) = (lo0.min(lo0 + end), hi0.max(hi0 + end));
    let near: Vec<Cand> = cands
        .iter()
        .copied()
        .filter(|c| {
            let (clo, chi) = shape_bounds(c.shape, c.pose);
            clo.cmple(hi).all() && chi.cmpge(lo).all()
        })
        .collect();
    if near.is_empty() || max <= 0.0 {
        return None;
    }
    let at = |t: f32| Pose::new(pose.position + dir * t, pose.rotation);
    let step = (min_extent(shape) * 0.5).max(0.005).max(max / 2048.0);
    let mut t_lo = 0.0;
    loop {
        let t_hi = (t_lo + step).min(max);
        if deepest(&near, shape, at(t_hi)).is_some() {
            // Bisect the first overlap inside (t_lo, t_hi].
            let (mut a, mut b) = (t_lo, t_hi);
            for _ in 0..24 {
                let m = 0.5 * (a + b);
                if deepest(&near, shape, at(m)).is_some() {
                    b = m;
                } else {
                    a = m;
                }
            }
            let (entity, c) = deepest(&near, shape, at(b))?;
            return Some(Hit { entity, point: c.point, normal: c.normal, distance: a, started_inside: false });
        }
        if t_hi >= max {
            return None;
        }
        t_lo = t_hi;
    }
}

/// World queries. Implemented for `hecs::World`, so
/// `world.raycast(origin, dir, 100.0, mask)` works directly.
pub trait PhysicsQueries {
    /// First collider hit by the ray. `dir` need not be normalised.
    /// `layer_mask` selects which collider layers the ray can hit.
    fn raycast(&self, origin: Vec3, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit>;
    fn raycast_filtered(&self, origin: Vec3, dir: Vec3, max: f32, filter: &QueryFilter) -> Option<Hit>;
    /// Sweeps an arbitrary shape along `dir` and returns the first impact.
    /// If it already overlaps something, returns `started_inside` at distance 0.
    fn shape_cast(&self, shape: ColliderShape, pose: Pose, dir: Vec3, max: f32, filter: &QueryFilter) -> Option<Hit>;
    fn spherecast(&self, origin: Vec3, radius: f32, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit>;
    /// Upright capsule sweep (`center`, cylinder `half_height`, `radius`).
    fn capsule_sweep(&self, center: Vec3, radius: f32, half_height: f32, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit>;
    /// Entities whose colliders overlap `shape` at `pose`, sorted by entity id.
    fn overlap_shape(&self, shape: ColliderShape, pose: Pose, filter: &QueryFilter) -> Vec<Entity>;
    fn overlap_sphere(&self, center: Vec3, radius: f32, layer_mask: u32) -> Vec<Entity>;
}

impl PhysicsQueries for World {
    fn raycast(&self, origin: Vec3, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit> {
        self.raycast_filtered(origin, dir, max, &QueryFilter::mask(layer_mask))
    }

    fn raycast_filtered(&self, origin: Vec3, dir: Vec3, max: f32, filter: &QueryFilter) -> Option<Hit> {
        let dir = dir.try_normalize()?;
        let mut best: Option<Hit> = None;
        for c in candidates(self, filter) {
            let Some((t, normal)) = ray_shape(c.shape, c.pose, origin, dir) else { continue };
            if t > max {
                continue;
            }
            // Candidates are id-sorted, so strict `<` keeps the lowest id on ties.
            if best.map_or(true, |b| t < b.distance) {
                best = Some(Hit {
                    entity: c.entity,
                    point: origin + dir * t,
                    normal,
                    distance: t,
                    started_inside: t == 0.0,
                });
            }
        }
        best
    }

    fn shape_cast(&self, shape: ColliderShape, pose: Pose, dir: Vec3, max: f32, filter: &QueryFilter) -> Option<Hit> {
        cast_against(&candidates(self, filter), shape, pose, dir, max)
    }

    fn spherecast(&self, origin: Vec3, radius: f32, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit> {
        self.shape_cast(ColliderShape::Sphere { radius }, Pose::at(origin), dir, max, &QueryFilter::mask(layer_mask))
    }

    fn capsule_sweep(&self, center: Vec3, radius: f32, half_height: f32, dir: Vec3, max: f32, layer_mask: u32) -> Option<Hit> {
        self.shape_cast(
            ColliderShape::Capsule { radius, half_height },
            Pose::new(center, Quat::IDENTITY),
            dir,
            max,
            &QueryFilter::mask(layer_mask),
        )
    }

    fn overlap_shape(&self, shape: ColliderShape, pose: Pose, filter: &QueryFilter) -> Vec<Entity> {
        candidates(self, filter)
            .into_iter()
            .filter(|c| collide(shape, pose, c.shape, c.pose).is_some())
            .map(|c| c.entity)
            .collect()
    }

    fn overlap_sphere(&self, center: Vec3, radius: f32, layer_mask: u32) -> Vec<Entity> {
        self.overlap_shape(ColliderShape::Sphere { radius }, Pose::at(center), &QueryFilter::mask(layer_mask))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(w: &mut World, pos: Vec3, shape: ColliderShape) -> Entity {
        w.spawn((Transform::from_position(pos), Collider { shape, is_trigger: false }))
    }
    fn spawn_rot(w: &mut World, pos: Vec3, rot: Quat, shape: ColliderShape) -> Entity {
        w.spawn((Transform { position: pos, rotation: rot, scale: Vec3::ONE }, Collider { shape, is_trigger: false }))
    }

    #[test]
    fn raycast_hits_each_shape() {
        let mut w = World::new();
        let s = spawn(&mut w, Vec3::new(0.0, 0.0, 10.0), ColliderShape::Sphere { radius: 1.0 });
        let a = spawn(&mut w, Vec3::new(5.0, 0.0, 10.0), ColliderShape::Aabb { half_extents: Vec3::splat(1.0) });
        let c = spawn(&mut w, Vec3::new(-5.0, 0.0, 10.0), ColliderShape::Capsule { radius: 0.5, half_height: 1.0 });
        let b = spawn_rot(
            &mut w,
            Vec3::new(10.0, 0.0, 10.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_4),
            ColliderShape::Box { half_extents: Vec3::splat(1.0) },
        );
        let h = w.raycast(Vec3::ZERO, Vec3::Z, 100.0, u32::MAX).unwrap();
        assert_eq!(h.entity, s);
        assert!((h.distance - 9.0).abs() < 1e-4);
        assert!((h.normal - -Vec3::Z).length() < 1e-4);
        assert!((h.point - Vec3::new(0.0, 0.0, 9.0)).length() < 1e-4);

        let h = w.raycast(Vec3::new(5.0, 0.0, 0.0), Vec3::Z, 100.0, u32::MAX).unwrap();
        assert_eq!(h.entity, a);
        assert!((h.distance - 9.0).abs() < 1e-4 && (h.normal - -Vec3::Z).length() < 1e-4);

        // Capsule: hit the cylinder side from -X side and the top cap from above.
        let h = w.raycast(Vec3::new(-8.0, 0.5, 10.0), Vec3::X, 100.0, u32::MAX).unwrap();
        assert_eq!(h.entity, c);
        assert!((h.distance - 2.5).abs() < 1e-4, "{}", h.distance);
        let h = w.raycast(Vec3::new(-5.0, 10.0, 10.0), -Vec3::Y, 100.0, u32::MAX).unwrap();
        assert_eq!(h.entity, c);
        assert!((h.distance - (10.0 - 1.5)).abs() < 1e-4 && (h.normal - Vec3::Y).length() < 1e-4);

        // Rotated box: diamond tip faces -Z at distance sqrt2 from centre.
        let h = w.raycast(Vec3::new(10.0, 0.0, 0.0), Vec3::Z, 100.0, u32::MAX).unwrap();
        assert_eq!(h.entity, b);
        assert!((h.distance - (10.0 - 2f32.sqrt())).abs() < 1e-3, "{}", h.distance);
    }

    #[test]
    fn raycast_nearest_wins_and_respects_max_and_miss() {
        let mut w = World::new();
        let _far = spawn(&mut w, Vec3::new(0.0, 0.0, 20.0), ColliderShape::Sphere { radius: 1.0 });
        let near = spawn(&mut w, Vec3::new(0.0, 0.0, 5.0), ColliderShape::Sphere { radius: 1.0 });
        assert_eq!(w.raycast(Vec3::ZERO, Vec3::Z, 100.0, u32::MAX).unwrap().entity, near);
        assert!(w.raycast(Vec3::ZERO, Vec3::Z, 3.0, u32::MAX).is_none());
        assert!(w.raycast(Vec3::ZERO, Vec3::X, 100.0, u32::MAX).is_none());
        assert!(w.raycast(Vec3::ZERO, Vec3::ZERO, 100.0, u32::MAX).is_none());
        // Unnormalised dir is fine.
        assert!((w.raycast(Vec3::ZERO, Vec3::Z * 7.0, 100.0, u32::MAX).unwrap().distance - 4.0).abs() < 1e-4);
    }

    #[test]
    fn raycast_layers_and_triggers() {
        let mut w = World::new();
        let wall = w.spawn((
            Transform::from_position(Vec3::new(0.0, 0.0, 5.0)),
            Collider { shape: ColliderShape::Sphere { radius: 1.0 }, is_trigger: false },
            CollisionLayers::new(LayerMask::bit(3), LayerMask::ALL),
        ));
        let _trig = w.spawn((
            Transform::from_position(Vec3::new(0.0, 0.0, 2.0)),
            Collider { shape: ColliderShape::Sphere { radius: 1.0 }, is_trigger: true },
        ));
        // Triggers ignored by default; layer 3 only visible with matching mask.
        assert!(w.raycast(Vec3::ZERO, Vec3::Z, 50.0, LayerMask::bit(0).0).is_none());
        assert_eq!(w.raycast(Vec3::ZERO, Vec3::Z, 50.0, LayerMask::bit(3).0).unwrap().entity, wall);
        let h = w.raycast_filtered(Vec3::ZERO, Vec3::Z, 50.0, &QueryFilter::default().with_triggers()).unwrap();
        assert_ne!(h.entity, wall);
        // Exclusion.
        assert!(w.raycast_filtered(Vec3::ZERO, Vec3::Z, 50.0, &QueryFilter::default().excluding(wall)).is_none());
    }

    #[test]
    fn raycast_from_inside_reports_started_inside() {
        let mut w = World::new();
        spawn(&mut w, Vec3::ZERO, ColliderShape::Aabb { half_extents: Vec3::splat(2.0) });
        let h = w.raycast(Vec3::ZERO, Vec3::X, 10.0, u32::MAX).unwrap();
        assert!(h.started_inside && h.distance == 0.0);
    }

    #[test]
    fn spherecast_stops_at_surface() {
        let mut w = World::new();
        let wall = spawn(&mut w, Vec3::new(0.0, 0.0, 10.0), ColliderShape::Aabb { half_extents: Vec3::new(5.0, 5.0, 1.0) });
        // Wall face at z=9; sphere r=0.5 centre stops at z=8.5.
        let h = w.spherecast(Vec3::ZERO, 0.5, Vec3::Z, 50.0, u32::MAX).unwrap();
        assert_eq!(h.entity, wall);
        assert!((h.distance - 8.5).abs() < 0.01, "{}", h.distance);
        assert!((h.normal - -Vec3::Z).length() < 1e-3);
        assert!(!h.started_inside);
        // A ray through the same line is not blocked by a small offset obstacle but the sphere is.
        let mut w2 = World::new();
        spawn(&mut w2, Vec3::new(0.7, 0.0, 5.0), ColliderShape::Sphere { radius: 0.3 });
        assert!(w2.raycast(Vec3::ZERO, Vec3::Z, 20.0, u32::MAX).is_none());
        assert!(w2.spherecast(Vec3::ZERO, 0.5, Vec3::Z, 20.0, u32::MAX).is_some());
        assert!(w2.spherecast(Vec3::ZERO, 0.3, Vec3::Z, 20.0, u32::MAX).is_none());
    }

    #[test]
    fn capsule_sweep_lands_on_floor_and_catches_thin_walls() {
        let mut w = World::new();
        let floor = spawn(&mut w, Vec3::new(0.0, -0.5, 0.0), ColliderShape::Aabb { half_extents: Vec3::new(20.0, 0.5, 20.0) });
        // Capsule (r .4, hh .5) bottom at centre-0.9; drop from y=5; lands with centre at 0.9.
        let h = w.capsule_sweep(Vec3::new(0.0, 5.0, 0.0), 0.4, 0.5, -Vec3::Y, 20.0, u32::MAX).unwrap();
        assert_eq!(h.entity, floor);
        assert!((h.distance - 4.1).abs() < 0.01, "{}", h.distance);
        assert!((h.normal - Vec3::Y).length() < 1e-3);
        // Thin wall (0.02 thick) must not be tunnelled by a fast long sweep.
        let mut w = World::new();
        spawn(&mut w, Vec3::new(30.0, 0.0, 0.0), ColliderShape::Aabb { half_extents: Vec3::new(0.01, 3.0, 3.0) });
        assert!(w.capsule_sweep(Vec3::ZERO, 0.4, 0.5, Vec3::X, 100.0, u32::MAX).is_some());
    }

    #[test]
    fn sweep_starting_inside_is_flagged() {
        let mut w = World::new();
        spawn(&mut w, Vec3::ZERO, ColliderShape::Aabb { half_extents: Vec3::splat(1.0) });
        let h = w.spherecast(Vec3::new(0.0, 1.2, 0.0), 0.5, Vec3::X, 5.0, u32::MAX).unwrap();
        assert!(h.started_inside && h.distance == 0.0);
    }

    #[test]
    fn overlap_queries() {
        let mut w = World::new();
        let a = spawn(&mut w, Vec3::new(1.0, 0.0, 0.0), ColliderShape::Sphere { radius: 0.5 });
        let b = spawn(&mut w, Vec3::new(0.0, 0.0, 1.5), ColliderShape::Aabb { half_extents: Vec3::splat(0.5) });
        let _far = spawn(&mut w, Vec3::new(20.0, 0.0, 0.0), ColliderShape::Sphere { radius: 0.5 });
        let mut got = w.overlap_sphere(Vec3::ZERO, 1.3, u32::MAX);
        got.sort_by_key(|e| e.to_bits());
        let mut want = vec![a, b];
        want.sort_by_key(|e| e.to_bits());
        assert_eq!(got, want);
        assert!(w.overlap_sphere(Vec3::new(0.0, 10.0, 0.0), 1.0, u32::MAX).is_empty());
        let caps = w.overlap_shape(
            ColliderShape::Capsule { radius: 0.3, half_height: 2.0 },
            Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY),
            &QueryFilter::default(),
        );
        assert_eq!(caps, vec![a]);
    }
}
