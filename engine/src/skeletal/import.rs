//! Asset import: glTF skins + animations, and a RON form of an
//! [`AnimationSet`] (so a glTF can be converted once and shipped as RON).
//!
//! glTF import (`gltf` crate, already an engine dependency):
//!
//! * Uses the document's **first skin**. Joints are re-ordered so parents
//!   come before children; `JOINTS_0` indices are remapped to match.
//! * Meshes imported: every node with `skin == that skin`, first triangle
//!   primitive only (same convention as `mesh::load_gltf`).
//! * Inverse-bind matrices come from the file when present, else are
//!   derived from the bind pose.
//! * Non-joint ancestors of a root joint (e.g. an "Armature" node) are
//!   folded into `Skeleton::root_transform`.
//! * Animations: translation / rotation / scale channels that target skin
//!   joints. `CUBICSPLINE` samplers are imported as `Linear` (tangents
//!   dropped, logged). Channels on non-joint nodes and morph-target
//!   weights are skipped (logged). glTF has no loop flag, so every clip
//!   imports with `looping: false`.
//! * Only `JOINTS_0`/`WEIGHTS_0` (4 influences) are read.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use glam::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use super::animator::AnimationSet;
use super::clip::{AnimationClip, Interpolation, JointChannel, Track};
use super::pose::JointTransform;
use super::skeleton::{Joint, Skeleton};
use super::skin::{SkinVertex, SkinnedMeshData};
use crate::mesh::{GltfImage, MeshData, Vertex};

/// One skinned mesh node from a glTF.
pub struct GltfSkinnedMesh {
    pub name: String,
    pub data: SkinnedMeshData,
    pub image: Option<GltfImage>,
}

pub struct GltfSkinnedModel {
    pub set: AnimationSet,
    pub meshes: Vec<GltfSkinnedMesh>,
}

pub fn load_gltf_skinned(path: &Path) -> anyhow::Result<GltfSkinnedModel> {
    let (document, buffers, images) = gltf::import(path).with_context(|| format!("importing {path:?}"))?;
    import_document(&document, &buffers, &images)
}

pub fn load_gltf_skinned_from_slice(bytes: &[u8]) -> anyhow::Result<GltfSkinnedModel> {
    let (document, buffers, images) = gltf::import_slice(bytes)?;
    import_document(&document, &buffers, &images)
}

fn mat4_from(m: [[f32; 4]; 4]) -> Mat4 {
    Mat4::from_cols_array_2d(&m)
}

fn node_local(node: &gltf::Node) -> JointTransform {
    let (t, r, s) = node.transform().decomposed();
    JointTransform::new(Vec3::from(t), Quat::from_xyzw(r[0], r[1], r[2], r[3]).normalize(), Vec3::from(s))
}

fn import_document(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
) -> anyhow::Result<GltfSkinnedModel> {
    let skin = document.skins().next().ok_or_else(|| anyhow!("glTF has no skin"))?;
    if document.skins().count() > 1 {
        log::warn!("glTF has {} skins; importing only the first", document.skins().count());
    }
    let skin_nodes: Vec<gltf::Node> = skin.joints().collect();
    if skin_nodes.is_empty() {
        bail!("glTF skin has no joints");
    }

    // node index -> parent node index
    let mut parent_of: BTreeMap<usize, usize> = BTreeMap::new();
    for node in document.nodes() {
        for child in node.children() {
            parent_of.insert(child.index(), node.index());
        }
    }
    // node index -> position in skin.joints() ("old" joint index)
    let old_index: BTreeMap<usize, usize> = skin_nodes.iter().enumerate().map(|(i, n)| (n.index(), i)).collect();

    // Nearest ancestor that is itself a joint.
    let joint_parent = |old: usize| -> Option<usize> {
        let mut cur = skin_nodes[old].index();
        while let Some(&p) = parent_of.get(&cur) {
            if let Some(&pi) = old_index.get(&p) {
                return Some(pi);
            }
            cur = p;
        }
        None
    };
    let old_parents: Vec<Option<usize>> = (0..skin_nodes.len()).map(joint_parent).collect();

    // Parents-before-children order: stable sort by hierarchy depth.
    let depth = |mut i: usize| {
        let mut d = 0;
        while let Some(p) = old_parents[i] {
            d += 1;
            i = p;
        }
        d
    };
    let mut order: Vec<usize> = (0..skin_nodes.len()).collect();
    order.sort_by_key(|&i| depth(i));
    let mut new_of_old = vec![0usize; order.len()];
    for (new, &old) in order.iter().enumerate() {
        new_of_old[old] = new;
    }

    let joints: Vec<Joint> = order
        .iter()
        .map(|&old| Joint {
            name: skin_nodes[old]
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("joint_{}", skin_nodes[old].index())),
            parent: old_parents[old].map(|p| new_of_old[p]),
            bind: node_local(&skin_nodes[old]),
        })
        .collect();

    // Fold non-joint ancestors of the first root joint into root_transform.
    let first_root = order.iter().copied().find(|&o| old_parents[o].is_none()).expect("a hierarchy has a root");
    let mut chain = Vec::new();
    let mut cur = skin_nodes[first_root].index();
    while let Some(&p) = parent_of.get(&cur) {
        chain.push(p);
        cur = p;
    }
    let nodes: Vec<gltf::Node> = document.nodes().collect();
    let root_transform = chain.iter().rev().fold(Mat4::IDENTITY, |acc, &n| acc * mat4_from(nodes[n].transform().matrix()));

    let reader = skin.reader(|b| Some(&buffers[b.index()]));
    let mut skeleton = match reader.read_inverse_bind_matrices() {
        Some(ibm) => {
            let ibm: Vec<Mat4> = ibm.map(mat4_from).collect();
            if ibm.len() != skin_nodes.len() {
                bail!("{} inverse-bind matrices for {} joints", ibm.len(), skin_nodes.len());
            }
            let reordered: Vec<Mat4> = order.iter().map(|&old| ibm[old]).collect();
            Skeleton::with_inverse_bind(joints, reordered)?
        }
        None => Skeleton::new(joints)?,
    };
    skeleton.root_transform = root_transform;
    if skin.inverse_bind_matrices().is_none() {
        skeleton.recompute_inverse_bind();
    }

    let clips = import_animations(document, buffers, &old_index, &new_of_old)?;

    // Meshes bound to this skin.
    let mut meshes = Vec::new();
    for node in document.nodes() {
        let (Some(mesh), Some(node_skin)) = (node.mesh(), node.skin()) else { continue };
        if node_skin.index() != skin.index() {
            continue;
        }
        let name = node.name().map(str::to_string).unwrap_or_else(|| format!("node_{}", node.index()));
        let Some(prim) = mesh.primitives().find(|p| p.mode() == gltf::mesh::Mode::Triangles) else {
            log::warn!("skinned node '{name}' has no triangle primitive; skipping");
            continue;
        };
        let r = prim.reader(|b| Some(&buffers[b.index()]));
        let Some(positions) = r.read_positions() else {
            log::warn!("skinned node '{name}' has no positions; skipping");
            continue;
        };
        let positions: Vec<[f32; 3]> = positions.collect();
        let normals: Vec<[f32; 3]> =
            r.read_normals().map(Iterator::collect).unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
        let uvs: Vec<[f32; 2]> =
            r.read_tex_coords(0).map(|i| i.into_f32().collect()).unwrap_or_else(|| vec![[0.0; 2]; positions.len()]);
        let indices: Vec<u32> =
            r.read_indices().map(|i| i.into_u32().collect()).unwrap_or_else(|| (0..positions.len() as u32).collect());
        let joints_attr: Vec<[u16; 4]> = r.read_joints(0).map(|j| j.into_u16().collect()).unwrap_or_default();
        let weights_attr: Vec<[f32; 4]> = r.read_weights(0).map(|w| w.into_f32().collect()).unwrap_or_default();
        if joints_attr.is_empty() || weights_attr.is_empty() {
            log::warn!("skinned node '{name}' lacks JOINTS_0/WEIGHTS_0; its vertices stay in bind pose");
        }

        let vertices: Vec<Vertex> = (0..positions.len())
            .map(|i| Vertex {
                position: positions[i],
                normal: normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                uv: uvs.get(i).copied().unwrap_or([0.0; 2]),
                color: [0.0; 3],
            })
            .collect();
        let skin_verts: Vec<SkinVertex> = (0..positions.len())
            .map(|i| match (joints_attr.get(i), weights_attr.get(i)) {
                (Some(j), Some(w)) => {
                    // Remap skin-order joint indices to our parents-first order. Zero-weight
                    // slots may carry garbage indices; keep them in range.
                    let remapped = j.map(|old| new_of_old.get(old as usize).copied().unwrap_or(0) as u16);
                    SkinVertex::new(remapped, *w)
                }
                _ => SkinVertex::default(),
            })
            .collect();

        let image = prim.material().pbr_metallic_roughness().base_color_texture().and_then(|info| {
            images.get(info.texture().source().index()).and_then(|d| {
                crate::mesh::gltf_loader_to_rgba8(&d.pixels, d.format)
                    .map(|rgba| GltfImage { rgba, width: d.width, height: d.height })
            })
        });
        meshes.push(GltfSkinnedMesh {
            name,
            data: SkinnedMeshData::new(MeshData { vertices, indices }, skin_verts)?,
            image,
        });
    }

    Ok(GltfSkinnedModel { set: AnimationSet { skeleton, clips }, meshes })
}

/// CUBICSPLINE outputs hold (in-tangent, value, out-tangent) per key; keep
/// the values.
fn pick_values<T>(v: Vec<T>, cubic: bool) -> Vec<T> {
    if cubic {
        v.into_iter().skip(1).step_by(3).collect()
    } else {
        v
    }
}

fn import_animations(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    old_index: &BTreeMap<usize, usize>,
    new_of_old: &[usize],
) -> anyhow::Result<Vec<AnimationClip>> {
    use gltf::animation::util::ReadOutputs;
    use gltf::animation::Interpolation as GI;

    let mut clips = Vec::new();
    for (ai, anim) in document.animations().enumerate() {
        let name = anim.name().map(str::to_string).unwrap_or_else(|| format!("animation_{ai}"));
        let mut by_joint: BTreeMap<usize, JointChannel> = BTreeMap::new();
        for ch in anim.channels() {
            let Some(&old) = old_index.get(&ch.target().node().index()) else {
                log::debug!("animation '{name}': channel on non-joint node {}; skipped", ch.target().node().index());
                continue;
            };
            let joint = new_of_old[old];
            let r = ch.reader(|b| Some(&buffers[b.index()]));
            let Some(times) = r.read_inputs().map(|i| i.collect::<Vec<f32>>()) else { continue };
            let Some(outputs) = r.read_outputs() else { continue };
            let (interp, cubic) = match ch.sampler().interpolation() {
                GI::Linear => (Interpolation::Linear, false),
                GI::Step => (Interpolation::Step, false),
                GI::CubicSpline => {
                    log::warn!("animation '{name}': CUBICSPLINE imported as linear (tangents dropped)");
                    (Interpolation::Linear, true)
                }
            };
            let entry = by_joint.entry(joint).or_insert_with(|| JointChannel::new(joint));
            let built = match outputs {
                ReadOutputs::Translations(it) => {
                    Track::new(times, pick_values(it.map(Vec3::from).collect(), cubic), interp).map(|t| entry.translation = Some(t))
                }
                ReadOutputs::Scales(it) => {
                    Track::new(times, pick_values(it.map(Vec3::from).collect(), cubic), interp).map(|t| entry.scale = Some(t))
                }
                ReadOutputs::Rotations(it) => {
                    let q: Vec<Quat> =
                        it.into_f32().map(|r| Quat::from_xyzw(r[0], r[1], r[2], r[3]).normalize()).collect();
                    Track::new(times, pick_values(q, cubic), interp).map(|t| entry.rotation = Some(t))
                }
                ReadOutputs::MorphTargetWeights(_) => {
                    log::debug!("animation '{name}': morph-target weights skipped");
                    Ok(())
                }
            };
            if let Err(e) = built {
                log::warn!("animation '{name}': dropping a channel on joint {joint}: {e}");
            }
        }
        let channels: Vec<JointChannel> = by_joint.into_values().collect();
        let duration = AnimationClip::duration_from_tracks(&channels);
        clips.push(AnimationClip { name, duration, looping: false, channels, root_motion: None });
    }
    Ok(clips)
}

// --- RON form of an AnimationSet --------------------------------------------

#[derive(Serialize, Deserialize)]
struct JointDef {
    name: String,
    parent: Option<usize>,
    translation: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
}

#[derive(Serialize, Deserialize)]
struct TrackDef<T> {
    times: Vec<f32>,
    values: Vec<T>,
    #[serde(default)]
    interpolation: Interpolation,
}

#[derive(Serialize, Deserialize)]
struct ChannelDef {
    joint: usize,
    #[serde(default)]
    translation: Option<TrackDef<[f32; 3]>>,
    #[serde(default)]
    rotation: Option<TrackDef<[f32; 4]>>,
    #[serde(default)]
    scale: Option<TrackDef<[f32; 3]>>,
}

#[derive(Serialize, Deserialize)]
struct RootMotionDef {
    joint: usize,
    translation_axes: [bool; 3],
    rotation: bool,
}

#[derive(Serialize, Deserialize)]
struct ClipDef {
    name: String,
    duration: f32,
    #[serde(default)]
    looping: bool,
    channels: Vec<ChannelDef>,
    #[serde(default)]
    root_motion: Option<RootMotionDef>,
}

#[derive(Serialize, Deserialize)]
struct SetDef {
    joints: Vec<JointDef>,
    /// Column-major, one per joint. Omit to derive from the bind pose.
    #[serde(default)]
    inverse_bind: Option<Vec<[f32; 16]>>,
    #[serde(default)]
    root_transform: Option<[f32; 16]>,
    clips: Vec<ClipDef>,
}

fn track_def<T: Copy, U>(t: &Track<T>, f: impl Fn(T) -> U) -> TrackDef<U> {
    TrackDef { times: t.times.clone(), values: t.values.iter().map(|v| f(*v)).collect(), interpolation: t.interpolation }
}

impl AnimationSet {
    pub fn to_ron_string(&self) -> anyhow::Result<String> {
        let sk = &self.skeleton;
        let def = SetDef {
            joints: sk
                .joints()
                .iter()
                .map(|j| JointDef {
                    name: j.name.clone(),
                    parent: j.parent,
                    translation: j.bind.translation.into(),
                    rotation: j.bind.rotation.to_array(),
                    scale: j.bind.scale.into(),
                })
                .collect(),
            inverse_bind: Some(sk.inverse_bind().iter().map(|m| m.to_cols_array()).collect()),
            root_transform: Some(sk.root_transform.to_cols_array()),
            clips: self
                .clips
                .iter()
                .map(|c| ClipDef {
                    name: c.name.clone(),
                    duration: c.duration,
                    looping: c.looping,
                    channels: c
                        .channels
                        .iter()
                        .map(|ch| ChannelDef {
                            joint: ch.joint,
                            translation: ch.translation.as_ref().map(|t| track_def(t, |v: Vec3| v.to_array())),
                            rotation: ch.rotation.as_ref().map(|t| track_def(t, |q: Quat| q.to_array())),
                            scale: ch.scale.as_ref().map(|t| track_def(t, |v: Vec3| v.to_array())),
                        })
                        .collect(),
                    root_motion: c.root_motion.map(|r| RootMotionDef {
                        joint: r.joint,
                        translation_axes: r.translation_axes,
                        rotation: r.rotation,
                    }),
                })
                .collect(),
        };
        Ok(ron::ser::to_string_pretty(&def, ron::ser::PrettyConfig::default())?)
    }

    pub fn from_ron_str(text: &str) -> anyhow::Result<Self> {
        let def: SetDef = ron::from_str(text)?;
        let joints: Vec<Joint> = def
            .joints
            .into_iter()
            .map(|j| Joint {
                name: j.name,
                parent: j.parent,
                bind: JointTransform::new(Vec3::from(j.translation), Quat::from_array(j.rotation).normalize(), Vec3::from(j.scale)),
            })
            .collect();
        let had_ibm = def.inverse_bind.is_some();
        let mut skeleton = match def.inverse_bind {
            Some(ibm) => Skeleton::with_inverse_bind(joints, ibm.iter().map(Mat4::from_cols_array).collect())?,
            None => Skeleton::new(joints)?,
        };
        if let Some(rt) = def.root_transform {
            skeleton.root_transform = Mat4::from_cols_array(&rt);
            // Derived inverse-bind depends on the root transform.
            if !had_ibm {
                skeleton.recompute_inverse_bind();
            }
        }
        let mut clips = Vec::new();
        for c in def.clips {
            let mut channels = Vec::new();
            for ch in c.channels {
                channels.push(JointChannel {
                    joint: ch.joint,
                    translation: ch
                        .translation
                        .map(|t| Track::new(t.times, t.values.into_iter().map(Vec3::from).collect(), t.interpolation))
                        .transpose()?,
                    rotation: ch
                        .rotation
                        .map(|t| {
                            Track::new(
                                t.times,
                                t.values.into_iter().map(|q| Quat::from_array(q).normalize()).collect(),
                                t.interpolation,
                            )
                        })
                        .transpose()?,
                    scale: ch
                        .scale
                        .map(|t| Track::new(t.times, t.values.into_iter().map(Vec3::from).collect(), t.interpolation))
                        .transpose()?,
                });
            }
            clips.push(AnimationClip {
                name: c.name,
                duration: c.duration,
                looping: c.looping,
                channels,
                root_motion: c.root_motion.map(|r| super::clip::RootMotion {
                    joint: r.joint,
                    translation_axes: r.translation_axes,
                    rotation: r.rotation,
                }),
            });
        }
        Ok(AnimationSet { skeleton, clips })
    }

    pub fn load_from_file(path: &Path) -> anyhow::Result<Self> {
        Self::from_ron_str(&std::fs::read_to_string(path)?)
    }

    pub fn save_to_file(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, self.to_ron_string()?)?;
        Ok(())
    }
}

/// Converter hook: glTF/GLB -> RON `AnimationSet` (skeleton + clips). The
/// skinned mesh data is not part of the RON form; load it from the glTF.
pub fn convert_gltf_to_ron(gltf_path: &Path, ron_path: &Path) -> anyhow::Result<()> {
    load_gltf_skinned(gltf_path)?.set.save_to_file(ron_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    fn f32s(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn translate(x: f32, y: f32, z: f32) -> [f32; 16] {
        [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., x, y, z, 1.]
    }

    /// Hand-built GLB. Node graph:
    ///   0 "armature" T(10,0,0)
    ///     1 "j_root"
    ///       3 "j_tip" T(0,1,0)
    ///     2 "body" (mesh 0, skin 0)
    /// skin.joints = [3, 1] (tip first, so import must reorder).
    fn test_glb() -> Vec<u8> {
        let mut bin: Vec<u8> = Vec::new();
        // 0: positions @0 (36 bytes)
        bin.extend(f32s(&[0., 0., 0., 0., 1., 0., 0., 0.5, 0.]));
        // 1: JOINTS_0 u8x4 @36 (12 bytes). skin order: 0 = tip, 1 = root.
        bin.extend([1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]);
        // 2: WEIGHTS_0 @48 (48 bytes)
        bin.extend(f32s(&[1., 0., 0., 0., 1., 0., 0., 0., 0.5, 0.5, 0., 0.]));
        // 3: indices u16 @96 (6 bytes) + 2 pad
        bin.extend([0u8, 0, 1, 0, 2, 0, 0, 0]);
        // 4: inverse bind @104 (128 bytes): tip = T(-10,-1,0), root = T(-10,0,0)
        bin.extend(f32s(&translate(-10., -1., 0.)));
        bin.extend(f32s(&translate(-10., 0., 0.)));
        // 5: times @232 (8)
        bin.extend(f32s(&[0., 1.]));
        // 6: tip translation @240 (24)
        bin.extend(f32s(&[0., 1., 0., 0., 3., 0.]));
        // 7: root rotation @264 (32): identity -> Rz(90)
        let s = (FRAC_PI_2 / 2.0).sin();
        let c = (FRAC_PI_2 / 2.0).cos();
        bin.extend(f32s(&[0., 0., 0., 1., 0., 0., s, c]));
        assert_eq!(bin.len(), 296);

        let json = r#"{
 "asset": {"version": "2.0"},
 "scene": 0,
 "scenes": [{"nodes": [0]}],
 "nodes": [
  {"name": "armature", "translation": [10, 0, 0], "children": [1, 2]},
  {"name": "j_root", "children": [3]},
  {"name": "body", "mesh": 0, "skin": 0},
  {"name": "j_tip", "translation": [0, 1, 0]}
 ],
 "skins": [{"joints": [3, 1], "inverseBindMatrices": 4, "skeleton": 1}],
 "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "JOINTS_0": 1, "WEIGHTS_0": 2}, "indices": 3}]}],
 "animations": [{
   "name": "wave",
   "samplers": [
     {"input": 5, "output": 6, "interpolation": "LINEAR"},
     {"input": 5, "output": 7, "interpolation": "LINEAR"}
   ],
   "channels": [
     {"sampler": 0, "target": {"node": 3, "path": "translation"}},
     {"sampler": 1, "target": {"node": 1, "path": "rotation"}},
     {"sampler": 0, "target": {"node": 2, "path": "translation"}}
   ]
 }],
 "buffers": [{"byteLength": 296}],
 "bufferViews": [
  {"buffer": 0, "byteOffset": 0,   "byteLength": 36},
  {"buffer": 0, "byteOffset": 36,  "byteLength": 12},
  {"buffer": 0, "byteOffset": 48,  "byteLength": 48},
  {"buffer": 0, "byteOffset": 96,  "byteLength": 6},
  {"buffer": 0, "byteOffset": 104, "byteLength": 128},
  {"buffer": 0, "byteOffset": 232, "byteLength": 8},
  {"buffer": 0, "byteOffset": 240, "byteLength": 24},
  {"buffer": 0, "byteOffset": 264, "byteLength": 32}
 ],
 "accessors": [
  {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [0, 1, 0]},
  {"bufferView": 1, "componentType": 5121, "count": 3, "type": "VEC4"},
  {"bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC4"},
  {"bufferView": 3, "componentType": 5123, "count": 3, "type": "SCALAR"},
  {"bufferView": 4, "componentType": 5126, "count": 2, "type": "MAT4"},
  {"bufferView": 5, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0], "max": [1]},
  {"bufferView": 6, "componentType": 5126, "count": 2, "type": "VEC3"},
  {"bufferView": 7, "componentType": 5126, "count": 2, "type": "VEC4"}
 ]
}"#;
        let mut json = json.as_bytes().to_vec();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend(b"glTF");
        out.extend(2u32.to_le_bytes());
        out.extend((total as u32).to_le_bytes());
        out.extend((json.len() as u32).to_le_bytes());
        out.extend(b"JSON");
        out.extend(&json);
        out.extend((bin.len() as u32).to_le_bytes());
        out.extend(b"BIN\0");
        out.extend(&bin);
        out
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn imports_skeleton_reordered_parents_first() {
        let m = load_gltf_skinned_from_slice(&test_glb()).unwrap();
        let sk = &m.set.skeleton;
        assert_eq!(sk.len(), 2);
        assert_eq!(sk.joints()[0].name, "j_root");
        assert_eq!(sk.joints()[0].parent, None);
        assert_eq!(sk.joints()[1].name, "j_tip");
        assert_eq!(sk.joints()[1].parent, Some(0));
        assert_eq!(sk.joints()[1].bind.translation, Vec3::Y);
        // armature node folded into root_transform
        assert!(close(sk.root_transform.transform_point3(Vec3::ZERO), Vec3::new(10.0, 0.0, 0.0)));
        // file IBMs were permuted into the new order: [root, tip]
        assert!(close(sk.inverse_bind()[0].transform_point3(Vec3::ZERO), Vec3::new(-10.0, 0.0, 0.0)));
        assert!(close(sk.inverse_bind()[1].transform_point3(Vec3::ZERO), Vec3::new(-10.0, -1.0, 0.0)));
        // bind palette is the identity
        for p in sk.joint_palette(&sk.bind_pose()) {
            assert!(p.to_cols_array().iter().zip(Mat4::IDENTITY.to_cols_array()).all(|(a, b)| (a - b).abs() < 1e-5));
        }
    }

    #[test]
    fn imports_clips_and_skips_non_joint_channels() {
        let m = load_gltf_skinned_from_slice(&test_glb()).unwrap();
        assert_eq!(m.set.clips.len(), 1);
        let clip = &m.set.clips[0];
        assert_eq!(clip.name, "wave");
        assert_eq!(clip.duration, 1.0);
        assert!(!clip.looping);
        // two joint channels (root rotation, tip translation); the channel on node 2 is dropped
        assert_eq!(clip.channels.len(), 2);
        let pose = clip.sample(&m.set.skeleton, 0.5);
        assert!(close(pose.joints[1].translation, Vec3::new(0.0, 2.0, 0.0)));
        let expect = Quat::from_rotation_z(FRAC_PI_2 / 2.0);
        assert!(pose.joints[0].rotation.dot(expect) > 0.99999);
    }

    #[test]
    fn imports_skin_weights_with_remapped_joint_indices() {
        let m = load_gltf_skinned_from_slice(&test_glb()).unwrap();
        assert_eq!(m.meshes.len(), 1);
        let d = &m.meshes[0].data;
        assert_eq!(m.meshes[0].name, "body");
        assert_eq!(d.mesh.vertices.len(), 3);
        assert_eq!(d.mesh.indices, vec![0, 1, 2]);
        // file: v0 -> skin joint 1 (root) ; new index 0
        assert_eq!(d.skin[0].joints[0], 0);
        assert_eq!(d.skin[0].weights, [1.0, 0.0, 0.0, 0.0]);
        // v1 -> skin joint 0 (tip); new index 1
        assert_eq!(d.skin[1].joints[0], 1);
        // v2 -> half root half tip
        assert_eq!(&d.skin[2].joints[..2], &[0, 1]);
        assert_eq!(&d.skin[2].weights[..2], &[0.5, 0.5]);
    }

    #[test]
    fn end_to_end_animated_skinning_matches_hand_computation() {
        let m = load_gltf_skinned_from_slice(&test_glb()).unwrap();
        let pose = m.set.clips[0].sample(&m.set.skeleton, 1.0);
        let palette = m.set.skeleton.joint_palette(&pose);
        let out = m.meshes[0].data.skin(&palette);
        // At t=1: root rotated Rz(90), tip translation (0,3,0).
        // v1 (0,1,0) is fully on the tip:
        //   IBM_tip T(-10,-1,0): (-10,0,0); tip local T(0,3,0): (-10,3,0)
        //   root Rz90 (x,y)->(-y,x): (-3,-10,0); root_transform T(10,0,0): (7,-10,0)
        assert!(close(out[1].position.into(), Vec3::new(7.0, -10.0, 0.0)), "{:?}", out[1].position);
        // v0 (0,0,0) is fully on the root:
        //   IBM_root T(-10,0,0): (-10,0,0); Rz90: (0,-10,0); +T(10,0,0): (10,-10,0)
        assert!(close(out[0].position.into(), Vec3::new(10.0, -10.0, 0.0)), "{:?}", out[0].position);
        // v2 (0,0.5,0) half/half:
        //   root: (-10,0.5,0) -> Rz90 (-0.5,-10,0) -> (9.5,-10,0)
        //   tip : (-10,-0.5,0) -> +T(0,3,0): (-10,2.5,0) -> Rz90: (-2.5,-10,0) -> (7.5,-10,0)
        //   blend (8.5,-10,0)
        assert!(close(out[2].position.into(), Vec3::new(8.5, -10.0, 0.0)), "{:?}", out[2].position);
    }

    #[test]
    fn ron_round_trip_preserves_everything() {
        let m = load_gltf_skinned_from_slice(&test_glb()).unwrap();
        let text = m.set.to_ron_string().unwrap();
        let back = AnimationSet::from_ron_str(&text).unwrap();
        assert_eq!(back.skeleton, m.set.skeleton);
        assert_eq!(back.clips, m.set.clips);
    }

    #[test]
    fn ron_without_inverse_bind_derives_it() {
        let text = r#"(
            joints: [
                (name: "a", parent: None, translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
                (name: "b", parent: Some(0), translation: (0.0, 1.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            ],
            clips: [],
        )"#;
        let set = AnimationSet::from_ron_str(text).unwrap();
        assert!(close(set.skeleton.inverse_bind()[1].transform_point3(Vec3::Y), Vec3::ZERO));
    }

    #[test]
    fn convert_hook_writes_a_loadable_file() {
        let dir = std::env::temp_dir().join(format!("skeletal_import_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let glb = dir.join("m.glb");
        let out = dir.join("m.ron");
        std::fs::write(&glb, test_glb()).unwrap();
        convert_gltf_to_ron(&glb, &out).unwrap();
        let set = AnimationSet::load_from_file(&out).unwrap();
        assert_eq!(set.skeleton.len(), 2);
        assert_eq!(set.clips[0].name, "wave");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gltf_without_skin_is_an_error() {
        let glb = {
            let json = br#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[]}],"scene":0}"#;
            let mut j = json.to_vec();
            while j.len() % 4 != 0 {
                j.push(b' ');
            }
            let mut out = Vec::new();
            out.extend(b"glTF");
            out.extend(2u32.to_le_bytes());
            out.extend(((12 + 8 + j.len()) as u32).to_le_bytes());
            out.extend((j.len() as u32).to_le_bytes());
            out.extend(b"JSON");
            out.extend(&j);
            out
        };
        let err = load_gltf_skinned_from_slice(&glb).err().expect("should fail");
        assert!(err.to_string().contains("no skin"), "{err}");
    }
}
