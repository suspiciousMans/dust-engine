//! The scene render target shown in the Viewport tab: an offscreen
//! framebuffer that follows the tab's pixel size, registered with egui once
//! and re-pointed on resize.

use super::shell::ViewportImage;
use crate::renderer::OffscreenFramebuffer;
use crate::ui::EguiState;

pub const MIN_SIZE: u32 = 16;
pub const MAX_SIZE: u32 = 4096;

/// Clamps a requested viewport size to something the GPU will accept.
pub fn clamp_size(want: [u32; 2]) -> [u32; 2] {
    [want[0].clamp(MIN_SIZE, MAX_SIZE), want[1].clamp(MIN_SIZE, MAX_SIZE)]
}

/// Whether a framebuffer of size `have` must be rebuilt for request `want`.
pub fn needs_resize(have: Option<[u32; 2]>, want: [u32; 2]) -> bool {
    have != Some(clamp_size(want))
}

#[derive(Default)]
pub struct ViewportTarget {
    fb: Option<OffscreenFramebuffer>,
    egui_id: Option<egui::TextureId>,
}

impl ViewportTarget {
    pub fn size(&self) -> Option<[u32; 2]> {
        self.fb.as_ref().map(|f| [f.width, f.height])
    }

    /// Creates or resizes to `want` (clamped). Call outside the egui closure.
    pub fn ensure(&mut self, gl: &glow::Context, egui: &mut EguiState, want: [u32; 2]) -> anyhow::Result<()> {
        if !needs_resize(self.size(), want) {
            return Ok(());
        }
        let [w, h] = clamp_size(want);
        let fresh = OffscreenFramebuffer::new(gl, w, h)?;
        match self.egui_id {
            Some(id) => egui.replace_texture(id, fresh.color_texture),
            None => self.egui_id = Some(egui.register_texture(fresh.color_texture)),
        }
        if let Some(old) = self.fb.replace(fresh) {
            // egui's painter now owns (and will delete) the old color texture.
            unsafe { old.destroy_except_texture(gl) };
        }
        Ok(())
    }

    /// Binds the framebuffer (and sets the GL viewport) for scene drawing.
    pub fn bind(&self, gl: &glow::Context) -> bool {
        match &self.fb {
            Some(f) => {
                f.bind(gl);
                true
            }
            None => false,
        }
    }

    pub fn image(&self) -> Option<ViewportImage> {
        Some(ViewportImage { texture: self.egui_id?, size_px: self.size()? })
    }

    pub fn aspect(&self) -> f32 {
        self.size().map_or(1.0, |[w, h]| w as f32 / h.max(1) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_policy() {
        assert_eq!(clamp_size([0, 99999]), [MIN_SIZE, MAX_SIZE]);
        assert!(needs_resize(None, [100, 100]));
        assert!(!needs_resize(Some([100, 100]), [100, 100]));
        assert!(!needs_resize(Some([MIN_SIZE, MIN_SIZE]), [1, 1]), "clamped request equals current");
        assert!(needs_resize(Some([100, 100]), [101, 100]));
    }
}
