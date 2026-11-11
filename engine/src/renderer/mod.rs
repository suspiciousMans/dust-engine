mod framebuffer;
mod grid;
mod particles;
mod post;
mod shadow;
mod shadow_math;
mod shadow_shader;
mod skybox;

pub use framebuffer::OffscreenFramebuffer;
pub use grid::GridPass;
pub use particles::ParticlePass;
pub use post::{
    CompositePass, PostParams, ScreenOverlay, TrailPass, DEFAULT_FRAGMENT_SRC, VIGNETTE_RINGS,
};
pub use shadow::ShadowPass;
pub use shadow_math::{
    aabb_corners, const_bias_depth, fit_directional, frustum_corners, is_lit, light_basis,
    normal_offset_world, ortho_from_aabb, project_to_shadow, quantize_texel_size,
    slope_bias_depth, slope_scaled_bias, FitOptions, LightFit, ShadowBounds, ShadowFilter,
    ShadowSettings,
};
pub use shadow_shader::{patch_mesh_shaders, SHADOW_DEPTH_FRAG, SHADOW_DEPTH_VERT};
pub use skybox::SkyboxPass;

use glow::HasContext;

use crate::mesh::GpuMesh;

/// Owns the low-res offscreen pass and the composite blit that upscales it
/// back to the window. Games render their scene between `begin_scene` and
/// `present`; everything in between lands in the low-res target.
pub struct Renderer {
    offscreen: OffscreenFramebuffer,
    composite: CompositePass,
    skybox: SkyboxPass,
    particles: ParticlePass,
    grid: Option<GridPass>,
    resolution_scale: f32,
    /// Vignette + grain applied by the composite pass (see `ScreenOverlay`).
    overlay: Option<ScreenOverlay>,
    /// Tracers: 0 = off. See `set_trails`.
    trail_amount: f32,
    trail: Option<TrailPass>,
    /// Ping-pong targets for tracers (created on first use).
    trail_targets: Vec<OffscreenFramebuffer>,
    trail_index: usize,
    /// Opt-in directional shadow map (see `set_shadow_settings`). `None`
    /// unless the game enabled shadows, so the default path allocates nothing.
    shadow: Option<ShadowPass>,
    shadow_settings: ShadowSettings,
    /// True between `begin_shadow_pass` and `end_shadow_pass`.
    shadow_open: bool,
}

/// Texture unit the shadow map is bound to by `Renderer::bind_shadows`.
pub const SHADOW_TEXTURE_UNIT: u32 = 7;

fn scaled_size((w, h): (u32, u32), scale: f32) -> (u32, u32) {
    (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    )
}

impl Renderer {
    pub fn new(
        gl: &glow::Context,
        drawable_size: (u32, u32),
        resolution_scale: f32,
        post_fragment_src: &str,
    ) -> anyhow::Result<Self> {
        let (w, h) = scaled_size(drawable_size, resolution_scale);
        Ok(Self {
            offscreen: OffscreenFramebuffer::new(gl, w, h)?,
            composite: CompositePass::new(gl, post_fragment_src)?,
            skybox: SkyboxPass::new(gl)?,
            particles: ParticlePass::new(gl)?,
            grid: None,
            resolution_scale,
            overlay: None,
            trail_amount: 0.0,
            trail: None,
            trail_targets: Vec::new(),
            trail_index: 0,
            shadow: None,
            shadow_settings: ShadowSettings::default(),
            shadow_open: false,
        })
    }

    /// Applies shadow settings. Disabled (the default) frees the shadow map;
    /// enabled (re)creates it when the resolution changed. A GL failure is
    /// logged and leaves shadows off. Cheap when nothing changed.
    pub fn set_shadow_settings(&mut self, gl: &glow::Context, mut settings: ShadowSettings) {
        settings.resolution = settings.clamped_resolution();
        if !settings.enabled {
            self.free_shadow(gl);
        } else if self.shadow.as_ref().map(|p| p.size()) != Some(settings.resolution) {
            self.free_shadow(gl);
            match ShadowPass::new(gl, settings.resolution) {
                Ok(p) => self.shadow = Some(p),
                Err(e) => {
                    log::error!("shadows disabled: {e}");
                    settings.enabled = false;
                }
            }
        }
        self.shadow_settings = settings;
    }

    pub fn shadow_settings(&self) -> &ShadowSettings {
        &self.shadow_settings
    }

    /// True when shadows are enabled and the GL resources exist.
    pub fn shadows_active(&self) -> bool {
        self.shadow_settings.enabled && self.shadow.is_some()
    }

    fn free_shadow(&mut self, gl: &glow::Context) {
        if let Some(p) = self.shadow.take() {
            unsafe { p.destroy(gl) };
        }
        self.shadow_open = false;
    }

    /// Starts the shadow-map pass for a directional light. `light_dir` points
    /// *toward* the light. Returns `false` (nothing to draw, skip casters and
    /// `end_shadow_pass`) when shadows are off or the bounds are degenerate.
    ///
    /// Call before `begin_scene`; draw casters with `shadow_draw`, then
    /// `end_shadow_pass`.
    pub fn begin_shadow_pass(
        &mut self,
        gl: &glow::Context,
        light_dir: glam::Vec3,
        bounds: &ShadowBounds,
    ) -> bool {
        if !self.shadow_settings.enabled {
            return false;
        }
        let settings = self.shadow_settings;
        let Some(pass) = self.shadow.as_mut() else { return false };
        self.shadow_open = pass.begin(gl, light_dir, bounds, &settings);
        self.shadow_open
    }

    /// Draws one caster into the open shadow pass. No-op outside a pass.
    pub fn shadow_draw(&self, gl: &glow::Context, model: &glam::Mat4, mesh: &GpuMesh) {
        if let (true, Some(p)) = (self.shadow_open, self.shadow.as_ref()) {
            p.draw_mesh(gl, model, mesh);
        }
    }

    /// Ends the shadow pass: binds the default framebuffer and restores the
    /// viewport/cull state. Follow with `begin_scene`.
    pub fn end_shadow_pass(&mut self, gl: &glow::Context) {
        if self.shadow_open {
            if let Some(p) = self.shadow.as_ref() {
                p.end(gl);
            }
            self.shadow_open = false;
        }
    }

    /// Uploads the shadow uniforms and binds the map (texture unit
    /// `SHADOW_TEXTURE_UNIT`) for `program`, which must be linked from
    /// `patch_mesh_shaders` output. Makes `program` current. When shadows are
    /// off or no pass ran this frame it sets `uShadowStrength = 0` instead.
    pub fn bind_shadows(&self, gl: &glow::Context, program: glow::Program) {
        match (&self.shadow, self.shadow_settings.enabled) {
            (Some(p), true) => p.bind(gl, program, &self.shadow_settings, SHADOW_TEXTURE_UNIT),
            _ => unsafe {
                gl.use_program(Some(program));
                let loc = gl.get_uniform_location(program, "uShadowStrength");
                gl.uniform_1_f32(loc.as_ref(), 0.0);
            },
        }
    }

    /// Draws the sky gradient — call right after `begin_scene`, before any
    /// mesh, so scene geometry draws over it normally.
    pub fn draw_skybox(
        &self,
        gl: &glow::Context,
        inv_view_proj: [f32; 16],
        horizon_color: [f32; 3],
        zenith_color: [f32; 3],
    ) {
        self.skybox
            .draw(gl, inv_view_proj, horizon_color, zenith_color);
    }

    /// Draws every `(mesh, model, color)` particle, blended — call after
    /// the opaque mesh loop so particles composite over real geometry.
    pub fn draw_particles(
        &self,
        gl: &glow::Context,
        view: &[f32; 16],
        proj: &[f32; 16],
        particles: impl Iterator<Item = (std::sync::Arc<GpuMesh>, [f32; 16], [f32; 4])>,
    ) {
        self.particles.begin(gl, view, proj);
        for (mesh, model, color) in particles {
            self.particles.draw_particle(gl, &mesh, &model, color);
        }
        self.particles.end(gl);
    }

    /// Editor ground grid (y = 0) into the bound scene target. Compiled on
    /// first use; a shader failure is logged once and the grid stays off.
    pub fn draw_grid(&mut self, gl: &glow::Context, view_proj: glam::Mat4, fade_distance: f32) {
        if self.grid.is_none() {
            match GridPass::new(gl) {
                Ok(g) => self.grid = Some(g),
                Err(e) => {
                    log::error!("editor grid disabled: {e}");
                    return;
                }
            }
        }
        if let Some(g) = &self.grid {
            g.draw(gl, view_proj.to_cols_array(), view_proj.inverse().to_cols_array(), fade_distance);
        }
    }

    /// Recompiles the composite pass with a different post fragment shader
    /// (e.g. when a shader profile is switched).
    pub fn set_composite_shader(
        &mut self,
        gl: &glow::Context,
        post_fragment_src: &str,
    ) -> anyhow::Result<()> {
        // Build the replacement first, so a compile/link failure leaves the
        // existing pass intact. Only once the new pass is ready do we tear
        // down the old one's GL program and VAO — `CompositePass` has no
        // `Drop`, so without this explicit `destroy` those objects would
        // leak in the driver on every shader-profile switch (unlike
        // `resize_if_needed`, which already destroys the old framebuffer).
        let mut composite = CompositePass::new(gl, post_fragment_src)?;
        composite.grain_tex = self.composite.grain_tex.take();
        unsafe {
            self.composite.destroy(gl);
        }
        self.composite = composite;
        Ok(())
    }

    /// Tracers: 0 = off, up to ~0.9 = long smeared trails behind anything
    /// that moves. Takes effect on the next `present`.
    pub fn set_trails(&mut self, amount: f32) {
        self.trail_amount = amount.clamp(0.0, 0.95);
    }

    /// The vignette + grain for the next `present` (`None` = none). Needs a
    /// composite shader with the `uOv*` uniforms; others ignore it.
    pub fn set_overlay(&mut self, overlay: Option<ScreenOverlay>) {
        self.overlay = overlay;
    }

    /// Uploads the film-grain tile (premultiplied RGBA8, `size` x `size`).
    pub fn set_grain_texture(&mut self, gl: &glow::Context, size: u32, rgba: &[u8]) {
        self.composite.set_grain_texture(gl, size, rgba);
    }

    pub fn set_resolution_scale(&mut self, scale: f32) {
        self.resolution_scale = scale;
    }

    /// Recreates the offscreen target if the window size or resolution scale
    /// changed since the last frame. Cheap no-op otherwise.
    pub fn resize_if_needed(
        &mut self,
        gl: &glow::Context,
        drawable_size: (u32, u32),
    ) -> anyhow::Result<()> {
        let (w, h) = scaled_size(drawable_size, self.resolution_scale);
        if (w, h) != (self.offscreen.width, self.offscreen.height) {
            unsafe {
                self.offscreen.destroy(gl);
            }
            self.offscreen = OffscreenFramebuffer::new(gl, w, h)?;
        }
        Ok(())
    }

    /// Binds the low-res offscreen target and sets its viewport. Call once
    /// per frame before issuing scene draw calls.
    ///
    /// Also unconditionally re-enables `GL_DEPTH_TEST`: an embedder's egui
    /// pass (drawn after `present()`, on top of the composited frame) draws
    /// through `egui_glow`'s own painter, which disables depth testing for
    /// its 2D drawing and does not restore it afterward. Since depth testing
    /// was otherwise only ever enabled once at app startup, every frame
    /// after the first egui paint would render the entire 3D scene with it
    /// off — occlusion silently falling back to raw draw order instead of
    /// actual distance, which reads as solid objects being invisible/
    /// see-through from certain angles. Re-enabling it here, before any
    /// scene geometry is drawn, makes each frame self-sufficient regardless
    /// of whatever GL state the previous frame's UI pass left behind.
    pub fn begin_scene(&self, gl: &glow::Context) {
        self.offscreen.bind(gl);
        unsafe {
            gl.enable(glow::DEPTH_TEST);
        }
    }

    /// Unbinds the offscreen target and blits it to the default framebuffer
    /// at the window's full drawable size.
    pub fn present(
        &mut self,
        gl: &glow::Context,
        window_drawable_size: (u32, u32),
        post_params: &PostParams,
    ) {
        self.present_to(gl, None, window_drawable_size, post_params);
    }

    /// Like [`present`](Self::present) but composites into `target` (the
    /// editor viewport framebuffer) instead of the window when given.
    /// `size` is the target's pixel size.
    pub fn present_to(
        &mut self,
        gl: &glow::Context,
        target: Option<&OffscreenFramebuffer>,
        window_drawable_size: (u32, u32),
        post_params: &PostParams,
    ) {
        let source = self.apply_trails(gl);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, target.map(|t| t.raw()));
            gl.viewport(
                0,
                0,
                window_drawable_size.0 as i32,
                window_drawable_size.1 as i32,
            );
        }
        self.composite.draw(
            gl,
            source,
            post_params,
            self.overlay.as_ref(),
            window_drawable_size.1 as f32,
        );
    }

    /// Blends the frame into the tracer ping-pong targets and returns the
    /// texture the composite pass should sample.
    fn apply_trails(&mut self, gl: &glow::Context) -> glow::Texture {
        let (w, h) = (self.offscreen.width, self.offscreen.height);
        if self.trail_amount <= 0.0 {
            // Drop the history so trails start clean next time.
            for t in self.trail_targets.drain(..) {
                unsafe { t.destroy(gl) };
            }
            return self.offscreen.color_texture;
        }
        let stale = self
            .trail_targets
            .first()
            .is_some_and(|t| (t.width, t.height) != (w, h));
        if stale || self.trail_targets.is_empty() {
            for t in self.trail_targets.drain(..) {
                unsafe { t.destroy(gl) };
            }
            for _ in 0..2 {
                match OffscreenFramebuffer::new(gl, w, h) {
                    Ok(t) => {
                        t.bind(gl);
                        unsafe {
                            gl.clear_color(0.0, 0.0, 0.0, 1.0);
                            gl.clear(glow::COLOR_BUFFER_BIT);
                        }
                        self.trail_targets.push(t);
                    }
                    Err(e) => {
                        log::warn!("tracers disabled: {e}");
                        self.trail_amount = 0.0;
                        return self.offscreen.color_texture;
                    }
                }
            }
        }
        if self.trail.is_none() {
            match TrailPass::new(gl) {
                Ok(p) => self.trail = Some(p),
                Err(e) => {
                    log::warn!("tracers disabled: {e}");
                    self.trail_amount = 0.0;
                    return self.offscreen.color_texture;
                }
            }
        }
        let prev = self.trail_index;
        let next = 1 - prev;
        self.trail_targets[next].bind(gl);
        if let Some(pass) = &self.trail {
            pass.draw(
                gl,
                self.offscreen.color_texture,
                self.trail_targets[prev].color_texture,
                self.trail_amount,
            );
        }
        self.trail_index = next;
        self.trail_targets[next].color_texture
    }
}
