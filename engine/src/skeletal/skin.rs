//! Per-vertex skin weights and linear-blend CPU skinning.
//!
//! The shared [`Vertex`] layout (position / normal / uv / baked color) is
//! deliberately **not** extended: every game's meshes, shaders and VAOs use
//! it. Skin data lives in a parallel array ([`SkinVertex`], 4 joint indices
//! as `u16` and 4 weights as `f32` per vertex) next to the bind-pose
//! [`MeshData`]. Each frame [`SkinnedMeshData::skin_into`] writes deformed
//! vertices that [`SkinnedMesh`] streams into a dynamic vertex buffer, so
//! the existing `mesh.vert`/`mesh.frag` shaders draw skinned meshes
//! unchanged.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};

use crate::mesh::{GpuMesh, MeshData, Vertex};

/// Up to four joint influences for one vertex. Joint indices refer to the
/// [`Skeleton`](super::skeleton::Skeleton) joint order (and so to the
/// palette).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct SkinVertex {
    pub joints: [u16; 4],
    pub weights: [f32; 4],
}

impl SkinVertex {
    /// Builds a vertex with weights rescaled to sum to 1 (negative / NaN
    /// weights count as 0). All-zero weights stay zero: such a vertex is
    /// left at its bind position by skinning.
    pub fn new(joints: [u16; 4], weights: [f32; 4]) -> Self {
        let mut w = weights.map(|w| if w.is_finite() && w > 0.0 { w } else { 0.0 });
        let sum: f32 = w.iter().sum();
        if sum > 0.0 {
            w = w.map(|x| x / sum);
        }
        Self { joints, weights: w }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SkinnedMeshData {
    /// Bind-pose geometry.
    pub mesh: MeshData,
    /// One entry per `mesh.vertices`.
    pub skin: Vec<SkinVertex>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{vertices} vertices but {skin} skin entries")]
pub struct SkinLengthMismatch {
    pub vertices: usize,
    pub skin: usize,
}

impl SkinnedMeshData {
    pub fn new(mesh: MeshData, skin: Vec<SkinVertex>) -> Result<Self, SkinLengthMismatch> {
        if mesh.vertices.len() != skin.len() {
            return Err(SkinLengthMismatch { vertices: mesh.vertices.len(), skin: skin.len() });
        }
        Ok(Self { mesh, skin })
    }

    /// Linear-blend skinning: each vertex becomes `sum(w_i * palette[j_i])`
    /// applied to its bind position, and to its normal (renormalized). The
    /// normal uses the blended matrix's linear part, which is exact for
    /// rotation + uniform scale (the usual skeleton case) but not for
    /// non-uniform joint scale. UV and baked color are copied.
    ///
    /// `out` is cleared and refilled (so it can be reused every frame). A
    /// joint index past the end of `palette` counts as identity; a vertex
    /// with no weight keeps its bind pose.
    pub fn skin_into(&self, palette: &[Mat4], out: &mut Vec<Vertex>) {
        out.clear();
        out.reserve(self.mesh.vertices.len());
        for (v, s) in self.mesh.vertices.iter().zip(&self.skin) {
            let wsum: f32 = s.weights.iter().sum();
            if wsum <= 0.0 {
                out.push(*v);
                continue;
            }
            let mut m = Mat4::ZERO;
            for k in 0..4 {
                let w = s.weights[k];
                if w > 0.0 {
                    m += palette.get(s.joints[k] as usize).copied().unwrap_or(Mat4::IDENTITY) * w;
                }
            }
            let p = m.transform_point3(Vec3::from(v.position));
            let n = m.transform_vector3(Vec3::from(v.normal));
            let n = if n.length_squared() > 0.0 { n.normalize() } else { Vec3::from(v.normal) };
            out.push(Vertex { position: p.into(), normal: n.into(), uv: v.uv, color: v.color });
        }
    }

    /// Convenience over [`SkinnedMeshData::skin_into`].
    pub fn skin(&self, palette: &[Mat4]) -> Vec<Vertex> {
        let mut out = Vec::new();
        self.skin_into(palette, &mut out);
        out
    }
}

/// A [`SkinnedMeshData`] plus the GPU mesh its deformed vertices stream
/// into. Draw [`SkinnedMesh::gpu`] with the game's ordinary mesh shader and
/// the entity's model matrix.
///
/// **Never exercised against a live GL context** — only the CPU half
/// (`SkinnedMeshData`) is unit tested.
pub struct SkinnedMesh {
    data: SkinnedMeshData,
    scratch: Vec<Vertex>,
    gpu: GpuMesh,
}

impl SkinnedMesh {
    pub fn upload(gl: &glow::Context, data: SkinnedMeshData) -> anyhow::Result<Self> {
        let gpu = GpuMesh::upload_dynamic(gl, &data.mesh)?;
        Ok(Self { scratch: Vec::with_capacity(data.mesh.vertices.len()), data, gpu })
    }

    /// Skins with `palette` (e.g. `SkeletalAnimator::palette()`) and uploads.
    pub fn update(&mut self, gl: &glow::Context, palette: &[Mat4]) {
        self.data.skin_into(palette, &mut self.scratch);
        self.gpu.update_vertices(gl, &self.scratch);
    }

    pub fn gpu(&self) -> &GpuMesh {
        &self.gpu
    }

    pub fn data(&self) -> &SkinnedMeshData {
        &self.data
    }

    /// # Safety
    /// The mesh must not be used (bound or drawn) after this call.
    pub unsafe fn destroy(&self, gl: &glow::Context) {
        self.gpu.destroy(gl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeletal::pose::JointTransform;
    use crate::skeletal::skeleton::{Joint, Skeleton};
    use glam::Quat;
    use std::f32::consts::FRAC_PI_2;

    fn vert(p: [f32; 3], n: [f32; 3]) -> Vertex {
        Vertex { position: p, normal: n, uv: [0.25, 0.75], color: [0.1, 0.2, 0.3] }
    }

    fn chain_palette() -> Vec<Mat4> {
        let s = Skeleton::new(vec![
            Joint { name: "root".into(), parent: None, bind: JointTransform::IDENTITY },
            Joint { name: "tip".into(), parent: Some(0), bind: JointTransform::from_translation(Vec3::Y) },
        ])
        .unwrap();
        let mut pose = s.bind_pose();
        pose.joints[0].rotation = Quat::from_rotation_z(FRAC_PI_2);
        pose.joints[1].translation = Vec3::new(0.0, 2.0, 0.0);
        // palette[0] = Rz(90); palette[1] = Rz(90) * T(0,1,0)
        s.joint_palette(&pose)
    }

    #[test]
    fn skin_vertex_normalizes_weights() {
        let s = SkinVertex::new([0, 1, 0, 0], [3.0, 1.0, 0.0, -2.0]);
        assert_eq!(s.weights, [0.75, 0.25, 0.0, 0.0]);
        assert_eq!(SkinVertex::new([0; 4], [0.0; 4]).weights, [0.0; 4]);
        assert_eq!(std::mem::size_of::<SkinVertex>(), 24);
    }

    #[test]
    fn length_mismatch_is_rejected() {
        let mesh = MeshData { vertices: vec![vert([0.0; 3], [0.0, 1.0, 0.0])], indices: vec![] };
        assert_eq!(SkinnedMeshData::new(mesh, vec![]).unwrap_err(), SkinLengthMismatch { vertices: 1, skin: 0 });
    }

    #[test]
    fn skinning_matches_hand_computed_blend() {
        let palette = chain_palette();
        let mesh = MeshData {
            vertices: vec![
                // 25% root / 75% tip. Vertex (0,1,0):
                //   root:  Rz90 * (0,1,0)           = (-1, 0, 0)
                //   tip:   Rz90 * T(0,1,0) * (0,1,0) = Rz90 * (0,2,0) = (-2, 0, 0)
                //   blend: 0.25*(-1,0,0) + 0.75*(-2,0,0) = (-1.75, 0, 0)
                vert([0.0, 1.0, 0.0], [0.0, 1.0, 0.0]),
                // fully on root: (1,0,0) -> Rz90 -> (0,1,0)
                vert([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
                // no weights: untouched
                vert([5.0, 5.0, 5.0], [0.0, 0.0, 1.0]),
                // 50/50 root & tip at the origin:
                //   root: (0,0,0); tip: Rz90*T(0,1,0)*0 = Rz90*(0,1,0) = (-1,0,0); blend (-0.5,0,0)
                vert([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ],
            indices: vec![0, 1, 2],
        };
        let skin = vec![
            SkinVertex::new([0, 1, 0, 0], [1.0, 3.0, 0.0, 0.0]),
            SkinVertex::new([0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
            SkinVertex::default(),
            SkinVertex::new([0, 1, 0, 0], [1.0, 1.0, 0.0, 0.0]),
        ];
        let data = SkinnedMeshData::new(mesh, skin).unwrap();
        let out = data.skin(&palette);

        let close = |a: [f32; 3], b: [f32; 3]| (Vec3::from(a) - Vec3::from(b)).length() < 1e-5;
        assert!(close(out[0].position, [-1.75, 0.0, 0.0]), "{:?}", out[0].position);
        // normal (0,1,0) under Rz90 on both joints -> (-1,0,0)
        assert!(close(out[0].normal, [-1.0, 0.0, 0.0]), "{:?}", out[0].normal);
        assert!(close(out[1].position, [0.0, 1.0, 0.0]));
        assert!(close(out[1].normal, [0.0, 1.0, 0.0]));
        assert_eq!(out[2].position, [5.0, 5.0, 5.0]);
        assert_eq!(out[2].normal, [0.0, 0.0, 1.0]);
        assert!(close(out[3].position, [-0.5, 0.0, 0.0]), "{:?}", out[3].position);
        // uv / color pass through; indices untouched
        assert_eq!(out[0].uv, [0.25, 0.75]);
        assert_eq!(out[0].color, [0.1, 0.2, 0.3]);
        assert_eq!(data.mesh.indices, vec![0, 1, 2]);
    }

    #[test]
    fn bind_pose_palette_reproduces_bind_mesh() {
        let s = Skeleton::new(vec![
            Joint { name: "a".into(), parent: None, bind: JointTransform::IDENTITY },
            Joint { name: "b".into(), parent: Some(0), bind: JointTransform::from_translation(Vec3::new(0.0, 1.0, 0.0)) },
        ])
        .unwrap();
        let palette = s.joint_palette(&s.bind_pose());
        let mesh = MeshData { vertices: vec![vert([0.3, 0.7, -0.2], [0.0, 0.6, 0.8])], indices: vec![] };
        let data = SkinnedMeshData::new(mesh.clone(), vec![SkinVertex::new([0, 1, 0, 0], [0.4, 0.6, 0.0, 0.0])]).unwrap();
        let out = data.skin(&palette);
        assert!((Vec3::from(out[0].position) - Vec3::from(mesh.vertices[0].position)).length() < 1e-6);
        assert!((Vec3::from(out[0].normal) - Vec3::from(mesh.vertices[0].normal)).length() < 1e-6);
    }

    #[test]
    fn out_of_range_joint_is_identity_and_buffer_is_reused() {
        let mesh = MeshData { vertices: vec![vert([1.0, 2.0, 3.0], [0.0, 1.0, 0.0])], indices: vec![] };
        let data = SkinnedMeshData::new(mesh, vec![SkinVertex::new([9, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])]).unwrap();
        let mut out = vec![vert([0.0; 3], [0.0; 3]); 5];
        data.skin_into(&[], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].position, [1.0, 2.0, 3.0]);
    }
}
