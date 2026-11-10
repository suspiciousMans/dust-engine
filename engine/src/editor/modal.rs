//! Blender-style modal transform: G / R / S, optional X/Y/Z constraint,
//! typed numeric input, Enter/click confirms, Esc/right-click cancels.
//!
//! `ModalSession` is pure: it turns cursor + typed input into a [`Delta`]
//! relative to the gesture start and a HUD string. Applying the delta and
//! committing the undo step is [`super::interaction`]'s job.

use egui::{Key, Pos2};
use glam::{Quat, Vec3};

use super::cam_matrices::CameraMatrices;
use super::gizmo::{ray_line_param, ray_plane, Delta};
use super::snapping::{snap_angle, snap_scale_factor, snap_to_targets_at_cursor, snap_value, snap_vec3, SnapSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalKind {
    Grab,
    Rotate,
    Scale,
}

impl ModalKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Grab => "Move",
            Self::Rotate => "Rotate",
            Self::Scale => "Scale",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    None,
    /// Only this axis (0..3).
    Axis(usize),
    /// Everything except this axis (Shift+X/Y/Z).
    Plane(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalAction {
    None,
    Confirm,
    Cancel,
}

const AXIS_NAMES: [&str; 3] = ["X", "Y", "Z"];
const PLANE_NAMES: [&str; 3] = ["YZ", "XZ", "XY"];

#[derive(Clone, Debug)]
pub struct ModalSession {
    pub kind: ModalKind,
    pub constraint: Constraint,
    /// Constraint axes are the primary's own axes instead of world axes.
    pub local: bool,
    typed: String,
    start_cursor: Pos2,
    pivot: Vec3,
    primary_rotation: Quat,
    // rotate: unwrapped on-screen angle accumulation
    prev_raw: f32,
    acc: f32,
}

impl ModalSession {
    pub fn new(kind: ModalKind, pivot: Vec3, primary_rotation: Quat, cursor: Pos2) -> Self {
        Self { kind, constraint: Constraint::None, local: false, typed: String::new(), start_cursor: cursor, pivot, primary_rotation, prev_raw: 0.0, acc: 0.0 }
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Orientation constraint axes live in.
    pub fn frame(&self) -> Quat {
        if self.local {
            self.primary_rotation
        } else {
            Quat::IDENTITY
        }
    }

    /// Blender cycle: 1st press = global axis, 2nd = local, 3rd = off.
    /// `plane` (Shift) does the same for the exclude-axis plane.
    pub fn press_axis(&mut self, i: usize, plane: bool) {
        let want = if plane { Constraint::Plane(i) } else { Constraint::Axis(i) };
        if self.constraint != want {
            self.constraint = want;
            self.local = false;
        } else if !self.local {
            self.local = true;
        } else {
            self.constraint = Constraint::None;
            self.local = false;
        }
    }

    /// Accepts digits, one `.`, and `-` (toggles sign).
    pub fn type_char(&mut self, c: char) -> bool {
        match c {
            '0'..='9' => self.typed.push(c),
            '.' | ',' => {
                if self.typed.contains('.') {
                    return false;
                }
                self.typed.push('.');
            }
            '-' => {
                if self.typed.starts_with('-') {
                    self.typed.remove(0);
                } else {
                    self.typed.insert(0, '-');
                }
            }
            _ => return false,
        }
        true
    }

    pub fn backspace(&mut self) {
        self.typed.pop();
    }

    /// Parsed typed value; `None` while empty / just "-" / ".".
    pub fn numeric(&self) -> Option<f32> {
        self.typed.parse::<f32>().ok().filter(|v| v.is_finite())
    }

    /// Keyboard handling for the keys that aren't text.
    pub fn handle_key(&mut self, key: Key, shift: bool) -> ModalAction {
        match key {
            Key::Enter => ModalAction::Confirm,
            Key::Escape => ModalAction::Cancel,
            Key::Backspace => {
                self.backspace();
                ModalAction::None
            }
            Key::X => self.axis_key(0, shift),
            Key::Y => self.axis_key(1, shift),
            Key::Z => self.axis_key(2, shift),
            _ => ModalAction::None,
        }
    }

    fn axis_key(&mut self, i: usize, shift: bool) -> ModalAction {
        self.press_axis(i, shift);
        ModalAction::None
    }

    fn axis_vec(&self, i: usize) -> Vec3 {
        self.frame() * Vec3::AXES[i]
    }

    /// Delta for the current cursor. `snap_on` already accounts for Ctrl.
    pub fn delta(&mut self, cam: &CameraMatrices, cursor: Pos2, snap: &SnapSettings, snap_on: bool, targets: &[Vec3]) -> Delta {
        match self.kind {
            ModalKind::Grab => Delta::Translate(self.grab(cam, cursor, snap, snap_on, targets)),
            ModalKind::Rotate => self.rotate(cam, cursor, snap, snap_on),
            ModalKind::Scale => Delta::Scale(self.scale(cam, cursor, snap, snap_on)),
        }
    }

    fn grab(&self, cam: &CameraMatrices, cursor: Pos2, snap: &SnapSettings, snap_on: bool, targets: &[Vec3]) -> Vec3 {
        if let Some(v) = self.numeric() {
            return match self.constraint {
                Constraint::Axis(i) => self.axis_vec(i) * v,
                Constraint::Plane(i) => (self.axis_vec((i + 1) % 3) + self.axis_vec((i + 2) % 3)) * v,
                Constraint::None => Vec3::X * v,
            };
        }
        let (r0, r1) = (cam.ray(self.start_cursor), cam.ray(cursor));
        let mut d = match self.constraint {
            Constraint::Axis(i) => {
                let a = self.axis_vec(i);
                match (ray_line_param(&r0, self.pivot, a), ray_line_param(&r1, self.pivot, a)) {
                    (Some(t0), Some(t1)) => {
                        let t = if snap_on { snap_value(t1 - t0, snap.grid) } else { t1 - t0 };
                        return a * t;
                    }
                    _ => Vec3::ZERO,
                }
            }
            Constraint::Plane(i) => {
                let n = self.axis_vec(i);
                match (ray_plane(&r0, self.pivot, n), ray_plane(&r1, self.pivot, n)) {
                    (Some(a), Some(b)) => b - a,
                    _ => Vec3::ZERO,
                }
            }
            Constraint::None => {
                let n = cam.to_viewer(self.pivot);
                match (ray_plane(&r0, self.pivot, n), ray_plane(&r1, self.pivot, n)) {
                    (Some(a), Some(b)) => b - a,
                    _ => Vec3::ZERO,
                }
            }
        };
        if snap_on {
            if self.constraint == Constraint::None && snap.bounds_snap {
                if let Some(t) = snap_to_targets_at_cursor(cursor, targets, cam, snap.bounds_radius_px) {
                    return t - self.pivot;
                }
            }
            d = snap_vec3(d, snap.grid);
        }
        d
    }

    fn rotate_axis(&self, cam: &CameraMatrices) -> Vec3 {
        match self.constraint {
            Constraint::Axis(i) | Constraint::Plane(i) => self.axis_vec(i),
            Constraint::None => cam.to_viewer(self.pivot),
        }
    }

    fn rotate(&mut self, cam: &CameraMatrices, cursor: Pos2, snap: &SnapSettings, snap_on: bool) -> Delta {
        let axis = self.rotate_axis(cam);
        if let Some(deg) = self.numeric() {
            return Delta::Rotate { axis, angle: deg.to_radians() };
        }
        let sp = cam.project(self.pivot).unwrap_or(self.start_cursor);
        let (v0, v1) = (self.start_cursor - sp, cursor - sp);
        if v1.length() > 2.0 && v0.length() > 2.0 {
            // screen y is down, so negate to get a counter-clockwise angle
            let raw = -(v0.x * v1.y - v0.y * v1.x).atan2(v0.dot(v1));
            let mut step = raw - self.prev_raw;
            if step > std::f32::consts::PI {
                step -= std::f32::consts::TAU;
            } else if step < -std::f32::consts::PI {
                step += std::f32::consts::TAU;
            }
            self.acc += step;
            self.prev_raw = raw;
        }
        // CCW on screen is positive about an axis pointing at the viewer.
        let facing = if axis.dot(cam.to_viewer(self.pivot)) < 0.0 { -1.0 } else { 1.0 };
        let mut angle = self.acc * facing;
        if snap_on {
            angle = snap_angle(angle, snap.angle_deg);
        }
        Delta::Rotate { axis, angle }
    }

    fn scale(&self, cam: &CameraMatrices, cursor: Pos2, snap: &SnapSettings, snap_on: bool) -> Vec3 {
        let mut f = match self.numeric() {
            Some(v) => v,
            None => {
                let sp = cam.project(self.pivot).unwrap_or(self.start_cursor);
                cursor.distance(sp) / self.start_cursor.distance(sp).max(4.0)
            }
        };
        if snap_on && self.numeric().is_none() {
            f = snap_scale_factor(f, snap.scale_step);
        }
        match self.constraint {
            Constraint::None => Vec3::splat(f),
            Constraint::Axis(i) => {
                let mut v = Vec3::ONE;
                v[i] = f;
                v
            }
            Constraint::Plane(i) => {
                let mut v = Vec3::splat(f);
                v[i] = 1.0;
                v
            }
        }
    }

    fn constraint_label(&self) -> String {
        let loc = if self.local { " (local)" } else { "" };
        match self.constraint {
            Constraint::None => String::new(),
            Constraint::Axis(i) => format!(" {}{}", AXIS_NAMES[i], loc),
            Constraint::Plane(i) => format!(" {}{}", PLANE_NAMES[i], loc),
        }
    }

    /// One-line HUD, e.g. `Move X: 1.2500 m`, `Rotate: 45.0°`, `Scale: 2.0000`.
    /// While typing, shows the raw typed text with a caret.
    pub fn hud(&self, delta: &Delta) -> String {
        let head = format!("{}{}", self.kind.label(), self.constraint_label());
        let value = if !self.typed.is_empty() {
            format!("{}_", self.typed)
        } else {
            match *delta {
                Delta::Translate(d) => match self.constraint {
                    Constraint::Axis(i) => format!("{:.4}", d.dot(self.axis_vec(i))),
                    _ => format!("{:.4}, {:.4}, {:.4}", d.x, d.y, d.z),
                },
                Delta::Rotate { angle, .. } => format!("{:.1}", angle.to_degrees() + 0.0),
                Delta::Scale(f) => match self.constraint {
                    Constraint::Axis(i) => format!("{:.4}", f[i]),
                    Constraint::Plane(i) => format!("{:.4}", f[(i + 1) % 3]),
                    Constraint::None => format!("{:.4}", f.x),
                },
            }
        };
        let unit = match self.kind {
            ModalKind::Grab => " m",
            ModalKind::Rotate => "°",
            ModalKind::Scale => "",
        };
        format!("{head}: {value}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::cam_matrices::test_util::*;

    fn sess(kind: ModalKind) -> (ModalSession, CameraMatrices) {
        let cam = cam_front();
        let c = cam.project(Vec3::ZERO).unwrap();
        (ModalSession::new(kind, Vec3::ZERO, Quat::from_rotation_z(0.5), c), cam)
    }

    fn off() -> SnapSettings {
        SnapSettings::default()
    }

    #[test]
    fn constraint_cycle_global_local_off() {
        let (mut s, _) = sess(ModalKind::Grab);
        s.press_axis(0, false);
        assert_eq!((s.constraint, s.local), (Constraint::Axis(0), false));
        s.press_axis(0, false);
        assert_eq!((s.constraint, s.local), (Constraint::Axis(0), true));
        s.press_axis(0, false);
        assert_eq!((s.constraint, s.local), (Constraint::None, false));
        s.press_axis(1, true);
        assert_eq!(s.constraint, Constraint::Plane(1));
        s.press_axis(2, false);
        assert_eq!((s.constraint, s.local), (Constraint::Axis(2), false));
    }

    #[test]
    fn typed_numbers() {
        let (mut s, _) = sess(ModalKind::Grab);
        assert!(s.numeric().is_none());
        for c in "1.5".chars() {
            assert!(s.type_char(c));
        }
        assert!(!s.type_char('.'));
        assert!(!s.type_char('q'));
        assert_eq!(s.numeric(), Some(1.5));
        s.type_char('-');
        assert_eq!(s.numeric(), Some(-1.5));
        s.type_char('-');
        s.backspace();
        s.backspace();
        s.backspace();
        assert!(s.numeric().is_none());
        s.type_char('-');
        assert!(s.numeric().is_none()); // bare minus
    }

    #[test]
    fn keys_confirm_cancel_and_axes() {
        let (mut s, _) = sess(ModalKind::Grab);
        assert_eq!(s.handle_key(Key::X, false), ModalAction::None);
        assert_eq!(s.constraint, Constraint::Axis(0));
        assert_eq!(s.handle_key(Key::Z, true), ModalAction::None);
        assert_eq!(s.constraint, Constraint::Plane(2));
        assert_eq!(s.handle_key(Key::Enter, false), ModalAction::Confirm);
        assert_eq!(s.handle_key(Key::Escape, false), ModalAction::Cancel);
    }

    #[test]
    fn grab_axis_constrained_ignores_other_axes() {
        let (mut s, cam) = sess(ModalKind::Grab);
        s.press_axis(0, false);
        let to = cam.project(Vec3::ZERO).unwrap() + egui::vec2(50.0, 40.0);
        let Delta::Translate(d) = s.delta(&cam, to, &off(), false, &[]) else { panic!() };
        assert!(d.x > 0.1 && d.y == 0.0 && d.z == 0.0, "{d:?}");
    }

    #[test]
    fn grab_free_moves_in_view_plane() {
        let (mut s, cam) = sess(ModalKind::Grab);
        let target = cam.project(Vec3::new(1.0, 2.0, 0.0)).unwrap();
        let Delta::Translate(d) = s.delta(&cam, target, &off(), false, &[]) else { panic!() };
        assert!((d - Vec3::new(1.0, 2.0, 0.0)).length() < 1e-3, "{d:?}");
    }

    #[test]
    fn grab_plane_constraint_keeps_excluded_axis() {
        let (mut s, cam) = sess(ModalKind::Grab);
        s.press_axis(2, true); // XY plane (exclude Z)
        let target = cam.project(Vec3::new(1.0, 1.0, 0.0)).unwrap();
        let Delta::Translate(d) = s.delta(&cam, target, &off(), false, &[]) else { panic!() };
        assert!(d.z.abs() < 1e-5 && d.x > 0.5);
    }

    #[test]
    fn typed_grab_and_hud() {
        let (mut s, cam) = sess(ModalKind::Grab);
        s.press_axis(1, false);
        for c in "2.5".chars() {
            s.type_char(c);
        }
        let d = s.delta(&cam, s.start_cursor, &off(), false, &[]);
        assert_eq!(d, Delta::Translate(Vec3::new(0.0, 2.5, 0.0)));
        assert_eq!(s.hud(&d), "Move Y: 2.5_ m");
    }

    #[test]
    fn hud_formats_without_typing() {
        let (mut s, cam) = sess(ModalKind::Grab);
        s.press_axis(0, false);
        assert_eq!(s.hud(&Delta::Translate(Vec3::new(1.25, 0.0, 0.0))), "Move X: 1.2500 m");
        s.press_axis(0, false);
        assert_eq!(s.hud(&Delta::Translate(Vec3::ZERO)), "Move X (local): 0.0000 m");
        s.press_axis(0, false);
        assert_eq!(s.hud(&Delta::Translate(Vec3::new(1.0, 2.0, 3.0))), "Move: 1.0000, 2.0000, 3.0000 m");
        let _ = cam;
        let r = ModalSession::new(ModalKind::Rotate, Vec3::ZERO, Quat::IDENTITY, Pos2::ZERO);
        assert_eq!(r.hud(&Delta::Rotate { axis: Vec3::Z, angle: 45f32.to_radians() }), "Rotate: 45.0°");
        let mut sc = ModalSession::new(ModalKind::Scale, Vec3::ZERO, Quat::IDENTITY, Pos2::ZERO);
        sc.press_axis(2, true);
        assert_eq!(sc.hud(&Delta::Scale(Vec3::new(1.5, 1.5, 1.0))), "Scale XY: 1.5000");
    }

    #[test]
    fn rotate_follows_cursor_and_snaps() {
        let (mut s, cam) = sess(ModalKind::Rotate);
        s.start_cursor = cam.project(Vec3::ZERO).unwrap() + egui::vec2(100.0, 0.0);
        let to = cam.project(Vec3::ZERO).unwrap() + egui::vec2(0.0, -100.0); // up on screen = CCW
        let Delta::Rotate { axis, angle } = s.delta(&cam, to, &off(), false, &[]) else { panic!() };
        assert!((axis - Vec3::Z).length() < 1e-4);
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "{angle}");
        let on = SnapSettings { angle_deg: 30.0, ..off() };
        let to = cam.project(Vec3::ZERO).unwrap() + egui::vec2(100.0, -50.0);
        let Delta::Rotate { angle, .. } = s.delta(&cam, to, &on, true, &[]) else { panic!() };
        assert!((angle.to_degrees() - 30.0).abs() < 1e-3, "{}", angle.to_degrees());
    }

    #[test]
    fn rotate_accumulates_past_180() {
        let (mut s, cam) = sess(ModalKind::Rotate);
        let c = cam.project(Vec3::ZERO).unwrap();
        s.start_cursor = c + egui::vec2(100.0, 0.0);
        let mut last = 0.0;
        for deg in (10..=270).step_by(10) {
            let a = (deg as f32).to_radians();
            let p = c + egui::vec2(a.cos() * 100.0, -a.sin() * 100.0);
            let Delta::Rotate { angle, .. } = s.delta(&cam, p, &off(), false, &[]) else { panic!() };
            last = angle.to_degrees();
        }
        assert!((last - 270.0).abs() < 0.5, "{last}");
    }

    #[test]
    fn scale_ratio_constraint_and_snap() {
        let (mut s, cam) = sess(ModalKind::Scale);
        let c = cam.project(Vec3::ZERO).unwrap();
        s.start_cursor = c + egui::vec2(50.0, 0.0);
        let Delta::Scale(f) = s.delta(&cam, c + egui::vec2(0.0, 100.0), &off(), false, &[]) else { panic!() };
        assert!((f - Vec3::splat(2.0)).length() < 1e-4);
        s.press_axis(1, false);
        let Delta::Scale(f) = s.delta(&cam, c + egui::vec2(0.0, 100.0), &off(), false, &[]) else { panic!() };
        assert_eq!(f, Vec3::new(1.0, 2.0, 1.0));
        let on = SnapSettings { scale_step: 0.5, ..off() };
        let Delta::Scale(f) = s.delta(&cam, c + egui::vec2(0.0, 61.0), &on, true, &[]) else { panic!() };
        assert_eq!(f.y, 1.0);
    }

    #[test]
    fn grid_and_bounds_snap_on_grab() {
        let (mut s, cam) = sess(ModalKind::Grab);
        let target = cam.project(Vec3::new(1.3, 0.2, 0.0)).unwrap();
        let on = SnapSettings { enabled: true, grid: 0.5, bounds_snap: false, ..off() };
        let Delta::Translate(d) = s.delta(&cam, target, &on, true, &[]) else { panic!() };
        assert!((d - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-3, "{d:?}");
        let on = SnapSettings { enabled: true, grid: 0.5, bounds_snap: true, ..off() };
        let corner = Vec3::new(1.3, 0.25, 0.0);
        let Delta::Translate(d) = s.delta(&cam, target, &on, true, &[corner]) else { panic!() };
        assert!((d - corner).length() < 1e-5);
    }
}
