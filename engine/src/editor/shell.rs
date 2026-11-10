//! `EditorShell`: the dockable editor window layout. Owns dock state,
//! preferences, the viewport camera and the console; draws every tab.
//!
//! The shell is engine-agnostic: the game provides the viewport texture and
//! any game-specific tabs through [`ShellHost`], and passes its `World` and
//! [`EditorState`] into [`EditorShell::ui`] each frame.

use egui_dock::{DockArea, DockState, TabViewer};
use glam::{Vec2, Vec3};
use hecs::{Entity, World};

use super::camera::{camera_key, CameraInput, EditorCamera};
use super::console::Console;
use super::prefs::{self, EditorPrefs};
use super::state::{EditorState, KeymapPreset};
use super::ui::{draw_hierarchy, draw_inspector};
use crate::ecs::Transform;
use crate::ui::dock_skeleton::{self, EditorTab, LayoutPreset};
use crate::ui::theme::EditorTheme;

/// A texture the host rendered the scene into.
#[derive(Clone, Copy, Debug)]
pub struct ViewportImage {
    pub texture: egui::TextureId,
    pub size_px: [u32; 2],
}

/// Hooks the embedding game implements. Everything has a default so a
/// headless/test host is `struct H; impl ShellHost for H {}`.
pub trait ShellHost {
    /// The scene render target to show in the Viewport tab, if any.
    fn viewport_image(&mut self) -> Option<ViewportImage> {
        None
    }
    /// Draws a tab the shell has no built-in content for (Assets, UI Designer
    /// and `Host(name)` tabs). Return `false` to show a placeholder.
    fn tab_ui(&mut self, _tab: &EditorTab, _ui: &mut egui::Ui) -> bool {
        false
    }
    /// World-space AABB of `selection` for frame-selected. Hosts with real
    /// mesh bounds override this.
    fn selection_bounds(&mut self, world: &World, selection: &[Entity]) -> Option<(Vec3, Vec3)> {
        transform_bounds(world, selection)
    }
}

/// Fallback bounds: each entity is a unit box scaled by its `Transform`.
pub fn transform_bounds(world: &World, selection: &[Entity]) -> Option<(Vec3, Vec3)> {
    let mut acc: Option<(Vec3, Vec3)> = None;
    for &e in selection {
        let Ok(t) = world.get::<&Transform>(e) else { continue };
        let half = t.scale.abs().max(Vec3::splat(0.05)) * 0.5;
        let (lo, hi) = (t.position - half, t.position + half);
        acc = Some(match acc {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }
    acc
}

/// Where the Viewport tab last drew, so the host can size its framebuffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewportState {
    /// Image rect in egui points.
    pub rect: Option<egui::Rect>,
    /// Same rect in physical pixels (what the framebuffer should be).
    pub size_px: [u32; 2],
    pub hovered: bool,
}

/// Perf numbers the host feeds the Profiler tab.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerfSnapshot {
    pub fps: f32,
    pub frame_ms: f32,
    pub entities: usize,
    pub draw_calls: u32,
}

pub struct EditorShell {
    pub dock: DockState<EditorTab>,
    pub prefs: EditorPrefs,
    pub host_tabs: Vec<String>,
    pub camera: EditorCamera,
    pub console: Console,
    pub viewport: ViewportState,
    pub perf: PerfSnapshot,
    applied_theme: Option<EditorTheme>,
}

impl EditorShell {
    pub fn new(host_tabs: Vec<String>) -> Self {
        let prefs = EditorPrefs::default();
        Self {
            dock: dock_skeleton::build_layout(prefs.layout_preset, &host_tabs),
            prefs,
            host_tabs,
            camera: EditorCamera::default(),
            console: Console::default(),
            viewport: ViewportState::default(),
            perf: PerfSnapshot::default(),
            applied_theme: None,
        }
    }

    /// Loads prefs and layout from `dir`; anything missing/corrupt falls back
    /// to defaults and is reported in the console.
    pub fn load(dir: &std::path::Path, host_tabs: Vec<String>) -> Self {
        let mut s = Self::new(host_tabs);
        let (prefs, err) = prefs::load_prefs(dir);
        s.prefs = prefs;
        if let Some(e) = err {
            s.console.warn(e);
        }
        let loaded = prefs::load_layout(dir).ok().and_then(|mut l| {
            if dock_skeleton::sanitize_layout(&mut l, &s.host_tabs) {
                Some(l)
            } else {
                s.console.warn("saved layout has no Viewport; using preset");
                None
            }
        });
        s.dock = loaded.unwrap_or_else(|| dock_skeleton::build_layout(s.prefs.layout_preset, &s.host_tabs));
        s
    }

    pub fn save(&self, dir: &std::path::Path) -> Result<(), String> {
        prefs::save_prefs(dir, &self.prefs)?;
        prefs::save_layout(dir, &self.dock)
    }

    pub fn apply_preset(&mut self, p: LayoutPreset) {
        self.prefs.layout_preset = p;
        self.dock = dock_skeleton::build_layout(p, &self.host_tabs);
    }

    pub fn layout_ron(&self) -> Result<String, String> {
        dock_skeleton::layout_to_ron(&self.dock)
    }

    /// Replaces the layout from RON; keeps the current one on error.
    pub fn restore_layout_ron(&mut self, text: &str) -> Result<(), String> {
        let mut l = dock_skeleton::layout_from_ron(text)?;
        if !dock_skeleton::sanitize_layout(&mut l, &self.host_tabs) {
            return Err("layout has no Viewport tab".into());
        }
        self.dock = l;
        Ok(())
    }

    /// Draws the whole editor for one frame. Returns the UI scale the host
    /// should apply to its egui backend.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        editor: &mut EditorState,
        world: &mut World,
        host: &mut dyn ShellHost,
    ) -> f32 {
        if self.applied_theme.as_ref() != Some(&self.prefs.theme) {
            self.prefs.theme.apply_to_egui(ctx);
            self.applied_theme = Some(self.prefs.theme.clone());
        }
        editor.keymap = self.prefs.keymap;
        super::ui::handle_shortcuts(ctx, editor, world);

        let style = dock_skeleton::dock_style(&self.prefs.theme, &ctx.style());
        let mut viewer = ShellViewer {
            editor,
            world,
            host,
            camera: &mut self.camera,
            console: &mut self.console,
            viewport: &mut self.viewport,
            perf: &self.perf,
            prefs: &mut self.prefs,
            preset_request: None,
        };
        viewer.viewport.hovered = false;
        DockArea::new(&mut self.dock).style(style).show(ctx, &mut viewer);
        let preset = viewer.preset_request.take();
        if let Some(p) = preset {
            self.apply_preset(p);
        }
        self.prefs.ui_scale
    }
}

struct ShellViewer<'a> {
    editor: &'a mut EditorState,
    world: &'a mut World,
    host: &'a mut dyn ShellHost,
    camera: &'a mut EditorCamera,
    console: &'a mut Console,
    viewport: &'a mut ViewportState,
    perf: &'a PerfSnapshot,
    prefs: &'a mut EditorPrefs,
    preset_request: Option<LayoutPreset>,
}

impl TabViewer for ShellViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut EditorTab) -> egui::WidgetText {
        tab.title().into()
    }

    fn closeable(&mut self, tab: &mut EditorTab) -> bool {
        *tab != EditorTab::Viewport
    }

    fn scroll_bars(&self, tab: &EditorTab) -> [bool; 2] {
        [false, matches!(tab, EditorTab::Settings | EditorTab::Profiler)]
    }

    fn clear_background(&self, tab: &EditorTab) -> bool {
        *tab != EditorTab::Viewport
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut EditorTab) {
        match tab {
            EditorTab::Hierarchy => draw_hierarchy(ui, self.editor, self.world),
            EditorTab::Inspector => draw_inspector(ui, self.editor, self.world),
            EditorTab::Viewport => self.viewport_ui(ui),
            EditorTab::Console => self.console.ui(ui),
            EditorTab::Profiler => {
                let p = self.perf;
                ui.label(format!("{:.0} FPS ({:.2} ms)", p.fps, p.frame_ms));
                ui.label(format!("Entities: {}", p.entities));
                ui.label(format!("Draw calls: {}", p.draw_calls));
            }
            EditorTab::Settings => settings_ui(ui, self.prefs, &mut self.preset_request),
            other => {
                if !self.host.tab_ui(other, ui) {
                    ui.weak(format!("{} is not connected in this build", other.title()));
                }
            }
        }
    }
}

impl ShellViewer<'_> {
    fn viewport_ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();
        self.viewport.rect = Some(rect);
        self.viewport.size_px = [(rect.width() * ppp).round().max(1.0) as u32, (rect.height() * ppp).round().max(1.0) as u32];
        self.viewport.hovered = resp.hovered();

        match self.host.viewport_image() {
            // GL textures are bottom-up: flip V.
            Some(img) => {
                ui.painter().image(
                    img.texture,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 1.0), egui::pos2(1.0, 0.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Viewport (no render target)",
                    egui::FontId::proportional(14.0),
                    ui.visuals().weak_text_color(),
                );
            }
        }
        self.drive_camera(ui, &resp, rect);
    }

    fn drive_camera(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        let preset = self.prefs.keymap;
        let active = resp.hovered() || resp.dragged();
        if !active {
            return;
        }
        let input = ui.input(|i| {
            let down = |k| i.key_down(k);
            let axis = |p, n| (down(p) as i32 - down(n) as i32) as f32;
            CameraInput {
                pointer_delta: Vec2::new(i.pointer.delta().x, i.pointer.delta().y),
                scroll: i.raw_scroll_delta.y / 50.0,
                lmb: i.pointer.primary_down(),
                mmb: i.pointer.middle_down(),
                rmb: i.pointer.secondary_down(),
                shift: i.modifiers.shift,
                ctrl: i.modifiers.ctrl || i.modifiers.command,
                alt: i.modifiers.alt,
                move_axes: Vec3::new(axis(egui::Key::D, egui::Key::A), axis(egui::Key::E, egui::Key::Q), axis(egui::Key::W, egui::Key::S)),
                dt: i.unstable_dt.min(0.1),
                viewport_height: rect.height() * ui.ctx().pixels_per_point(),
            }
        });
        // Dragging only counts when the drag began in the viewport; scroll only while hovered.
        let mut input = input;
        if !resp.hovered() {
            input.scroll = 0.0;
        }
        if self.camera.apply_input(preset, &input) {
            ui.ctx().request_repaint();
        }
        if resp.hovered() && !ui.ctx().wants_keyboard_input() {
            let keys: Vec<_> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some((*key, *modifiers)),
                        _ => None,
                    })
                    .collect()
            });
            for (k, m) in keys {
                if let Some(a) = camera_key(preset, k, m) {
                    let sel = self.editor.selection.to_vec();
                    let bounds = self.host.selection_bounds(self.world, &sel);
                    self.camera.apply_key_action(a, bounds);
                }
            }
        }
    }
}

fn colour_row(ui: &mut egui::Ui, label: &str, c: &mut [f32; 3]) {
    ui.horizontal(|ui| {
        ui.color_edit_button_rgb(c);
        ui.label(label);
    });
}

fn settings_ui(ui: &mut egui::Ui, prefs: &mut EditorPrefs, preset_request: &mut Option<LayoutPreset>) {
    ui.heading("Keymap");
    egui::ComboBox::from_id_salt("keymap_preset").selected_text(prefs.keymap.label()).show_ui(ui, |ui| {
        for k in KeymapPreset::ALL {
            ui.selectable_value(&mut prefs.keymap, k, k.label());
        }
    });
    ui.add_space(6.0);
    ui.heading("Interface");
    ui.add(egui::Slider::new(&mut prefs.ui_scale, 0.5..=3.0).text("UI scale"));
    ui.add(egui::Slider::new(&mut prefs.theme.font_size, 8.0..=24.0).text("Font size"));
    ui.checkbox(&mut prefs.show_grid, "Viewport grid");
    ui.add_space(6.0);
    ui.heading("Theme");
    let t = &mut prefs.theme;
    colour_row(ui, "Accent", &mut t.accent);
    colour_row(ui, "Background", &mut t.background);
    colour_row(ui, "Panel", &mut t.panel);
    colour_row(ui, "Panel header", &mut t.panel_header);
    colour_row(ui, "Text", &mut t.foreground);
    colour_row(ui, "Muted text", &mut t.muted_foreground);
    colour_row(ui, "Border", &mut t.border);
    if ui.button("Reset theme").clicked() {
        *t = EditorTheme::builtin_default();
    }
    ui.add_space(6.0);
    ui.heading("Layout");
    ui.horizontal(|ui| {
        for p in LayoutPreset::ALL {
            if ui.selectable_label(prefs.layout_preset == p, p.label()).clicked() {
                *preset_request = Some(p);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct H;
    impl ShellHost for H {}

    fn tab_names(s: &EditorShell) -> Vec<String> {
        let mut v: Vec<String> = s.dock.iter_all_tabs().map(|(_, t)| t.title()).collect();
        v.sort();
        v
    }

    #[test]
    fn layout_serialize_roundtrip_through_shell() {
        let mut a = EditorShell::new(vec!["Level".into()]);
        a.apply_preset(LayoutPreset::Tall);
        let text = a.layout_ron().unwrap();
        let mut b = EditorShell::new(vec!["Level".into()]);
        b.restore_layout_ron(&text).unwrap();
        assert_eq!(b.layout_ron().unwrap(), text);
        assert_eq!(tab_names(&a), tab_names(&b));
        // Bad input leaves the layout untouched.
        assert!(b.restore_layout_ron("nonsense").is_err());
        assert_eq!(b.layout_ron().unwrap(), text);
    }

    #[test]
    fn presets_apply_and_record_choice() {
        let mut s = EditorShell::new(vec![]);
        for p in LayoutPreset::ALL {
            s.apply_preset(p);
            assert_eq!(s.prefs.layout_preset, p);
            assert!(dock_skeleton::contains_tab(&s.dock, &EditorTab::Viewport));
        }
    }

    #[test]
    fn save_and_load_dir_restores_everything() {
        let dir = std::env::temp_dir().join(format!("jame_shell_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = EditorShell::new(vec![]);
        s.apply_preset(LayoutPreset::Minimal);
        s.prefs.keymap = KeymapPreset::Blender;
        s.prefs.ui_scale = 1.25;
        s.save(&dir).unwrap();
        let l = EditorShell::load(&dir, vec![]);
        assert_eq!(l.prefs, s.prefs);
        assert_eq!(l.layout_ron().unwrap(), s.layout_ron().unwrap());
        // Missing dir => defaults, no panic.
        let d = EditorShell::load(&dir.join("nope"), vec![]);
        assert_eq!(d.prefs, EditorPrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whole_shell_draws_headless_in_every_preset() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        let mut editor = EditorState::new();
        let mut shell = EditorShell::new(vec!["Level".into()]);
        for p in LayoutPreset::ALL {
            shell.apply_preset(p);
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0))),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    shell.ui(ctx, &mut editor, &mut world, &mut H);
                });
            }
            assert!(shell.viewport.rect.is_some(), "{p:?}");
            assert!(shell.viewport.size_px[0] > 100);
        }
    }

    #[test]
    fn frame_bounds_from_transforms() {
        let mut w = World::new();
        let a = w.spawn((Transform::from_position(Vec3::new(0.0, 0.0, 0.0)),));
        let b = w.spawn((Transform::from_position(Vec3::new(10.0, 0.0, 0.0)),));
        let (lo, hi) = transform_bounds(&w, &[a, b]).unwrap();
        assert!(lo.x < -0.4 && hi.x > 10.4);
        assert!(transform_bounds(&w, &[]).is_none());
    }
}
