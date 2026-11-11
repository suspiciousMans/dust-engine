//! Ray casting, click selection (with cycling) and marquee selection.
//! Pure math over `World` + [`CameraMatrices`]; no GL.

use egui::{Modifiers, Pos2, Rect};
use glam::{Mat4, Vec3};
use hecs::{Entity, World};

use super::bounds::{local_bounds, Aabb, PickTriangles, GIZMO_ICON_RADIUS};
use super::cam_matrices::{CameraMatrices, Ray};
use super::hierarchy::{is_locked, is_visible};
use super::selection::Selection;
use super::world_xform::world_matrix;
use crate::ecs::Transform;

// ------------------------------------------------------------ primitives

/// Slab test. Returns the entry distance `t >= 0` (0 if the origin is inside).
pub fn ray_aabb(ray: &Ray, b: &Aabb) -> Option<f32> {
    let mut t0 = 0.0f32;
    let mut t1 = f32::INFINITY;
    for i in 0..3 {
        let (o, d) = (ray.origin[i], ray.dir[i]);
        if d.abs() < 1e-9 {
            if o < b.min[i] || o > b.max[i] {
                return None;
            }
        } else {
            let (mut a, mut c) = ((b.min[i] - o) / d, (b.max[i] - o) / d);
            if a > c {
                std::mem::swap(&mut a, &mut c);
            }
            t0 = t0.max(a);
            t1 = t1.min(c);
            if t0 > t1 {
                return None;
            }
        }
    }
    Some(t0)
}

/// Oriented box = local `b` under `m`. `t` is in world units even with scale,
/// because the ray is transformed with an un-normalised direction.
pub fn ray_obb(ray: &Ray, b: &Aabb, m: &Mat4) -> Option<f32> {
    let inv = m.inverse();
    if !inv.is_finite() {
        return None;
    }
    let local = Ray { origin: inv.transform_point3(ray.origin), dir: inv.transform_vector3(ray.dir) };
    ray_aabb(&local, b)
}

pub fn ray_sphere(ray: &Ray, center: Vec3, radius: f32) -> Option<f32> {
    let oc = ray.origin - center;
    let b = oc.dot(ray.dir);
    let c = oc.length_squared() - radius * radius;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t = -b - s;
    if t >= 0.0 {
        Some(t)
    } else if -b + s >= 0.0 {
        Some(0.0)
    } else {
        None
    }
}

/// Moller-Trumbore, double sided.
pub fn ray_triangle(ray: &Ray, tri: &[Vec3; 3]) -> Option<f32> {
    let e1 = tri[1] - tri[0];
    let e2 = tri[2] - tri[0];
    let p = ray.dir.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = ray.origin - tri[0];
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = ray.dir.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t >= 0.0).then_some(t)
}

// ------------------------------------------------------------ world pick

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PickHit {
    pub entity: Entity,
    pub t: f32,
    pub point: Vec3,
}

/// Can the viewport interact with this entity at all?
pub fn is_pickable(world: &World, e: Entity) -> bool {
    is_visible(world, e) && !is_locked(world, e)
}

/// Ray distance to one entity's pick shape, if hit.
pub fn ray_entity(world: &World, ray: &Ray, e: Entity) -> Option<f32> {
    if !world.satisfies::<&Transform>(e).unwrap_or(false) {
        return None;
    }
    let m = world_matrix(world, e);
    match local_bounds(world, e) {
        Some(b) => {
            // Exact triangles refine the box when the entity provides them.
            if let Ok(tris) = world.get::<&PickTriangles>(e) {
                ray_obb(ray, &b, &m)?;
                return tris
                    .0
                    .iter()
                    .filter_map(|t| ray_triangle(ray, &[m.transform_point3(t[0]), m.transform_point3(t[1]), m.transform_point3(t[2])]))
                    .min_by(|a, b| a.total_cmp(b));
            }
            ray_obb(ray, &b, &m)
        }
        None => ray_sphere(ray, m.w_axis.truncate(), GIZMO_ICON_RADIUS),
    }
}

/// All pickable entities under the ray, nearest first.
pub fn pick_all(world: &World, ray: &Ray) -> Vec<PickHit> {
    let mut hits: Vec<PickHit> = world
        .query::<&Transform>()
        .iter()
        .map(|(e, _)| e)
        .filter(|e| is_pickable(world, *e))
        .filter_map(|e| ray_entity(world, ray, e).map(|t| PickHit { entity: e, t, point: ray.at(t) }))
        .collect();
    hits.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.entity.id().cmp(&b.entity.id())));
    hits
}

pub fn pick_nearest(world: &World, ray: &Ray) -> Option<PickHit> {
    pick_all(world, ray).into_iter().next()
}

// ---------------------------------------------------------- click select

/// Click-selection state: remembers the previous click so repeating it at the
/// same spot walks through the overlapping entities (nearest, next, ...).
#[derive(Clone, Debug, Default)]
pub struct ClickSelector {
    last_pos: Option<Pos2>,
    last_hits: Vec<Entity>,
    cycle: usize,
}

/// Cursor movement (points) still counted as "the same place".
pub const CYCLE_TOLERANCE: f32 = 3.0;

impl ClickSelector {
    /// Applies a click. `hits` must be nearest-first (from [`pick_all`]).
    /// - plain: replace selection; repeat click cycles through `hits`
    /// - shift: add the hit (cycling too)
    /// - ctrl/cmd: toggle the hit
    /// - plain click on nothing clears; modified click on nothing keeps.
    /// Returns the entity acted on.
    pub fn click(&mut self, sel: &mut Selection, hits: &[Entity], pos: Pos2, mods: Modifiers) -> Option<Entity> {
        if hits.is_empty() {
            self.last_pos = None;
            self.last_hits.clear();
            if !(mods.shift || mods.ctrl || mods.command) {
                sel.clear();
            }
            return None;
        }
        let same = self.last_pos.map(|p| p.distance(pos) <= CYCLE_TOLERANCE).unwrap_or(false) && self.last_hits == hits;
        self.cycle = if same { (self.cycle + 1) % hits.len() } else { 0 };
        self.last_pos = Some(pos);
        self.last_hits = hits.to_vec();
        let e = hits[self.cycle];
        if mods.ctrl || mods.command {
            sel.toggle(e);
        } else if mods.shift {
            sel.add(e);
        } else {
            sel.select_only(e);
        }
        Some(e)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ----------------------------------------------------------- box select

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BoxMode {
    /// Anything whose projected bounds touch the marquee.
    #[default]
    Touch,
    /// Only entities whose projected bounds are fully inside.
    Contain,
}

/// Screen-space bounds of an entity's pick box. `None` if entirely behind the
/// camera. Corners behind the near plane are clipped against it first, so a
/// huge floor plane crossing the camera still reports sane bounds.
pub fn projected_bounds(cam: &CameraMatrices, local: &Aabb, m: &Mat4) -> Option<Rect> {
    let vp = cam.view_proj();
    const NEAR_W: f32 = 1e-3;
    let clip: Vec<glam::Vec4> = local.corners().iter().map(|c| vp * m.transform_point3(*c).extend(1.0)).collect();
    // The 12 box edges, as corner index pairs.
    const EDGES: [(usize, usize); 12] = [(0, 1), (2, 3), (4, 5), (6, 7), (0, 2), (1, 3), (4, 6), (5, 7), (0, 4), (1, 5), (2, 6), (3, 7)];
    let mut pts: Vec<glam::Vec4> = clip.iter().copied().filter(|c| c.w > NEAR_W).collect();
    for (a, b) in EDGES {
        let (ca, cb) = (clip[a], clip[b]);
        if (ca.w > NEAR_W) != (cb.w > NEAR_W) {
            let t = (NEAR_W - ca.w) / (cb.w - ca.w);
            pts.push(ca + (cb - ca) * t);
        }
    }
    let screen = |c: glam::Vec4| {
        let n = c.truncate() / c.w;
        Pos2::new(
            cam.rect.left() + (n.x * 0.5 + 0.5) * cam.rect.width(),
            cam.rect.top() + (0.5 - n.y * 0.5) * cam.rect.height(),
        )
    };
    let mut it = pts.into_iter().map(screen);
    let first = it.next()?;
    Some(it.fold(Rect::from_min_max(first, first), |r, p| r.union(Rect::from_min_max(p, p))))
}

/// Entities selected by dragging `marquee` (screen rect).
pub fn box_select(world: &World, cam: &CameraMatrices, marquee: Rect, mode: BoxMode) -> Vec<Entity> {
    let mut out = Vec::new();
    for (e, _) in world.query::<&Transform>().iter() {
        if !is_pickable(world, e) {
            continue;
        }
        let m = world_matrix(world, e);
        let b = local_bounds(world, e).unwrap_or_else(|| Aabb::from_center_half(Vec3::ZERO, Vec3::splat(GIZMO_ICON_RADIUS)));
        let Some(r) = projected_bounds(cam, &b, &m) else { continue };
        let ok = match mode {
            BoxMode::Touch => marquee.intersects(r),
            BoxMode::Contain => marquee.contains_rect(r),
        };
        if ok {
            out.push(e);
        }
    }
    out.sort_by_key(|e| e.id());
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxOp {
    Replace,
    Add,
    Subtract,
}

impl BoxOp {
    /// shift = add, ctrl = subtract (Blender-style), none = replace.
    pub fn from_modifiers(m: Modifiers) -> Self {
        if m.ctrl || m.command {
            Self::Subtract
        } else if m.shift {
            Self::Add
        } else {
            Self::Replace
        }
    }
}

pub fn apply_box_selection(sel: &mut Selection, found: &[Entity], op: BoxOp) {
    match op {
        BoxOp::Replace => {
            sel.clear();
            for e in found {
                sel.add(*e);
            }
        }
        BoxOp::Add => found.iter().for_each(|e| sel.add(*e)),
        BoxOp::Subtract => found.iter().for_each(|e| sel.remove(*e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::cam_matrices::test_util::*;
    use crate::editor::hierarchy::{reparent, Locked, Primitive, PrimitiveShape, Visible};
    use egui::vec2;
    use glam::Quat;

    fn ray_z(x: f32, y: f32) -> Ray {
        Ray::new(Vec3::new(x, y, 10.0), Vec3::NEG_Z)
    }
    fn cube_at(w: &mut World, p: Vec3) -> Entity {
        w.spawn((Transform::from_position(p), Primitive(PrimitiveShape::Cube)))
    }

    #[test]
    fn aabb_hit_miss_inside() {
        let b = Aabb::from_center_half(Vec3::ZERO, Vec3::splat(1.0));
        assert!((ray_aabb(&ray_z(0.0, 0.0), &b).unwrap() - 9.0).abs() < 1e-5);
        assert!(ray_aabb(&ray_z(1.5, 0.0), &b).is_none());
        assert_eq!(ray_aabb(&Ray::new(Vec3::ZERO, Vec3::X), &b), Some(0.0));
        // behind the origin
        assert!(ray_aabb(&Ray::new(Vec3::new(0.0, 0.0, -5.0), Vec3::NEG_Z), &b).is_none());
        // axis-parallel ray outside the slab
        assert!(ray_aabb(&Ray::new(Vec3::new(0.0, 2.0, 5.0), Vec3::NEG_Z), &b).is_none());
    }

    #[test]
    fn obb_respects_rotation_and_scale() {
        let b = Aabb::from_center_half(Vec3::ZERO, Vec3::splat(0.5));
        // 4 long in X, rotated 90deg about Y => long along Z
        let m = Mat4::from_scale_rotation_translation(Vec3::new(4.0, 1.0, 1.0), Quat::from_rotation_y(90f32.to_radians()), Vec3::ZERO);
        let r = Ray::new(Vec3::new(0.0, 0.0, 10.0), Vec3::NEG_Z);
        assert!((ray_obb(&r, &b, &m).unwrap() - 8.0).abs() < 1e-4); // hits face at z=2
        let off = Ray::new(Vec3::new(1.5, 0.0, 10.0), Vec3::NEG_Z);
        assert!(ray_obb(&off, &b, &m).is_none());
        let side = Ray::new(Vec3::new(0.0, 0.0, 1.9), Vec3::NEG_Z);
        assert!(ray_obb(&side, &b, &m).is_some());
        assert!(ray_obb(&r, &b, &Mat4::ZERO).is_none());
    }

    #[test]
    fn sphere_cases() {
        assert!((ray_sphere(&ray_z(0.0, 0.0), Vec3::ZERO, 1.0).unwrap() - 9.0).abs() < 1e-5);
        assert!(ray_sphere(&ray_z(1.1, 0.0), Vec3::ZERO, 1.0).is_none());
        assert_eq!(ray_sphere(&Ray::new(Vec3::ZERO, Vec3::X), Vec3::ZERO, 1.0), Some(0.0));
        assert!(ray_sphere(&Ray::new(Vec3::new(0.0, 0.0, 5.0), Vec3::Z), Vec3::ZERO, 1.0).is_none());
        // tangent-ish
        assert!(ray_sphere(&ray_z(0.999, 0.0), Vec3::ZERO, 1.0).is_some());
    }

    #[test]
    fn triangle_moller_trumbore() {
        let tri = [Vec3::new(-1.0, -1.0, 0.0), Vec3::new(1.0, -1.0, 0.0), Vec3::new(0.0, 1.0, 0.0)];
        assert!((ray_triangle(&ray_z(0.0, 0.0), &tri).unwrap() - 10.0).abs() < 1e-5);
        assert!(ray_triangle(&ray_z(0.9, 0.9), &tri).is_none());
        assert!(ray_triangle(&ray_z(-0.99, -0.99), &tri).is_some()); // near vertex
        // back face still hit
        assert!(ray_triangle(&Ray::new(Vec3::new(0.0, 0.0, -3.0), Vec3::Z), &tri).is_some());
        // parallel
        assert!(ray_triangle(&Ray::new(Vec3::new(0.0, 0.0, 1.0), Vec3::X), &tri).is_none());
        // behind
        assert!(ray_triangle(&Ray::new(Vec3::new(0.0, 0.0, 10.0), Vec3::Z), &tri).is_none());
    }

    #[test]
    fn picks_nearest_and_sorted() {
        let mut w = World::new();
        let far = cube_at(&mut w, Vec3::new(0.0, 0.0, -3.0));
        let near = cube_at(&mut w, Vec3::new(0.0, 0.0, 3.0));
        let _off = cube_at(&mut w, Vec3::new(5.0, 0.0, 0.0));
        let hits = pick_all(&w, &ray_z(0.0, 0.0));
        assert_eq!(hits.iter().map(|h| h.entity).collect::<Vec<_>>(), vec![near, far]);
        assert!((hits[0].t - 6.5).abs() < 1e-4);
        assert!((hits[0].point.z - 3.5).abs() < 1e-4);
        assert_eq!(pick_nearest(&w, &ray_z(0.0, 0.0)).unwrap().entity, near);
        assert!(pick_nearest(&w, &ray_z(20.0, 0.0)).is_none());
    }

    #[test]
    fn hidden_and_locked_are_skipped_including_via_ancestors() {
        let mut w = World::new();
        let hidden = cube_at(&mut w, Vec3::new(0.0, 0.0, 4.0));
        w.insert_one(hidden, Visible(false)).unwrap();
        let locked = cube_at(&mut w, Vec3::new(0.0, 0.0, 2.0));
        w.insert_one(locked, Locked(true)).unwrap();
        let ok = cube_at(&mut w, Vec3::ZERO);
        assert_eq!(pick_all(&w, &ray_z(0.0, 0.0)).len(), 1);
        assert_eq!(pick_nearest(&w, &ray_z(0.0, 0.0)).unwrap().entity, ok);
        // child of a hidden parent is hidden too
        let parent = w.spawn((Transform::default(), Visible(false)));
        reparent(&mut w, ok, Some(parent), None).unwrap();
        assert!(pick_nearest(&w, &ray_z(0.0, 0.0)).is_none());
    }

    #[test]
    fn child_picks_at_its_world_position() {
        let mut w = World::new();
        let p = w.spawn((Transform::from_position(Vec3::new(10.0, 0.0, 0.0)),));
        let c = cube_at(&mut w, Vec3::ZERO);
        reparent(&mut w, c, Some(p), None).unwrap();
        assert!(pick_nearest(&w, &ray_z(0.0, 0.0)).is_none());
        // the parent is an empty: picked via its icon sphere
        let hit = pick_nearest(&w, &ray_z(10.0, 0.0)).unwrap();
        assert!(hit.entity == c || hit.entity == p);
    }

    #[test]
    fn empty_entities_use_icon_sphere() {
        let mut w = World::new();
        let e = w.spawn((Transform::default(),));
        assert_eq!(pick_nearest(&w, &ray_z(0.2, 0.0)).unwrap().entity, e);
        assert!(pick_nearest(&w, &ray_z(0.5, 0.0)).is_none());
    }

    #[test]
    fn pick_triangles_refine_box() {
        let mut w = World::new();
        // Box is 1x1x1 but the triangle only covers the left half.
        let tri = [Vec3::new(-0.5, -0.5, 0.0), Vec3::new(0.0, -0.5, 0.0), Vec3::new(-0.5, 0.5, 0.0)];
        let e = w.spawn((Transform::default(), Primitive(PrimitiveShape::Cube), PickTriangles(std::sync::Arc::new(vec![tri]))));
        assert_eq!(pick_nearest(&w, &ray_z(-0.4, -0.3)).unwrap().entity, e);
        assert!(pick_nearest(&w, &ray_z(0.4, 0.3)).is_none());
    }

    #[test]
    fn click_semantics() {
        let mut w = World::new();
        let (a, b) = (cube_at(&mut w, Vec3::ZERO), cube_at(&mut w, Vec3::X));
        let mut sel = Selection::default();
        let mut cs = ClickSelector::default();
        let p = Pos2::new(10.0, 10.0);
        cs.click(&mut sel, &[a], p, Modifiers::NONE);
        assert_eq!(sel.to_vec(), vec![a]);
        cs.click(&mut sel, &[b], Pos2::new(50.0, 50.0), Modifiers::SHIFT);
        assert_eq!(sel.to_vec(), vec![a, b]);
        assert_eq!(sel.primary(), Some(b));
        cs.click(&mut sel, &[a], Pos2::new(90.0, 90.0), Modifiers::CTRL);
        assert_eq!(sel.to_vec(), vec![b]); // toggled off
        cs.click(&mut sel, &[a], Pos2::new(90.0, 90.0), Modifiers::CTRL);
        assert!(sel.contains(a));
        // empty space: modified click keeps, plain click clears
        cs.click(&mut sel, &[], p, Modifiers::SHIFT);
        assert_eq!(sel.len(), 2);
        cs.click(&mut sel, &[], p, Modifiers::NONE);
        assert!(sel.is_empty());
    }

    #[test]
    fn repeated_click_cycles_through_overlaps() {
        let mut w = World::new();
        let (a, b, c) = (cube_at(&mut w, Vec3::ZERO), cube_at(&mut w, Vec3::ZERO), cube_at(&mut w, Vec3::ZERO));
        let hits = [a, b, c];
        let mut sel = Selection::default();
        let mut cs = ClickSelector::default();
        let mut seen = Vec::new();
        for i in 0..4 {
            // 1px jitter still counts as the same spot
            cs.click(&mut sel, &hits, Pos2::new(100.0 + i as f32 * 0.5, 100.0), Modifiers::NONE);
            seen.push(sel.primary().unwrap());
        }
        assert_eq!(seen, vec![a, b, c, a]);
        // moving away resets to nearest
        cs.click(&mut sel, &hits, Pos2::new(200.0, 100.0), Modifiers::NONE);
        assert_eq!(sel.primary(), Some(a));
        // a different hit stack at the same spot also resets
        cs.click(&mut sel, &[b, a], Pos2::new(200.0, 100.0), Modifiers::NONE);
        assert_eq!(sel.primary(), Some(b));
    }

    #[test]
    fn box_select_touch_vs_contain() {
        let mut w = World::new();
        let cam = cam_front();
        let centre = cube_at(&mut w, Vec3::ZERO);
        let right = cube_at(&mut w, Vec3::new(3.0, 0.0, 0.0));
        let c = cam.project(Vec3::ZERO).unwrap();
        let small = Rect::from_center_size(c, vec2(10.0, 10.0));
        assert_eq!(box_select(&w, &cam, small, BoxMode::Touch), vec![centre]);
        assert!(box_select(&w, &cam, small, BoxMode::Contain).is_empty());
        let big = Rect::from_center_size(c, vec2(150.0, 150.0));
        assert_eq!(box_select(&w, &cam, big, BoxMode::Contain), vec![centre]);
        let all = Rect::from_min_max(cam.rect.min, cam.rect.max);
        assert_eq!(box_select(&w, &cam, all, BoxMode::Contain).len(), 2);
        let _ = right;
    }

    #[test]
    fn box_select_skips_locked_and_behind_camera() {
        let mut w = World::new();
        let cam = cam_front();
        let locked = cube_at(&mut w, Vec3::ZERO);
        w.insert_one(locked, Locked(true)).unwrap();
        let _behind = cube_at(&mut w, Vec3::new(0.0, 0.0, 30.0));
        assert!(box_select(&w, &cam, cam.rect, BoxMode::Touch).is_empty());
    }

    #[test]
    fn box_select_handles_box_crossing_near_plane() {
        let mut w = World::new();
        let cam = cam_front(); // eye z=10
        let floor = w.spawn((Transform { scale: Vec3::new(100.0, 1.0, 100.0), ..Default::default() }, Primitive(PrimitiveShape::Plane)));
        let found = box_select(&w, &cam, Rect::from_center_size(cam.rect.center(), vec2(4.0, 4.0)), BoxMode::Touch);
        assert_eq!(found, vec![floor]);
    }

    #[test]
    fn box_ops() {
        let mut w = World::new();
        let (a, b, c) = (cube_at(&mut w, Vec3::ZERO), cube_at(&mut w, Vec3::ZERO), cube_at(&mut w, Vec3::ZERO));
        let mut sel = Selection::default();
        sel.select_only(a);
        apply_box_selection(&mut sel, &[b, c], BoxOp::Add);
        assert_eq!(sel.len(), 3);
        apply_box_selection(&mut sel, &[a], BoxOp::Subtract);
        assert_eq!(sel.to_vec(), vec![b, c]);
        apply_box_selection(&mut sel, &[a], BoxOp::Replace);
        assert_eq!(sel.to_vec(), vec![a]);
        assert_eq!(BoxOp::from_modifiers(Modifiers::SHIFT), BoxOp::Add);
        assert_eq!(BoxOp::from_modifiers(Modifiers::CTRL), BoxOp::Subtract);
        assert_eq!(BoxOp::from_modifiers(Modifiers::NONE), BoxOp::Replace);
    }

    #[test]
    fn screen_click_through_camera_ray() {
        let mut w = World::new();
        let cam = cam_at(Vec3::new(4.0, 3.0, 8.0));
        let e = cube_at(&mut w, Vec3::new(0.5, 0.2, -1.0));
        let _other = cube_at(&mut w, Vec3::new(-6.0, 0.0, 0.0));
        let s = cam.project(Vec3::new(0.5, 0.2, -1.0)).unwrap();
        assert_eq!(pick_nearest(&w, &cam.ray(s)).unwrap().entity, e);
    }
}
