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

use super::asset_tab::AssetsPanel;
use super::cam_matrices::CameraMatrices;
use super::designer_tab::DesignerTab;
use super::inspector::Registry;
use super::interaction::{ViewportEvent, ViewportInput, ViewportInteraction};
use super::shell::{EditorShell, PerfSnapshot, ShellHost, ViewportImage};
use super::state::{EditorState, KeymapPreset};
use super::viewport_target::ViewportTarget;
use crate::ui::EguiState;

/// A host with no game-specific tabs and default bounds.
pub struct NoHost;
impl ShellHost for NoHost {}

/// Adds the viewport image and the built-in Assets / UI Designer tab content
/// to whatever host the game passes in. The host's own `tab_ui` wins: the
/// built-ins are used only when it returns `false`.
struct WithImage<'a> {
    image: Option<ViewportImage>,
    inner: &'a mut dyn ShellHost,
    assets: &'a mut AssetsPanel,
    designer: &'a mut DesignerTab,
}

impl ShellHost for WithImage<'_> {
    fn viewport_image(&mut self) -> Option<ViewportImage> {
        self.image
    }
    fn tab_ui(&mut self, tab: &crate::ui::EditorTab, world: &mut World, ui: &mut egui::Ui) -> bool {
        if self.inner.tab_ui(tab, world, ui) {
            return true;
        }
        match tab {
            crate::ui::EditorTab::Assets => {
                self.assets.draw(ui);
                true
            }
            crate::ui::EditorTab::UiDesigner => {
                self.designer.draw(ui);
                true
            }
            _ => false,
        }
    }
    fn accepts_asset_drop(&mut self, path: &std::path::Path) -> bool {
        self.inner.accepts_asset_drop(path)
    }
    fn selection_bounds(&mut self, world: &World, selection: &[hecs::Entity]) -> Option<(glam::Vec3, glam::Vec3)> {
        self.inner.selection_bounds(world, selection)
    }
    fn viewport_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect, world: &mut World) {
        self.inner.viewport_overlay(ctx, viewport, world)
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
    /// Built-in content of the Assets tab (used when the host's `tab_ui`
    /// declines it). Configure with [`Self::set_asset_root`].
    pub assets: AssetsPanel,
    /// Built-in content of the UI Designer tab. Configure with
    /// [`Self::set_ui_dir`].
    pub designer: DesignerTab,
    config_dir: Option<PathBuf>,
}

impl EditorRuntime {
    /// `config_dir` holds saved prefs and dock layout (created on save).
    pub fn new(gl: Arc<glow::Context>, mut registry: Registry, config_dir: Option<PathBuf>, host_tabs: Vec<String>) -> anyhow::Result<Self> {
        // Scene files and prefab instances need entity ids / prefab links to
        // survive undo, duplicate and play snapshots.
        super::scene::register_scene_components(&mut registry);
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
            assets: AssetsPanel::default(),
            designer: DesignerTab::default(),
            config_dir,
        })
    }

    /// Folder the Assets tab browses (`None`, the default, shows an empty state).
    pub fn set_asset_root(&mut self, root: Option<PathBuf>) {
        self.assets.set_root(root);
    }

    /// Folder of `.ron` UI documents the UI Designer tab opens and saves to
    /// (`None`, the default: only the built-in examples).
    pub fn set_ui_dir(&mut self, dir: Option<PathBuf>) {
        self.designer.set_dir(dir);
    }

    /// Builder form of [`Self::set_asset_root`].
    pub fn with_asset_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.set_asset_root(Some(root.into()));
        self
    }

    /// Builder form of [`Self::set_ui_dir`].
    pub fn with_ui_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.set_ui_dir(Some(dir.into()));
        self
    }

    /// Project folder for scenes (`<dir>/scenes`) and prefabs (`<dir>/prefabs`).
    /// `None` (default) disables the prefab menu entries.
    pub fn set_project_dir(&mut self, dir: Option<PathBuf>) {
        self.shell.set_project_dir(dir);
    }

    pub fn with_project_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.set_project_dir(Some(dir.into()));
        self
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
        // Two-phase thumbnail loading: GL upload + egui registration must
        // happen before `egui.run` takes the exclusive borrow.
        self.assets.browser.ensure_thumbnails_loaded(gl, &mut self.egui);
        let Self { egui, shell, state, interaction, assets, designer, .. } = self;
        let mut scale = 1.0;
        let mut events = Vec::new();
        let output = egui.run(drawable, |ctx| {
            let mut host = WithImage { image, inner: host, assets, designer };
            scale = shell.ui(ctx, state, world, &mut host);
            for path in host.assets.take_opened() {
                shell.push_event(super::shell::ShellEvent::AssetOpened(path));
            }
            if let Some(rect) = shell.viewport.rect {
                host.viewport_overlay(ctx, rect, world);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::EditorTab;

    struct Claims(EditorTab);
    impl ShellHost for Claims {
        fn tab_ui(&mut self, tab: &EditorTab, _w: &mut World, ui: &mut egui::Ui) -> bool {
            if *tab == self.0 {
                ui.label("host content");
                return true;
            }
            false
        }
    }

    /// Runs `tab_ui` for `tab` inside a bare egui frame.
    fn draw_tab(inner: &mut dyn ShellHost, assets: &mut AssetsPanel, designer: &mut DesignerTab, tab: EditorTab) -> bool {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1500.0, 900.0))),
            ..Default::default()
        };
        let mut handled = false;
        let mut world = World::new();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut host = WithImage { image: None, inner, assets, designer };
                handled = host.tab_ui(&tab, &mut world, ui);
            });
        });
        handled
    }

    #[test]
    fn builtin_tabs_fill_in_when_the_host_declines() {
        let (mut a, mut d) = (AssetsPanel::default(), DesignerTab::default());
        assert!(draw_tab(&mut NoHost, &mut a, &mut d, EditorTab::Assets));
        assert!(draw_tab(&mut NoHost, &mut a, &mut d, EditorTab::UiDesigner));
        // Other tabs stay the host's business.
        assert!(!draw_tab(&mut NoHost, &mut a, &mut d, EditorTab::Host("Level".into())));
    }

    #[test]
    fn host_content_wins_over_the_builtins() {
        let (mut a, mut d) = (AssetsPanel::default(), DesignerTab::default());
        let mut host = Claims(EditorTab::Assets);
        assert!(draw_tab(&mut host, &mut a, &mut d, EditorTab::Assets));
        // UiDesigner is not claimed, so the built-in one answers.
        assert!(draw_tab(&mut host, &mut a, &mut d, EditorTab::UiDesigner));
    }
}
