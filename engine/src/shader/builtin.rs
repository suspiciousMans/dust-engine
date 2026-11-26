//! Built-in "hi-fi" shaders: per-pixel metal/rough shading for meshes and an
//! HDR tonemapping composite. Opt-in; nothing in the engine uses them unless
//! a game does.
//!
//! ```ignore
//! use dust_engine::renderer::TargetOptions;
//! use dust_engine::shader::{builtin, ShaderVariantCache, TONEMAP_COMPOSITE_FRAG};
//!
//! let mut renderer = Renderer::new(gl, size, 1.0, TONEMAP_COMPOSITE_FRAG)?;
//! renderer.set_target_options(gl, TargetOptions::hifi())?;
//! let mut pbr = ShaderVariantCache::new(builtin::PBR_MESH_VERT, builtin::PBR_MESH_FRAG);
//! // per frame: program = pbr.get_or_compile(gl, 0)?; use it;
//! // lighting.upload(gl, program, camera_pos); per draw: material.upload(gl, program);
//! ```
//!
//! For shadows, run the two sources through `renderer::patch_mesh_shaders`
//! (output `FragColor`) before handing them to `ShaderVariantCache`.

use glow::HasContext;

/// Vertex stage (no `#version` line; for `ShaderVariantCache`). Supports
/// the `AFFINE_UV` and `INSTANCED` variant bits.
pub const PBR_MESH_VERT: &str = include_str!("builtin/pbr_mesh.vert");
/// Fragment stage (no `#version` line). Writes linear colour to `FragColor`.
pub const PBR_MESH_FRAG: &str = include_str!("builtin/pbr_mesh.frag");
/// A full composite fragment shader (with `#version`) for `Renderer::new` /
/// `set_composite_shader`: exposure, ACES, sRGB, tint, optional posterize.
pub const TONEMAP_COMPOSITE_FRAG: &str = include_str!("builtin/tonemap_composite.frag");

/// Point lights the built-in fragment shader loops over.
pub const MAX_POINT_LIGHTS: usize = 8;

/// One point light (linear colour; `intensity` scales it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointLight {
    pub position: [f32; 3],
    pub color: [f32; 3],
    pub intensity: f32,
    pub range: f32,
}

/// Per-frame scene lighting for the built-in PBR shader. Colours are linear.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneLighting {
    /// Direction *toward* the sun (normalised by the shader).
    pub sun_dir: [f32; 3],
    pub sun_color: [f32; 3],
    pub sky_color: [f32; 3],
    pub ground_color: [f32; 3],
    pub fog_color: [f32; 3],
    pub fog_start: f32,
    pub fog_end: f32,
    /// Only the first [`MAX_POINT_LIGHTS`] are used.
    pub point_lights: Vec<PointLight>,
}

impl Default for SceneLighting {
    /// A warm key light from upper left, cool sky fill, fog pushed far away.
    fn default() -> Self {
        Self {
            sun_dir: [-0.4, 1.0, 0.35],
            sun_color: [3.2, 3.0, 2.7],
            sky_color: [0.22, 0.26, 0.34],
            ground_color: [0.08, 0.07, 0.06],
            fog_color: [0.02, 0.02, 0.03],
            fog_start: 1.0e4,
            fog_end: 2.0e4,
            point_lights: Vec::new(),
        }
    }
}

impl SceneLighting {
    /// Uploads lighting, fog and the camera position to `program` (makes it
    /// current). Uniforms the program does not declare are skipped.
    pub fn upload(&self, gl: &glow::Context, program: glow::Program, camera_pos: [f32; 3]) {
        unsafe {
            gl.use_program(Some(program));
            let loc = |n: &str| gl.get_uniform_location(program, n);
            let v3 = |n: &str, v: [f32; 3]| gl.uniform_3_f32(loc(n).as_ref(), v[0], v[1], v[2]);
            v3("uCameraPos", camera_pos);
            v3("uLightDir", self.sun_dir);
            v3("uLightColor", self.sun_color);
            v3("uSkyColor", self.sky_color);
            v3("uGroundColor", self.ground_color);
            v3("uFogColor", self.fog_color);
            gl.uniform_1_f32(loc("uFogStart").as_ref(), self.fog_start);
            gl.uniform_1_f32(loc("uFogEnd").as_ref(), self.fog_end);
            let lights = &self.point_lights[..self.point_lights.len().min(MAX_POINT_LIGHTS)];
            gl.uniform_1_i32(loc("uPointLightCount").as_ref(), lights.len() as i32);
            for (i, l) in lights.iter().enumerate() {
                v3(&format!("uPointLightPos[{i}]"), l.position);
                v3(&format!("uPointLightColor[{i}]"), l.color);
                gl.uniform_1_f32(loc(&format!("uPointLightIntensity[{i}]")).as_ref(), l.intensity);
                gl.uniform_1_f32(loc(&format!("uPointLightRange[{i}]")).as_ref(), l.range);
            }
        }
    }
}

/// Per-draw surface values for the built-in PBR shader.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PbrMaterial {
    /// Linear RGB + alpha, multiplied with the base colour texture.
    pub base_color: [f32; 4],
    pub roughness: f32,
    pub metallic: f32,
    pub emissive: [f32; 3],
    pub normal_scale: f32,
    pub unlit: bool,
    /// Bound to unit 0 (`uTex`). `None` = the caller already bound one.
    pub base_texture: Option<glow::Texture>,
    /// Bound to unit 1 when present.
    pub normal_map: Option<glow::Texture>,
    /// Bound to unit 2 when present (G = roughness, B = metallic).
    pub metal_rough_map: Option<glow::Texture>,
}

impl Default for PbrMaterial {
    fn default() -> Self {
        Self {
            base_color: [1.0, 1.0, 1.0, 1.0],
            roughness: 0.5,
            metallic: 0.0,
            emissive: [0.0; 3],
            normal_scale: 1.0,
            unlit: false,
            base_texture: None,
            normal_map: None,
            metal_rough_map: None,
        }
    }
}

impl PbrMaterial {
    /// A plain painted-plastic look in `rgb` (linear).
    pub fn plastic(rgb: [f32; 3]) -> Self {
        Self { base_color: [rgb[0], rgb[1], rgb[2], 1.0], roughness: 0.45, ..Self::default() }
    }

    /// A metal in `rgb` (linear) with the given roughness.
    pub fn metal(rgb: [f32; 3], roughness: f32) -> Self {
        Self {
            base_color: [rgb[0], rgb[1], rgb[2], 1.0],
            roughness,
            metallic: 1.0,
            ..Self::default()
        }
    }

    /// Uploads the material and binds its textures. `program` must be current.
    pub fn upload(&self, gl: &glow::Context, program: glow::Program) {
        unsafe {
            let loc = |n: &str| gl.get_uniform_location(program, n);
            let c = self.base_color;
            gl.uniform_4_f32(loc("uBaseColorFactor").as_ref(), c[0], c[1], c[2], c[3]);
            gl.uniform_1_f32(loc("uRoughness").as_ref(), self.roughness);
            gl.uniform_1_f32(loc("uMetallic").as_ref(), self.metallic);
            let e = self.emissive;
            gl.uniform_3_f32(loc("uEmissive").as_ref(), e[0], e[1], e[2]);
            gl.uniform_1_f32(loc("uNormalScale").as_ref(), self.normal_scale);
            gl.uniform_1_i32(loc("uUnlit").as_ref(), self.unlit as i32);
            gl.uniform_1_i32(loc("uTex").as_ref(), 0);
            gl.uniform_1_i32(loc("uNormalMap").as_ref(), 1);
            gl.uniform_1_i32(loc("uMetalRoughMap").as_ref(), 2);
            if let Some(t) = self.base_texture {
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
            }
            gl.uniform_1_i32(loc("uHasNormalMap").as_ref(), self.normal_map.is_some() as i32);
            if let Some(t) = self.normal_map {
                gl.active_texture(glow::TEXTURE1);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
            }
            gl.uniform_1_i32(
                loc("uHasMetalRoughMap").as_ref(),
                self.metal_rough_map.is_some() as i32,
            );
            if let Some(t) = self.metal_rough_map {
                gl.active_texture(glow::TEXTURE2);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
            }
            gl.active_texture(glow::TEXTURE0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_stages_have_no_version_line_and_composite_does() {
        assert!(!PBR_MESH_VERT.contains("#version"));
        assert!(!PBR_MESH_FRAG.contains("#version"));
        assert!(TONEMAP_COMPOSITE_FRAG.trim_start().starts_with("#version 330 core"));
    }

    #[test]
    fn mesh_stages_meet_the_shadow_patcher_contract() {
        let (v, f) =
            crate::renderer::patch_mesh_shaders(PBR_MESH_VERT, PBR_MESH_FRAG, "FragColor").unwrap();
        assert!(v.contains("dust_user_main") && f.contains("dust_user_main"));
    }

    #[test]
    fn fragment_light_array_matches_the_rust_limit() {
        assert!(PBR_MESH_FRAG.contains(&format!("#define DUST_MAX_POINT_LIGHTS {MAX_POINT_LIGHTS}")));
    }

    #[test]
    fn composite_declares_every_uniform_the_composite_pass_drives() {
        for u in ["uSourceTex", "uExposure", "uTintColor", "uTintStrength", "uColorLevels", "uDitherStrength"] {
            assert!(TONEMAP_COMPOSITE_FRAG.contains(u), "{u}");
        }
    }
}
