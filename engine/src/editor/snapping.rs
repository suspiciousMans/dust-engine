//! Grid / angle / scale increments and bounds-corner snapping.

use egui::Pos2;
use glam::Vec3;
use hecs::{Entity, World};

use super::bounds::{local_bounds, GIZMO_ICON_RADIUS};
use super::cam_matrices::CameraMatrices;
use super::picking::is_pickable;
use super::world_xform::world_matrix;
use crate::ecs::Transform;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapSettings {
    /// Snapping on by default; holding Ctrl inverts this per gesture.
    pub enabled: bool,
    /// Translation step in world units.
    pub grid: f32,
    pub angle_deg: f32,
    /// Scale factor step (1.0 -> 1.1 -> ...).
    pub scale_step: f32,
    /// Also snap to corners/centres of nearby entity bounds.
    pub bounds_snap: bool,
    /// Pull radius for bounds snapping, in screen points.
    pub bounds_radius_px: f32,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self { enabled: false, grid: 1.0, angle_deg: 15.0, scale_step: 0.1, bounds_snap: true, bounds_radius_px: 12.0 }
    }
}

impl SnapSettings {
    /// Effective on/off for one gesture given whether Ctrl is held.
    pub fn active(&self, ctrl_held: bool) -> bool {
        self.enabled != ctrl_held
    }
}

/// Rounds to the nearest multiple of `step` (`step <= 0` = no snapping).
pub fn snap_value(v: f32, step: f32) -> f32 {
    if step > 1e-9 {
        // +0.0 so -0.0 never leaks into HUD strings
        (v / step).round() * step + 0.0
    } else {
        v
    }
}

pub fn snap_vec3(v: Vec3, step: f32) -> Vec3 {
    Vec3::new(snap_value(v.x, step), snap_value(v.y, step), snap_value(v.z, step))
}

/// Radians in, radians out, snapped to `step_deg`.
pub fn snap_angle(rad: f32, step_deg: f32) -> f32 {
    snap_value(rad.to_degrees(), step_deg).to_radians()
}

/// Scale *factors* (1.0 = unchanged) snap to multiples of `step`, never to 0.
pub fn snap_scale_factor(f: f32, step: f32) -> f32 {
    let s = snap_value(f, step);
    if s.abs() < 1e-6 && step > 0.0 {
        step * f.signum()
    } else {
        s
    }
}

/// World-space snap targets: the 8 corners and centre of every pickable
/// entity's bounds (oriented), excluding `exclude` (the things being moved).
pub fn collect_snap_targets(world: &World, exclude: &[Entity]) -> Vec<Vec3> {
    let mut out = Vec::new();
    for (e, _) in world.query::<&Transform>().iter() {
        if exclude.contains(&e) || !is_pickable(world, e) {
            continue;
        }
        let m = world_matrix(world, e);
        match local_bounds(world, e) {
            Some(b) => {
                out.extend(b.corners().iter().map(|c| m.transform_point3(*c)));
                out.push(m.transform_point3(b.center()));
            }
            None => out.push(m.w_axis.truncate()),
        }
    }
    let _ = GIZMO_ICON_RADIUS;
    out
}

/// Closest target to `point` whose *screen* position is within `radius_px` of
/// the cursor-projected `point`. Returns the target's world position.
pub fn snap_to_targets(point: Vec3, targets: &[Vec3], cam: &CameraMatrices, radius_px: f32) -> Option<Vec3> {
    let sp = cam.project(point)?;
    targets
        .iter()
        .filter_map(|t| cam.project(*t).map(|s| (*t, s.distance(sp))))
        .filter(|(_, d)| *d <= radius_px)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, _)| t)
}

/// Cursor-based variant: nearest target to a *screen* position.
pub fn snap_to_targets_at_cursor(cursor: Pos2, targets: &[Vec3], cam: &CameraMatrices, radius_px: f32) -> Option<Vec3> {
    targets
        .iter()
        .filter_map(|t| cam.project(*t).map(|s| (*t, s.distance(cursor))))
        .filter(|(_, d)| *d <= radius_px)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::cam_matrices::test_util::*;
    use crate::editor::hierarchy::{Locked, Primitive, PrimitiveShape};

    #[test]
    fn value_snapping() {
        assert_eq!(snap_value(1.26, 0.5), 1.5);
        assert_eq!(snap_value(1.24, 0.5), 1.0);
        assert_eq!(snap_value(-0.2, 0.5), 0.0);
        assert!(snap_value(-0.2, 0.5).is_sign_positive());
        assert_eq!(snap_value(1.23, 0.0), 1.23);
        assert_eq!(snap_vec3(Vec3::new(0.9, 1.6, -1.4), 1.0), Vec3::new(1.0, 2.0, -1.0));
    }

    #[test]
    fn angle_snapping() {
        let s = snap_angle(40f32.to_radians(), 15.0);
        assert!((s.to_degrees() - 45.0).abs() < 1e-4);
        let s = snap_angle((-100f32).to_radians(), 15.0);
        assert!((s.to_degrees() + 105.0).abs() < 1e-3);
    }

    #[test]
    fn scale_never_snaps_to_zero() {
        assert!((snap_scale_factor(1.04, 0.1) - 1.0).abs() < 1e-6);
        assert!((snap_scale_factor(1.26, 0.1) - 1.3).abs() < 1e-5);
        assert!((snap_scale_factor(0.01, 0.1) - 0.1).abs() < 1e-6);
        assert!((snap_scale_factor(-0.01, 0.1) + 0.1).abs() < 1e-6);
    }

    #[test]
    fn ctrl_inverts() {
        let mut s = SnapSettings::default();
        assert!(!s.active(false));
        assert!(s.active(true));
        s.enabled = true;
        assert!(s.active(false));
        assert!(!s.active(true));
    }

    #[test]
    fn bounds_targets_and_exclusion() {
        let mut w = World::new();
        let a = w.spawn((Transform::from_position(Vec3::new(2.0, 0.0, 0.0)), Primitive(PrimitiveShape::Cube)));
        let b = w.spawn((Transform::default(), Primitive(PrimitiveShape::Cube)));
        let locked = w.spawn((Transform::from_position(Vec3::Y * 9.0), Primitive(PrimitiveShape::Cube), Locked(true)));
        let t = collect_snap_targets(&w, &[b]);
        assert_eq!(t.len(), 9); // only `a`: 8 corners + centre
        assert!(t.contains(&Vec3::new(2.5, 0.5, 0.5)));
        let _ = (a, locked);
        // no mesh -> one point
        w.spawn((Transform::from_position(Vec3::Z * 5.0),));
        assert_eq!(collect_snap_targets(&w, &[b]).len(), 10);
    }

    #[test]
    fn snaps_to_nearest_projected_target_within_radius() {
        let cam = cam_front();
        let targets = [Vec3::new(1.0, 1.0, 0.0), Vec3::new(1.02, 1.0, 0.0), Vec3::new(5.0, 5.0, 0.0)];
        let near = Vec3::new(1.015, 1.0, 0.0);
        assert_eq!(snap_to_targets(near, &targets, &cam, 12.0), Some(targets[1]));
        assert_eq!(snap_to_targets(Vec3::new(-3.0, 0.0, 0.0), &targets, &cam, 12.0), None);
        let cursor = cam.project(Vec3::new(5.0, 5.0, 0.0)).unwrap() + egui::vec2(3.0, -2.0);
        assert_eq!(snap_to_targets_at_cursor(cursor, &targets, &cam, 12.0), Some(targets[2]));
    }
}
