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
use super::ron_scene_io::{resolve_scene_path, RonSceneIo};
use super::scene_io::{RegistrySnapshot, SceneIo, SceneSnapshot};
use super::scene_prompts::{draw_confirm, Choice, Confirm, PendingAction};
use super::state::{EditorAction, EditorMode, EditorState, KeymapPreset, Tool};
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
    /// and `Host(name)` tabs). `world` is the shell's world, handed through so
    /// hosts that keep their own copy can swap it in. Return `false` to show a placeholder.
    fn tab_ui(&mut self, _tab: &EditorTab, _world: &mut World, _ui: &mut egui::Ui) -> bool {
        false
    }
    /// World-space AABB of `selection` for frame-selected. Hosts with real
    /// mesh bounds override this.
    fn selection_bounds(&mut self, world: &World, selection: &[Entity]) -> Option<(Vec3, Vec3)> {
        transform_bounds(world, selection)
    }
    /// Called every frame after the dock is drawn, once the Viewport tab has a
    /// rect (`viewport`, in egui points). For game overlays that belong on the
    /// scene image (HUD, pause menu, dialogue choices): draw with `ctx`.
    fn viewport_overlay(&mut self, _ctx: &egui::Context, _viewport: egui::Rect, _world: &mut World) {}
    /// Whether an asset dragged from the Assets tab may be dropped on the
    /// viewport / hierarchy. When `false` the drop is ignored and no
    /// `ShellEvent::AssetDropped` is emitted. Default: accept everything.
    fn accepts_asset_drop(&mut self, _path: &std::path::Path) -> bool {
        true
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

/// Things the host may need to react to, drained with [`EditorShell::take_events`].
#[derive(Clone, Debug, PartialEq)]
pub enum ShellEvent {
    PlayStarted,
    Paused,
    Resumed,
    /// Play stopped and the scene snapshot (if any) was restored.
    Stopped { restored: bool },
    SceneNew,
    SceneOpened(std::path::PathBuf),
    SceneSaved(std::path::PathBuf),
    /// An asset was double-clicked in the Assets tab (absolute path).
    AssetOpened(std::path::PathBuf),
    /// An asset was dragged from the Assets tab and released over the
    /// viewport or the hierarchy. `path` is the asset root joined with the
    /// asset's relative path. `target` is the entity under the cursor
    /// (viewport pick, or the hierarchy row). `world_pos` is the point the
    /// editor-camera ray hits on the `y = 0` plane (viewport drops only).
    AssetDropped { path: std::path::PathBuf, target: Option<Entity>, world_pos: Option<Vec3> },
    /// [`EditorShell::request_quit`] cleared (saved, discarded or nothing to
    /// save): the host may close the window now.
    QuitApproved,
}

/// Pending File > Open / Save As path prompt.
#[derive(Clone, Debug)]
struct PathPrompt {
    save: bool,
    buffer: String,
    /// Continue with this once the save succeeded (unsaved-changes flow).
    then: Option<PendingAction>,
}

pub struct EditorShell {
    pub snapshot: Box<dyn SceneSnapshot>,
    pub scene_io: Box<dyn SceneIo>,
    events: Vec<ShellEvent>,
    prompt: Option<PathPrompt>,
    confirm: Option<Confirm>,
    project_dir: Option<std::path::PathBuf>,
    show_about: bool,
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
            snapshot: Box::new(RegistrySnapshot::default()),
            scene_io: Box::new(RonSceneIo::new()),
            events: Vec::new(),
            prompt: None,
            confirm: None,
            project_dir: None,
            show_about: false,
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

    /// Queues an event for the host (used by built-in tab content owned
    /// outside the shell, e.g. the runtime's Assets panel).
    pub fn push_event(&mut self, event: ShellEvent) {
        self.events.push(event);
    }

    pub fn take_events(&mut self) -> Vec<ShellEvent> {
        std::mem::take(&mut self.events)
    }

    // ---- play control: snapshot on Play, restore on Stop

    pub fn play(&mut self, editor: &mut EditorState, world: &World) {
        if editor.mode == EditorMode::Edit {
            self.snapshot.capture(world, &editor.registry);
            editor.play();
            self.events.push(ShellEvent::PlayStarted);
        }
    }

    pub fn toggle_pause(&mut self, editor: &mut EditorState) {
        let before = editor.mode;
        editor.toggle_pause();
        match (before, editor.mode) {
            (EditorMode::Play, EditorMode::Pause) => self.events.push(ShellEvent::Paused),
            (EditorMode::Pause, EditorMode::Play) => self.events.push(ShellEvent::Resumed),
            _ => {}
        }
    }

    pub fn stop(&mut self, editor: &mut EditorState, world: &mut World) {
        if editor.mode == EditorMode::Edit {
            return;
        }
        editor.stop();
        let restored = self.snapshot.restore(world, &editor.registry);
        self.snapshot.discard();
        editor.selection.prune(world);
        self.events.push(ShellEvent::Stopped { restored });
    }

    /// Play/Stop toggle used by the host's F3 key.
    pub fn toggle_play(&mut self, editor: &mut EditorState, world: &mut World) {
        if editor.is_editing() {
            self.play(editor, world)
        } else {
            self.stop(editor, world)
        }
    }

    // ---- project

    /// Project folder: scenes live in `<dir>/scenes` (relative paths typed in
    /// the File prompts resolve there) and prefabs in `<dir>/prefabs`. With
    /// `None` (the default) the prefab entries are disabled and scene paths
    /// are used as typed.
    pub fn set_project_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.project_dir = dir;
    }

    pub fn project_dir(&self) -> Option<&std::path::Path> {
        self.project_dir.as_deref()
    }

    pub fn scenes_dir(&self) -> Option<std::path::PathBuf> {
        self.project_dir.as_ref().map(|d| d.join("scenes"))
    }

    pub fn prefabs_dir(&self) -> Option<std::path::PathBuf> {
        self.project_dir.as_ref().map(|d| d.join("prefabs"))
    }

    /// Open scene's name, `*`-suffixed while it has unsaved changes.
    pub fn scene_title(&self) -> String {
        self.scene_io.title()
    }

    /// Text for the OS window title, e.g. `level.ron* - Dust Editor`.
    pub fn window_title(&self, app: &str) -> String {
        format!("{} - {app}", self.scene_title())
    }

    pub fn is_scene_dirty(&self) -> bool {
        self.scene_io.is_dirty()
    }

    /// The prompt currently waiting for an answer, if any.
    pub fn pending_confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    /// Pushes project/undo state into the scene backend and prefab UI.
    pub(crate) fn sync(&mut self, editor: &mut EditorState) {
        editor.prefab_ui.set_root(self.prefabs_dir());
        self.scene_io.set_prefab_library(editor.prefab_ui.library());
        self.scene_io.sync_undo_dirty(editor.undo.is_dirty());
    }

    fn drain_scene_messages(&mut self) {
        for m in self.scene_io.take_messages() {
            self.console.warn(m);
        }
    }

    fn after_load(&mut self, editor: &mut EditorState) {
        editor.selection.clear();
        editor.undo.clear();
        self.sync(editor);
        self.drain_scene_messages();
    }

    // ---- scene file operations (Edit mode only; results go to the console)
    //
    // `new_scene` / `open_scene` / `save_scene` / `revert_scene` act
    // immediately. The `request_*` variants are what the menu uses: they ask
    // first when there are unsaved changes, an autosave to recover, or a
    // legacy level file about to be converted.

    pub fn new_scene(&mut self, editor: &mut EditorState, world: &mut World) {
        if !editor.is_editing() {
            self.console.warn("stop playing before creating a new scene");
            return;
        }
        match self.scene_io.new_scene_with(world, &editor.registry) {
            Ok(()) => {
                self.after_load(editor);
                self.console.info("new scene");
                self.events.push(ShellEvent::SceneNew);
            }
            Err(e) => self.console.error(e),
        }
    }

    pub fn open_scene(&mut self, editor: &mut EditorState, world: &mut World, path: &std::path::Path) {
        if !editor.is_editing() {
            self.console.warn("stop playing before opening a scene");
            return;
        }
        match self.scene_io.open_with(path, world, &editor.registry) {
            Ok(()) => {
                self.after_load(editor);
                self.console.info(format!("opened {}", path.display()));
                self.events.push(ShellEvent::SceneOpened(path.to_path_buf()));
            }
            Err(e) => self.console.error(e),
        }
    }

    /// Opens the autosave of `path` (the scene stays dirty until saved).
    pub fn open_recovery(&mut self, editor: &mut EditorState, world: &mut World, path: &std::path::Path) {
        if !editor.is_editing() {
            return;
        }
        match self.scene_io.open_recovery_with(path, world, &editor.registry) {
            Ok(()) => {
                self.after_load(editor);
                self.console.warn(format!("recovered autosave of {}; save to keep it", path.display()));
                self.events.push(ShellEvent::SceneOpened(path.to_path_buf()));
            }
            Err(e) => self.console.error(e),
        }
    }

    /// Reloads the current file, dropping unsaved changes.
    pub fn revert_scene(&mut self, editor: &mut EditorState, world: &mut World) {
        if !editor.is_editing() {
            return;
        }
        match self.scene_io.revert_with(world, &editor.registry) {
            Ok(()) => {
                self.after_load(editor);
                self.console.info("reverted to saved scene");
                if let Some(p) = self.scene_io.current_path().map(|p| p.to_path_buf()) {
                    self.events.push(ShellEvent::SceneOpened(p));
                }
            }
            Err(e) => self.console.error(e),
        }
    }

    /// Writes the scene to `path` without asking anything; returns success.
    pub fn save_scene(&mut self, editor: &mut EditorState, world: &mut World, path: &std::path::Path) -> bool {
        self.sync(editor);
        match self.scene_io.save_with(world, &editor.registry, path) {
            Ok(()) => {
                editor.undo.mark_saved();
                self.sync(editor);
                self.drain_scene_messages();
                self.console.info(format!("saved {}", path.display()));
                self.events.push(ShellEvent::SceneSaved(path.to_path_buf()));
                true
            }
            Err(e) => {
                self.console.error(e);
                false
            }
        }
    }

    /// Saves to `path`, asking first if the backend wants confirmation
    /// (legacy level -> scene format). `then` runs after a successful save.
    fn save_then(&mut self, editor: &mut EditorState, world: &mut World, path: std::path::PathBuf, then: Option<PendingAction>) {
        if let Some(message) = self.scene_io.save_confirmation(&path) {
            self.confirm = Some(Confirm::LevelSave { path, message, then });
            return;
        }
        if self.save_scene(editor, world, &path) {
            if let Some(a) = then {
                self.run_action(editor, world, a);
            }
        }
    }

    fn default_save_name(&self) -> String {
        if self.project_dir.is_some() {
            "untitled.scene.ron".into()
        } else {
            "level.ron".into()
        }
    }

    /// File > Save: current path, or prompts when there is none.
    pub fn save_current(&mut self, editor: &mut EditorState, world: &mut World) {
        match self.scene_io.current_path().map(|p| p.to_path_buf()) {
            Some(p) => self.save_then(editor, world, p, None),
            None => self.prompt = Some(PathPrompt { save: true, buffer: self.default_save_name(), then: None }),
        }
    }

    /// File > Save As: the typed path goes through the same confirmations.
    pub fn save_scene_as(&mut self, editor: &mut EditorState, world: &mut World, path: &std::path::Path) {
        self.save_then(editor, world, path.to_path_buf(), None);
    }

    fn run_action(&mut self, editor: &mut EditorState, world: &mut World, action: PendingAction) {
        match action {
            PendingAction::NewScene => self.new_scene(editor, world),
            PendingAction::OpenScene(p) => {
                if self.scene_io.recovery_for(&p).is_some() {
                    self.confirm = Some(Confirm::Recover(p));
                } else {
                    self.open_scene(editor, world, &p);
                }
            }
            PendingAction::Quit => {
                self.scene_io.close_clean();
                self.events.push(ShellEvent::QuitApproved);
            }
        }
    }

    fn request(&mut self, editor: &mut EditorState, world: &mut World, action: PendingAction) {
        self.sync(editor);
        if self.scene_io.is_dirty() {
            self.confirm = Some(Confirm::Unsaved(action));
        } else {
            self.run_action(editor, world, action);
        }
    }

    /// File > New Scene, asking about unsaved changes first.
    pub fn request_new_scene(&mut self, editor: &mut EditorState, world: &mut World) {
        if editor.is_editing() {
            self.request(editor, world, PendingAction::NewScene);
        } else {
            self.console.warn("stop playing before creating a new scene");
        }
    }

    /// Opens `path`, asking about unsaved changes first and offering the
    /// autosave when a newer one exists.
    pub fn request_open_scene(&mut self, editor: &mut EditorState, world: &mut World, path: &std::path::Path) {
        if editor.is_editing() {
            self.request(editor, world, PendingAction::OpenScene(path.to_path_buf()));
        } else {
            self.console.warn("stop playing before opening a scene");
        }
    }

    /// File > Revert, asking first if there is anything to lose.
    pub fn request_revert(&mut self, editor: &mut EditorState, world: &mut World) {
        self.sync(editor);
        if self.scene_io.is_dirty() {
            self.confirm = Some(Confirm::Revert);
        } else {
            self.revert_scene(editor, world);
        }
    }

    /// The host wants to close the editor. Returns `true` when that is fine
    /// right now (and emits [`ShellEvent::QuitApproved`]); otherwise a prompt
    /// is shown and `QuitApproved` follows once the user saved or discarded.
    /// Playing is stopped first so the scene on disk and in memory agree.
    pub fn request_quit(&mut self, editor: &mut EditorState, world: &mut World) -> bool {
        self.stop(editor, world);
        self.sync(editor);
        if self.scene_io.is_dirty() {
            self.confirm = Some(Confirm::Unsaved(PendingAction::Quit));
            false
        } else {
            self.run_action(editor, world, PendingAction::Quit);
            true
        }
    }

    /// Applies the user's answer to the pending prompt.
    pub fn resolve_confirm(&mut self, editor: &mut EditorState, world: &mut World, choice: Choice) {
        let Some(c) = self.confirm.take() else { return };
        if choice == Choice::Cancel {
            return;
        }
        match (c, choice) {
            (Confirm::Unsaved(a), Choice::Primary) => match self.scene_io.current_path().map(|p| p.to_path_buf()) {
                Some(p) => self.save_then(editor, world, p, Some(a)),
                None => self.prompt = Some(PathPrompt { save: true, buffer: self.default_save_name(), then: Some(a) }),
            },
            (Confirm::Unsaved(a), _) => self.run_action(editor, world, a),
            (Confirm::Recover(p), Choice::Primary) => self.open_recovery(editor, world, &p),
            (Confirm::Recover(p), _) => {
                self.scene_io.discard_autosave(&p);
                self.open_scene(editor, world, &p);
            }
            (Confirm::LevelSave { path, then, .. }, _) => {
                if self.save_scene(editor, world, &path) {
                    if let Some(a) = then {
                        self.run_action(editor, world, a);
                    }
                }
            }
            (Confirm::Revert, _) => self.revert_scene(editor, world),
        }
    }

    /// `(is_save, text)` of the File > Open / Save As path prompt, if shown.
    pub fn pending_path_prompt(&self) -> Option<(bool, &str)> {
        self.prompt.as_ref().map(|p| (p.save, p.buffer.as_str()))
    }

    /// Answers the path prompt with `typed` (what pressing Open / Save does).
    pub fn submit_path_prompt(&mut self, editor: &mut EditorState, world: &mut World, typed: &str) {
        if let Some(mut pr) = self.prompt.take() {
            pr.buffer = typed.to_string();
            if !typed.trim().is_empty() {
                self.run_path_prompt(editor, world, pr);
            }
        }
    }

    fn run_path_prompt(&mut self, editor: &mut EditorState, world: &mut World, mut pr: PathPrompt) {
        let path = resolve_scene_path(self.project_dir.as_deref(), &pr.buffer);
        if pr.save {
            self.save_then(editor, world, path, pr.then.take());
        } else {
            self.request_open_scene(editor, world, &path);
        }
    }

    /// File menu contents (also drawn headless in tests).
    pub fn file_menu(&mut self, ui: &mut egui::Ui, editor: &mut EditorState, world: &mut World) {
        let editing = editor.is_editing();
        let has_path = self.scene_io.current_path().is_some();
        if ui.add_enabled(editing, egui::Button::new("New Scene")).clicked() {
            self.request_new_scene(editor, world);
            ui.close_menu();
        }
        if ui.add_enabled(editing, egui::Button::new("Open...")).clicked() {
            self.prompt = Some(PathPrompt { save: false, buffer: String::new(), then: None });
            ui.close_menu();
        }
        ui.separator();
        if ui.add_enabled(editing, egui::Button::new("Save")).clicked() {
            self.save_current(editor, world);
            ui.close_menu();
        }
        if ui.add_enabled(editing, egui::Button::new("Save As...")).clicked() {
            let cur = self.scene_io.current_path().map(|p| p.display().to_string()).unwrap_or_else(|| self.default_save_name());
            self.prompt = Some(PathPrompt { save: true, buffer: cur, then: None });
            ui.close_menu();
        }
        if ui.add_enabled(editing && has_path, egui::Button::new("Revert")).on_disabled_hover_text("Nothing to revert to: the scene was never saved").clicked() {
            self.request_revert(editor, world);
            ui.close_menu();
        }
        ui.separator();
        match &self.project_dir {
            Some(d) => ui.weak(format!("Project: {}", d.display())),
            None => ui.weak("No project directory: scene paths are used as typed, prefabs are disabled"),
        };
    }

    fn menu_bar(&mut self, ctx: &egui::Context, editor: &mut EditorState, world: &mut World) {
        egui::TopBottomPanel::top("editor_menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| self.file_menu(ui, editor, world));
                ui.menu_button("Edit", |ui| {
                    let mut act = None;
                    for (label, a) in [
                        ("Undo", EditorAction::Undo),
                        ("Redo", EditorAction::Redo),
                        ("Duplicate", EditorAction::Duplicate),
                        ("Delete", EditorAction::Delete),
                        ("Select All", EditorAction::SelectAll),
                        ("Deselect All", EditorAction::DeselectAll),
                    ] {
                        if ui.add_enabled(editor.is_editing(), egui::Button::new(label)).clicked() {
                            act = Some(a);
                            ui.close_menu();
                        }
                    }
                    if let Some(a) = act {
                        editor.perform(world, a);
                    }
                });
                ui.menu_button("View", |ui| {
                    for tab in EditorTab::BUILTIN.iter().cloned().chain(self.host_tabs.iter().cloned().map(EditorTab::Host)) {
                        let open = dock_skeleton::contains_tab(&self.dock, &tab);
                        let mut v = open;
                        if tab != EditorTab::Viewport && ui.checkbox(&mut v, tab.title()).clicked() {
                            dock_skeleton::toggle_tab(&mut self.dock, &tab);
                        }
                    }
                    ui.separator();
                    ui.menu_button("Layout", |ui| {
                        for p in LayoutPreset::ALL {
                            if ui.selectable_label(self.prefs.layout_preset == p, p.label()).clicked() {
                                self.apply_preset(p);
                                ui.close_menu();
                            }
                        }
                    });
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Keyboard shortcuts").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(self.scene_title());
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context, editor: &mut EditorState, world: &mut World) {
        egui::TopBottomPanel::top("editor_toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let mode = editor.mode;
                let playing = mode != EditorMode::Edit;
                if ui.add_enabled(!playing, egui::Button::new("Play")).clicked() {
                    self.play(editor, world);
                }
                let pause_label = if mode == EditorMode::Pause { "Resume" } else { "Pause" };
                if ui.add_enabled(playing, egui::Button::new(pause_label)).clicked() {
                    self.toggle_pause(editor);
                }
                if ui.add_enabled(playing, egui::Button::new("Stop")).clicked() {
                    self.stop(editor, world);
                }
                ui.separator();
                for (t, label) in [(Tool::Move, "Move"), (Tool::Rotate, "Rotate"), (Tool::Scale, "Scale")] {
                    if ui.selectable_label(editor.tool == t, label).clicked() {
                        editor.tool = t;
                    }
                }
                ui.separator();
                ui.weak(match mode {
                    EditorMode::Edit => "Edit",
                    EditorMode::Play => "Playing",
                    EditorMode::Pause => "Paused",
                });
                if let Some(e) = &editor.last_error {
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 80), e);
                }
            });
        });
    }

    fn windows(&mut self, ctx: &egui::Context, editor: &mut EditorState, world: &mut World) {
        if let Some(mut pr) = self.prompt.take() {
            let mut keep = true;
            let mut submit = false;
            egui::Window::new(if pr.save { "Save Scene As" } else { "Open Scene" })
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    let hint = if self.project_dir.is_some() { "name.scene.ron (relative to <project>/scenes)" } else { "path/to/level.ron" };
                    let r = ui.add(egui::TextEdit::singleline(&mut pr.buffer).hint_text(hint).desired_width(320.0));
                    ui.horizontal(|ui| {
                        submit = ui.button(if pr.save { "Save" } else { "Open" }).clicked()
                            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                });
            if submit && !pr.buffer.trim().is_empty() {
                self.run_path_prompt(editor, world, pr);
            } else if keep {
                self.prompt = Some(pr);
            }
        }
        if let Some(c) = self.confirm.clone() {
            if let Some(choice) = draw_confirm(ctx, &c) {
                self.resolve_confirm(editor, world, choice);
            }
        }
        if self.show_about {
            egui::Window::new("Keyboard shortcuts").open(&mut self.show_about).show(ctx, |ui| {
                match self.prefs.keymap {
                    KeymapPreset::Unity => {
                        ui.label("W/E/R  tools    Ctrl+D duplicate    Del delete    F2 rename");
                        ui.label("Viewport: hold RMB + WASD/QE fly, Alt+LMB orbit, MMB pan, wheel zoom, F frame");
                    }
                    KeymapPreset::Blender => {
                        ui.label("G/R/S  tools    Shift+D duplicate    X delete    A select all");
                        ui.label("Viewport: MMB orbit, Shift+MMB pan, wheel zoom, numpad 1/3/7 views, . frame");
                    }
                }
                ui.label("Ctrl+Z undo    Ctrl+Shift+Z redo    F3 play/stop");
            });
        }
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
        self.sync(editor);
        if editor.is_editing() {
            if let Err(e) = self.scene_io.autosave_tick(world, &editor.registry, std::time::Instant::now()) {
                self.console.warn(e);
            }
        }
        super::ui::handle_shortcuts(ctx, editor, world);

        self.menu_bar(ctx, editor, world);
        self.toolbar(ctx, editor, world);
        self.windows(ctx, editor, world);

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
            events: &mut self.events,
        };
        viewer.viewport.hovered = false;
        DockArea::new(&mut self.dock).style(style).show(ctx, &mut viewer);
        let preset = viewer.preset_request.take();
        if let Some(p) = preset {
            self.apply_preset(p);
        }
        super::asset_tab::paint_drag_ghost(ctx);
        self.scene_io.sync_undo_dirty(editor.undo.is_dirty());
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
    events: &'a mut Vec<ShellEvent>,
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
            EditorTab::Hierarchy => {
                let (editor, world) = (&mut *self.editor, &mut *self.world);
                let dropped = super::asset_drop::with_hierarchy_drop(ui, |ui| draw_hierarchy(ui, editor, world));
                if let Some((drag, target)) = dropped {
                    if self.host.accepts_asset_drop(&drag.path) {
                        self.events.push(super::asset_drop::hierarchy_drop_event(drag.path, target));
                    }
                }
            }
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
                if !self.host.tab_ui(other, self.world, ui) {
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
        if super::asset_tab::is_dragging_asset(ui.ctx()) {
            self.viewport.hovered = false;
            self.asset_drop_ui(ui, &resp, rect);
            return;
        }
        self.drive_camera(ui, &resp, rect);
    }

    /// Feedback while an asset is dragged over the viewport, and the drop itself.
    fn asset_drop_ui(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        let aspect = rect.width() / rect.height().max(1.0);
        let (view, proj) = (self.camera.view_matrix(), self.camera.projection_matrix(aspect));
        if let Some(cursor) = resp.dnd_hover_payload::<super::asset_tab::AssetDrag>().and_then(|_| ui.input(|i| i.pointer.interact_pos())) {
            let stroke = egui::Stroke::new(2.0_f32, ui.visuals().selection.stroke.color);
            ui.painter().rect_stroke(rect.shrink(1.0), 0.0, stroke);
            if let Some(p) = super::asset_drop::ground_point(cursor, rect, view, proj) {
                ui.painter().circle_stroke(cursor, 5.0, stroke);
                ui.painter().text(
                    cursor + egui::vec2(10.0, -14.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{:.1}, {:.1}", p.x, p.z),
                    egui::FontId::proportional(11.0),
                    ui.visuals().selection.stroke.color,
                );
            }
        }
        if let Some(drag) = resp.dnd_release_payload::<super::asset_tab::AssetDrag>() {
            let Some(cursor) = ui.input(|i| i.pointer.interact_pos()) else { return };
            if self.host.accepts_asset_drop(&drag.path) {
                self.events.push(super::asset_drop::viewport_drop_event(drag.path.clone(), cursor, rect, view, proj, self.world));
            }
        }
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
    fn play_pause_stop_restores_world_and_emits_events() {
        use crate::editor::hierarchy::Name;
        let mut shell = EditorShell::new(vec![]);
        let mut editor = EditorState::new();
        let mut world = World::new();
        let e = world.spawn((Name("a".into()), Transform::from_position(Vec3::ONE)));
        shell.stop(&mut editor, &mut world); // no-op in Edit
        assert!(shell.take_events().is_empty());

        shell.play(&mut editor, &world);
        assert_eq!(editor.mode, EditorMode::Play);
        world.get::<&mut Transform>(e).unwrap().position = Vec3::splat(50.0);
        world.spawn((Name("tmp".into()),));
        shell.toggle_pause(&mut editor);
        assert_eq!(editor.mode, EditorMode::Pause);
        shell.toggle_pause(&mut editor);
        shell.stop(&mut editor, &mut world);
        assert_eq!(editor.mode, EditorMode::Edit);
        assert_eq!(world.len(), 1);
        assert_eq!(world.get::<&Transform>(e).unwrap().position, Vec3::ONE);
        assert_eq!(
            shell.take_events(),
            vec![ShellEvent::PlayStarted, ShellEvent::Paused, ShellEvent::Resumed, ShellEvent::Stopped { restored: true }]
        );
        shell.toggle_play(&mut editor, &mut world);
        assert!(!editor.is_editing());
        shell.toggle_play(&mut editor, &mut world);
        assert!(editor.is_editing());
    }

    #[test]
    fn scene_files_via_shell_report_to_console() {
        let dir = std::env::temp_dir().join(format!("jame_shellio_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.ron");
        let mut shell = EditorShell::new(vec![]);
        let mut editor = EditorState::new();
        let mut world = World::new();
        editor.spawn(&mut world, crate::editor::command::SpawnSpec::primitive(crate::editor::hierarchy::PrimitiveShape::Cube, None));
        assert!(shell.save_scene(&mut editor, &mut world, &path));
        shell.new_scene(&mut editor, &mut world);
        assert_eq!(world.len(), 0);
        shell.open_scene(&mut editor, &mut world, &path);
        assert_eq!(world.len(), 1);
        shell.open_scene(&mut editor, &mut world, &dir.join("missing.ron"));
        assert_eq!(shell.console.count(crate::editor::console::LogLevel::Error), 1);
        // Open is refused while playing.
        shell.play(&mut editor, &world);
        shell.new_scene(&mut editor, &mut world);
        assert_eq!(world.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
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
