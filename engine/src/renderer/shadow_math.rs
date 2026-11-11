//! Pure math for directional-light shadow maps — no GL, fully unit-tested.
//!
//! Conventions: `light_dir` everywhere is the direction *toward* the light
//! (same as `RenderParams::light_dir` / `uLightDir`). All matrices are glam
//! column-major, right-handed, GL clip space (`z` in `[-1, 1]`).

use glam::{Mat4, Vec3, Vec4};
use serde::{Deserialize, Serialize};

/// How the shadow edge is filtered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ShadowFilter {
    /// Percentage-closer filtering: soft, smooth edge (`softness` texels).
    #[default]
    Pcf,
    /// One depth tap, no filtering: crisp stair-stepped edge.
    Hard,
    /// PCF result thresholded against a 4x4 Bayer matrix in screen space:
    /// a stippled, dithered edge (the PS2 look). Combine with a low `resolution`.
    Dithered,
}

impl ShadowFilter {
    /// Value of the `uShadowDither` uniform.
    pub fn dither_flag(self) -> i32 {
        matches!(self, ShadowFilter::Dithered) as i32
    }
}

/// Opt-in shadow configuration. `enabled` is `false` by default: a game that
/// never turns it on renders exactly as before.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowSettings {
    pub enabled: bool,
    /// Shadow map side in texels (clamped to 64..=4096 when used).
    pub resolution: u32,
    /// Constant depth bias in **shadow-map texels** (scale-independent).
    pub bias: f32,
    /// Normal-offset in **texels**: the receiver is pushed along its normal
    /// by this much (scaled by sin of the light angle) before the lookup.
    pub normal_bias: f32,
    /// 0 = shadows invisible, 1 = fully black umbra.
    pub strength: f32,
    /// PCF radius in texels (0 = single tap). Capped at 2.
    pub softness: f32,
    pub filter: ShadowFilter,
    /// When fitting to the camera frustum: only this far from the eye is covered.
    pub max_distance: f32,
    /// When fitting to the camera frustum: how far toward the light the map
    /// reaches so off-screen casters still shadow visible receivers.
    pub caster_extend: f32,
    /// Quantise the texel size and snap the map to the texel grid so shadow
    /// edges do not shimmer while the camera moves.
    pub texel_snap: bool,
}

impl Default for ShadowSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            resolution: 1024,
            bias: 0.75,
            normal_bias: 1.0,
            strength: 0.55,
            softness: 1.0,
            filter: ShadowFilter::Pcf,
            max_distance: 40.0,
            caster_extend: 30.0,
            texel_snap: true,
        }
    }
}

pub const MIN_RESOLUTION: u32 = 64;
pub const MAX_RESOLUTION: u32 = 4096;
const MAX_PCF_RADIUS: i32 = 2;
/// Upper bound on `tan(theta)` used by the slope-scaled bias.
pub const MAX_SLOPE_TAN: f32 = 8.0;

impl ShadowSettings {
    /// Smooth modern-ish preset: 2048 map, PCF.
    pub fn smooth() -> Self {
        Self { enabled: true, resolution: 2048, softness: 1.5, ..Self::default() }
    }

    /// PS2-flavoured preset: low-res map, dithered hard edge.
    pub fn ps2() -> Self {
        Self {
            enabled: true,
            resolution: 256,
            filter: ShadowFilter::Dithered,
            softness: 1.0,
            strength: 0.6,
            ..Self::default()
        }
    }

    pub fn clamped_resolution(&self) -> u32 {
        self.resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION)
    }

    /// `(radius, step)`: the PCF kernel is `(2*radius+1)^2` taps spaced `step`
    /// texels apart. `Hard` always yields `(0, 0)`.
    pub fn pcf_kernel(&self) -> (i32, f32) {
        if self.filter == ShadowFilter::Hard || self.softness <= 0.0 {
            return (0, 0.0);
        }
        let s = self.softness.min(MAX_PCF_RADIUS as f32);
        let radius = (s.ceil() as i32).clamp(1, MAX_PCF_RADIUS);
        (radius, s / radius as f32)
    }
}

/// Result of fitting a directional light to some bounds.
#[derive(Clone, Copy, Debug)]
pub struct LightFit {
    pub view: Mat4,
    pub proj: Mat4,
    pub view_proj: Mat4,
    /// World-space size of one shadow texel.
    pub texel_world: f32,
    /// Distance between the near and far planes (world units).
    pub depth_range: f32,
    pub half_extent: f32,
}

impl LightFit {
    /// World -> shadow texture coords (`xy` and depth all in `[0, 1]`).
    pub fn texture_matrix(&self) -> Mat4 {
        Mat4::from_translation(Vec3::splat(0.5)) * Mat4::from_scale(Vec3::splat(0.5)) * self.view_proj
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FitOptions {
    pub resolution: u32,
    /// Quantise texel size + snap the centre to the texel grid.
    pub snap: bool,
    /// Size the square from the bounding *sphere* of the points so that the
    /// size does not change when the camera rotates (use for frustums).
    pub stable_sphere: bool,
    /// Extra reach toward the light (world units) for casters outside the points.
    pub caster_extend: f32,
}

impl FitOptions {
    pub fn new(resolution: u32) -> Self {
        Self { resolution, snap: true, stable_sphere: false, caster_extend: 0.0 }
    }
}

/// Rotation-only light view (origin at 0) looking along `-light_dir`.
pub fn light_basis(light_dir: Vec3) -> Mat4 {
    let to_light = light_dir.try_normalize().unwrap_or(Vec3::Y);
    let fwd = -to_light;
    let up = if fwd.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
    Mat4::look_to_rh(Vec3::ZERO, fwd, up)
}

/// Rounds a texel size up to the next `2^(k/4)`: the map's scale only
/// changes in coarse steps, and in between it is a pure translation that
/// [`fit_directional`] snaps to whole texels.
pub fn quantize_texel_size(t: f32) -> f32 {
    let t = t.max(1e-6);
    (((t.log2() * 4.0) - 1e-4).ceil() / 4.0).exp2()
}

/// Fits an orthographic light frustum around `points` (world space).
/// Returns `None` for an empty point set or a zero resolution.
pub fn fit_directional(light_dir: Vec3, points: &[Vec3], opts: &FitOptions) -> Option<LightFit> {
    if points.is_empty() || opts.resolution == 0 {
        return None;
    }
    let rot = light_basis(light_dir);
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for p in points {
        let q = rot.transform_point3(*p);
        lo = lo.min(q);
        hi = hi.max(q);
    }
    let res = opts.resolution as f32;
    let mut center = (lo + hi) * 0.5;
    let mut size = (hi.x - lo.x).max(hi.y - lo.y);
    if opts.stable_sphere {
        let r = points
            .iter()
            .map(|p| (rot.transform_point3(*p) - center).length())
            .fold(0.0_f32, f32::max);
        size = size.max(2.0 * r);
    }
    size = size.max(1e-3);
    let mut texel = size / res;
    let half;
    if opts.snap {
        texel = quantize_texel_size(texel);
        half = texel * res * 0.5;
        center.x = (center.x / texel).round() * texel;
        center.y = (center.y / texel).round() * texel;
    } else {
        half = size * 0.5;
    }
    // View space looks down -z: the nearest point to the light has max z.
    let near = -hi.z - opts.caster_extend.max(0.0);
    let far = (-lo.z).max(near + 1e-2);
    let proj = Mat4::orthographic_rh_gl(
        center.x - half,
        center.x + half,
        center.y - half,
        center.y + half,
        near,
        far,
    );
    Some(LightFit {
        view: rot,
        proj,
        view_proj: proj * rot,
        texel_world: texel,
        depth_range: far - near,
        half_extent: half,
    })
}

/// The 8 corners of an AABB.
pub fn aabb_corners(min: Vec3, max: Vec3) -> [Vec3; 8] {
    let mut out = [Vec3::ZERO; 8];
    for (i, c) in out.iter_mut().enumerate() {
        *c = Vec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
    }
    out
}

/// Orthographic projection tightly bounding `min..max` as seen from the
/// light (no snapping). Returns `(view, proj)`.
pub fn ortho_from_aabb(light_dir: Vec3, min: Vec3, max: Vec3) -> (Mat4, Mat4) {
    let rot = light_basis(light_dir);
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for c in aabb_corners(min, max) {
        let q = rot.transform_point3(c);
        lo = lo.min(q);
        hi = hi.max(q);
    }
    (rot, Mat4::orthographic_rh_gl(lo.x, hi.x, lo.y, hi.y, -hi.z, (-lo.z).max(-hi.z + 1e-2)))
}

/// World-space corners of the camera frustum, with the far plane pulled in
/// to `max_distance` (view-space depth) when that is closer. Order: 4 near
/// then 4 far corners.
pub fn frustum_corners(view: Mat4, proj: Mat4, max_distance: f32) -> [Vec3; 8] {
    let inv = (proj * view).inverse();
    let mut near = [Vec3::ZERO; 4];
    let mut far = [Vec3::ZERO; 4];
    let xy = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    for (i, (x, y)) in xy.iter().enumerate() {
        near[i] = inv.project_point3(Vec3::new(*x, *y, -1.0));
        far[i] = inv.project_point3(Vec3::new(*x, *y, 1.0));
    }
    let depth = |p: Vec3| -view.transform_point3(p).z;
    let (n, f) = (depth(near[0]), depth(far[0]));
    let t = if f > n { ((max_distance - n) / (f - n)).clamp(0.0, 1.0) } else { 1.0 };
    let mut out = [Vec3::ZERO; 8];
    for i in 0..4 {
        out[i] = near[i];
        out[4 + i] = near[i].lerp(far[i], t);
    }
    out
}

/// What the shadow map should cover.
#[derive(Clone, Copy, Debug)]
pub enum ShadowBounds {
    /// A fixed world box containing both casters and receivers.
    Aabb { min: Vec3, max: Vec3 },
    /// A bounding sphere containing both casters and receivers.
    Sphere { center: Vec3, radius: f32 },
    /// The visible part of a camera frustum (rotation-stable, snapped).
    Frustum { view: Mat4, proj: Mat4, max_distance: f32, caster_extend: f32 },
}

impl ShadowBounds {
    /// Frustum bounds using `settings.max_distance` / `settings.caster_extend`.
    pub fn camera(view: Mat4, proj: Mat4, settings: &ShadowSettings) -> Self {
        ShadowBounds::Frustum {
            view,
            proj,
            max_distance: settings.max_distance,
            caster_extend: settings.caster_extend,
        }
    }

    /// Fits the light for these bounds.
    pub fn fit(&self, light_dir: Vec3, settings: &ShadowSettings) -> Option<LightFit> {
        let mut opts = FitOptions::new(settings.clamped_resolution());
        opts.snap = settings.texel_snap;
        match *self {
            ShadowBounds::Aabb { min, max } => {
                fit_directional(light_dir, &aabb_corners(min, max), &opts)
            }
            ShadowBounds::Sphere { center, radius } => {
                // Exact extreme points of the sphere in the light frame.
                let inv = light_basis(light_dir).inverse();
                let axes = [Vec3::X, Vec3::Y, Vec3::Z];
                let pts: Vec<Vec3> = axes
                    .iter()
                    .flat_map(|a| {
                        let d = inv.transform_vector3(*a) * radius;
                        [center + d, center - d]
                    })
                    .collect();
                opts.stable_sphere = true;
                fit_directional(light_dir, &pts, &opts)
            }
            ShadowBounds::Frustum { view, proj, max_distance, caster_extend } => {
                opts.stable_sphere = true;
                opts.caster_extend = caster_extend;
                fit_directional(light_dir, &frustum_corners(view, proj, max_distance), &opts)
            }
        }
    }
}

/// Constant bias converted from texels to normalised depth units.
pub fn const_bias_depth(bias_texels: f32, texel_world: f32, depth_range: f32) -> f32 {
    bias_texels.max(0.0) * texel_world / depth_range.max(1e-6)
}

/// Slope term in depth units per unit `tan(theta)` (one texel's worth).
pub fn slope_bias_depth(texel_world: f32, depth_range: f32) -> f32 {
    texel_world / depth_range.max(1e-6)
}

/// Total depth bias for a surface with the given `n . l` — mirrors the
/// formula in the injected fragment shader (`jame_shadow_lit`).
pub fn slope_scaled_bias(const_bias: f32, slope_bias: f32, n_dot_l: f32) -> f32 {
    let n = n_dot_l.clamp(0.0, 1.0);
    let tan = ((1.0 - n * n).max(0.0).sqrt() / n.max(0.05)).min(MAX_SLOPE_TAN);
    const_bias + slope_bias * tan
}

/// World-space normal offset for a receiver: grows with the grazing angle
/// (`sin theta`), zero when facing the light head-on. Mirrors the vertex shader.
pub fn normal_offset_world(normal_bias_texels: f32, texel_world: f32, n_dot_l: f32) -> f32 {
    let n = n_dot_l.clamp(0.0, 1.0);
    normal_bias_texels.max(0.0) * texel_world * (1.0 - n * n).max(0.0).sqrt()
}

/// Depth-compare lookup the GLSL performs: `true` = lit.
pub fn is_lit(receiver_depth: f32, stored_depth: f32, bias: f32) -> bool {
    receiver_depth - bias <= stored_depth
}

/// Projects a world point through a fit into shadow texture space.
pub fn project_to_shadow(fit: &LightFit, world: Vec3) -> Vec3 {
    let p = fit.texture_matrix() * Vec4::new(world.x, world.y, world.z, 1.0);
    Vec3::new(p.x, p.y, p.z) / p.w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    fn sun() -> Vec3 {
        Vec3::new(0.5, 0.7, 0.5).normalize()
    }

    #[test]
    fn defaults_are_off_and_serde_roundtrips() {
        let s = ShadowSettings::default();
        assert!(!s.enabled);
        let text = ron::to_string(&s).unwrap();
        let back: ShadowSettings = ron::from_str(&text).unwrap();
        assert_eq!(s, back);
        // Missing fields fall back to defaults.
        let partial: ShadowSettings = ron::from_str("(enabled: true)").unwrap();
        assert!(partial.enabled);
        assert_eq!(partial.resolution, 1024);
    }

    #[test]
    fn presets() {
        let p = ShadowSettings::ps2();
        assert!(p.enabled && p.resolution == 256 && p.filter == ShadowFilter::Dithered);
        assert_eq!(p.filter.dither_flag(), 1);
        assert_eq!(ShadowFilter::Pcf.dither_flag(), 0);
        assert!(ShadowSettings::smooth().enabled);
    }

    #[test]
    fn resolution_clamps() {
        let mut s = ShadowSettings::default();
        s.resolution = 1;
        assert_eq!(s.clamped_resolution(), MIN_RESOLUTION);
        s.resolution = 100_000;
        assert_eq!(s.clamped_resolution(), MAX_RESOLUTION);
    }

    #[test]
    fn pcf_kernel_rules() {
        let mut s = ShadowSettings::default();
        s.softness = 0.0;
        assert_eq!(s.pcf_kernel(), (0, 0.0));
        s.softness = 1.0;
        assert_eq!(s.pcf_kernel(), (1, 1.0));
        s.softness = 2.0;
        assert_eq!(s.pcf_kernel(), (2, 1.0));
        s.softness = 99.0; // capped
        assert_eq!(s.pcf_kernel(), (2, 1.0));
        s.softness = 0.5;
        assert_eq!(s.pcf_kernel(), (1, 0.5));
        s.filter = ShadowFilter::Hard;
        assert_eq!(s.pcf_kernel(), (0, 0.0));
    }

    #[test]
    fn quantize_is_monotone_idempotent_and_bounded() {
        let mut prev = 0.0;
        for i in 1..200 {
            let t = i as f32 * 0.013;
            let q = quantize_texel_size(t);
            assert!(q >= t * (1.0 - 1e-3), "{t} -> {q}");
            assert!(q <= t * 1.2 + 1e-4, "{t} -> {q}");
            assert!(q >= prev);
            assert!(close(quantize_texel_size(q), q, q * 1e-4));
            prev = q;
        }
    }

    #[test]
    fn light_basis_looks_along_minus_light_dir() {
        for d in [sun(), Vec3::Y, -Vec3::Y, Vec3::X] {
            let v = light_basis(d);
            // The direction toward the light is +z in light view space.
            let z = v.transform_vector3(d.normalize());
            assert!(close(z.z, 1.0, 1e-5) && z.x.abs() < 1e-5 && z.y.abs() < 1e-5, "{d:?} {z:?}");
        }
    }

    #[test]
    fn aabb_corners_are_the_eight() {
        let c = aabb_corners(Vec3::new(-1.0, -2.0, -3.0), Vec3::new(1.0, 2.0, 3.0));
        let mut seen = std::collections::HashSet::new();
        for p in c {
            assert_eq!(p.abs(), Vec3::new(1.0, 2.0, 3.0));
            seen.insert((p.x as i32, p.y as i32, p.z as i32));
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn fit_contains_all_points() {
        let pts = aabb_corners(Vec3::new(-10.0, 0.0, -10.0), Vec3::new(10.0, 3.0, 10.0));
        let opts = FitOptions::new(1024);
        for snap in [false, true] {
            let fit = fit_directional(sun(), &pts, &FitOptions { snap, ..opts }).unwrap();
            for p in pts {
                let t = project_to_shadow(&fit, p);
                assert!((-1e-4..=1.0 + 1e-4).contains(&t.x), "snap={snap} x={}", t.x);
                assert!((-1e-4..=1.0 + 1e-4).contains(&t.y), "snap={snap} y={}", t.y);
                assert!((-1e-4..=1.0 + 1e-4).contains(&t.z), "snap={snap} z={}", t.z);
            }
        }
    }

    #[test]
    fn ortho_from_aabb_contains_box() {
        let (v, p) = ortho_from_aabb(sun(), Vec3::new(-2.0, 0.0, -3.0), Vec3::new(4.0, 5.0, 1.0));
        for c in aabb_corners(Vec3::new(-2.0, 0.0, -3.0), Vec3::new(4.0, 5.0, 1.0)) {
            let n = (p * v).project_point3(c);
            assert!(n.abs().max_element() <= 1.0 + 1e-4, "{n:?}");
        }
    }

    #[test]
    fn empty_points_and_zero_res_give_none() {
        assert!(fit_directional(sun(), &[], &FitOptions::new(256)).is_none());
        assert!(fit_directional(sun(), &[Vec3::ZERO], &FitOptions::new(0)).is_none());
    }

    #[test]
    fn caster_extend_moves_near_plane_toward_light() {
        let pts = aabb_corners(Vec3::splat(-1.0), Vec3::splat(1.0));
        let a = fit_directional(sun(), &pts, &FitOptions::new(512)).unwrap();
        let b = fit_directional(sun(), &pts, &FitOptions { caster_extend: 25.0, ..FitOptions::new(512) })
            .unwrap();
        assert!(close(b.depth_range - a.depth_range, 25.0, 1e-3));
    }

    /// Texel snapping: sliding the whole point set by arbitrary sub-texel
    /// amounts (what a moving camera does) must keep the world->texel phase
    /// of a fixed receiver constant — i.e. no shimmer.
    #[test]
    fn texel_snapping_keeps_phase_constant() {
        let res = 512;
        let base: Vec<Vec3> = aabb_corners(Vec3::new(-8.0, 0.0, -8.0), Vec3::new(8.0, 2.0, 8.0)).to_vec();
        let receiver = Vec3::new(1.234, 0.0, -2.345);
        let phase = |offset: Vec3| {
            let pts: Vec<Vec3> = base.iter().map(|p| *p + offset).collect();
            let fit = fit_directional(sun(), &pts, &FitOptions::new(res)).unwrap();
            let t = project_to_shadow(&fit, receiver) * res as f32; // texel coords
            (t.x.rem_euclid(1.0), t.y.rem_euclid(1.0), fit.texel_world)
        };
        let (px, py, tw) = phase(Vec3::ZERO);
        for i in 1..40 {
            let off = Vec3::new(0.0137 * i as f32, 0.0, -0.0091 * i as f32);
            let (qx, qy, qt) = phase(off);
            assert!(close(qt, tw, tw * 1e-4), "texel size changed: {tw} -> {qt}");
            let dx = (qx - px).abs().min(1.0 - (qx - px).abs());
            let dy = (qy - py).abs().min(1.0 - (qy - py).abs());
            assert!(dx < 0.02 && dy < 0.02, "shimmer at i={i}: dx={dx} dy={dy}");
        }
    }

    /// Contrast: without snapping the phase DOES drift (proves the test bites).
    #[test]
    fn without_snapping_phase_drifts() {
        let res = 512;
        let base: Vec<Vec3> = aabb_corners(Vec3::new(-8.0, 0.0, -8.0), Vec3::new(8.0, 2.0, 8.0)).to_vec();
        let receiver = Vec3::new(1.234, 0.0, -2.345);
        let phase = |offset: Vec3| {
            let pts: Vec<Vec3> = base.iter().map(|p| *p + offset).collect();
            let opts = FitOptions { snap: false, ..FitOptions::new(res) };
            let fit = fit_directional(sun(), &pts, &opts).unwrap();
            let t = project_to_shadow(&fit, receiver) * res as f32;
            t.x.rem_euclid(1.0)
        };
        let p0 = phase(Vec3::ZERO);
        let drift = (1..20)
            .map(|i| {
                let d = (phase(Vec3::new(0.0137 * i as f32, 0.0, 0.0)) - p0).abs();
                d.min(1.0 - d)
            })
            .fold(0.0_f32, f32::max);
        assert!(drift > 0.05, "expected drift without snapping, got {drift}");
    }

    #[test]
    fn frustum_corners_clip_far_plane() {
        let view = Mat4::look_at_rh(Vec3::new(0.0, 2.0, 10.0), Vec3::new(0.0, 2.0, 0.0), Vec3::Y);
        let proj = Mat4::perspective_rh_gl(60f32.to_radians(), 1.5, 0.1, 200.0);
        let full = frustum_corners(view, proj, 1000.0);
        let clip = frustum_corners(view, proj, 30.0);
        let depth = |p: Vec3| -view.transform_point3(p).z;
        assert!(close(depth(full[0]), 0.1, 1e-3));
        assert!(depth(full[4]) > 1.0);
        assert!(close(depth(full[7]), 200.0, 0.1));
        assert!(close(depth(clip[0]), 0.1, 1e-3));
        for c in &clip[4..] {
            assert!(close(depth(*c), 30.0, 0.05), "{}", depth(*c));
        }
        // Near corners unchanged.
        for i in 0..4 {
            assert!((full[i] - clip[i]).length() < 1e-4);
        }
    }

    #[test]
    fn frustum_fit_is_rotation_stable_in_size() {
        let proj = Mat4::perspective_rh_gl(60f32.to_radians(), 1.5, 0.1, 200.0);
        let s = ShadowSettings::default();
        let mut sizes = Vec::new();
        for yaw in 0..12 {
            let a = yaw as f32 * 0.5;
            let eye = Vec3::new(3.0, 2.0, 5.0);
            let view = Mat4::look_to_rh(eye, Vec3::new(a.sin(), -0.1, -a.cos()), Vec3::Y);
            let fit = ShadowBounds::camera(view, proj, &s).fit(sun(), &s).unwrap();
            sizes.push(fit.half_extent);
        }
        let first = sizes[0];
        // Quantisation steps are ~19%: rotation may change the bucket by at most one.
        for h in &sizes {
            assert!(*h / first < 1.25 && first / *h < 1.25, "{sizes:?}");
        }
    }

    #[test]
    fn sphere_bounds_cover_sphere() {
        let s = ShadowSettings::default();
        let center = Vec3::new(2.0, 1.0, -3.0);
        let fit = ShadowBounds::Sphere { center, radius: 5.0 }.fit(sun(), &s).unwrap();
        // Points on the sphere surface in many directions land inside the map.
        for i in 0..64 {
            let a = i as f32 * 0.7;
            let d = Vec3::new(a.cos() * (a * 0.3).cos(), (a * 0.3).sin(), a.sin() * (a * 0.3).cos()).normalize();
            let t = project_to_shadow(&fit, center + d * 5.0);
            assert!((0.0..=1.0).contains(&t.x) && (0.0..=1.0).contains(&t.y) && (0.0..=1.0).contains(&t.z), "{t:?}");
        }
    }

    #[test]
    fn bias_math() {
        // 1 texel of 0.05 world units across a 50-unit range.
        assert!(close(const_bias_depth(1.0, 0.05, 50.0), 0.001, 1e-9));
        assert_eq!(const_bias_depth(-3.0, 0.05, 50.0), 0.0);
        assert!(close(slope_bias_depth(0.05, 50.0), 0.001, 1e-9));
        // Facing the light head-on: only the constant term.
        assert!(close(slope_scaled_bias(0.001, 0.002, 1.0), 0.001, 1e-9));
        // 45 degrees: tan = 1.
        let n = std::f32::consts::FRAC_1_SQRT_2;
        assert!(close(slope_scaled_bias(0.001, 0.002, n), 0.003, 1e-6));
        // Grazing: tan clamped to MAX_SLOPE_TAN.
        assert!(close(slope_scaled_bias(0.001, 0.002, 0.0), 0.001 + 0.002 * MAX_SLOPE_TAN, 1e-6));
        // Monotone: more grazing -> more bias.
        let mut prev = 0.0;
        for i in (0..=20).rev() {
            let b = slope_scaled_bias(0.0, 1.0, i as f32 / 20.0);
            assert!(b >= prev);
            prev = b;
        }
    }

    #[test]
    fn normal_offset_math() {
        assert_eq!(normal_offset_world(1.0, 0.1, 1.0), 0.0);
        assert!(close(normal_offset_world(1.0, 0.1, 0.0), 0.1, 1e-6));
        assert!(close(normal_offset_world(2.0, 0.1, 0.0), 0.2, 1e-6));
        assert_eq!(normal_offset_world(-1.0, 0.1, 0.0), 0.0);
    }

    #[test]
    fn lit_compare() {
        assert!(is_lit(0.5, 0.5, 0.0));
        assert!(!is_lit(0.6, 0.5, 0.0));
        assert!(is_lit(0.6, 0.5, 0.1001));
    }

    /// End-to-end on the CPU: a cube above a ground plane occludes the point
    /// under it and not a point far to the side — via the same projection and
    /// compare the shader performs, using a software depth map.
    #[test]
    fn software_shadow_cube_over_ground() {
        let res = 256u32;
        let s = ShadowSettings { resolution: res, ..ShadowSettings::default() };
        let (lo, hi) = (Vec3::new(-6.0, 0.0, -6.0), Vec3::new(6.0, 3.0, 6.0));
        let fit = ShadowBounds::Aabb { min: lo, max: hi }.fit(sun(), &s).unwrap();
        // Cube centred (0,1.5,0) half 0.5: rasterise its corners' hull coarsely by sampling.
        let mut depth = vec![1.0f32; (res * res) as usize];
        let n = 20;
        for ix in 0..=n {
            for iy in 0..=n {
                for iz in 0..=n {
                    let p = Vec3::new(-0.5, 1.0, -0.5) + Vec3::new(ix as f32, iy as f32, iz as f32) / n as f32;
                    let t = project_to_shadow(&fit, p);
                    let (x, y) = ((t.x * res as f32) as usize, (t.y * res as f32) as usize);
                    let d = &mut depth[y * res as usize + x];
                    *d = d.min(t.z);
                }
            }
        }
        let bias = const_bias_depth(s.bias, fit.texel_world, fit.depth_range)
            + slope_bias_depth(fit.texel_world, fit.depth_range) * 0.0;
        let lit = |p: Vec3| {
            let t = project_to_shadow(&fit, p);
            let (x, y) = ((t.x * res as f32) as usize, (t.y * res as f32) as usize);
            is_lit(t.z, depth[y * res as usize + x], bias + slope_bias_depth(fit.texel_world, fit.depth_range))
        };
        // Ground point on the shadow ray below the cube centre (toward -light).
        let shadowed = Vec3::new(0.0, 1.5, 0.0) - sun() * (1.5 / sun().y);
        assert!(!lit(shadowed), "point under the cube should be shadowed: {shadowed:?}");
        assert!(lit(Vec3::new(4.0, 0.0, 4.0)), "far point should be lit");
        assert!(lit(Vec3::new(0.0, 3.0, 0.0)), "point above the cube should be lit");
    }
}
