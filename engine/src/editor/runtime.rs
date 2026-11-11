//! `EditorRuntime`: everything a game needs to host the editor shell, in one
//! struct, so wiring a game takes a handful of calls instead of a copy of the
//! editor code.
//!
//! Per frame the game does:
//! 1. `handle_event` for every SDL event;
//! 2. `ensure_target` + `view_proj` to learn where and how to render;
//! 3. renders its scene into `target` (`Renderer::present_to`);
//! 4. `run_ui` to draw the docked editor around it and run gizmos/picking.

use std::path::PathBuf;
use std::sync::Arc;

use glam::Mat4;
use glow::HasContext;
use hecs::World;
use sdl2::event::Event;

use super::cam_matrices::CameraMatrices;
use super::inspector::Registry;
use super::interaction::{ViewportEvent, ViewportInput, ViewportInteraction};
use super::shell::{EditorShell, PerfSnapshot, ShellHost, ViewportImage};
use super::state::{EditorState, KeymapPreset};
use super::viewport_target::ViewportTarget;
use crate::ui::EguiState;

/// A host with no game-specific tabs and default bounds.
pub struct NoHost;
impl ShellHost for NoHost {}

/// Adds the viewport image to whatever host the game passes in.
struct WithImage<'a> {
    image: Option<ViewportImage>,
    inner: &'a mut dyn ShellHost,
}

impl ShellHost for WithImage<'_> {
    fn viewport_image(&mut self) -> Option<ViewportImage> {
        self.image
    }
    fn tab_ui(&mut self, tab: &crate::ui::EditorTab, world: &mut World, ui: &mut egui::Ui) -> bool {
        self.inner.tab_ui(tab, world, ui)
    }
    fn selection_bounds(&mut self, world: &World, selection: &[hecs::Entity]) -> Option<(glam::Vec3, glam::Vec3)> {
        self.inner.selection_bounds(world, selection)
    }
}

pub struct EditorRuntime {
    pub egui: EguiState,
    pub shell: EditorShell,
    pub state: EditorState,
    pub target: ViewportTarget,
    pub interaction: ViewportInteraction,
    /// Editor on/off. When off the game runs as a plain game.
    pub open: bool,
    config_dir: Option<PathBuf>,
}

impl EditorRuntime {
    /// `config_dir` holds saved prefs and dock layout (created on save).
    pub fn new(gl: Arc<glow::Context>, registry: Registry, config_dir: Option<PathBuf>, host_tabs: Vec<String>) -> anyhow::Result<Self> {
        let shell = match &config_dir {
            Some(dir) => EditorShell::load(dir, host_tabs),
            None => EditorShell::new(host_tabs),
        };
        Ok(Self {
            egui: EguiState::new(gl)?,
            shell,
            state: EditorState::with_registry(registry),
            target: ViewportTarget::default(),
            interaction: ViewportInteraction::new(),
            open: false,
            config_dir,
        })
    }

    pub fn handle_event(&mut self, event: &Event) {
        if self.open {
            self.egui.handle_event(event);
        }
    }

    pub fn wants_pointer(&self) -> bool {
        self.open && self.egui.wants_pointer_input()
    }

    pub fn wants_keyboard(&self) -> bool {
        self.open && self.egui.wants_keyboard_input()
    }

    /// True while the scene should be simulated (Play, not Edit/Pause).
    pub fn is_simulating(&self) -> bool {
        !self.open || self.state.mode == super::state::EditorMode::Play
    }

    /// Creates/resizes the viewport framebuffer to match the Viewport tab
    /// (`fallback` until the tab has been laid out once). Returns its size.
    pub fn ensure_target(&mut self, gl: &glow::Context, fallback: (u32, u32)) -> anyhow::Result<(u32, u32)> {
        let want = match self.shell.viewport.rect {
            Some(_) => self.shell.viewport.size_px,
            None => [fallback.0, fallback.1],
        };
        self.target.ensure(gl, &mut self.egui, want)?;
        let [w, h] = self.target.size().unwrap_or([fallback.0, fallback.1]);
        Ok((w, h))
    }

    /// View and projection of the editor camera for the current viewport.
    pub fn view_proj(&self) -> (Mat4, Mat4) {
        (self.shell.camera.view_matrix(), self.shell.camera.projection_matrix(self.target.aspect()))
    }

    /// Draws the whole editor over the window and runs picking/gizmos.
    /// Returns the viewport events from this frame (selection changed,
    /// transform committed, ...).
    pub fn run_ui(
        &mut self,
        gl: &glow::Context,
        drawable: (u32, u32),
        world: &mut World,
        host: &mut dyn ShellHost,
        perf: PerfSnapshot,
    ) -> Vec<ViewportEvent> {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, drawable.0 as i32, drawable.1 as i32);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.clear_color(0.09, 0.09, 0.10, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        self.shell.perf = perf;
        let image = self.target.image();
        let Self { egui, shell, state, interaction, .. } = self;
        let mut scale = 1.0;
        let mut events = Vec::new();
        let output = egui.run(drawable, |ctx| {
            let mut host = WithImage { image, inner: host };
            scale = shell.ui(ctx, state, world, &mut host);
            if let Some(rect) = shell.viewport.rect {
                let aspect = rect.width() / rect.height().max(1.0);
                let cam = CameraMatrices::new(shell.camera.view_matrix(), shell.camera.projection_matrix(aspect), rect);
                let mut input = ctx.input(|i| ViewportInput::from_egui(i, shell.viewport.hovered));
                // Unity-like keymap already uses R for the scale tool.
                input.modal_hotkeys = state.keymap == KeymapPreset::Blender;
                events = interaction.update(&input, &cam, world, state);
                let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("viewport_gizmo"));
                interaction.draw(&ctx.layer_painter(layer).with_clip_rect(rect), &cam);
            }
        });
        self.egui.ui_scale = scale;
        self.egui.paint(drawable, output);
        events
    }

    /// Persists prefs and dock layout, if a config dir was given.
    pub fn save_layout(&self) {
        if let Some(dir) = &self.config_dir {
            if let Err(e) = self.shell.save(dir) {
                log::warn!("editor layout not saved: {e}");
            }
        }
    }
}
