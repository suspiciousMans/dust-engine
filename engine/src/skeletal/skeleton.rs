//! Joint hierarchy, bind pose and inverse-bind matrices, and the joint
//! palette builder (`palette[i] = world[i] * inverse_bind[i]`).

use glam::Mat4;

use super::pose::{JointTransform, Pose};

#[derive(Clone, Debug, PartialEq)]
pub struct Joint {
    pub name: String,
    /// Index of the parent joint; `None` for a root. A parent must have a
    /// smaller index than its child (checked by [`Skeleton::new`]) so world
    /// matrices resolve in one forward pass.
    pub parent: Option<usize>,
    /// Local bind (rest) transform.
    pub bind: JointTransform,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SkeletonError {
    #[error("joint {joint} ('{name}') has parent {parent}, which is not an earlier joint")]
    BadParent { joint: usize, name: String, parent: usize },
    #[error("{got} inverse-bind matrices for {expected} joints")]
    InverseBindCount { expected: usize, got: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skeleton {
    joints: Vec<Joint>,
    inverse_bind: Vec<Mat4>,
    /// Transform applied above every root joint (identity unless an importer
    /// folded non-joint ancestor nodes into it). Mesh-space -> model-space.
    pub root_transform: Mat4,
}

impl Skeleton {
    /// Builds a skeleton and derives each inverse-bind matrix from the bind
    /// pose (`inverse(world_bind[i])`).
    pub fn new(joints: Vec<Joint>) -> Result<Self, SkeletonError> {
        Self::validate(&joints)?;
        let mut s = Self { inverse_bind: Vec::new(), joints, root_transform: Mat4::IDENTITY };
        s.recompute_inverse_bind();
        Ok(s)
    }

    /// Like [`Skeleton::new`] but with explicit inverse-bind matrices (as
    /// stored by glTF skins).
    pub fn with_inverse_bind(joints: Vec<Joint>, inverse_bind: Vec<Mat4>) -> Result<Self, SkeletonError> {
        Self::validate(&joints)?;
        if inverse_bind.len() != joints.len() {
            return Err(SkeletonError::InverseBindCount { expected: joints.len(), got: inverse_bind.len() });
        }
        Ok(Self { joints, inverse_bind, root_transform: Mat4::IDENTITY })
    }

    fn validate(joints: &[Joint]) -> Result<(), SkeletonError> {
        for (i, j) in joints.iter().enumerate() {
            if let Some(p) = j.parent {
                if p >= i {
                    return Err(SkeletonError::BadParent { joint: i, name: j.name.clone(), parent: p });
                }
            }
        }
        Ok(())
    }

    /// Recomputes inverse-bind matrices from the current bind pose and
    /// `root_transform`.
    pub fn recompute_inverse_bind(&mut self) {
        let bind = self.world_matrices(&self.bind_pose());
        self.inverse_bind = bind.iter().map(|m| m.inverse()).collect();
    }

    pub fn len(&self) -> usize {
        self.joints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.joints.is_empty()
    }

    pub fn joints(&self) -> &[Joint] {
        &self.joints
    }

    pub fn inverse_bind(&self) -> &[Mat4] {
        &self.inverse_bind
    }

    pub fn joint_index(&self, name: &str) -> Option<usize> {
        self.joints.iter().position(|j| j.name == name)
    }

    pub fn bind_pose(&self) -> Pose {
        Pose { joints: self.joints.iter().map(|j| j.bind).collect() }
    }

    /// Model-space matrix of every joint for `pose`. A pose shorter than the
    /// skeleton uses the bind transform for the missing joints.
    pub fn world_matrices(&self, pose: &Pose) -> Vec<Mat4> {
        let mut out = Vec::with_capacity(self.joints.len());
        self.world_matrices_into(pose, &mut out);
        out
    }

    pub fn world_matrices_into(&self, pose: &Pose, out: &mut Vec<Mat4>) {
        out.clear();
        for (i, joint) in self.joints.iter().enumerate() {
            let local = pose.joints.get(i).copied().unwrap_or(joint.bind).to_mat4();
            let parent = match joint.parent {
                Some(p) => out[p],
                None => self.root_transform,
            };
            out.push(parent * local);
        }
    }

    /// Skinning palette: `world[i] * inverse_bind[i]`. Skinning a bind-space
    /// vertex `v` by joint `i` is `palette[i] * v`; at the bind pose every
    /// entry is the identity.
    pub fn joint_palette(&self, pose: &Pose) -> Vec<Mat4> {
        let mut out = Vec::with_capacity(self.joints.len());
        self.joint_palette_into(pose, &mut out);
        out
    }

    pub fn joint_palette_into(&self, pose: &Pose, out: &mut Vec<Mat4>) {
        self.world_matrices_into(pose, out);
        for (m, ibm) in out.iter_mut().zip(&self.inverse_bind) {
            *m *= *ibm;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use glam::{Quat, Vec3};
    use std::f32::consts::FRAC_PI_2;

    /// root at origin; child 1 unit up +Y from the root.
    pub fn two_joint_chain() -> Skeleton {
        Skeleton::new(vec![
            Joint { name: "root".into(), parent: None, bind: JointTransform::IDENTITY },
            Joint { name: "tip".into(), parent: Some(0), bind: JointTransform::from_translation(Vec3::Y) },
        ])
        .unwrap()
    }

    fn mat_close(a: Mat4, b: Mat4) -> bool {
        a.to_cols_array().iter().zip(b.to_cols_array()).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn rejects_parent_not_before_child() {
        let err = Skeleton::new(vec![
            Joint { name: "a".into(), parent: Some(1), bind: JointTransform::IDENTITY },
            Joint { name: "b".into(), parent: None, bind: JointTransform::IDENTITY },
        ])
        .unwrap_err();
        assert!(matches!(err, SkeletonError::BadParent { joint: 0, .. }));
        let err = Skeleton::with_inverse_bind(
            vec![Joint { name: "a".into(), parent: None, bind: JointTransform::IDENTITY }],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, SkeletonError::InverseBindCount { expected: 1, got: 0 });
    }

    #[test]
    fn inverse_bind_matches_hand_computation() {
        let s = two_joint_chain();
        assert!(mat_close(s.inverse_bind()[0], Mat4::IDENTITY));
        // tip world bind = translate(0,1,0); inverse = translate(0,-1,0)
        assert!(mat_close(s.inverse_bind()[1], Mat4::from_translation(Vec3::new(0.0, -1.0, 0.0))));
        assert_eq!(s.joint_index("tip"), Some(1));
        assert_eq!(s.joint_index("nope"), None);
    }

    #[test]
    fn bind_pose_palette_is_identity() {
        let s = two_joint_chain();
        for m in s.joint_palette(&s.bind_pose()) {
            assert!(mat_close(m, Mat4::IDENTITY), "{m:?}");
        }
    }

    #[test]
    fn palette_of_two_joint_chain_hand_computed() {
        let s = two_joint_chain();
        // Rotate root 90deg about Z, and move tip a further 2 units along its local Y.
        let mut pose = s.bind_pose();
        pose.joints[0].rotation = Quat::from_rotation_z(FRAC_PI_2);
        pose.joints[1].translation = Vec3::new(0.0, 2.0, 0.0);

        let world = s.world_matrices(&pose);
        // root world = Rz(90): maps (x,y,z) -> (-y, x, z).
        // tip world  = Rz(90) * T(0,2,0): origin goes to Rz*(0,2,0) = (-2, 0, 0).
        assert!(mat_close(world[0], Mat4::from_rotation_z(FRAC_PI_2)));
        assert!((world[1].transform_point3(Vec3::ZERO) - Vec3::new(-2.0, 0.0, 0.0)).length() < 1e-5);

        let palette = s.joint_palette(&pose);
        // palette[0] = Rz(90) * I
        assert!(mat_close(palette[0], Mat4::from_rotation_z(FRAC_PI_2)));
        // palette[1] = Rz(90) * T(0,2,0) * T(0,-1,0) = Rz(90) * T(0,1,0)
        let expect1 = Mat4::from_rotation_z(FRAC_PI_2) * Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0));
        assert!(mat_close(palette[1], expect1));
        // A vertex at the bind position of the tip (0,1,0), fully weighted to the
        // tip, ends at the tip's new origin (-2, 0, 0).
        let v = palette[1].transform_point3(Vec3::new(0.0, 1.0, 0.0));
        assert!((v - Vec3::new(-2.0, 0.0, 0.0)).length() < 1e-5, "{v:?}");
    }

    #[test]
    fn root_transform_is_applied_above_roots() {
        let mut s = two_joint_chain();
        s.root_transform = Mat4::from_translation(Vec3::new(5.0, 0.0, 0.0));
        s.recompute_inverse_bind();
        // bind pose still gives identity palette (ibm absorbed it) ...
        for m in s.joint_palette(&s.bind_pose()) {
            assert!(mat_close(m, Mat4::IDENTITY));
        }
        // ... and a moved joint carries the root transform in world space.
        let mut p = s.bind_pose();
        p.joints[0].translation = Vec3::new(0.0, 0.0, 3.0);
        let w = s.world_matrices(&p);
        assert!((w[0].transform_point3(Vec3::ZERO) - Vec3::new(5.0, 0.0, 3.0)).length() < 1e-5);
    }
}
