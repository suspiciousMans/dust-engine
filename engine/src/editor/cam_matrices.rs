//! Camera input for viewport interaction code: view/projection matrices plus
//! the viewport rectangle in egui points. Deliberately independent of the
//! engine camera and of the shell's viewport types.

use egui::{Pos2, Rect};
use glam::{Mat4, Vec3, Vec4};

/// A world-space ray. `dir` is normalised.
#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    pub fn new(origin: Vec3, dir: Vec3) -> Self {
        Self { origin, dir: dir.normalize_or_zero() }
    }
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.dir * t
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CameraMatrices {
    pub view: Mat4,
    pub proj: Mat4,
    /// Viewport rectangle in screen points (y down).
    pub rect: Rect,
}

impl CameraMatrices {
    pub fn new(view: Mat4, proj: Mat4, rect: Rect) -> Self {
        Self { view, proj, rect }
    }

    pub fn view_proj(&self) -> Mat4 {
        self.proj * self.view
    }
    pub fn eye(&self) -> Vec3 {
        self.view.inverse().w_axis.truncate()
    }
    /// Direction the camera looks in (world space).
    pub fn forward(&self) -> Vec3 {
        -self.view.inverse().z_axis.truncate().normalize_or_zero()
    }
    pub fn right(&self) -> Vec3 {
        self.view.inverse().x_axis.truncate().normalize_or_zero()
    }
    pub fn up(&self) -> Vec3 {
        self.view.inverse().y_axis.truncate().normalize_or_zero()
    }
    pub fn is_ortho(&self) -> bool {
        self.proj.w_axis.w == 1.0 && self.proj.z_axis.w == 0.0
    }

    /// Unit vector from `p` toward the viewer (works for ortho too).
    pub fn to_viewer(&self, p: Vec3) -> Vec3 {
        if self.is_ortho() {
            -self.forward()
        } else {
            (self.eye() - p).normalize_or_zero()
        }
    }

    /// Screen position of a world point; `None` when behind the camera.
    pub fn project(&self, p: Vec3) -> Option<Pos2> {
        let c = self.view_proj() * p.extend(1.0);
        if c.w <= 1e-6 {
            return None;
        }
        let ndc = c.truncate() / c.w;
        Some(Pos2::new(
            self.rect.left() + (ndc.x * 0.5 + 0.5) * self.rect.width(),
            self.rect.top() + (0.5 - ndc.y * 0.5) * self.rect.height(),
        ))
    }

    /// Clip-space `w` (view depth for perspective, 1 for ortho).
    pub fn clip_w(&self, p: Vec3) -> f32 {
        (self.view_proj() * p.extend(1.0)).w
    }

    /// World-space ray through a screen position. Independent of the depth
    /// range convention (GL -1..1 or 0..1): only NDC z = 1 (far) is used.
    pub fn ray(&self, cursor: Pos2) -> Ray {
        let nx = (cursor.x - self.rect.left()) / self.rect.width().max(1.0) * 2.0 - 1.0;
        let ny = 1.0 - (cursor.y - self.rect.top()) / self.rect.height().max(1.0) * 2.0;
        let inv = self.view_proj().inverse();
        let h: Vec4 = inv * Vec4::new(nx, ny, 1.0, 1.0);
        let far = h.truncate() / h.w;
        if self.is_ortho() {
            let dir = self.forward();
            let eye = self.eye();
            let origin = far - dir * (far - eye).dot(dir);
            Ray::new(origin, dir)
        } else {
            Ray::new(self.eye(), far - self.eye())
        }
    }

    /// World size of one screen point at `p` (for constant-size handles).
    pub fn world_per_point(&self, p: Vec3) -> f32 {
        let w = self.clip_w(p).max(1e-4);
        w * 2.0 / (self.proj.y_axis.y.abs().max(1e-6) * self.rect.height().max(1.0))
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    /// 800x600 viewport at (100, 50), camera at `eye` looking at the origin.
    pub fn cam_at(eye: Vec3) -> CameraMatrices {
        let rect = Rect::from_min_size(Pos2::new(100.0, 50.0), egui::vec2(800.0, 600.0));
        let view = Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
        let proj = Mat4::perspective_rh_gl(60f32.to_radians(), 800.0 / 600.0, 0.1, 500.0);
        CameraMatrices::new(view, proj, rect)
    }
    pub fn cam_front() -> CameraMatrices {
        cam_at(Vec3::new(0.0, 0.0, 10.0))
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn project_and_ray_round_trip() {
        let cam = cam_at(Vec3::new(3.0, 4.0, 9.0));
        let p = Vec3::new(1.0, 0.5, -2.0);
        let s = cam.project(p).unwrap();
        let r = cam.ray(s);
        // The ray must pass through p.
        let t = (p - r.origin).dot(r.dir);
        assert!((r.at(t) - p).length() < 1e-3);
    }

    #[test]
    fn screen_centre_projects_origin() {
        let cam = cam_front();
        let s = cam.project(Vec3::ZERO).unwrap();
        assert!((s - cam.rect.center()).length() < 1e-3);
    }

    #[test]
    fn behind_camera_is_none() {
        assert!(cam_front().project(Vec3::new(0.0, 0.0, 20.0)).is_none());
    }

    #[test]
    fn world_per_point_matches_projection() {
        let cam = cam_front();
        let a = cam.project(Vec3::ZERO).unwrap();
        let b = cam.project(Vec3::X).unwrap();
        let wpp = cam.world_per_point(Vec3::ZERO);
        assert!((wpp * (b - a).length() - 1.0).abs() < 0.01);
    }

    #[test]
    fn ortho_ray_is_parallel() {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(200.0, 100.0));
        let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 10.0), Vec3::ZERO, Vec3::Y);
        let proj = Mat4::orthographic_rh_gl(-2.0, 2.0, -1.0, 1.0, 0.1, 50.0);
        let cam = CameraMatrices::new(view, proj, rect);
        assert!(cam.is_ortho());
        let r = cam.ray(Pos2::new(150.0, 50.0));
        assert!((r.dir - Vec3::NEG_Z).length() < 1e-4);
        assert!((r.origin.x - 1.0).abs() < 1e-3 && r.origin.y.abs() < 1e-3);
    }
}
