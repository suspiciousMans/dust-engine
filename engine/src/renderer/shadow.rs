use glam::{Mat4, Vec3};
use glow::HasContext;

use super::shadow_math::{
    const_bias_depth, normal_offset_world, slope_bias_depth, LightFit, ShadowBounds,
    ShadowSettings,
};
use super::shadow_shader::{SHADOW_DEPTH_FRAG, SHADOW_DEPTH_VERT};
use crate::mesh::GpuMesh;
use crate::shader::link_program;

/// Depth-only render target + program for one directional-light shadow map.
/// Owned by `Renderer` (see `Renderer::set_shadow_settings`).
pub struct ShadowPass {
    fbo: glow::Framebuffer,
    depth_tex: glow::Texture,
    program: glow::Program,
    loc_vp: Option<glow::UniformLocation>,
    loc_model: Option<glow::UniformLocation>,
    size: u32,
    /// Fit used by the most recent `begin`; `None` until a pass ran.
    fit: Option<LightFit>,
    light_dir: Vec3,
    prev_cull: bool,
    prev_viewport: [i32; 4],
}

impl ShadowPass {
    pub fn new(gl: &glow::Context, size: u32) -> anyhow::Result<Self> {
        let size = size.max(1);
        unsafe {
            let program = link_program(gl, SHADOW_DEPTH_VERT, SHADOW_DEPTH_FRAG)?;
            let fbo = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
            let depth_tex = gl.create_texture().map_err(anyhow::Error::msg)?;
            gl.bind_texture(glow::TEXTURE_2D, Some(depth_tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::DEPTH_COMPONENT24 as i32,
                size as i32,
                size as i32,
                0,
                glow::DEPTH_COMPONENT,
                glow::UNSIGNED_INT,
                None,
            );
            // Plain (non-comparison) depth sampling: the shader compares.
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::TEXTURE_2D,
                Some(depth_tex),
                0,
            );
            gl.draw_buffers(&[glow::NONE]);
            gl.read_buffer(glow::NONE);
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(depth_tex);
                gl.delete_program(program);
                anyhow::bail!("shadow framebuffer incomplete: 0x{status:x}");
            }
            Ok(Self {
                fbo,
                depth_tex,
                loc_vp: gl.get_uniform_location(program, "uLightViewProj"),
                loc_model: gl.get_uniform_location(program, "uModel"),
                program,
                size,
                fit: None,
                light_dir: Vec3::Y,
                prev_cull: false,
                prev_viewport: [0, 0, 1, 1],
            })
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn fit(&self) -> Option<&LightFit> {
        self.fit.as_ref()
    }

    /// # Safety
    /// Must not be used after this call.
    pub unsafe fn destroy(&self, gl: &glow::Context) {
        gl.delete_framebuffer(self.fbo);
        gl.delete_texture(self.depth_tex);
        gl.delete_program(self.program);
    }

    /// Fits the light, binds the depth target, clears it and prepares state.
    /// Returns `false` (and draws nothing) when the bounds are degenerate.
    pub fn begin(
        &mut self,
        gl: &glow::Context,
        light_dir: Vec3,
        bounds: &ShadowBounds,
        settings: &ShadowSettings,
    ) -> bool {
        self.fit = bounds.fit(light_dir, settings);
        let Some(fit) = self.fit else { return false };
        self.light_dir = light_dir.try_normalize().unwrap_or(Vec3::Y);
        unsafe {
            let mut vp = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut vp);
            self.prev_viewport = vp;
            self.prev_cull = gl.is_enabled(glow::CULL_FACE);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.viewport(0, 0, self.size as i32, self.size as i32);
            gl.enable(glow::DEPTH_TEST);
            gl.depth_mask(true);
            // Both faces cast: single-sided planes and thin slabs still shadow.
            gl.disable(glow::CULL_FACE);
            gl.clear_depth_f32(1.0);
            gl.clear(glow::DEPTH_BUFFER_BIT);
            gl.use_program(Some(self.program));
            gl.uniform_matrix_4_f32_slice(self.loc_vp.as_ref(), false, &fit.view_proj.to_cols_array());
        }
        true
    }

    pub fn draw_mesh(&self, gl: &glow::Context, model: &Mat4, mesh: &GpuMesh) {
        unsafe {
            gl.uniform_matrix_4_f32_slice(self.loc_model.as_ref(), false, &model.to_cols_array());
        }
        mesh.draw(gl);
    }

    /// Leaves the shadow target: binds the default framebuffer (callers
    /// follow with `Renderer::begin_scene`), restores viewport + cull state.
    pub fn end(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            let v = self.prev_viewport;
            gl.viewport(v[0], v[1], v[2], v[3]);
            if self.prev_cull {
                gl.enable(glow::CULL_FACE);
            }
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }

    /// Uploads shadow uniforms to a program built from
    /// [`super::patch_mesh_shaders`] output and binds the depth map to
    /// texture `unit`. Leaves `program` current and `TEXTURE0` active.
    /// With no fitted pass this frame it sets `uShadowStrength = 0`.
    pub fn bind(
        &self,
        gl: &glow::Context,
        program: glow::Program,
        settings: &ShadowSettings,
        unit: u32,
    ) {
        unsafe {
            gl.use_program(Some(program));
            let loc = |n: &str| gl.get_uniform_location(program, n);
            let Some(fit) = self.fit else {
                gl.uniform_1_f32(loc("uShadowStrength").as_ref(), 0.0);
                return;
            };
            gl.active_texture(glow::TEXTURE0 + unit);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.depth_tex));
            gl.active_texture(glow::TEXTURE0);
            let (radius, step) = settings.pcf_kernel();
            gl.uniform_1_i32(loc("uShadowMap").as_ref(), unit as i32);
            gl.uniform_matrix_4_f32_slice(
                loc("uShadowMatrix").as_ref(),
                false,
                &fit.texture_matrix().to_cols_array(),
            );
            gl.uniform_3_f32(
                loc("uShadowLightDir").as_ref(),
                self.light_dir.x,
                self.light_dir.y,
                self.light_dir.z,
            );
            gl.uniform_1_f32(
                loc("uShadowNormalOffset").as_ref(),
                normal_offset_world(settings.normal_bias, fit.texel_world, 0.0),
            );
            gl.uniform_2_f32(
                loc("uShadowBias").as_ref(),
                const_bias_depth(settings.bias, fit.texel_world, fit.depth_range),
                slope_bias_depth(fit.texel_world, fit.depth_range),
            );
            gl.uniform_1_f32(loc("uShadowTexel").as_ref(), 1.0 / self.size as f32);
            gl.uniform_1_i32(loc("uShadowPcfRadius").as_ref(), radius);
            gl.uniform_1_f32(loc("uShadowPcfStep").as_ref(), step);
            gl.uniform_1_i32(loc("uShadowDither").as_ref(), settings.filter.dither_flag());
            gl.uniform_1_f32(loc("uShadowStrength").as_ref(), settings.strength.clamp(0.0, 1.0));
        }
    }
}
