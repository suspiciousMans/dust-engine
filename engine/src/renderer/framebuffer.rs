use glow::HasContext;

/// How the scene render target is allocated. The default reproduces the
/// classic Dust target exactly (RGBA8, no MSAA, `GL_NEAREST`), so the PS2
/// pixelation look is unchanged unless a game opts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetOptions {
    /// Store colour as `RGBA16F` so lighting can exceed 1.0 and be
    /// tonemapped in the composite pass. Falls back to `RGBA8` (with a
    /// warning) where float colour attachments are unsupported, which on
    /// WebGL2 means `EXT_color_buffer_float` is missing.
    pub hdr: bool,
    /// MSAA sample count: 0 or 1 = off. Clamped to `GL_MAX_SAMPLES`.
    pub samples: u8,
    /// Sample the resolved colour with `GL_LINEAR` instead of `GL_NEAREST`
    /// when it is scaled to the window (smooth instead of blocky).
    pub linear: bool,
}

impl Default for TargetOptions {
    fn default() -> Self {
        Self::retro()
    }
}

impl TargetOptions {
    /// The original low-res look: 8-bit colour, no MSAA, nearest upscale.
    pub const fn retro() -> Self {
        Self { hdr: false, samples: 0, linear: false }
    }

    /// Clean modern look: HDR colour, 4x MSAA, linear filtering. Pair with
    /// `resolution_scale = 1.0` and the [`crate::shader::TONEMAP_COMPOSITE_FRAG`]
    /// composite shader.
    pub const fn hifi() -> Self {
        Self { hdr: true, samples: 4, linear: true }
    }

    /// Sample count actually requested from GL (0 = single-sampled).
    pub fn effective_samples(&self, max_samples: i32) -> i32 {
        let s = self.samples as i32;
        if s <= 1 {
            0
        } else {
            s.min(max_samples.max(1))
        }
    }
}

/// The scene colour+depth render target. With default [`TargetOptions`] it
/// is a low-res RGBA8 texture sampled `GL_NEAREST` — sampling it into a
/// larger viewport during the composite pass is what produces the blocky
/// PS2-style pixelation. With MSAA on, the scene draws into multisampled
/// renderbuffers and [`OffscreenFramebuffer::resolve`] blits them into
/// `color_texture`.
pub struct OffscreenFramebuffer {
    fbo: glow::Framebuffer,
    pub color_texture: glow::Texture,
    depth_renderbuffer: glow::Renderbuffer,
    /// Multisampled draw target (fbo, colour rb, depth rb) when MSAA is on.
    msaa: Option<(glow::Framebuffer, glow::Renderbuffer, glow::Renderbuffer)>,
    pub width: u32,
    pub height: u32,
    options: TargetOptions,
    /// True when the colour attachment really is `RGBA16F`.
    hdr_active: bool,
}

impl OffscreenFramebuffer {
    pub fn new(gl: &glow::Context, width: u32, height: u32) -> anyhow::Result<Self> {
        Self::with_options(gl, width, height, TargetOptions::default())
    }

    /// Creates the target with the given options. An HDR request that the
    /// driver rejects is retried as `RGBA8`; an MSAA request that fails is
    /// retried without MSAA. Both are logged, never fatal.
    pub fn with_options(
        gl: &glow::Context,
        width: u32,
        height: u32,
        options: TargetOptions,
    ) -> anyhow::Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        if options.hdr {
            match Self::build(gl, width, height, options, true) {
                Ok(fb) => return Ok(fb),
                Err(e) => log::warn!("HDR render target unavailable, using RGBA8: {e}"),
            }
        }
        match Self::build(gl, width, height, options, false) {
            Ok(fb) => Ok(fb),
            Err(e) if options.samples > 1 => {
                log::warn!("MSAA render target unavailable, using no MSAA: {e}");
                let fallback = TargetOptions { samples: 0, ..options };
                Self::build(gl, width, height, fallback, false)
            }
            Err(e) => Err(e),
        }
    }

    fn build(
        gl: &glow::Context,
        width: u32,
        height: u32,
        options: TargetOptions,
        hdr: bool,
    ) -> anyhow::Result<Self> {
        let (internal, format, ty) = if hdr {
            (glow::RGBA16F, glow::RGBA, glow::HALF_FLOAT)
        } else {
            (glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE)
        };
        let filter = if options.linear { glow::LINEAR } else { glow::NEAREST };
        unsafe {
            let fbo = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));

            let color_texture = gl.create_texture().map_err(anyhow::Error::msg)?;
            gl.bind_texture(glow::TEXTURE_2D, Some(color_texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                width as i32,
                height as i32,
                0,
                format,
                ty,
                None,
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter as i32);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(color_texture),
                0,
            );

            let depth_renderbuffer = gl.create_renderbuffer().map_err(anyhow::Error::msg)?;
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth_renderbuffer));
            gl.renderbuffer_storage(
                glow::RENDERBUFFER,
                glow::DEPTH_COMPONENT24,
                width as i32,
                height as i32,
            );
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::RENDERBUFFER,
                Some(depth_renderbuffer),
            );

            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                // Free the three objects created above and unbind, or they'd
                // be orphaned with no handle to reclaim them.
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.bind_renderbuffer(glow::RENDERBUFFER, None);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.delete_renderbuffer(depth_renderbuffer);
                gl.delete_texture(color_texture);
                gl.delete_framebuffer(fbo);
                anyhow::bail!("offscreen framebuffer incomplete: 0x{status:x}");
            }

            let max_samples = gl.get_parameter_i32(glow::MAX_SAMPLES);
            let samples = options.effective_samples(max_samples);
            let msaa = if samples > 1 {
                match Self::build_msaa(gl, width, height, samples, internal) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        gl.bind_texture(glow::TEXTURE_2D, None);
                        gl.bind_renderbuffer(glow::RENDERBUFFER, None);
                        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                        gl.delete_renderbuffer(depth_renderbuffer);
                        gl.delete_texture(color_texture);
                        gl.delete_framebuffer(fbo);
                        return Err(e);
                    }
                }
            } else {
                None
            };

            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);

            Ok(Self {
                fbo,
                color_texture,
                depth_renderbuffer,
                msaa,
                width,
                height,
                options,
                hdr_active: hdr,
            })
        }
    }

    unsafe fn build_msaa(
        gl: &glow::Context,
        width: u32,
        height: u32,
        samples: i32,
        internal: u32,
    ) -> anyhow::Result<(glow::Framebuffer, glow::Renderbuffer, glow::Renderbuffer)> {
        let fbo = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        let color = gl.create_renderbuffer().map_err(anyhow::Error::msg)?;
        gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
        gl.renderbuffer_storage_multisample(
            glow::RENDERBUFFER,
            samples,
            internal,
            width as i32,
            height as i32,
        );
        gl.framebuffer_renderbuffer(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::RENDERBUFFER,
            Some(color),
        );
        let depth = gl.create_renderbuffer().map_err(anyhow::Error::msg)?;
        gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
        gl.renderbuffer_storage_multisample(
            glow::RENDERBUFFER,
            samples,
            glow::DEPTH_COMPONENT24,
            width as i32,
            height as i32,
        );
        gl.framebuffer_renderbuffer(
            glow::FRAMEBUFFER,
            glow::DEPTH_ATTACHMENT,
            glow::RENDERBUFFER,
            Some(depth),
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_renderbuffer(color);
            gl.delete_renderbuffer(depth);
            gl.delete_framebuffer(fbo);
            anyhow::bail!("multisampled framebuffer incomplete: 0x{status:x}");
        }
        Ok((fbo, color, depth))
    }

    /// The framebuffer scene draws go to: the multisampled one with MSAA on,
    /// otherwise the one that owns `color_texture`.
    pub(crate) fn raw(&self) -> glow::Framebuffer {
        self.msaa.map(|m| m.0).unwrap_or(self.fbo)
    }

    /// The options this target was asked for.
    pub fn options(&self) -> TargetOptions {
        self.options
    }

    /// True when colour is stored as `RGBA16F` (HDR was requested and the
    /// driver accepted it).
    pub fn is_hdr(&self) -> bool {
        self.hdr_active
    }

    /// MSAA is active (a multisampled draw target exists).
    pub fn is_multisampled(&self) -> bool {
        self.msaa.is_some()
    }

    pub fn bind(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.raw()));
            gl.viewport(0, 0, self.width as i32, self.height as i32);
        }
    }

    /// With MSAA on, blits the multisampled colour into `color_texture` so it
    /// can be sampled. A no-op without MSAA. `Renderer::present` calls this.
    pub fn resolve(&self, gl: &glow::Context) {
        let Some((msaa_fbo, _, _)) = self.msaa else { return };
        let (w, h) = (self.width as i32, self.height as i32);
        unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(msaa_fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.fbo));
            gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        }
    }

    /// # Safety
    /// The framebuffer must not be bound or used after this call.
    pub unsafe fn destroy(&self, gl: &glow::Context) {
        self.destroy_msaa(gl);
        gl.delete_framebuffer(self.fbo);
        gl.delete_texture(self.color_texture);
        gl.delete_renderbuffer(self.depth_renderbuffer);
    }

    /// Frees the framebuffer and depth buffer but NOT the color texture —
    /// for when something else (egui's painter) took ownership of it.
    ///
    /// # Safety
    /// The framebuffer must not be bound or used after this call.
    pub unsafe fn destroy_except_texture(&self, gl: &glow::Context) {
        self.destroy_msaa(gl);
        gl.delete_framebuffer(self.fbo);
        gl.delete_renderbuffer(self.depth_renderbuffer);
    }

    unsafe fn destroy_msaa(&self, gl: &glow::Context) {
        if let Some((fbo, color, depth)) = self.msaa {
            gl.delete_framebuffer(fbo);
            gl.delete_renderbuffer(color);
            gl.delete_renderbuffer(depth);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_the_classic_retro_target() {
        assert_eq!(TargetOptions::default(), TargetOptions { hdr: false, samples: 0, linear: false });
    }

    #[test]
    fn sample_count_is_off_below_two_and_clamped_to_the_driver_max() {
        let o = |samples| TargetOptions { samples, ..TargetOptions::hifi() };
        assert_eq!(o(0).effective_samples(8), 0);
        assert_eq!(o(1).effective_samples(8), 0);
        assert_eq!(o(4).effective_samples(8), 4);
        assert_eq!(o(16).effective_samples(8), 8);
        assert_eq!(o(4).effective_samples(0), 1);
    }
}
