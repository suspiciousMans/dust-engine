//! Transform gizmo: handle hit-testing, drag state machine and painter
//! drawing, plus the pure [`apply_delta`] maths shared with the modal G/R/S
//! operators. Nothing here touches the `World`.

use egui::{Color32, Painter, Pos2, Shape, Stroke};
use glam::{Quat, Vec3};

use super::cam_matrices::{CameraMatrices, Ray};
use crate::ecs::Transform;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GizmoMode {
    #[default]
    Translate,
    Rotate,
    Scale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Space {
    Local,
    #[default]
    Global,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pivot {
    /// Mean of the selected origins.
    #[default]
    Median,
    /// Centre of the box around the selected origins.
    BoundsCenter,
    /// The primary (last selected) entity's origin.
    Active,
    /// Every entity rotates/scales about its own origin.
    Individual,
}

/// Where the pivot sits for `positions` (world origins); `primary` indexes
/// the active one.
pub fn pivot_point(positions: &[Vec3], primary: usize, mode: Pivot) -> Vec3 {
    if positions.is_empty() {
        return Vec3::ZERO;
    }
    match mode {
        Pivot::Active => positions[primary.min(positions.len() - 1)],
        Pivot::BoundsCenter => {
            let (mut lo, mut hi) = (positions[0], positions[0]);
            for p in positions {
                lo = lo.min(*p);
                hi = hi.max(*p);
            }
            (lo + hi) * 0.5
        }
        Pivot::Median | Pivot::Individual => positions.iter().copied().sum::<Vec3>() / positions.len() as f32,
    }
}

/// One incremental change relative to the gesture start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Delta {
    /// World-space offset.
    Translate(Vec3),
    /// Rotation about a world-space axis through the pivot.
    Rotate { axis: Vec3, angle: f32 },
    /// Per-axis factors expressed in the gesture frame.
    Scale(Vec3),
}

/// Applies `delta` to world-space `initial` transforms. `frame` is the
/// orientation scale factors are expressed in; with `Pivot::Individual` and
/// `Space::Local` each entity uses its own orientation instead.
pub fn apply_delta(initial: &[Transform], pivot: Vec3, frame: Quat, pivot_mode: Pivot, space: Space, delta: &Delta) -> Vec<Transform> {
    initial
        .iter()
        .map(|t| {
            let individual = pivot_mode == Pivot::Individual;
            let piv = if individual { t.position } else { pivot };
            let mut out = *t;
            match *delta {
                Delta::Translate(d) => out.position += d,
                Delta::Rotate { axis, angle } => {
                    let q = Quat::from_axis_angle(axis.normalize_or_zero(), angle);
                    out.position = piv + q * (t.position - piv);
                    out.rotation = (q * t.rotation).normalize();
                }
                Delta::Scale(f) => {
                    let fr = if individual && space == Space::Local { t.rotation } else { frame };
                    let off = fr.inverse() * (t.position - piv);
                    out.position = piv + fr * (off * f);
                    // Entity axes pick up the factor in proportion to how
                    // well they line up with each frame axis (exact when
                    // aligned, a sensible approximation otherwise).
                    let fa = [fr * Vec3::X, fr * Vec3::Y, fr * Vec3::Z];
                    let ra = [t.rotation * Vec3::X, t.rotation * Vec3::Y, t.rotation * Vec3::Z];
                    let mut s = Vec3::ZERO;
                    for j in 0..3 {
                        let mut acc = 0.0;
                        for k in 0..3 {
                            let c = ra[j].dot(fa[k]);
                            acc += c * c * f[k];
                        }
                        s[j] = t.scale[j] * acc;
                    }
                    out.scale = s;
                }
            }
            out
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    /// Translate/scale along (or rotate about) axis 0..3 = X/Y/Z.
    Axis(usize),
    /// Plane whose normal is axis 0..3 (translate/scale modes).
    Plane(usize),
    /// Screen-plane move / uniform scale.
    Center,
}

pub const AXIS_COLORS: [Color32; 3] = [Color32::from_rgb(230, 60, 60), Color32::from_rgb(90, 200, 60), Color32::from_rgb(60, 110, 230)];
pub const HOVER_COLOR: Color32 = Color32::from_rgb(255, 220, 80);
pub const ACTIVE_COLOR: Color32 = Color32::from_rgb(255, 255, 255);

const PICK_PX: f32 = 8.0;
const PLANE_LO: f32 = 0.25;
const PLANE_HI: f32 = 0.55;
const ROT_RADIUS: f32 = 0.9;

#[derive(Clone, Copy, Debug)]
struct Drag {
    handle: Handle,
    origin: Vec3,
    frame: Quat,
    size: f32,
    /// Line parameter at drag start (axis handles).
    start_t: f32,
    /// Plane hit at drag start (planar / rotate handles).
    start_hit: Vec3,
    start_cursor_dist: f32,
    last_angle: f32,
}

#[derive(Clone, Debug)]
pub struct Gizmo {
    pub mode: GizmoMode,
    pub space: Space,
    pub pivot: Pivot,
    /// Handle length in screen points (constant on screen).
    pub size_px: f32,
    pub hover: Option<Handle>,
    drag: Option<Drag>,
}

impl Default for Gizmo {
    fn default() -> Self {
        Self { mode: GizmoMode::Translate, space: Space::Global, pivot: Pivot::Median, size_px: 90.0, hover: None, drag: None }
    }
}

fn axis_dir(frame: Quat, i: usize) -> Vec3 {
    frame * Vec3::AXES[i]
}

fn dist_point_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let l2 = ab.length_sq();
    if l2 < 1e-6 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / l2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

fn point_in_quad(p: Pos2, q: &[Pos2; 4]) -> bool {
    // Edge-on planes collapse to a line: not clickable (the axis wins).
    let area: f32 = (0..4).map(|i| q[i].x * q[(i + 1) % 4].y - q[(i + 1) % 4].x * q[i].y).sum::<f32>().abs() * 0.5;
    if area < 30.0 {
        return false;
    }
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let c = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        if c.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = c.signum();
        } else if sign != c.signum() {
            return false;
        }
    }
    true
}

/// Parameter `t` along the line `o + t*a` closest to `ray`.
pub fn ray_line_param(ray: &Ray, o: Vec3, a: Vec3) -> Option<f32> {
    let a = a.normalize_or_zero();
    let w0 = o - ray.origin;
    let b = a.dot(ray.dir);
    let d = a.dot(w0);
    let e = ray.dir.dot(w0);
    let den = 1.0 - b * b;
    if den < 1e-5 {
        return None;
    }
    Some((b * e - d) / den)
}

pub fn ray_plane(ray: &Ray, p0: Vec3, n: Vec3) -> Option<Vec3> {
    let den = ray.dir.dot(n);
    if den.abs() < 1e-6 {
        return None;
    }
    let t = (p0 - ray.origin).dot(n) / den;
    (t >= 0.0).then(|| ray.at(t))
}

/// World length of the handles so they stay `px` points long on screen.
pub fn gizmo_world_size(cam: &CameraMatrices, origin: Vec3, px: f32) -> f32 {
    cam.world_per_point(origin) * px
}

impl Gizmo {
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
    pub fn active_handle(&self) -> Option<Handle> {
        self.drag.map(|d| d.handle)
    }

    /// Orientation of the handles given the primary entity's world rotation.
    pub fn frame(&self, primary_rotation: Quat) -> Quat {
        match self.space {
            Space::Local => primary_rotation,
            Space::Global => Quat::IDENTITY,
        }
    }

    fn plane_quad(&self, cam: &CameraMatrices, origin: Vec3, frame: Quat, size: f32, n: usize) -> Option<[Pos2; 4]> {
        let (u, v) = ((n + 1) % 3, (n + 2) % 3);
        let (du, dv) = (axis_dir(frame, u), axis_dir(frame, v));
        let c = |a: f32, b: f32| cam.project(origin + du * (size * a) + dv * (size * b));
        Some([c(PLANE_LO, PLANE_LO)?, c(PLANE_HI, PLANE_LO)?, c(PLANE_HI, PLANE_HI)?, c(PLANE_LO, PLANE_HI)?])
    }

    fn circle_points(&self, origin: Vec3, frame: Quat, size: f32, n: usize) -> Vec<Vec3> {
        let (u, v) = (axis_dir(frame, (n + 1) % 3), axis_dir(frame, (n + 2) % 3));
        (0..=48)
            .map(|i| {
                let a = i as f32 / 48.0 * std::f32::consts::TAU;
                origin + (u * a.cos() + v * a.sin()) * (size * ROT_RADIUS)
            })
            .collect()
    }

    /// Which handle is under `cursor`.
    pub fn hit_test(&self, cam: &CameraMatrices, origin: Vec3, frame: Quat, cursor: Pos2) -> Option<Handle> {
        let size = gizmo_world_size(cam, origin, self.size_px);
        let o = cam.project(origin)?;
        match self.mode {
            GizmoMode::Rotate => {
                let mut best: Option<(usize, f32)> = None;
                let to_cam = cam.to_viewer(origin);
                for n in 0..3 {
                    // Only the half facing the camera is pickable.
                    let pts: Vec<(Pos2, bool)> = self
                        .circle_points(origin, frame, size, n)
                        .iter()
                        .filter_map(|p| cam.project(*p).map(|s| (s, (*p - origin).dot(to_cam) >= -0.05 * size)))
                        .collect();
                    for w in pts.windows(2) {
                        if w[0].1 && w[1].1 {
                            let d = dist_point_segment(cursor, w[0].0, w[1].0);
                            if d <= PICK_PX && best.map(|b| d < b.1).unwrap_or(true) {
                                best = Some((n, d));
                            }
                        }
                    }
                }
                best.map(|(n, _)| Handle::Axis(n))
            }
            GizmoMode::Translate | GizmoMode::Scale => {
                if cursor.distance(o) <= PICK_PX {
                    return Some(Handle::Center);
                }
                for n in 0..3 {
                    if let Some(q) = self.plane_quad(cam, origin, frame, size, n) {
                        if point_in_quad(cursor, &q) {
                            return Some(Handle::Plane(n));
                        }
                    }
                }
                let mut best: Option<(usize, f32)> = None;
                for n in 0..3 {
                    if let Some(e) = cam.project(origin + axis_dir(frame, n) * size) {
                        let d = dist_point_segment(cursor, o, e);
                        if d <= PICK_PX && best.map(|b| d < b.1).unwrap_or(true) {
                            best = Some((n, d));
                        }
                    }
                }
                best.map(|(n, _)| Handle::Axis(n))
            }
        }
    }

    pub fn update_hover(&mut self, cam: &CameraMatrices, origin: Vec3, frame: Quat, cursor: Option<Pos2>) {
        if self.drag.is_none() {
            self.hover = cursor.and_then(|c| self.hit_test(cam, origin, frame, c));
        }
    }

    fn plane_normal(&self, cam: &CameraMatrices, origin: Vec3, frame: Quat, h: Handle) -> Vec3 {
        match (self.mode, h) {
            (GizmoMode::Rotate, Handle::Axis(n)) | (_, Handle::Plane(n)) => axis_dir(frame, n),
            _ => cam.to_viewer(origin),
        }
    }

    /// Starts dragging `handle`. Returns false if the cursor ray can't be
    /// resolved against it (e.g. an axis seen exactly end-on).
    pub fn begin_drag(&mut self, handle: Handle, cam: &CameraMatrices, origin: Vec3, frame: Quat, cursor: Pos2) -> bool {
        let size = gizmo_world_size(cam, origin, self.size_px);
        let ray = cam.ray(cursor);
        let screen_o = cam.project(origin).unwrap_or(cursor);
        let mut d = Drag {
            handle,
            origin,
            frame,
            size,
            start_t: 0.0,
            start_hit: origin,
            start_cursor_dist: cursor.distance(screen_o).max(4.0),
            last_angle: 0.0,
        };
        match (self.mode, handle) {
            (GizmoMode::Rotate, _) | (_, Handle::Plane(_)) | (_, Handle::Center) => {
                let n = self.plane_normal(cam, origin, frame, handle);
                match ray_plane(&ray, origin, n) {
                    Some(h) => d.start_hit = h,
                    None => return false,
                }
            }
            (_, Handle::Axis(n)) => match ray_line_param(&ray, origin, axis_dir(frame, n)) {
                Some(t) => d.start_t = t,
                None => return false,
            },
        }
        self.drag = Some(d);
        true
    }

    /// Current gesture delta for the cursor position (always relative to the
    /// drag start, so it never accumulates error).
    pub fn drag_delta(&mut self, cam: &CameraMatrices, cursor: Pos2) -> Option<Delta> {
        let mode = self.mode;
        let mut d = self.drag?;
        let ray = cam.ray(cursor);
        let out = match (mode, d.handle) {
            (GizmoMode::Rotate, Handle::Axis(n)) => {
                let axis = axis_dir(d.frame, n);
                if let Some(h) = ray_plane(&ray, d.origin, axis) {
                    let (v0, v1) = (d.start_hit - d.origin, h - d.origin);
                    if v0.length() > 1e-5 && v1.length() > 1e-5 {
                        d.last_angle = axis.dot(v0.cross(v1)).atan2(v0.dot(v1));
                    }
                }
                Delta::Rotate { axis, angle: d.last_angle }
            }
            (GizmoMode::Rotate, _) => return None,
            (GizmoMode::Translate, Handle::Axis(n)) => {
                let a = axis_dir(d.frame, n);
                let t = ray_line_param(&ray, d.origin, a).unwrap_or(d.start_t);
                Delta::Translate(a * (t - d.start_t))
            }
            (GizmoMode::Translate, Handle::Plane(_) | Handle::Center) => {
                let n = self.plane_normal(cam, d.origin, d.frame, d.handle);
                let h = ray_plane(&ray, d.origin, n).unwrap_or(d.start_hit);
                Delta::Translate(h - d.start_hit)
            }
            (GizmoMode::Scale, Handle::Axis(n)) => {
                let a = axis_dir(d.frame, n);
                let t = ray_line_param(&ray, d.origin, a).unwrap_or(d.start_t);
                let mut f = Vec3::ONE;
                f[n] = (1.0 + (t - d.start_t) / d.size).max(0.01);
                Delta::Scale(f)
            }
            (GizmoMode::Scale, Handle::Plane(n)) => {
                let h = ray_plane(&ray, d.origin, axis_dir(d.frame, n)).unwrap_or(d.start_hit);
                let mut f = Vec3::ONE;
                for k in [(n + 1) % 3, (n + 2) % 3] {
                    f[k] = (1.0 + (h - d.start_hit).dot(axis_dir(d.frame, k)) / d.size).max(0.01);
                }
                Delta::Scale(f)
            }
            (GizmoMode::Scale, Handle::Center) => {
                let so = cam.project(d.origin)?;
                Delta::Scale(Vec3::splat((cursor.distance(so) / d.start_cursor_dist).max(0.01)))
            }
        };
        self.drag = Some(d);
        Some(out)
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    // ------------------------------------------------------------ drawing

    fn color(&self, h: Handle, base: Color32) -> Color32 {
        if self.drag.map(|d| d.handle) == Some(h) {
            ACTIVE_COLOR
        } else if self.hover == Some(h) && self.drag.is_none() {
            HOVER_COLOR
        } else {
            base
        }
    }

    pub fn draw(&self, painter: &Painter, cam: &CameraMatrices, origin: Vec3, frame: Quat) {
        let size = gizmo_world_size(cam, origin, self.size_px);
        let Some(o) = cam.project(origin) else { return };
        // While dragging, only the dragged handle is drawn.
        let shown = |h: Handle| self.drag.map(|d| d.handle == h).unwrap_or(true);
        match self.mode {
            GizmoMode::Rotate => {
                let to_cam = cam.to_viewer(origin);
                for n in 0..3 {
                    if !shown(Handle::Axis(n)) {
                        continue;
                    }
                    let col = self.color(Handle::Axis(n), AXIS_COLORS[n]);
                    let mut run: Vec<Pos2> = Vec::new();
                    for p in self.circle_points(origin, frame, size, n) {
                        let front = (p - origin).dot(to_cam) >= -0.05 * size || self.drag.is_some();
                        match (front, cam.project(p)) {
                            (true, Some(s)) => run.push(s),
                            _ => {
                                if run.len() > 1 {
                                    painter.add(Shape::line(std::mem::take(&mut run), Stroke::new(2.0, col)));
                                }
                                run.clear();
                            }
                        }
                    }
                    if run.len() > 1 {
                        painter.add(Shape::line(run, Stroke::new(2.0, col)));
                    }
                }
            }
            GizmoMode::Translate | GizmoMode::Scale => {
                for n in 0..3 {
                    if shown(Handle::Plane(n)) && self.drag.is_none() {
                        if let Some(q) = self.plane_quad(cam, origin, frame, size, n) {
                            let c = self.color(Handle::Plane(n), AXIS_COLORS[n]);
                            let fill = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 90);
                            painter.add(Shape::convex_polygon(q.to_vec(), fill, Stroke::new(1.0, c)));
                        }
                    }
                    if !shown(Handle::Axis(n)) {
                        continue;
                    }
                    let col = self.color(Handle::Axis(n), AXIS_COLORS[n]);
                    let Some(tip) = cam.project(origin + axis_dir(frame, n) * size) else { continue };
                    painter.line_segment([o, tip], Stroke::new(2.5, col));
                    if self.mode == GizmoMode::Scale {
                        painter.rect_filled(egui::Rect::from_center_size(tip, egui::vec2(9.0, 9.0)), 0.0, col);
                    } else {
                        let dir = (tip - o).normalized();
                        let perp = egui::vec2(-dir.y, dir.x);
                        painter.add(Shape::convex_polygon(vec![tip + dir * 12.0, tip + perp * 5.0, tip - perp * 5.0], col, Stroke::NONE));
                    }
                }
                if shown(Handle::Center) {
                    painter.circle_filled(o, 5.0, self.color(Handle::Center, Color32::from_gray(220)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::cam_matrices::test_util::*;

    fn t(p: Vec3) -> Transform {
        Transform::from_position(p)
    }

    #[test]
    fn translate_delta() {
        let out = apply_delta(&[t(Vec3::X)], Vec3::X, Quat::IDENTITY, Pivot::Median, Space::Global, &Delta::Translate(Vec3::new(0.0, 2.0, 0.0)));
        assert_eq!(out[0].position, Vec3::new(1.0, 2.0, 0.0));
    }

    #[test]
    fn rotate_about_median_vs_individual() {
        let init = [t(Vec3::new(-1.0, 0.0, 0.0)), t(Vec3::new(1.0, 0.0, 0.0))];
        let d = Delta::Rotate { axis: Vec3::Z, angle: std::f32::consts::FRAC_PI_2 };
        let med = apply_delta(&init, Vec3::ZERO, Quat::IDENTITY, Pivot::Median, Space::Global, &d);
        assert!((med[1].position - Vec3::Y).length() < 1e-5);
        assert!((med[0].position + Vec3::Y).length() < 1e-5);
        let ind = apply_delta(&init, Vec3::ZERO, Quat::IDENTITY, Pivot::Individual, Space::Global, &d);
        assert_eq!(ind[1].position, init[1].position);
        assert!(ind[1].rotation.angle_between(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)) < 1e-5);
    }

    #[test]
    fn scale_about_pivot_and_own_axes() {
        let init = [t(Vec3::new(2.0, 0.0, 0.0))];
        let d = Delta::Scale(Vec3::new(2.0, 1.0, 1.0));
        let o = apply_delta(&init, Vec3::ZERO, Quat::IDENTITY, Pivot::Median, Space::Global, &d);
        assert!((o[0].position - Vec3::new(4.0, 0.0, 0.0)).length() < 1e-5);
        assert!((o[0].scale - Vec3::new(2.0, 1.0, 1.0)).length() < 1e-5);
        // object rotated 90deg about Z: global X scaling stretches its local Y
        let mut r = t(Vec3::ZERO);
        r.rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let o = apply_delta(&[r], Vec3::ZERO, Quat::IDENTITY, Pivot::Median, Space::Global, &d);
        assert!((o[0].scale - Vec3::new(1.0, 2.0, 1.0)).length() < 1e-4, "{:?}", o[0].scale);
    }

    #[test]
    fn pivots() {
        let p = [Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0), Vec3::new(5.0, 3.0, 0.0)];
        assert_eq!(pivot_point(&p, 1, Pivot::Active), p[1]);
        assert_eq!(pivot_point(&p, 0, Pivot::BoundsCenter), Vec3::new(2.5, 1.5, 0.0));
        assert!((pivot_point(&p, 0, Pivot::Median) - Vec3::new(3.0, 1.0, 0.0)).length() < 1e-6);
    }

    #[test]
    fn constant_screen_size() {
        let g = Gizmo::default();
        for cam in [cam_at(Vec3::new(0.0, 0.0, 5.0)), cam_at(Vec3::new(0.0, 0.0, 50.0))] {
            let a = cam.project(Vec3::ZERO).unwrap();
            let s = gizmo_world_size(&cam, Vec3::ZERO, g.size_px);
            let b = cam.project(Vec3::X * s).unwrap();
            assert!((a.distance(b) - g.size_px).abs() < 1.0);
        }
    }

    #[test]
    fn hit_test_axes_planes_center() {
        let cam = cam_front();
        let g = Gizmo::default();
        let o = cam.project(Vec3::ZERO).unwrap();
        let hit = |dx: f32, dy: f32| g.hit_test(&cam, Vec3::ZERO, Quat::IDENTITY, o + egui::vec2(dx, dy));
        assert_eq!(hit(0.0, 0.0), Some(Handle::Center));
        assert_eq!(hit(60.0, 0.0), Some(Handle::Axis(0)));
        assert_eq!(hit(0.0, -60.0), Some(Handle::Axis(1)));
        // XY plane quad lies between 25%-55% along X and Y
        assert_eq!(hit(36.0, -36.0), Some(Handle::Plane(2)));
        assert_eq!(hit(-80.0, 80.0), None);
    }

    #[test]
    fn translate_axis_drag_follows_cursor() {
        let cam = cam_front();
        let mut g = Gizmo::default();
        let o = cam.project(Vec3::ZERO).unwrap();
        let start = o + egui::vec2(40.0, 0.0);
        assert!(g.begin_drag(Handle::Axis(0), &cam, Vec3::ZERO, Quat::IDENTITY, start));
        // cursor moves to where world (2,0,0) projects, plus the grab offset
        let target = cam.project(Vec3::new(2.0, 0.0, 0.0)).unwrap() + (start - o);
        let Some(Delta::Translate(d)) = g.drag_delta(&cam, target) else { panic!() };
        assert!(d.y.abs() < 1e-4 && d.z.abs() < 1e-4);
        assert!(d.x > 1.0);
        g.end_drag();
        assert!(!g.is_dragging());
    }

    #[test]
    fn rotate_drag_quarter_turn() {
        let cam = cam_front();
        let mut g = Gizmo { mode: GizmoMode::Rotate, ..Default::default() };
        let o = cam.project(Vec3::ZERO).unwrap();
        assert!(g.begin_drag(Handle::Axis(2), &cam, Vec3::ZERO, Quat::IDENTITY, o + egui::vec2(50.0, 0.0)));
        let Some(Delta::Rotate { axis, angle }) = g.drag_delta(&cam, o + egui::vec2(0.0, -50.0)) else { panic!() };
        assert_eq!(axis, Vec3::Z);
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 0.02, "{angle}");
    }

    #[test]
    fn uniform_scale_from_center_drag() {
        let cam = cam_front();
        let mut g = Gizmo { mode: GizmoMode::Scale, ..Default::default() };
        let o = cam.project(Vec3::ZERO).unwrap();
        assert!(g.begin_drag(Handle::Center, &cam, Vec3::ZERO, Quat::IDENTITY, o + egui::vec2(40.0, 0.0)));
        let Some(Delta::Scale(f)) = g.drag_delta(&cam, o + egui::vec2(80.0, 0.0)) else { panic!() };
        assert!((f - Vec3::splat(2.0)).length() < 1e-4);
    }

    #[test]
    fn hover_only_updates_when_not_dragging() {
        let cam = cam_front();
        let mut g = Gizmo::default();
        let o = cam.project(Vec3::ZERO).unwrap();
        g.update_hover(&cam, Vec3::ZERO, Quat::IDENTITY, Some(o));
        assert_eq!(g.hover, Some(Handle::Center));
        g.begin_drag(Handle::Axis(0), &cam, Vec3::ZERO, Quat::IDENTITY, o + egui::vec2(40.0, 0.0));
        g.update_hover(&cam, Vec3::ZERO, Quat::IDENTITY, None);
        assert_eq!(g.hover, Some(Handle::Center));
    }

    #[test]
    fn draw_smoke_all_modes() {
        let ctx = egui::Context::default();
        let cam = cam_front();
        let mut ran = false;
        let _ = ctx.run(Default::default(), |ctx| {
            let p = ctx.layer_painter(egui::LayerId::background());
            for mode in [GizmoMode::Translate, GizmoMode::Rotate, GizmoMode::Scale] {
                Gizmo { mode, hover: Some(Handle::Axis(1)), ..Default::default() }.draw(&p, &cam, Vec3::ZERO, Quat::IDENTITY);
            }
            ran = true;
        });
        assert!(ran);
    }
}
