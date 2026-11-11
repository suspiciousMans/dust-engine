//! Joint-local transforms and poses, plus the blend operators on them.
//!
//! Everything here is pure math (no GL, no ECS) so it is exhaustively unit
//! tested.

use glam::{Mat4, Quat, Vec3};

/// Translation / rotation / scale of one joint, local to its parent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JointTransform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl JointTransform {
    pub const IDENTITY: Self = Self { translation: Vec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::ONE };

    pub fn new(translation: Vec3, rotation: Quat, scale: Vec3) -> Self {
        Self { translation, rotation, scale }
    }

    pub fn from_translation(translation: Vec3) -> Self {
        Self { translation, ..Self::IDENTITY }
    }

    pub fn from_rotation(rotation: Quat) -> Self {
        Self { rotation, ..Self::IDENTITY }
    }

    /// `T * R * S`.
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Interpolates translation/scale linearly and rotation along the
    /// shortest arc.
    pub fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            translation: self.translation.lerp(other.translation, t),
            rotation: slerp_shortest(self.rotation, other.rotation, t),
            scale: self.scale.lerp(other.scale, t),
        }
    }
}

impl Default for JointTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Spherical interpolation that always takes the shortest arc (`q` and `-q`
/// are the same rotation; if the 4D dot product is negative, flip `b`).
/// Falls back to a normalized lerp for nearly-parallel inputs, where slerp's
/// `sin` denominator vanishes.
pub fn slerp_shortest(a: Quat, b: Quat, t: f32) -> Quat {
    let b = if a.dot(b) < 0.0 { -b } else { b };
    let d = a.dot(b);
    let out = if d > 0.9995 {
        (a * (1.0 - t) + b * t).normalize()
    } else {
        let theta = d.clamp(-1.0, 1.0).acos();
        let sin = theta.sin();
        (a * (((1.0 - t) * theta).sin() / sin) + b * ((t * theta).sin() / sin)).normalize()
    };
    out
}

/// Scales non-negative finite weights to sum to 1. Negative / NaN weights
/// count as 0. If every weight is 0 the result is all zeros (callers decide
/// what that means).
pub fn normalize_weights(weights: &[f32]) -> Vec<f32> {
    let clean: Vec<f32> = weights.iter().map(|w| if w.is_finite() && *w > 0.0 { *w } else { 0.0 }).collect();
    let sum: f32 = clean.iter().sum();
    if sum <= 0.0 {
        return clean;
    }
    clean.iter().map(|w| w / sum).collect()
}

/// One local transform per skeleton joint.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Pose {
    pub joints: Vec<JointTransform>,
}

impl Pose {
    pub fn identity(len: usize) -> Self {
        Self { joints: vec![JointTransform::IDENTITY; len] }
    }

    pub fn len(&self) -> usize {
        self.joints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.joints.is_empty()
    }

    /// `self * (1 - w) + other * w`, per joint. Joints beyond the shorter
    /// pose keep `self`'s value.
    pub fn blend(&self, other: &Pose, w: f32) -> Pose {
        let mut out = self.clone();
        for (o, b) in out.joints.iter_mut().zip(&other.joints) {
            *o = o.lerp(b, w);
        }
        out
    }

    /// Like [`Pose::blend`] but with a per-joint mask multiplied into `w`
    /// (e.g. upper-body-only layers). Joints past the end of `mask` get 0.
    pub fn blend_masked(&self, other: &Pose, w: f32, mask: &[f32]) -> Pose {
        let mut out = self.clone();
        for (i, (o, b)) in out.joints.iter_mut().zip(&other.joints).enumerate() {
            let m = mask.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            *o = o.lerp(b, w * m);
        }
        out
    }

    /// N-way weighted blend. Weights are normalized to sum to 1 first;
    /// entries with a non-positive weight are ignored. `None` when no entry
    /// has positive weight. Implemented as a running sequence of two-way
    /// shortest-arc blends, so two inputs give exactly [`Pose::blend`].
    pub fn blend_weighted(poses: &[(&Pose, f32)]) -> Option<Pose> {
        let weights = normalize_weights(&poses.iter().map(|(_, w)| *w).collect::<Vec<_>>());
        let mut acc: Option<(Pose, f32)> = None;
        for ((pose, _), w) in poses.iter().zip(weights) {
            if w <= 0.0 {
                continue;
            }
            acc = Some(match acc {
                None => ((*pose).clone(), w),
                Some((cur, total)) => {
                    let new_total = total + w;
                    (cur.blend(pose, w / new_total), new_total)
                }
            });
        }
        acc.map(|(p, _)| p)
    }

    /// Adds `additive - reference` on top of `self`, scaled by `weight`.
    ///
    /// * translation: `self + w * (additive - reference)`
    /// * rotation: `slerp(I, additive * reference^-1, w) * self`
    /// * scale: `self + w * (additive - reference)`
    ///
    /// With `additive == reference` (or `weight == 0`) the pose is unchanged.
    pub fn apply_additive(&mut self, additive: &Pose, reference: &Pose, weight: f32) {
        for ((o, a), r) in self.joints.iter_mut().zip(&additive.joints).zip(&reference.joints) {
            o.translation += (a.translation - r.translation) * weight;
            let delta = a.rotation * r.rotation.inverse();
            o.rotation = (slerp_shortest(Quat::IDENTITY, delta, weight) * o.rotation).normalize();
            o.scale += (a.scale - r.scale) * weight;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, PI};

    fn close(a: Quat, b: Quat) -> bool {
        a.dot(b).abs() > 0.99999
    }

    #[test]
    fn slerp_endpoints_and_midpoint() {
        let a = Quat::IDENTITY;
        let b = Quat::from_rotation_z(FRAC_PI_2);
        assert!(close(slerp_shortest(a, b, 0.0), a));
        assert!(close(slerp_shortest(a, b, 1.0), b));
        assert!(close(slerp_shortest(a, b, 0.5), Quat::from_rotation_z(FRAC_PI_2 / 2.0)));
    }

    #[test]
    fn slerp_takes_shortest_path() {
        // Rz(350deg) is the rotation -10deg, but as a quaternion it lies in
        // the opposite hemisphere from Rz(10deg); naive slerp from +10deg to
        // it would swing 340deg the long way round.
        let a = Quat::from_rotation_z(10f32.to_radians());
        let b = Quat::from_rotation_z(350f32.to_radians());
        assert!(a.dot(b) < 0.0, "test premise: hemispheres differ");
        let mid = slerp_shortest(a, b, 0.5);
        // shortest arc 10deg -> -10deg passes through 0deg
        assert!(close(mid, Quat::IDENTITY), "got {mid:?}");
        // and the result is unit
        assert!((mid.length() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn slerp_near_half_turn_stays_unit() {
        let b = Quat::from_rotation_y(PI * 170.0 / 180.0);
        let m = slerp_shortest(Quat::IDENTITY, b, 0.5);
        assert!((m.length() - 1.0).abs() < 1e-5);
        assert!(close(m, Quat::from_rotation_y(PI * 85.0 / 180.0)));
    }

    #[test]
    fn normalize_weights_sum_to_one() {
        let n = normalize_weights(&[2.0, 6.0, -1.0, f32::NAN]);
        assert_eq!(n, vec![0.25, 0.75, 0.0, 0.0]);
        assert!((n.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert_eq!(normalize_weights(&[0.0, -2.0]), vec![0.0, 0.0]);
    }

    fn pose_t(x: f32) -> Pose {
        Pose { joints: vec![JointTransform::from_translation(Vec3::new(x, 0.0, 0.0))] }
    }

    #[test]
    fn blend_midpoint_and_endpoints() {
        let (a, b) = (pose_t(0.0), pose_t(10.0));
        assert_eq!(a.blend(&b, 0.0).joints[0].translation.x, 0.0);
        assert_eq!(a.blend(&b, 1.0).joints[0].translation.x, 10.0);
        assert_eq!(a.blend(&b, 0.5).joints[0].translation.x, 5.0);
    }

    #[test]
    fn weighted_blend_normalizes_weights() {
        let (a, b, c) = (pose_t(0.0), pose_t(10.0), pose_t(100.0));
        // weights 1:1 (sum 2) behave like 0.5/0.5; c has zero weight.
        let p = Pose::blend_weighted(&[(&a, 1.0), (&b, 1.0), (&c, 0.0)]).unwrap();
        assert!((p.joints[0].translation.x - 5.0).abs() < 1e-5);
        // 1:1:2 -> 0.25*0 + 0.25*10 + 0.5*100 = 52.5
        let p = Pose::blend_weighted(&[(&a, 1.0), (&b, 1.0), (&c, 2.0)]).unwrap();
        assert!((p.joints[0].translation.x - 52.5).abs() < 1e-4);
        // scaling all weights does not change the result
        let q = Pose::blend_weighted(&[(&a, 10.0), (&b, 10.0), (&c, 20.0)]).unwrap();
        assert!((q.joints[0].translation.x - p.joints[0].translation.x).abs() < 1e-4);
        assert!(Pose::blend_weighted(&[(&a, 0.0)]).is_none());
        assert!(Pose::blend_weighted(&[]).is_none());
    }

    #[test]
    fn weighted_two_way_equals_blend() {
        let a = Pose { joints: vec![JointTransform::from_rotation(Quat::from_rotation_x(0.2))] };
        let b = Pose { joints: vec![JointTransform::from_rotation(Quat::from_rotation_x(1.4))] };
        let w = Pose::blend_weighted(&[(&a, 3.0), (&b, 1.0)]).unwrap();
        let direct = a.blend(&b, 0.25);
        assert!(close(w.joints[0].rotation, direct.joints[0].rotation));
    }

    #[test]
    fn masked_blend_respects_mask() {
        let a = Pose { joints: vec![JointTransform::IDENTITY; 2] };
        let b = Pose {
            joints: vec![JointTransform::from_translation(Vec3::X * 4.0); 2],
        };
        let p = a.blend_masked(&b, 1.0, &[0.0, 0.5]);
        assert_eq!(p.joints[0].translation.x, 0.0);
        assert_eq!(p.joints[1].translation.x, 2.0);
    }

    #[test]
    fn additive_layer() {
        let reference = Pose { joints: vec![JointTransform::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY, Vec3::ONE)] };
        let additive = Pose {
            joints: vec![JointTransform::new(
                Vec3::new(1.0, 2.0, 0.0),
                Quat::from_rotation_z(FRAC_PI_2),
                Vec3::splat(2.0),
            )],
        };
        let base = Pose {
            joints: vec![JointTransform::new(Vec3::new(10.0, 0.0, 0.0), Quat::from_rotation_x(FRAC_PI_2), Vec3::ONE)],
        };

        // weight 0: unchanged
        let mut p = base.clone();
        p.apply_additive(&additive, &reference, 0.0);
        assert_eq!(p.joints[0].translation, base.joints[0].translation);
        assert!(close(p.joints[0].rotation, base.joints[0].rotation));

        // additive == reference: unchanged at any weight
        let mut p = base.clone();
        p.apply_additive(&reference, &reference, 1.0);
        assert!(close(p.joints[0].rotation, base.joints[0].rotation));
        assert_eq!(p.joints[0].translation, base.joints[0].translation);

        // weight 1: translation += (0,2,0); scale 1 + (2-1) = 2; rotation = Rz(90) * Rx(90)
        let mut p = base.clone();
        p.apply_additive(&additive, &reference, 1.0);
        assert!((p.joints[0].translation - Vec3::new(10.0, 2.0, 0.0)).length() < 1e-5);
        assert!((p.joints[0].scale - Vec3::splat(2.0)).length() < 1e-5);
        assert!(close(p.joints[0].rotation, Quat::from_rotation_z(FRAC_PI_2) * Quat::from_rotation_x(FRAC_PI_2)));

        // weight 0.5: half of everything
        let mut p = base.clone();
        p.apply_additive(&additive, &reference, 0.5);
        assert!((p.joints[0].translation - Vec3::new(10.0, 1.0, 0.0)).length() < 1e-5);
        assert!((p.joints[0].scale - Vec3::splat(1.5)).length() < 1e-5);
        assert!(close(
            p.joints[0].rotation,
            Quat::from_rotation_z(FRAC_PI_2 / 2.0) * Quat::from_rotation_x(FRAC_PI_2)
        ));
    }
}
