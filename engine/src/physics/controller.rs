//! Kinematic capsule character controller: move-and-slide, step offset,
//! slope limit and grounded detection, built on the sweep queries.
//!
//! The controller is a component; call [`CharacterController::move_and_slide`]
//! once per (fixed) step with the desired displacement. It moves the entity's
//! `Transform` directly. It does not need (and should not have) a `RigidBody`.

use glam::{Quat, Vec3};
use hecs::{Entity, World};

use super::layers::{CollisionLayers, LayerMask};
use super::narrow::Pose;
use super::query::{Hit, PhysicsQueries, QueryFilter};
use super::ColliderShape;
use crate::ecs::Transform;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharacterController {
    pub radius: f32,
    /// Half length of the straight section (upright capsule).
    pub half_height: f32,
    /// Highest ledge it will walk up.
    pub step_height: f32,
    /// Steepest walkable slope in degrees.
    pub max_slope_deg: f32,
    /// Gap kept between capsule and surfaces.
    pub skin: f32,
    pub grounded: bool,
}

impl Default for CharacterController {
    fn default() -> Self {
        Self { radius: 0.35, half_height: 0.5, step_height: 0.35, max_slope_deg: 45.0, skin: 0.02, grounded: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoveResult {
    pub position: Vec3,
    pub grounded: bool,
    /// Normal of the ground under the capsule (`Vec3::Y` if airborne).
    pub ground_normal: Vec3,
    /// Hit a wall / too-steep slope / ceiling during this move.
    pub blocked: bool,
    pub stepped: bool,
    /// Last thing collided with.
    pub last_hit: Option<Entity>,
}

impl CharacterController {
    fn shape(&self) -> ColliderShape {
        ColliderShape::Capsule { radius: self.radius, half_height: self.half_height }
    }
    fn min_ground_y(&self) -> f32 {
        self.max_slope_deg.to_radians().cos()
    }

    fn cast(&self, world: &World, pos: Vec3, dir: Vec3, dist: f32, filter: &QueryFilter) -> Option<Hit> {
        if dist <= 1e-6 {
            return None;
        }
        world.shape_cast(self.shape(), Pose::new(pos, Quat::IDENTITY), dir, dist, filter)
    }

    /// Slides `motion` along surfaces. Returns (final pos, blocked, last hit).
    fn slide(&self, world: &World, mut pos: Vec3, mut motion: Vec3, filter: &QueryFilter, climb: bool) -> (Vec3, bool, Option<Entity>) {
        let mut blocked = false;
        let mut last = None;
        let mut planes: Vec<Vec3> = Vec::new();
        for _ in 0..4 {
            let dist = motion.length();
            if dist < 1e-5 {
                break;
            }
            let dir = motion / dist;
            match self.cast(world, pos, dir, dist + self.skin, filter) {
                None => {
                    pos += motion;
                    break;
                }
                Some(h) => {
                    last = Some(h.entity);
                    let travel = (h.distance - self.skin).max(0.0).min(dist);
                    pos += dir * travel;
                    let remaining = motion - dir * travel;
                    let mut n = h.normal;
                    let walkable = n.y >= self.min_ground_y();
                    if !walkable {
                        blocked = true;
                        // Never be pushed up a steep slope.
                        if !climb && n.y > 0.0 {
                            n = Vec3::new(n.x, 0.0, n.z).try_normalize().unwrap_or(n);
                        }
                    }
                    if planes.iter().any(|p| p.dot(n) > 0.999) {
                        break; // wedged against the same plane
                    }
                    planes.push(n);
                    let mut next = remaining - n * remaining.dot(n);
                    // Crease between two planes: slide along their intersection.
                    if planes.len() >= 2 {
                        let m = planes.len();
                        let crease = planes[m - 2].cross(planes[m - 1]);
                        if crease.length_squared() > 1e-6 && next.dot(planes[m - 2]) < -1e-4 {
                            let c = crease.normalize();
                            next = c * remaining.dot(c);
                        }
                    }
                    motion = next;
                }
            }
        }
        (pos, blocked, last)
    }

    fn probe_ground(&self, world: &World, pos: Vec3, dist: f32, filter: &QueryFilter) -> Option<Hit> {
        self.cast(world, pos, Vec3::NEG_Y, dist, filter).filter(|h| h.normal.y >= self.min_ground_y())
    }

    pub fn move_and_slide(&mut self, world: &mut World, entity: Entity, motion: Vec3) -> Option<MoveResult> {
        let start = world.get::<&Transform>(entity).ok()?.position;
        let mask = world.get::<&CollisionLayers>(entity).map(|l| l.mask).unwrap_or(LayerMask::ALL);
        let filter = QueryFilter { mask, exclude: Some(entity), include_triggers: false };
        let r = self.move_at(world, start, motion, &filter);
        world.get::<&mut Transform>(entity).ok()?.position = r.position;
        self.grounded = r.grounded;
        Some(r)
    }

    /// Pure version: computes the move from `start` without touching the world.
    pub fn move_at(&self, world: &World, start: Vec3, motion: Vec3, filter: &QueryFilter) -> MoveResult {
        let was_grounded = self.grounded;
        let horizontal = Vec3::new(motion.x, 0.0, motion.z);
        let vertical = Vec3::new(0.0, motion.y, 0.0);
        let mut pos = start;
        let mut blocked = false;
        let mut stepped = false;
        let mut last_hit = None;

        // Horizontal first (with step-up), then vertical (gravity / jump).
        if horizontal.length_squared() > 1e-10 {
            let (p, b, h) = self.slide(world, pos, horizontal, filter, true);
            let mut best = p;
            if b && (was_grounded || self.probe_ground(world, pos, self.step_height + self.skin * 2.0, filter).is_some()) && self.step_height > 0.0 {
                // Try stepping: up, over, down.
                let up_dist = self.step_height;
                let up = match self.cast(world, pos, Vec3::Y, up_dist + self.skin, filter) {
                    Some(h) => (h.distance - self.skin).max(0.0),
                    None => up_dist,
                };
                let raised = pos + Vec3::Y * up;
                let (over, _, _) = self.slide(world, raised, horizontal, filter, false);
                let moved_h = Vec3::new(over.x - raised.x, 0.0, over.z - raised.z);
                let plain_h = Vec3::new(p.x - pos.x, 0.0, p.z - pos.z);
                if moved_h.length() > plain_h.length() + 1e-3 {
                    if let Some(g) = self.probe_ground(world, over, up + self.skin * 2.0, filter) {
                        let down = (g.distance - self.skin).max(0.0);
                        let landed = over - Vec3::Y * down;
                        // Must actually be a step, not a cliff we climbed.
                        if landed.y - pos.y <= self.step_height + 1e-3 {
                            best = landed;
                            stepped = landed.y - pos.y > 0.02;
                        }
                    }
                }
            }
            blocked |= b && !stepped;
            last_hit = h;
            pos = best;
        }

        let jumping = motion.y > 1e-4;
        if vertical.y.abs() > 1e-6 {
            let (p, b, h) = self.slide(world, pos, vertical, filter, true);
            pos = p;
            if b || h.is_some() && vertical.y > 0.0 {
                blocked |= vertical.y > 0.0 && h.is_some();
                last_hit = h.or(last_hit);
            }
        }

        // Stick to ground when walking down slopes / small drops.
        let mut ground = self.probe_ground(world, pos, self.skin * 2.0 + 0.03, filter);
        if ground.is_none() && was_grounded && !jumping && self.step_height > 0.0 {
            if let Some(g) = self.probe_ground(world, pos, self.step_height + self.skin, filter) {
                pos -= Vec3::Y * (g.distance - self.skin).max(0.0);
                ground = self.probe_ground(world, pos, self.skin * 2.0 + 0.03, filter);
            }
        }
        if jumping {
            ground = None;
        }
        MoveResult {
            position: pos,
            grounded: ground.is_some(),
            ground_normal: ground.map_or(Vec3::Y, |g| g.normal),
            blocked,
            stepped,
            last_hit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::Collider;

    fn floor(w: &mut World) {
        w.spawn((Transform::from_position(Vec3::new(0.0, -0.5, 0.0)), Collider::aabb(Vec3::new(50.0, 0.5, 50.0))));
    }
    fn player(w: &mut World, y: f32) -> (Entity, CharacterController) {
        let cc = CharacterController::default();
        let e = w.spawn((Transform::from_position(Vec3::new(0.0, y, 0.0)), cc));
        (e, cc)
    }
    // capsule center resting on y=0 floor: half_height + radius + skin
    const REST: f32 = 0.5 + 0.35 + 0.02;

    #[test]
    fn falls_and_grounds() {
        let mut w = World::new();
        floor(&mut w);
        let (e, mut cc) = player(&mut w, 2.0);
        let mut grounded = false;
        for _ in 0..60 {
            let r = cc.move_and_slide(&mut w, e, Vec3::new(0.0, -0.2, 0.0)).unwrap();
            grounded = r.grounded;
        }
        let y = w.get::<&Transform>(e).unwrap().position.y;
        assert!(grounded, "grounded");
        assert!((y - REST).abs() < 0.03, "y={y}");
    }

    #[test]
    fn slides_along_wall() {
        let mut w = World::new();
        floor(&mut w);
        w.spawn((Transform::from_position(Vec3::new(2.0, 1.0, 0.0)), Collider::aabb(Vec3::new(0.5, 2.0, 5.0))));
        let (e, mut cc) = player(&mut w, REST);
        let mut r = None;
        for _ in 0..30 {
            r = cc.move_and_slide(&mut w, e, Vec3::new(0.1, 0.0, 0.05));
        }
        let p = w.get::<&Transform>(e).unwrap().position;
        assert!(p.x < 1.5 - 0.35 + 0.01, "stopped at wall: {p:?}");
        assert!(p.z > 1.0, "slid along z: {p:?}");
        assert!(r.unwrap().blocked);
    }

    #[test]
    fn steps_up_small_ledge_but_not_big_one() {
        let mut w = World::new();
        floor(&mut w);
        // 0.25 high ledge at x=2 (top y=0.25), 1.0 high block at x=-2
        w.spawn((Transform::from_position(Vec3::new(2.0, 0.125, 0.0)), Collider::aabb(Vec3::new(0.5, 0.125, 5.0))));
        w.spawn((Transform::from_position(Vec3::new(-2.0, 0.5, 0.0)), Collider::aabb(Vec3::new(0.5, 0.5, 5.0))));
        let (e, mut cc) = player(&mut w, REST);
        cc.grounded = true;
        for _ in 0..40 {
            cc.move_and_slide(&mut w, e, Vec3::new(0.1, -0.05, 0.0));
        }
        let p = w.get::<&Transform>(e).unwrap().position;
        assert!(p.x > 2.0 && (p.y - (REST + 0.25)).abs() < 0.04, "stepped: {p:?}");

        let (e2, mut cc2) = player(&mut w, REST);
        w.get::<&mut Transform>(e2).unwrap().position.z = 3.0;
        cc2.grounded = true;
        for _ in 0..40 {
            cc2.move_and_slide(&mut w, e2, Vec3::new(-0.1, -0.05, 0.0));
        }
        let p2 = w.get::<&Transform>(e2).unwrap().position;
        assert!(p2.x > -1.5 + 0.3 && p2.y < REST + 0.05, "blocked by tall: {p2:?}");
    }

    fn ramp(w: &mut World, deg: f32) {
        // Big box rotated about Z so its top surface is a ramp rising toward +x.
        let rot = Quat::from_rotation_z(deg.to_radians());
        w.spawn((
            Transform { position: Vec3::new(6.0, -2.0, 0.0), rotation: rot, scale: Vec3::ONE },
            Collider::oriented_box(Vec3::new(8.0, 2.0, 5.0)),
        ));
    }

    #[test]
    fn walks_up_gentle_slope_not_steep() {
        let mut w = World::new();
        floor(&mut w);
        ramp(&mut w, 25.0);
        let (e, mut cc) = player(&mut w, 3.0);
        for _ in 0..30 {
            cc.move_and_slide(&mut w, e, Vec3::new(0.0, -0.2, 0.0));
        }
        let mut last = None;
        for _ in 0..40 {
            last = cc.move_and_slide(&mut w, e, Vec3::new(0.08, -0.05, 0.0));
        }
        let p = w.get::<&Transform>(e).unwrap().position;
        assert!(last.unwrap().grounded);
        assert!(p.y > 1.0, "climbed gentle slope: {p:?}");

        let mut w = World::new();
        floor(&mut w);
        ramp(&mut w, 65.0);
        let (e, mut cc) = player(&mut w, REST);
        w.get::<&mut Transform>(e).unwrap().position.x = -3.0;
        cc.grounded = true;
        for _ in 0..80 {
            cc.move_and_slide(&mut w, e, Vec3::new(0.08, -0.05, 0.0));
        }
        let p = w.get::<&Transform>(e).unwrap().position;
        assert!(p.y < 1.0, "did not climb 65 deg slope: {p:?}");
    }

    #[test]
    fn ceiling_blocks_jump() {
        let mut w = World::new();
        floor(&mut w);
        w.spawn((Transform::from_position(Vec3::new(0.0, 3.0, 0.0)), Collider::aabb(Vec3::new(5.0, 0.5, 5.0))));
        let (e, mut cc) = player(&mut w, REST);
        cc.grounded = true;
        for _ in 0..20 {
            cc.move_and_slide(&mut w, e, Vec3::new(0.0, 0.3, 0.0));
        }
        let y = w.get::<&Transform>(e).unwrap().position.y;
        assert!(y + 0.85 <= 2.5 + 1e-3, "head below ceiling: {y}");
    }
}
