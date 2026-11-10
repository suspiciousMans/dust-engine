//! Editor viewport camera: pure math, no GL/SDL/egui-context.
//!
//! One representation serves both navigation styles: a pivot (`target`), a
//! `distance` from it and yaw/pitch. Orbiting rotates around the pivot;
//! fly-look rotates *in place* (the eye stays, the pivot is re-derived), so
//! switching between the two never makes the view jump.
//!
//! Conventions match `engine::camera::OrbitCamera` (yaw 0 = eye on +Z looking
//! toward -Z, Y up), so scenes look the same through either camera.
//!
//! Navigation is driven by a per-frame [`CameraInput`] snapshot and the
//! editor's [`KeymapPreset`]:
//! * **Blender-like**: MMB orbit, Shift+MMB pan, Ctrl+MMB dolly, wheel zoom,
//!   numpad 1/3/7 (+Ctrl for the opposite side) views, numpad 5 ortho,
//!   `.` frame selected.
//! * **Unity-like**: hold RMB to fly (WASD, Q/E down/up, Shift = fast, wheel =
//!   fly speed), Alt+LMB orbit, Alt+RMB dolly, MMB pan, wheel zoom, `F` frame.

use std::f32::consts::{FRAC_PI_2, PI};

use egui::{Key, Modifiers};
use glam::{Mat4, Vec2, Vec3, Vec4};

use super::state::KeymapPreset;

/// ~89.94 degrees. The view matrix is built from an explicit basis, so unlike
/// `look_at` it stays well-defined right up to the poles (Top/Bottom views).
pub const PITCH_LIMIT: f32 = FRAC_PI_2 - 0.001;

/// Canonical axis-aligned views (Blender's numpad views).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewPreset {
    Front,
    Back,
    Right,
    Left,
    Top,
    Bottom,
}

impl ViewPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Front => "Front",
            Self::Back => "Back",
            Self::Right => "Right",
            Self::Left => "Left",
            Self::Top => "Top",
            Self::Bottom => "Bottom",
        }
    }

    /// (yaw, pitch) of the eye relative to the pivot.
    pub fn angles(self) -> (f32, f32) {
        match self {
            Self::Front => (0.0, 0.0),
            Self::Back => (PI, 0.0),
            Self::Right => (FRAC_PI_2, 0.0),
            Self::Left => (-FRAC_PI_2, 0.0),
            Self::Top => (0.0, PITCH_LIMIT),
            Self::Bottom => (0.0, -PITCH_LIMIT),
        }
    }
}

/// Everything the camera needs from the input layer for one frame.
/// Built by the viewport widget; plain data so tests can drive it.
#[derive(Clone, Copy, Debug, Default)]
pub struct CameraInput {
    /// Pointer movement since last frame, in pixels (+x right, +y down).
    pub pointer_delta: Vec2,
    /// Wheel notches this frame (+ = scroll up / away from the user).
    pub scroll: f32,
    pub lmb: bool,
    pub mmb: bool,
    pub rmb: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Fly movement request: x = right, y = up (world), z = forward; each in -1..=1.
    pub move_axes: Vec3,
    pub dt: f32,
    /// Viewport height in pixels (pan speed is expressed per pixel).
    pub viewport_height: f32,
}

/// One-shot camera commands bound to keys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CameraKeyAction {
    SnapView(ViewPreset),
    ToggleOrtho,
    FrameSelected,
}

/// Key -> camera command for `preset`. Only called while the viewport is hovered.
///
/// Egui reports numpad digits as the digit keys, so Blender's numpad 1/3/7
/// are also reachable from the number row (harmless: the number row has no
/// other editor meaning).
pub fn camera_key(preset: KeymapPreset, key: Key, m: Modifiers) -> Option<CameraKeyAction> {
    if m.alt {
        return None;
    }
    match preset {
        KeymapPreset::Blender => {
            let opposite = m.ctrl || m.command;
            match key {
                Key::Num1 => Some(CameraKeyAction::SnapView(if opposite { ViewPreset::Back } else { ViewPreset::Front })),
                Key::Num3 => Some(CameraKeyAction::SnapView(if opposite { ViewPreset::Left } else { ViewPreset::Right })),
                Key::Num7 => Some(CameraKeyAction::SnapView(if opposite { ViewPreset::Bottom } else { ViewPreset::Top })),
                Key::Num5 => Some(CameraKeyAction::ToggleOrtho),
                Key::Period | Key::F if !m.ctrl && !m.shift => Some(CameraKeyAction::FrameSelected),
                _ => None,
            }
        }
        KeymapPreset::Unity => match key {
            Key::F if !m.ctrl && !m.shift => Some(CameraKeyAction::FrameSelected),
            Key::Period if !m.ctrl && !m.shift => Some(CameraKeyAction::FrameSelected),
            _ => None,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditorCamera {
    pub target: Vec3,
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub fov_y: f32,
    pub near: f32,
    pub far: f32,
    /// Orthographic projection (height derived from `distance`, like Blender).
    pub ortho: bool,
    /// Fly speed in world units per second (wheel-adjustable in fly mode).
    pub fly_speed: f32,
    pub min_distance: f32,
    pub max_distance: f32,
}

impl Default for EditorCamera {
    fn default() -> Self {
        Self::new(Vec3::ZERO, 10.0)
    }
}

impl EditorCamera {
    pub const MOUSE_SENSITIVITY: f32 = 0.005;
    pub const FAST_MULTIPLIER: f32 = 4.0;

    pub fn new(target: Vec3, distance: f32) -> Self {
        Self {
            target,
            distance,
            yaw: 0.6,
            pitch: 0.4,
            fov_y: 45f32.to_radians(),
            near: 0.1,
            far: 2000.0,
            ortho: false,
            fly_speed: 8.0,
            min_distance: 0.05,
            max_distance: 5000.0,
        }
    }

    /// Adopts the pose of the engine's simple orbit camera.
    pub fn from_orbit(o: &crate::camera::OrbitCamera) -> Self {
        let mut c = Self::new(o.target, o.distance);
        c.yaw = o.yaw;
        c.pitch = o.pitch;
        c.fov_y = o.fov_y_radians;
        c.near = o.near;
        c.far = o.far.max(c.far);
        c
    }

    /// Unit vector from the pivot toward the eye.
    fn eye_dir(&self) -> Vec3 {
        Vec3::new(self.pitch.cos() * self.yaw.sin(), self.pitch.sin(), self.pitch.cos() * self.yaw.cos())
    }

    pub fn position(&self) -> Vec3 {
        self.target + self.eye_dir() * self.distance
    }

    pub fn forward(&self) -> Vec3 {
        -self.eye_dir()
    }

    /// Horizontal right vector; well-defined even looking straight down.
    pub fn right(&self) -> Vec3 {
        Vec3::new(self.yaw.cos(), 0.0, -self.yaw.sin())
    }

    pub fn up(&self) -> Vec3 {
        self.right().cross(self.forward()).normalize()
    }

    pub fn view_matrix(&self) -> Mat4 {
        Mat4::look_to_rh(self.position(), self.forward(), self.up())
    }

    pub fn projection_matrix(&self, aspect: f32) -> Mat4 {
        let aspect = aspect.max(1e-4);
        if self.ortho {
            let half_h = self.distance * (self.fov_y * 0.5).tan();
            let half_w = half_h * aspect;
            // Symmetric depth range: geometry between eye and pivot must stay visible.
            Mat4::orthographic_rh_gl(-half_w, half_w, -half_h, half_h, -self.far, self.far)
        } else {
            Mat4::perspective_rh_gl(self.fov_y, aspect, self.near, self.far)
        }
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        self.projection_matrix(aspect) * self.view_matrix()
    }

    /// World units covered by one viewport pixel at the pivot's depth.
    pub fn world_per_pixel(&self, viewport_height: f32) -> f32 {
        2.0 * self.distance * (self.fov_y * 0.5).tan() / viewport_height.max(1.0)
    }

    // ---- navigation primitives ------------------------------------------

    /// Turntable orbit around the pivot. +dx_px drags the scene to the right.
    pub fn orbit_pixels(&mut self, dx_px: f32, dy_px: f32) {
        self.yaw -= dx_px * Self::MOUSE_SENSITIVITY;
        self.pitch = (self.pitch + dy_px * Self::MOUSE_SENSITIVITY).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Slides pivot and eye together so the scene follows the cursor.
    pub fn pan_pixels(&mut self, dx_px: f32, dy_px: f32, viewport_height: f32) {
        let k = self.world_per_pixel(viewport_height);
        self.target += (-self.right() * dx_px + self.up() * dy_px) * k;
    }

    /// Multiplicative zoom: each notch is 10 % closer (negative = farther).
    pub fn zoom_notches(&mut self, notches: f32) {
        self.distance = (self.distance * 0.9f32.powf(notches)).clamp(self.min_distance, self.max_distance);
    }

    /// Rotates in place: the eye stays put and the pivot is re-derived.
    pub fn fly_look_pixels(&mut self, dx_px: f32, dy_px: f32) {
        let eye = self.position();
        self.yaw -= dx_px * Self::MOUSE_SENSITIVITY;
        self.pitch = (self.pitch + dy_px * Self::MOUSE_SENSITIVITY).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        self.target = eye - self.eye_dir() * self.distance;
    }

    /// `axes`: x = right, y = world up, z = forward (along the view direction).
    pub fn fly_move(&mut self, axes: Vec3, dt: f32, fast: bool) {
        if axes == Vec3::ZERO {
            return;
        }
        let local = self.right() * axes.x + self.forward() * axes.z + Vec3::Y * axes.y;
        let speed = self.fly_speed * if fast { Self::FAST_MULTIPLIER } else { 1.0 };
        self.target += local.normalize_or_zero() * speed * dt;
    }

    /// Wheel while flying: each notch scales speed by 20 %.
    pub fn adjust_fly_speed(&mut self, notches: f32) {
        self.fly_speed = (self.fly_speed * 1.2f32.powf(notches)).clamp(0.05, 1000.0);
    }

    pub fn snap_to(&mut self, view: ViewPreset) {
        let (yaw, pitch) = view.angles();
        self.yaw = yaw;
        self.pitch = pitch;
    }

    /// Centers a sphere in view.
    pub fn frame_sphere(&mut self, center: Vec3, radius: f32) {
        let radius = radius.max(0.05);
        self.target = center;
        // Fit the sphere in the narrower field of view with a little margin.
        self.distance = (radius / (self.fov_y * 0.5).sin() * 1.15).clamp(self.min_distance, self.max_distance);
    }

    /// Frames an axis-aligned box (e.g. the selection's bounds).
    pub fn frame_bounds(&mut self, min: Vec3, max: Vec3) {
        self.frame_sphere((min + max) * 0.5, (max - min).length() * 0.5);
    }

    // ---- input ----------------------------------------------------------

    /// Applies one frame of pointer/keyboard input for `preset`.
    /// Returns `true` when the camera moved.
    pub fn apply_input(&mut self, preset: KeymapPreset, i: &CameraInput) -> bool {
        let before = *self;
        let d = i.pointer_delta;
        match preset {
            KeymapPreset::Blender => {
                if i.mmb {
                    if i.shift {
                        self.pan_pixels(d.x, d.y, i.viewport_height);
                    } else if i.ctrl {
                        self.zoom_notches(-d.y * 0.05);
                    } else {
                        self.orbit_pixels(d.x, d.y);
                    }
                }
                if i.scroll != 0.0 {
                    self.zoom_notches(i.scroll);
                }
            }
            KeymapPreset::Unity => {
                if i.rmb && !i.alt {
                    self.fly_look_pixels(d.x, d.y);
                    self.fly_move(i.move_axes, i.dt, i.shift);
                    if i.scroll != 0.0 {
                        self.adjust_fly_speed(i.scroll);
                    }
                } else {
                    if i.alt && i.lmb {
                        self.orbit_pixels(d.x, d.y);
                    } else if i.alt && i.rmb {
                        self.zoom_notches(-d.y * 0.05);
                    } else if i.mmb {
                        self.pan_pixels(d.x, d.y, i.viewport_height);
                    }
                    if i.scroll != 0.0 {
                        self.zoom_notches(i.scroll);
                    }
                }
            }
        }
        *self != before
    }

    pub fn apply_key_action(&mut self, action: CameraKeyAction, selection_bounds: Option<(Vec3, Vec3)>) {
        match action {
            CameraKeyAction::SnapView(v) => self.snap_to(v),
            CameraKeyAction::ToggleOrtho => self.ortho = !self.ortho,
            CameraKeyAction::FrameSelected => {
                if let Some((min, max)) = selection_bounds {
                    self.frame_bounds(min, max);
                }
            }
        }
    }
}

/// A world-space ray, from the near plane through a viewport pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewRay {
    pub origin: Vec3,
    /// Unit length.
    pub dir: Vec3,
}

/// Turns a cursor position (egui points) over the viewport image `rect` into a
/// world ray. `view_proj_inv` is `(proj * view).inverse()` of the camera that
/// rendered the image. Works for perspective and orthographic projections.
pub fn viewport_ray(cursor: egui::Pos2, rect: egui::Rect, view_proj_inv: Mat4) -> ViewRay {
    let w = rect.width().max(1.0);
    let h = rect.height().max(1.0);
    let ndc_x = (cursor.x - rect.min.x) / w * 2.0 - 1.0;
    let ndc_y = 1.0 - (cursor.y - rect.min.y) / h * 2.0;
    let unproject = |z: f32| {
        let p = view_proj_inv * Vec4::new(ndc_x, ndc_y, z, 1.0);
        p.truncate() / p.w
    };
    let near = unproject(-1.0);
    let far = unproject(1.0);
    ViewRay { origin: near, dir: (far - near).normalize_or_zero() }
}

/// Intersection of `ray` with the horizontal plane `y = height`, if in front.
pub fn ray_plane_y(ray: &ViewRay, height: f32) -> Option<Vec3> {
    if ray.dir.y.abs() < 1e-6 {
        return None;
    }
    let t = (height - ray.origin.y) / ray.dir.y;
    (t >= 0.0).then(|| ray.origin + ray.dir * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-3
    }

    #[test]
    fn matches_orbit_camera_convention() {
        let o = crate::camera::OrbitCamera::new(Vec3::new(1.0, 2.0, 3.0), 7.0);
        let c = EditorCamera::from_orbit(&o);
        assert!(approx(c.position(), o.position()));
        assert!(approx(c.forward(), o.forward()));
        assert!(approx(c.right(), o.right()));
        let (a, b) = (c.view_matrix(), o.view_matrix());
        assert!(a.abs_diff_eq(b, 1e-4), "{a:?} vs {b:?}");
    }

    #[test]
    fn orbit_keeps_distance_and_pivot() {
        let mut c = EditorCamera::new(Vec3::new(1.0, 0.0, 2.0), 10.0);
        c.orbit_pixels(120.0, -40.0);
        assert!(((c.position() - c.target).length() - 10.0).abs() < 1e-4);
        assert_eq!(c.target, Vec3::new(1.0, 0.0, 2.0));
        c.orbit_pixels(0.0, 1e6);
        assert!(c.pitch <= PITCH_LIMIT);
    }

    #[test]
    fn pan_moves_pivot_in_view_plane_proportionally() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        c.snap_to(ViewPreset::Front);
        let k = c.world_per_pixel(600.0);
        c.pan_pixels(100.0, 0.0, 600.0);
        // Dragging right drags the scene right => the camera moves left.
        assert!(approx(c.target, Vec3::new(-100.0 * k, 0.0, 0.0)));
        c.pan_pixels(0.0, 50.0, 600.0);
        assert!((c.target.y - 50.0 * k).abs() < 1e-4, "dragging down moves camera up: {:?}", c.target);
        assert!(c.target.z.abs() < 1e-5);
    }

    #[test]
    fn zoom_is_multiplicative_and_clamped() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        c.zoom_notches(1.0);
        assert!((c.distance - 9.0).abs() < 1e-4);
        c.zoom_notches(-1.0);
        assert!((c.distance - 10.0).abs() < 1e-3);
        c.zoom_notches(10_000.0);
        assert_eq!(c.distance, c.min_distance);
        c.zoom_notches(-10_000.0);
        assert_eq!(c.distance, c.max_distance);
    }

    #[test]
    fn fly_look_keeps_eye_in_place() {
        let mut c = EditorCamera::new(Vec3::new(0.0, 1.0, 0.0), 5.0);
        let eye = c.position();
        c.fly_look_pixels(80.0, 30.0);
        assert!(approx(c.position(), eye));
        assert!(((c.position() - c.target).length() - 5.0).abs() < 1e-4);
    }

    #[test]
    fn fly_look_right_turns_view_right() {
        let mut c = EditorCamera::new(Vec3::ZERO, 5.0);
        c.snap_to(ViewPreset::Front); // looking toward -Z, right = +X
        let r = c.right();
        c.fly_look_pixels(50.0, 0.0);
        assert!(c.forward().dot(r) > 0.1, "forward should swing toward old right");
    }

    #[test]
    fn fly_move_follows_view_and_speed() {
        let mut c = EditorCamera::new(Vec3::ZERO, 5.0);
        c.snap_to(ViewPreset::Front);
        c.fly_speed = 10.0;
        let eye = c.position();
        c.fly_move(Vec3::new(0.0, 0.0, 1.0), 0.5, false);
        assert!(approx(c.position() - eye, Vec3::new(0.0, 0.0, -5.0)));
        let eye = c.position();
        c.fly_move(Vec3::new(1.0, 0.0, 0.0), 0.5, true);
        assert!(approx(c.position() - eye, Vec3::new(20.0, 0.0, 0.0)));
        let eye = c.position();
        c.fly_move(Vec3::new(1.0, 1.0, 0.0), 1.0, false); // diagonal is normalised
        assert!(((c.position() - eye).length() - 10.0).abs() < 1e-3);
    }

    #[test]
    fn fly_speed_wheel() {
        let mut c = EditorCamera::default();
        let s = c.fly_speed;
        c.adjust_fly_speed(2.0);
        assert!((c.fly_speed - s * 1.44).abs() < 1e-3);
        c.adjust_fly_speed(-1000.0);
        assert_eq!(c.fly_speed, 0.05);
    }

    #[test]
    fn numpad_views_point_the_right_way() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        c.snap_to(ViewPreset::Front);
        assert!(approx(c.position(), Vec3::new(0.0, 0.0, 10.0)));
        c.snap_to(ViewPreset::Right);
        assert!(approx(c.position(), Vec3::new(10.0, 0.0, 0.0)));
        c.snap_to(ViewPreset::Left);
        assert!(approx(c.position(), Vec3::new(-10.0, 0.0, 0.0)));
        c.snap_to(ViewPreset::Back);
        assert!(approx(c.position(), Vec3::new(0.0, 0.0, -10.0)));
        c.snap_to(ViewPreset::Top);
        assert!(c.position().y > 9.99 && c.view_matrix().is_finite());
        // Looking straight down, +X is still screen-right and -Z is screen-up.
        let v = c.view_matrix();
        let px = v.transform_point3(Vec3::X);
        let pz = v.transform_point3(-Vec3::Z);
        assert!(px.x > 0.9 && px.y.abs() < 0.05, "{px:?}");
        assert!(pz.y > 0.9 && pz.x.abs() < 0.05, "{pz:?}");
    }

    #[test]
    fn frame_sphere_centres_and_fits() {
        let mut c = EditorCamera::new(Vec3::ZERO, 1.0);
        c.frame_bounds(Vec3::new(8.0, 0.0, 0.0), Vec3::new(12.0, 2.0, 2.0));
        assert!(approx(c.target, Vec3::new(10.0, 1.0, 1.0)));
        let radius = Vec3::new(4.0, 2.0, 2.0).length() * 0.5;
        // The sphere's centre projects to the screen centre and it fits vertically.
        let vp = c.view_proj(1.0);
        let ndc = vp.project_point3(c.target);
        assert!(ndc.x.abs() < 1e-3 && ndc.y.abs() < 1e-3);
        let top = vp.project_point3(c.target + c.up() * radius);
        assert!(top.y < 1.0 && top.y > 0.5, "{top:?}");
    }

    #[test]
    fn blender_mmb_orbit_pan_and_scroll() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        let base = CameraInput { viewport_height: 600.0, pointer_delta: Vec2::new(30.0, 10.0), ..Default::default() };
        // No button held => nothing moves.
        assert!(!c.apply_input(KeymapPreset::Blender, &base));
        let yaw0 = c.yaw;
        assert!(c.apply_input(KeymapPreset::Blender, &CameraInput { mmb: true, ..base }));
        assert!(c.yaw != yaw0 && c.target == Vec3::ZERO);
        let yaw1 = c.yaw;
        assert!(c.apply_input(KeymapPreset::Blender, &CameraInput { mmb: true, shift: true, ..base }));
        assert_eq!(c.yaw, yaw1);
        assert!(c.target != Vec3::ZERO);
        let d = c.distance;
        c.apply_input(KeymapPreset::Blender, &CameraInput { scroll: 1.0, ..Default::default() });
        assert!(c.distance < d);
        // RMB does nothing in the Blender preset (it is select there).
        let before = c;
        c.apply_input(KeymapPreset::Blender, &CameraInput { rmb: true, ..base });
        assert_eq!(c, before);
    }

    #[test]
    fn unity_rmb_flies_and_wheel_changes_speed_not_distance() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        let eye = c.position();
        let dist = c.distance;
        let speed = c.fly_speed;
        let i = CameraInput {
            rmb: true,
            scroll: 1.0,
            move_axes: Vec3::new(0.0, 0.0, 1.0),
            dt: 0.1,
            pointer_delta: Vec2::ZERO,
            viewport_height: 600.0,
            ..Default::default()
        };
        assert!(c.apply_input(KeymapPreset::Unity, &i));
        assert_eq!(c.distance, dist);
        assert!(c.fly_speed > speed);
        assert!(c.position().distance(eye) > 0.1);
        // MMB pans, Alt+LMB orbits, plain wheel zooms.
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        let d = CameraInput { pointer_delta: Vec2::new(20.0, 0.0), viewport_height: 600.0, ..Default::default() };
        c.apply_input(KeymapPreset::Unity, &CameraInput { mmb: true, ..d });
        assert!(c.target != Vec3::ZERO);
        let y = c.yaw;
        c.apply_input(KeymapPreset::Unity, &CameraInput { alt: true, lmb: true, ..d });
        assert!(c.yaw != y);
        c.apply_input(KeymapPreset::Unity, &CameraInput { scroll: 1.0, ..Default::default() });
        assert!(c.distance < 10.0);
    }

    #[test]
    fn key_bindings_per_preset() {
        let none = Modifiers::NONE;
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Num1, none), Some(CameraKeyAction::SnapView(ViewPreset::Front)));
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Num1, ctrl), Some(CameraKeyAction::SnapView(ViewPreset::Back)));
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Num3, none), Some(CameraKeyAction::SnapView(ViewPreset::Right)));
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Num7, none), Some(CameraKeyAction::SnapView(ViewPreset::Top)));
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Num7, ctrl), Some(CameraKeyAction::SnapView(ViewPreset::Bottom)));
        assert_eq!(camera_key(KeymapPreset::Blender, Key::Period, none), Some(CameraKeyAction::FrameSelected));
        assert_eq!(camera_key(KeymapPreset::Unity, Key::F, none), Some(CameraKeyAction::FrameSelected));
        assert_eq!(camera_key(KeymapPreset::Unity, Key::Num1, none), None);
    }

    #[test]
    fn viewport_ray_centre_and_corners_perspective() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        c.snap_to(ViewPreset::Front);
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 200.0));
        let inv = c.view_proj(2.0).inverse();
        let r = viewport_ray(rect.center(), rect, inv);
        assert!(approx(r.dir, Vec3::new(0.0, 0.0, -1.0)), "{:?}", r.dir);
        assert!((r.dir.length() - 1.0).abs() < 1e-5);
        // The centre ray hits the pivot.
        let hit = r.origin + r.dir * (r.origin.distance(Vec3::ZERO));
        assert!(approx(hit, Vec3::ZERO));
        // Right edge ray leans toward +X, top edge toward +Y.
        let right = viewport_ray(egui::pos2(rect.max.x, rect.center().y), rect, inv);
        assert!(right.dir.x > 0.1 && right.dir.y.abs() < 1e-4);
        let top = viewport_ray(egui::pos2(rect.center().x, rect.min.y), rect, inv);
        assert!(top.dir.y > 0.1 && top.dir.x.abs() < 1e-4);
    }

    #[test]
    fn viewport_ray_orthographic_is_parallel() {
        let mut c = EditorCamera::new(Vec3::ZERO, 10.0);
        c.snap_to(ViewPreset::Top);
        c.ortho = true;
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 300.0));
        let inv = c.view_proj(1.0).inverse();
        let a = viewport_ray(egui::pos2(10.0, 10.0), rect, inv);
        let b = viewport_ray(egui::pos2(290.0, 250.0), rect, inv);
        assert!(a.dir.dot(b.dir) > 0.9999);
        assert!(a.dir.y < -0.99);
        assert!(a.origin.distance(b.origin) > 1.0);
    }

    #[test]
    fn ray_hits_ground_under_cursor() {
        let mut c = EditorCamera::new(Vec3::new(3.0, 0.0, -2.0), 12.0);
        c.pitch = 0.9;
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 480.0));
        let inv = c.view_proj(640.0 / 480.0).inverse();
        let ray = viewport_ray(rect.center(), rect, inv);
        let hit = ray_plane_y(&ray, 0.0).expect("hits ground");
        assert!(approx(hit, c.target), "{hit:?} vs {:?}", c.target);
        // Looking up never hits the ground.
        let up = ViewRay { origin: Vec3::new(0.0, 1.0, 0.0), dir: Vec3::Y };
        assert!(ray_plane_y(&up, 0.0).is_none());
    }
}
