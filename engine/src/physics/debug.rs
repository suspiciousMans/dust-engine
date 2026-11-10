//! Wireframe list for debug drawing. Pure data: the renderer (or an egui
//! painter) draws the returned segments however it likes.

use glam::{Quat, Vec3};
use hecs::World;

use super::narrow::Pose;
use super::{Collider, ColliderShape, RigidBody};
use crate::ecs::Transform;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugColor {
    Static,
    Dynamic,
    Sleeping,
    Trigger,
    Contact,
}

impl DebugColor {
    pub fn rgb(self) -> [f32; 3] {
        match self {
            DebugColor::Static => [0.3, 0.6, 1.0],
            DebugColor::Dynamic => [0.2, 1.0, 0.3],
            DebugColor::Sleeping => [0.5, 0.5, 0.5],
            DebugColor::Trigger => [1.0, 0.8, 0.1],
            DebugColor::Contact => [1.0, 0.2, 0.2],
        }
    }
}

/// One world-space line segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DebugShape {
    pub a: Vec3,
    pub b: Vec3,
    pub color: DebugColor,
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicsDebugOptions {
    pub circle_segments: usize,
}

impl Default for PhysicsDebugOptions {
    fn default() -> Self {
        Self { circle_segments: 16 }
    }
}

#[derive(Default, Clone, Debug)]
pub struct PhysicsDebug {
    pub lines: Vec<DebugShape>,
}

impl PhysicsDebug {
    /// Wireframes for every collider. `is_sleeping` colours sleeping bodies
    /// differently (pass `|_| false` if unused).
    pub fn build(world: &World, opts: &PhysicsDebugOptions, is_sleeping: impl Fn(hecs::Entity) -> bool) -> Self {
        let mut items: Vec<_> = world
            .query::<(&Transform, &Collider, Option<&RigidBody>)>()
            .iter()
            .map(|(e, (t, c, rb))| (e, Pose::from(t), *c, rb.is_some()))
            .collect();
        items.sort_by_key(|i| i.0);
        let mut d = PhysicsDebug::default();
        for (e, pose, c, dynamic) in items {
            let color = if c.is_trigger {
                DebugColor::Trigger
            } else if !dynamic {
                DebugColor::Static
            } else if is_sleeping(e) {
                DebugColor::Sleeping
            } else {
                DebugColor::Dynamic
            };
            d.add_shape(c.shape, pose, color, opts.circle_segments.max(4));
        }
        d
    }

    pub fn add_contact(&mut self, point: Vec3, normal: Vec3) {
        self.line(point, point + normal * 0.3, DebugColor::Contact);
    }

    fn line(&mut self, a: Vec3, b: Vec3, color: DebugColor) {
        self.lines.push(DebugShape { a, b, color });
    }

    fn circle(&mut self, center: Vec3, rot: Quat, u: Vec3, v: Vec3, r: f32, n: usize, color: DebugColor, arc: Option<(f32, f32)>) {
        let (s, e) = arc.unwrap_or((0.0, std::f32::consts::TAU));
        let pt = |t: f32| center + rot * (u * t.cos() + v * t.sin()) * r;
        for i in 0..n {
            let t0 = s + (e - s) * i as f32 / n as f32;
            let t1 = s + (e - s) * (i + 1) as f32 / n as f32;
            self.line(pt(t0), pt(t1), color);
        }
    }

    fn add_shape(&mut self, shape: ColliderShape, pose: Pose, color: DebugColor, n: usize) {
        match shape {
            ColliderShape::Aabb { half_extents } => self.add_box(pose.position, Quat::IDENTITY, half_extents, color),
            ColliderShape::Box { half_extents } => self.add_box(pose.position, pose.rotation, half_extents, color),
            ColliderShape::Sphere { radius } => {
                let r = pose.rotation;
                self.circle(pose.position, r, Vec3::X, Vec3::Y, radius, n, color, None);
                self.circle(pose.position, r, Vec3::Y, Vec3::Z, radius, n, color, None);
                self.circle(pose.position, r, Vec3::X, Vec3::Z, radius, n, color, None);
            }
            ColliderShape::Capsule { radius, half_height } => {
                let r = pose.rotation;
                let top = pose.position + r * Vec3::Y * half_height;
                let bot = pose.position - r * Vec3::Y * half_height;
                self.circle(top, r, Vec3::X, Vec3::Z, radius, n, color, None);
                self.circle(bot, r, Vec3::X, Vec3::Z, radius, n, color, None);
                let pi = std::f32::consts::PI;
                for (center, sign) in [(top, 1.0f32), (bot, -1.0)] {
                    let arc = if sign > 0.0 { (0.0, pi) } else { (pi, 2.0 * pi) };
                    self.circle(center, r, Vec3::X, Vec3::Y, radius, n / 2, color, Some(arc));
                    self.circle(center, r, Vec3::Z, Vec3::Y, radius, n / 2, color, Some(arc));
                }
                for off in [Vec3::X, -Vec3::X, Vec3::Z, -Vec3::Z] {
                    let o = r * off * radius;
                    self.line(top + o, bot + o, color);
                }
            }
        }
    }

    fn add_box(&mut self, c: Vec3, rot: Quat, h: Vec3, color: DebugColor) {
        let corner = |i: usize| {
            let s = Vec3::new(
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
            );
            c + rot * (s * h)
        };
        for i in 0..8usize {
            for bit in [1usize, 2, 4] {
                if i & bit == 0 {
                    self.line(corner(i), corner(i | bit), color);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_has_12_edges_and_sphere_48() {
        let mut w = World::new();
        w.spawn((Transform::default(), Collider::aabb(Vec3::ONE)));
        let d = PhysicsDebug::build(&w, &Default::default(), |_| false);
        assert_eq!(d.lines.len(), 12);
        assert!(d.lines.iter().all(|l| l.color == DebugColor::Static));
        let mut w = World::new();
        w.spawn((Transform::default(), Collider::sphere(1.0), RigidBody::default()));
        let d = PhysicsDebug::build(&w, &Default::default(), |_| true);
        assert_eq!(d.lines.len(), 48);
        assert_eq!(d.lines[0].color, DebugColor::Sleeping);
    }

    #[test]
    fn capsule_and_trigger_colors() {
        let mut w = World::new();
        w.spawn((Transform::default(), Collider::trigger(ColliderShape::Capsule { radius: 0.5, half_height: 1.0 })));
        let d = PhysicsDebug::build(&w, &Default::default(), |_| false);
        assert!(d.lines.len() > 30);
        assert!(d.lines.iter().all(|l| l.color == DebugColor::Trigger));
    }
}
