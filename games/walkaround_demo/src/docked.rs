//! The editor: the engine's dock (`engine::editor::runtime::EditorRuntime` with
//! Hierarchy, Inspector, Viewport with gizmos, Assets, Console, ...). F1, F2 and
//! F4 (or `DUST_EDITOR=1` at launch) open it; F4 closes it again.
//!
//! The game's own panels (level files, music, rigs, characters, spawners,
//! classes, particle emitters, scripts, texture browser, shader profile,
//! multiplayer / appearance) are host tabs (Level, Selected, Create, Render), and
//! the bridge below keeps the game's per-entity metadata in step with what the
//! shell edits. The File menu saves and loads the game's own `levels/*.ron`
//! ([`GameSceneIo`]).
//!
//! Only used when `editor_available()`: in release builds no `EditorRuntime`
//! is ever created.

use super::*;

use engine::editor::bounds::{Aabb, LocalBounds};
use engine::editor::runtime::EditorRuntime;
use engine::editor::scene_io::{NullSnapshot, SceneIo};
use engine::editor::shell::{PerfSnapshot, ShellEvent, ShellHost};
use engine::editor::interaction::ViewportEvent;
use engine::editor::{EditorMode as ShellMode, Locked, Name, Registry, Selection};
use engine::ui::{AppearanceAction, EditorTab, MultiplayerAction};
use std::cell::RefCell;
use std::rc::Rc;

const TAB_LEVEL: &str = "Level";
const TAB_SELECTED: &str = "Selected";
const TAB_CREATE: &str = "Create";
const TAB_RENDER: &str = "Render";

// Clone-able mirrors of game components that are not `Clone` themselves. The
// shell duplicates / restores deleted entities by cloning registered
// components, so each mirror carries what is needed to rebuild the real one
// (`heal_from_mirrors`).
#[derive(Clone)]
struct MirrorHealth(f32);
#[derive(Clone)]
struct MirrorDialogue(Vec<DialogueNode>);
#[derive(Clone)]
struct MirrorAnimator {
    kind: AnimationKind,
    base_position: Vec3,
    base_rotation: Quat,
}
#[derive(Clone)]
struct MirrorEmitter(ParticleEmitterDef);
#[derive(Clone)]
struct MirrorBrain;
#[derive(Clone)]
struct MirrorSpawnerState;

/// Docked-editor state owned by `Sandbox` (`None` in release builds).
pub(crate) struct Docked {
    rt: EditorRuntime,
    bridge: Bridge,
    io: SharedIo,
}

/// A File-menu operation the shell asked for. The shell only holds the world while it
/// runs its UI, but loading and saving a level needs the whole game (GL, caches, level
/// metadata), so [`GameSceneIo`] queues the work and `docked_apply_io` does it right
/// after the UI pass.
enum IoOp {
    New,
    Open(Level, PathBuf),
    Save(PathBuf),
}

#[derive(Default)]
struct IoShared {
    ops: Vec<IoOp>,
    levels_dir: PathBuf,
    /// The game changed its current level behind the File menu's back (Level tab).
    current_update: Option<Option<PathBuf>>,
}
type SharedIo = Rc<RefCell<IoShared>>;

/// File menu backend over the game's own level files (`levels/<name>.ron`).
struct GameSceneIo {
    shared: SharedIo,
    current: Option<PathBuf>,
    dirty: bool,
}

impl GameSceneIo {
    fn resolve(&self, path: &Path) -> PathBuf {
        match (path.is_absolute(), path.file_name()) {
            (false, Some(name)) => self.shared.borrow().levels_dir.join(name),
            _ => path.to_path_buf(),
        }
    }
}

impl SceneIo for GameSceneIo {
    fn new_scene(&mut self, _: &mut World) -> Result<(), String> {
        self.shared.borrow_mut().ops.push(IoOp::New);
        self.current = None;
        Ok(())
    }

    fn open(&mut self, path: &Path, _: &mut World) -> Result<(), String> {
        let path = self.resolve(path);
        let level = engine::level::load_from_file(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        self.shared.borrow_mut().ops.push(IoOp::Open(level, path.clone()));
        self.current = Some(path);
        Ok(())
    }

    /// Queued: the write happens right after the UI pass, errors go to the Console.
    fn save(&mut self, _: &World, path: &Path) -> Result<(), String> {
        let path = self.resolve(path);
        if path.file_stem().and_then(|s| s.to_str()).is_none_or(str::is_empty) {
            return Err(format!("invalid level file name {}", path.display()));
        }
        self.shared.borrow_mut().ops.push(IoOp::Save(path.clone()));
        self.current = Some(path);
        Ok(())
    }

    fn current_path(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Called by the shell once per frame; also where the game's own level changes arrive.
    fn sync_undo_dirty(&mut self, undo_dirty: bool) {
        self.dirty = undo_dirty;
        if let Some(current) = self.shared.borrow_mut().current_update.take() {
            self.current = current;
        }
    }
}

/// Book-keeping for the game <-> shell bridge (no GL, so unit-testable).
#[derive(Default)]
pub(crate) struct Bridge {
    /// Mesh-source -> local bounds, so each mesh file is read once.
    bounds: HashMap<String, Option<Aabb>>,
    /// Last name both `Name` and the game's own name field agreed on.
    names: HashMap<Entity, String>,
    last_shell_sel: Vec<Entity>,
    last_game_sel: Vec<Entity>,
    seen_level_gen: u64,
    /// `current_level_name` as the File menu last heard it.
    seen_level_name: String,
    /// Entities whose script was already (re)attached once, so a broken script
    /// is reported once rather than every frame.
    script_tried: HashSet<Entity>,
}

/// What the overlay asked for this frame; applied after the UI pass, when
/// `Context` is available again.
#[derive(Default)]
pub(crate) struct PlayActions {
    pub pause: Option<PauseMenuAction>,
    pub multiplayer: Option<MultiplayerAction>,
    pub appearance: Option<AppearanceAction>,
}

/// Inputs the pause-menu side panels need (computed before the egui pass,
/// which needs `&mut self`).
pub(crate) struct PauseUi {
    rig_names: Vec<String>,
    current_appearance: Option<String>,
    net_status: String,
    net_connected: bool,
    net_players: Vec<(NetId, String, bool)>,
}

/// `ShellHost` for the game: game tabs plus the viewport overlay. The shell's
/// world is swapped into `game.world` for the duration of each call so the
/// existing panels (which read `self.world`) work unchanged.
struct DockHost<'a> {
    game: &'a mut Sandbox,
    gl: &'a glow::Context,
    drawable: (u32, u32),
    pause_ui: PauseUi,
    actions: PlayActions,
    picked_texture: Option<PathBuf>,
    profiler: engine::ui::ProfilerStats,
}

impl ShellHost for DockHost<'_> {
    fn tab_ui(&mut self, tab: &EditorTab, world: &mut World, ui: &mut egui::Ui) -> bool {
        let EditorTab::Host(name) = tab else { return false };
        std::mem::swap(&mut self.game.world, world);
        let handled = self.game.docked_tab(name, ui, self.gl, self.drawable);
        std::mem::swap(&mut self.game.world, world);
        handled
    }

    fn viewport_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect, world: &mut World) {
        std::mem::swap(&mut self.game.world, world);
        if let Some(path) = self.game.texture_asset_browser.show(ctx, &self.game.asset_root) {
            self.picked_texture = Some(path);
        }
        // HUD, dialogue, pause menu and profiler anchor to `ctx.screen_rect()`; point that at
        // the viewport for the duration so they sit on the scene image, not the whole window.
        let window = ctx.screen_rect();
        ctx.input_mut(|i| i.screen_rect = viewport);
        if self.game.profiler_visible {
            engine::ui::draw_profiler_overlay(ctx, &self.profiler);
        }
        if self.game.mode == EditorMode::Play {
            self.game.draw_play_overlay(ctx, viewport, &self.pause_ui, &mut self.actions);
        }
        ctx.input_mut(|i| i.screen_rect = window);
        std::mem::swap(&mut self.game.world, world);
    }

    /// Images (drop on an object to texture it) and levels (drop anywhere to open).
    fn accepts_asset_drop(&mut self, path: &Path) -> bool {
        match asset_kind(path) {
            AssetKind::Image => true,
            AssetKind::Level => path.starts_with(&self.game.levels_dir),
            AssetKind::Other => false,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum AssetKind {
    Image,
    Level,
    Other,
}

fn asset_kind(path: &Path) -> AssetKind {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png" | "jpg" | "jpeg") => AssetKind::Image,
        Some("ron") => AssetKind::Level,
        _ => AssetKind::Other,
    }
}

fn euler_deg_of(q: Quat) -> Vec3 {
    let (x, y, z) = q.to_euler(glam::EulerRot::XYZ);
    Vec3::new(x.to_degrees(), y.to_degrees(), z.to_degrees())
}

fn build_registry() -> Registry {
    let mut r = Registry::with_defaults();
    r.register_cloneable::<Collider>();
    r.register_cloneable::<RigidBody>();
    r.register_cloneable::<ClassMember>();
    r.register_cloneable::<CharacterMeta>();
    r.register_cloneable::<SpawnerConfig>();
    r.register_cloneable::<MirrorHealth>();
    r.register_cloneable::<MirrorDialogue>();
    r.register_cloneable::<MirrorAnimator>();
    r.register_cloneable::<MirrorEmitter>();
    r.register_cloneable::<MirrorBrain>();
    r.register_cloneable::<MirrorSpawnerState>();
    r
}

impl Sandbox {
    pub(crate) fn docked_active(&self) -> bool {
        self.docked_on
    }

    // ------------------------------------------------------------- toggling

    /// Builds the editor runtime (once, at startup; never in release builds). It starts
    /// closed: the scene is shown through the editor camera until F1/F2/F4 open it.
    pub(crate) fn docked_create(&mut self, ctx: &mut Context) {
        if !editor_available() || self.docked.is_some() {
            return;
        }
        let host_tabs = [TAB_LEVEL, TAB_SELECTED, TAB_CREATE, TAB_RENDER].map(String::from).to_vec();
        match EditorRuntime::new(ctx.gl_arc(), build_registry(), Some(self.asset_root.join(".editor")), host_tabs) {
            Ok(mut rt) => {
                // The game restores itself from a `Level` snapshot on Stop and owns
                // level files, so the shell's registry snapshot and primitive-only
                // scene I/O would only get in the way.
                rt.shell.snapshot = Box::new(NullSnapshot);
                let io: SharedIo = Rc::new(RefCell::new(IoShared { levels_dir: self.levels_dir.clone(), ..Default::default() }));
                let current = self.current_level_file();
                rt.shell.scene_io = Box::new(GameSceneIo { shared: io.clone(), current, dirty: false });
                rt.set_asset_root(Some(self.asset_root.clone()));
                let ui_dir = self.asset_root.join("assets").join("ui");
                let _ = std::fs::create_dir_all(&ui_dir);
                rt.set_ui_dir(Some(ui_dir));
                let bridge = Bridge { seen_level_name: self.current_level_name.clone(), ..Bridge::default() };
                self.docked = Some(Docked { rt, bridge, io });
            }
            Err(err) => {
                // No editor to land in: behave like a shipped build instead of a blank view.
                log::error!("editor unavailable, starting in Play mode: {err:#}");
                self.enter_play_mode(ctx);
            }
        }
    }

    /// `levels/<current level>.ron` if that file exists.
    fn current_level_file(&self) -> Option<PathBuf> {
        Some(self.levels_dir.join(format!("{}.ron", self.current_level_name))).filter(|p| p.is_file())
    }

    /// View / projection while the editor is closed (Edit mode, no UI).
    pub(crate) fn closed_dock_view(&self, aspect: f32) -> (Mat4, Mat4) {
        match &self.docked {
            Some(d) => (d.rt.shell.camera.view_matrix(), d.rt.shell.camera.projection_matrix(aspect)),
            None => (Mat4::look_at_rh(Vec3::new(0.0, 4.0, 8.0), Vec3::ZERO, Vec3::Y), Mat4::perspective_rh_gl(1.0, aspect, 0.1, 500.0)),
        }
    }

    /// Opens the editor (Edit mode only) or, when it is open, closes it.
    pub(crate) fn docked_toggle(&mut self, ctx: &mut Context) {
        if !editor_available() {
            return;
        }
        if self.docked_on {
            let Some(mut d) = self.docked.take() else { return };
            if d.rt.state.mode != ShellMode::Edit {
                d.rt.shell.stop(&mut d.rt.state, &mut self.world);
            }
            let _ = d.rt.shell.take_events();
            if self.mode == EditorMode::Play {
                self.exit_play_mode(ctx);
            }
            d.rt.state.selection.clear();
            d.rt.state.undo.clear();
            d.rt.open = false;
            d.rt.save_layout();
            self.docked_on = false;
            self.docked = Some(d);
            log::info!("docked editor closed");
        } else {
            if self.mode != EditorMode::Edit {
                return;
            }
            self.docked_create(ctx);
            let (level_gen, level_name, level_file) = (self.level_gen, self.current_level_name.clone(), self.current_level_file());
            let Some(d) = self.docked.as_mut() else { return };
            d.rt.open = true;
            d.rt.state.selection.clear();
            d.rt.state.undo.clear();
            d.bridge = Bridge { seen_level_gen: level_gen, seen_level_name: level_name, ..Bridge::default() };
            d.io.borrow_mut().current_update = Some(level_file);
            self.docked_on = true;
            log::info!("docked editor opened");
        }
        // Thumbnails are registered per egui context; the other mode's ids are stale.
        self.texture_asset_browser = engine::ui::AssetBrowserState::default();
        self.pending_texture_pick = None;
    }

    /// Editor key handling. Returns `true` when the event is consumed.
    pub(crate) fn docked_event(&mut self, ctx: &mut Context, event: &Event) -> bool {
        if let Event::KeyDown { keycode: Some(key @ (Keycode::F1 | Keycode::F2 | Keycode::F4)), repeat: false, .. } = event {
            if !editor_available() || !(self.docked_on || self.mode == EditorMode::Edit) {
                return false;
            }
            // F1/F2 only open (the editor's own Hierarchy / Level tab replace them); F4 toggles.
            if *key == Keycode::F4 || !self.docked_on {
                self.docked_toggle(ctx);
            }
            return true;
        }
        if !self.docked_on {
            return false;
        }
        let Some(mut d) = self.docked.take() else { return false };
        d.rt.handle_event(event);
        let mut consumed = true;
        match event {
            Event::KeyDown { keycode: Some(Keycode::F3), repeat: false, .. } => {
                d.rt.shell.toggle_play(&mut d.rt.state, &mut self.world);
                self.docked_sync_events(ctx, &mut d);
            }
            Event::KeyDown { keycode: Some(Keycode::Escape), repeat: false, .. } => {
                d.rt.shell.toggle_pause(&mut d.rt.state);
                self.docked_sync_events(ctx, &mut d);
            }
            // The window's close button: `App` has already set `should_quit`. Veto it while the
            // shell asks about unsaved changes; `ShellEvent::QuitApproved` sets it again.
            Event::Quit { .. } => {
                consumed = false;
                if !d.rt.shell.request_quit(&mut d.rt.state, &mut self.world) {
                    ctx.should_quit = false;
                }
                self.docked_sync_events(ctx, &mut d);
            }
            // Gameplay keys (E, F9, F10, dialogue numbers) only while actually playing.
            _ => consumed = !(self.mode == EditorMode::Play && !self.paused && !d.rt.wants_keyboard()),
        }
        self.docked = Some(d);
        consumed
    }

    /// Applies the shell's Play / Pause / Stop transitions to the game.
    pub(crate) fn docked_sync_events(&mut self, ctx: &mut Context, d: &mut Docked) {
        for event in d.rt.shell.take_events() {
            match event {
                ShellEvent::PlayStarted => self.enter_play_mode(ctx),
                ShellEvent::Paused if self.mode == EditorMode::Play => self.pause(ctx),
                ShellEvent::Resumed if self.mode == EditorMode::Play => self.resume(ctx),
                ShellEvent::Stopped { .. } => {
                    self.exit_play_mode(ctx);
                    // exit_play_mode rebuilt the world: every entity id changed.
                    d.rt.state.selection.clear();
                    d.rt.state.undo.clear();
                }
                ShellEvent::AssetDropped { path, target, .. } => self.docked_asset(ctx, d, path, target),
                ShellEvent::AssetOpened(path) => self.docked_asset(ctx, d, path, None),
                ShellEvent::QuitApproved => ctx.should_quit = true,
                _ => {}
            }
        }
        self.docked_apply_io(ctx, d);
    }

    /// An asset dropped on the viewport / hierarchy, or double-clicked in the Assets tab:
    /// an image textures the object under the cursor, a level file opens.
    fn docked_asset(&mut self, ctx: &mut Context, d: &mut Docked, path: PathBuf, target: Option<Entity>) {
        match asset_kind(&path) {
            AssetKind::Image => {
                let Some(entity) = target else {
                    d.rt.shell.console.info("drop an image onto an object to use it as its texture");
                    return;
                };
                let Ok(relative) = path.strip_prefix(&self.asset_root).map(Path::to_path_buf) else {
                    d.rt.shell.console.warn(format!("{} is outside the asset folder", path.display()));
                    return;
                };
                self.pending_texture_pick = if self.world.satisfies::<&CharacterMeta>(entity).unwrap_or(false) {
                    Some(PendingTexturePick::ForCharacter(entity))
                } else if self.world.satisfies::<&LevelObjectMeta>(entity).unwrap_or(false) {
                    Some(PendingTexturePick::ForEntity(entity))
                } else {
                    None
                };
                if self.pending_texture_pick.is_some() {
                    self.push_undo_snapshot();
                    self.apply_picked_texture(ctx.gl(), relative);
                } else {
                    d.rt.shell.console.info("that object cannot take a texture");
                }
            }
            AssetKind::Level if path.starts_with(&self.levels_dir) => {
                d.rt.shell.request_open_scene(&mut d.rt.state, &mut self.world, &path);
            }
            AssetKind::Level => d.rt.shell.console.info("only files in levels/ open as levels"),
            AssetKind::Other => {}
        }
    }

    /// Runs the File-menu operations [`GameSceneIo`] queued (after the UI pass, once the
    /// world is back in `self` and the bridge has written shell edits into the game's fields).
    fn docked_apply_io(&mut self, ctx: &mut Context, d: &mut Docked) {
        let ops = std::mem::take(&mut d.io.borrow_mut().ops);
        for op in ops {
            if self.mode != EditorMode::Edit {
                d.rt.shell.console.warn("stop playing before using the File menu");
                continue;
            }
            match op {
                IoOp::New => self.io_load(ctx.gl(), Level { name: "untitled".into(), ..Level::default() }),
                IoOp::Open(level, path) => {
                    log::info!("opening level file {path:?}");
                    self.io_load(ctx.gl(), level);
                }
                IoOp::Save(path) => {
                    if let Err(err) = self.io_save(&path) {
                        d.rt.shell.console.error(err);
                    }
                }
            }
            d.bridge.seen_level_name = self.current_level_name.clone();
        }
    }

    /// File > New / Open: like the Level tab's Load (edits to the level being left stay in
    /// the cache), plus a panel-undo step so the switch itself can be undone there.
    fn io_load(&mut self, gl: &glow::Context, level: Level) {
        self.sync_current_level_into_cache();
        self.push_undo_snapshot();
        self.cache_level(level.clone());
        if let Err(err) = self.apply_level(gl, &level) {
            log::error!("failed to load level '{}': {err}", level.name);
        }
    }

    /// File > Save / Save As: the level as `build_level_from_ecs` sees it, named after the file.
    fn io_save(&mut self, path: &Path) -> Result<(), String> {
        let name = path.file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).ok_or("invalid level file name")?;
        let mut level = self.build_level_from_ecs();
        level.name = name.to_string();
        engine::level::save_to_file(&level, path).map_err(|e| format!("save {}: {e}", path.display()))?;
        log::info!("saved level '{name}' to {path:?}");
        self.current_level_name = level.name.clone();
        self.cache_level(level);
        Ok(())
    }

    fn cache_level(&mut self, level: Level) {
        match self.levels.iter_mut().find(|l| l.name == level.name) {
            Some(existing) => *existing = level,
            None => self.levels.push(level),
        }
    }

    /// Called from `Game::update` while docked.
    pub(crate) fn docked_update(&mut self, ctx: &mut Context) {
        let Some(mut d) = self.docked.take() else { return };
        self.docked_sync_events(ctx, &mut d);
        self.docked = Some(d);
    }

    // ------------------------------------------------------------ rendering

    pub(crate) fn render_docked(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let Some(mut d) = self.docked.take() else { return Ok(()) };
        let result = self.render_docked_inner(ctx, &mut d);
        self.docked = Some(d);
        result
    }

    fn render_docked_inner(&mut self, ctx: &mut Context, d: &mut Docked) -> anyhow::Result<()> {
        let drawable = ctx.drawable_size();
        let delta = ctx.time.delta;
        let gl = ctx.gl();
        if d.bridge.seen_level_name != self.current_level_name {
            // The Level tab switched levels: tell the File menu which file it is on now.
            d.bridge.seen_level_name = self.current_level_name.clone();
            d.io.borrow_mut().current_update = Some(self.current_level_file());
        }
        let size = d.rt.ensure_target(gl, drawable)?;
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        // Play shows the game camera; Edit and Pause use the editor camera.
        let simulating = self.mode == EditorMode::Play && !self.paused;
        let (view, proj) = if simulating {
            let eye = self.player_eye_position();
            (self.fp_camera.view_matrix(eye), self.fp_camera.projection_matrix(aspect))
        } else {
            d.rt.view_proj()
        };
        let draw_calls = self.render_scene(gl, size, view, proj)?;
        let params = self.render_params;
        let (tint_color, tint_strength) = self.screen_effects.current();
        {
            let renderer = self.renderer.as_mut().expect("renderer set up in init");
            if !simulating {
                renderer.draw_grid(gl, proj * view, 80.0);
            }
            renderer.present_to(
                gl,
                d.rt.target.framebuffer(),
                size,
                &PostParams {
                    color_levels: params.color_levels as f32,
                    dither_strength: params.dither_strength,
                    tint_color,
                    tint_strength,
                },
            );
        }

        self.frame_times.push_back(delta);
        while self.frame_times.len() > 30 {
            self.frame_times.pop_front();
        }
        let avg = self.frame_times.iter().sum::<f32>() / self.frame_times.len().max(1) as f32;
        let perf = PerfSnapshot {
            fps: if avg > 0.0 { 1.0 / avg } else { 0.0 },
            frame_ms: avg * 1000.0,
            entities: self.world.len() as usize,
            draw_calls,
        };

        if self.texture_asset_browser.is_open() {
            self.texture_asset_browser.ensure_thumbnails_loaded(gl, &mut d.rt.egui);
        }
        let editing = d.rt.state.mode != ShellMode::Play;
        if editing {
            self.bridge_before_ui(&mut d.bridge);
        }

        // The shell gets the real world; the host swaps it into `self.world` per call.
        let mut world = std::mem::take(&mut self.world);
        let pause_ui = self.pause_ui_inputs();
        let profiler = engine::ui::ProfilerStats {
            fps: perf.fps,
            frame_time_ms: perf.frame_ms,
            entity_count: perf.entities,
            draw_calls: perf.draw_calls,
        };
        let mut host = DockHost { game: self, gl, drawable, pause_ui, actions: PlayActions::default(), picked_texture: None, profiler };
        let events = d.rt.run_ui(gl, drawable, &mut world, &mut host, perf);
        let DockHost { actions, picked_texture, .. } = host;
        self.world = world;

        if self.level_gen != d.bridge.seen_level_gen {
            // A level (re)load respawned everything; the shell's ids and undo history are dead.
            d.rt.state.selection.clear();
            d.rt.state.undo.clear();
            d.bridge = Bridge { seen_level_gen: self.level_gen, seen_level_name: d.bridge.seen_level_name.clone(), ..Bridge::default() };
        }
        if editing {
            let selected = d.rt.state.selection.to_vec();
            self.bridge_after_ui(&selected, &events);
        }
        self.sync_selection(&mut d.bridge, &mut d.rt.state.selection);

        if let Some(path) = picked_texture {
            self.apply_picked_texture(gl, path);
        }
        self.apply_play_actions(ctx, d, actions);
        self.docked_sync_events(ctx, d);
        Ok(())
    }

    fn apply_play_actions(&mut self, ctx: &mut Context, d: &mut Docked, actions: PlayActions) {
        match actions.pause {
            Some(PauseMenuAction::Resume) => d.rt.shell.toggle_pause(&mut d.rt.state),
            Some(PauseMenuAction::ExitToEditor) => d.rt.shell.stop(&mut d.rt.state, &mut self.world),
            Some(PauseMenuAction::RestartLevel) => self.restart_level(ctx),
            Some(PauseMenuAction::QuitGame) => ctx.should_quit = true,
            None => {}
        }
        self.apply_net_actions(actions.multiplayer, actions.appearance);
    }

    /// Multiplayer / appearance panel results (identical in both editors).
    pub(crate) fn apply_net_actions(&mut self, multiplayer: Option<MultiplayerAction>, appearance: Option<AppearanceAction>) {
        match multiplayer {
            Some(MultiplayerAction::Host) => self.start_hosting(),
            Some(MultiplayerAction::Join) => self.join_game(),
            Some(MultiplayerAction::Disconnect) => self.disconnect_net(),
            Some(MultiplayerAction::SetLevelSwitchPermission { net_id, allowed }) => {
                self.set_level_switch_permission(net_id, allowed);
            }
            Some(MultiplayerAction::CancelReconnect) => self.cancel_reconnect(),
            None => {}
        }
        match appearance {
            Some(AppearanceAction::ImportNewModel) => self.pick_new_appearance_model(),
            Some(AppearanceAction::SelectExisting(index)) => self.select_appearance(index),
            Some(AppearanceAction::ResetToDefault) => self.reset_appearance(),
            None => {}
        }
    }

    // ------------------------------------------------- play-mode egui overlay

    pub(crate) fn pause_ui_inputs(&self) -> PauseUi {
        let net_players = if let NetMode::Host(host) = &self.net_mode {
            host.connected_ids()
                .into_iter()
                .filter_map(|net_id| {
                    host.player_name(net_id).map(|name| {
                        let allowed = self.remote_level_switch_permission.get(&net_id).copied().unwrap_or(false);
                        (net_id, name.to_string(), allowed)
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        PauseUi {
            rig_names: self.rigs.iter().map(|rig| rig.name.clone()).collect(),
            current_appearance: self
                .local_appearance
                .as_deref()
                .and_then(|path| self.find_rig_asset(path))
                .map(|rig| rig.name.clone()),
            net_status: self.net_status_text(),
            net_connected: matches!(self.net_mode, NetMode::Host(_))
                || matches!(&self.net_mode, NetMode::Client(client) if client.is_connected()),
            net_players,
        }
    }

    /// The pause menu and its multiplayer / appearance side panels.
    pub(crate) fn draw_pause_overlays(&mut self, ctx: &egui::Context, ui_in: &PauseUi, out: &mut PlayActions) {
        out.pause = draw_pause_menu(ctx, editor_available());
        out.multiplayer = engine::ui::draw_multiplayer_panel(
            ctx,
            &ui_in.net_status,
            ui_in.net_connected,
            self.net_reconnect_attempts_left > 0,
            &mut self.net_join_address,
            &mut self.net_player_name,
            &ui_in.net_players,
        );
        out.appearance = engine::ui::draw_appearance_panel(ctx, ui_in.current_appearance.as_deref(), &ui_in.rig_names);
    }

    fn draw_play_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect, pause_ui: &PauseUi, out: &mut PlayActions) {
        egui::Area::new("play_mode_indicator".into())
            .fixed_pos(viewport.center_top() + egui::vec2(0.0, 12.0))
            .pivot(egui::Align2::CENTER_TOP)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let escape_hint = if self.paused { "Esc to resume" } else { "Esc to pause" };
                    ui.label(format!(
                        "Play Mode \u{2014} {escape_hint} \u{b7} F3 stop \u{b7} WASD + mouse \u{b7} Space jump \u{b7} E interact \u{b7} F9 checkpoint \u{b7} F10 load checkpoint"
                    ));
                });
            });
        draw_hud(ctx, &self.hud, &self.render_params.hud);
        self.draw_dialogue_choices(ctx);
        if self.paused {
            self.draw_pause_overlays(ctx, pause_ui, out);
        }
    }

    // -------------------------------------------------------------- host tabs

    fn docked_tab(&mut self, name: &str, ui: &mut egui::Ui, gl: &glow::Context, drawable: (u32, u32)) -> bool {
        match name {
            TAB_LEVEL => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.track_undo_selection();
                    self.ui_level_section(ui, gl);
                });
            }
            TAB_SELECTED => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.track_undo_selection();
                    let mut any = false;
                    if self.selected_entity.is_some() {
                        self.ui_selected_object(ui, gl);
                        ui.separator();
                        any = true;
                    }
                    if self.selected_rig.is_some() {
                        self.ui_selected_rig(ui, gl);
                        ui.separator();
                        any = true;
                    }
                    if self.selected_character.is_some() {
                        self.ui_selected_character(ui, gl);
                        ui.separator();
                        any = true;
                    }
                    if self.selected_spawner.is_some() {
                        self.ui_selected_spawner(ui);
                        ui.separator();
                        any = true;
                    }
                    if self.selected_class.is_some() {
                        self.ui_selected_class(ui, gl);
                        any = true;
                    }
                    if !any {
                        ui.label("Nothing selected. Pick an object, character, spawner or rig in the Viewport or Hierarchy.");
                    }
                });
            }
            TAB_CREATE => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Rigs");
                    self.ui_rig_create(ui, gl);
                    ui.separator();
                    ui.heading("Characters");
                    self.ui_character_create(ui, gl);
                    ui.separator();
                    ui.heading("Spawners");
                    self.ui_spawner_create(ui);
                    ui.separator();
                    ui.heading("Classes");
                    self.ui_class_create(ui, gl);
                });
            }
            TAB_RENDER => {
                egui::ScrollArea::vertical().show(ui, |ui| self.draw_shader_editor_ui(ui, gl, drawable));
            }
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------- selection

    /// Game-side selection (what the Selected tab shows) as a flat list, primary last.
    fn game_selection(&self) -> Vec<Entity> {
        let primary = self.selected_entity.or(self.selected_character).or(self.selected_spawner).or(self.selected_rig);
        let mut v: Vec<Entity> = [self.selected_entity, self.selected_character, self.selected_spawner, self.selected_rig]
            .into_iter()
            .flatten()
            .filter(|&e| self.world.contains(e) && Some(e) != primary)
            .collect();
        v.sort_by_key(|e| e.to_bits());
        v.dedup();
        v.extend(primary.filter(|&e| self.world.contains(e)));
        v
    }

    fn set_game_selection(&mut self, list: &[Entity]) {
        let previous_rig = self.selected_rig;
        self.selected_entity = None;
        self.selected_character = None;
        self.selected_spawner = None;
        self.selected_rig = None;
        for &e in list {
            if self.world.satisfies::<&RigRoot>(e).unwrap_or(false) {
                self.selected_rig = Some(e);
            } else if self.world.satisfies::<&CharacterMeta>(e).unwrap_or(false) {
                self.selected_character = Some(e);
            } else if self.world.satisfies::<&SpawnerConfig>(e).unwrap_or(false) {
                self.selected_spawner = Some(e);
            } else if self.world.satisfies::<&LevelObjectMeta>(e).unwrap_or(false) {
                self.selected_entity = Some(e);
            }
        }
        if self.selected_rig != previous_rig {
            self.selected_rig_part = None;
        }
    }

    /// Whichever side changed this frame wins; the shell wins a tie.
    fn sync_selection(&mut self, b: &mut Bridge, selection: &mut Selection) {
        let shell_now = selection.to_vec();
        if shell_now != b.last_shell_sel {
            self.set_game_selection(&shell_now);
        } else {
            let game_now = self.game_selection();
            if game_now != b.last_game_sel {
                selection.clear();
                for e in game_now {
                    selection.add(e);
                }
            }
        }
        b.last_shell_sel = selection.to_vec();
        b.last_game_sel = self.game_selection();
    }

    // ---------------------------------------------------------------- bridge

    fn mesh_bounds(&self, d_bounds: &mut HashMap<String, Option<Aabb>>, source: &MeshSource) -> Option<Aabb> {
        d_bounds
            .entry(format!("{source:?}"))
            .or_insert_with(|| {
                let data = self.resolve_mesh_data(source).ok()?;
                let b = Aabb::of_mesh(&data)?;
                // Flat meshes (plane) would otherwise have zero thickness and be unpickable.
                let half = b.half_extents().max(Vec3::splat(0.02));
                Some(Aabb::from_center_half(b.center(), half))
            })
            .clone()
    }

    /// Makes game entities legible to the shell (names, pick bounds, locked rig
    /// parts) and rebuilds components the shell can only copy through mirrors.
    fn bridge_before_ui(&mut self, b: &mut Bridge) {
        // Names: `Name` is what the Hierarchy shows and renames; the game keeps its own copy.
        let mut named: Vec<(Entity, String)> = Vec::new();
        named.extend(self.world.query::<&LevelObjectMeta>().iter().map(|(e, m)| (e, m.name.clone())));
        named.extend(self.world.query::<&CharacterMeta>().iter().map(|(e, m)| (e, m.name.clone())));
        named.extend(self.world.query::<&SpawnerConfig>().iter().map(|(e, c)| (e, c.name.clone())));
        named.extend(self.world.query::<&RigRoot>().iter().map(|(e, r)| (e, r.name.clone())));
        for (e, game_name) in named {
            let shell_name = self.world.get::<&Name>(e).ok().map(|n| n.0.clone());
            let agreed = match shell_name {
                None => {
                    let _ = self.world.insert_one(e, Name(game_name.clone()));
                    game_name
                }
                Some(n) if n == game_name => n,
                // The shell renamed it (or this is a fresh duplicate): push into the game's field.
                Some(n) if b.names.get(&e) != Some(&n) => {
                    self.set_game_name(e, &n);
                    n
                }
                // The game's panel renamed it.
                Some(_) => {
                    let _ = self.world.insert_one(e, Name(game_name.clone()));
                    game_name
                }
            };
            b.names.insert(e, agreed);
        }
        b.names.retain(|&e, _| self.world.contains(e));

        // Pick bounds for meshes (the GPU mesh keeps no CPU copy).
        let missing: Vec<(Entity, MeshSource)> = self
            .world
            .query::<(&LevelObjectMeta, &MeshRenderer)>()
            .without::<&LocalBounds>()
            .iter()
            .map(|(e, (m, _))| (e, m.mesh_source.clone()))
            .collect();
        for (e, source) in missing {
            if let Some(b) = self.mesh_bounds(&mut b.bounds, &source) {
                let _ = self.world.insert_one(e, LocalBounds(b));
            }
        }

        // Rig parts follow their root's pose every frame, so dragging one would just snap back.
        let parts: Vec<(Entity, bool)> = self
            .world
            .query::<&RigPart>()
            .without::<&RigRoot>()
            .iter()
            .map(|(e, _)| (e, self.world.satisfies::<&Locked>(e).unwrap_or(false)))
            .filter(|&(_, locked)| !locked)
            .collect();
        for (e, _) in parts {
            let _ = self.world.insert_one(e, Locked(true));
            if !self.world.satisfies::<&Name>(e).unwrap_or(false) {
                let _ = self.world.insert_one(e, Name("rig part".into()));
            }
        }

        self.sync_mirrors(&mut b.script_tried);
    }

    fn set_game_name(&mut self, e: Entity, name: &str) {
        if let Ok(mut m) = self.world.get::<&mut LevelObjectMeta>(e) {
            m.name = name.to_string();
        }
        if let Ok(mut m) = self.world.get::<&mut CharacterMeta>(e) {
            m.name = name.to_string();
        }
        if let Ok(mut c) = self.world.get::<&mut SpawnerConfig>(e) {
            c.name = name.to_string();
        }
        if let Ok(mut r) = self.world.get::<&mut RigRoot>(e) {
            r.name = name.to_string();
        }
    }

    /// Keeps the clone-able mirrors current, and rebuilds the real component
    /// wherever a mirror exists without one (shell duplicate / undo of delete).
    fn sync_mirrors(&mut self, script_tried: &mut HashSet<Entity>) {
        let w = &mut self.world;
        // real -> mirror
        let health: Vec<(Entity, f32)> = w.query::<(&Health, &LevelObjectMeta)>().iter().map(|(e, (h, _))| (e, h.max)).collect();
        let health_c: Vec<(Entity, f32)> = w.query::<(&Health, &CharacterMeta)>().iter().map(|(e, (h, _))| (e, h.max)).collect();
        for (e, max) in health.into_iter().chain(health_c) {
            let _ = w.insert_one(e, MirrorHealth(max));
        }
        let dialogue: Vec<(Entity, Vec<DialogueNode>)> = w.query::<&Dialogue>().iter().map(|(e, d)| (e, d.nodes.clone())).collect();
        for (e, nodes) in dialogue {
            let _ = w.insert_one(e, MirrorDialogue(nodes));
        }
        let anim: Vec<(Entity, MirrorAnimator)> = w
            .query::<(&Animator, &LevelObjectMeta)>()
            .iter()
            .map(|(e, (a, _))| (e, MirrorAnimator { kind: a.kind, base_position: a.base_position, base_rotation: a.base_rotation }))
            .collect();
        for (e, m) in anim {
            let _ = w.insert_one(e, m);
        }
        let emitters: Vec<(Entity, ParticleEmitterDef)> =
            w.query::<(&ParticleEmitter, &LevelObjectMeta)>().iter().map(|(e, (p, _))| (e, p.def)).collect();
        for (e, def) in emitters {
            let _ = w.insert_one(e, MirrorEmitter(def));
        }
        let brains: Vec<Entity> = w.query::<&CharacterBrain>().iter().map(|(e, _)| e).collect();
        for e in brains {
            let _ = w.insert_one(e, MirrorBrain);
        }
        let spawners: Vec<Entity> = w.query::<&SpawnerState>().iter().map(|(e, _)| e).collect();
        for e in spawners {
            let _ = w.insert_one(e, MirrorSpawnerState);
        }

        // mirror -> real
        let need_health: Vec<(Entity, f32)> = w.query::<&MirrorHealth>().without::<&Health>().iter().map(|(e, m)| (e, m.0)).collect();
        for (e, max) in need_health {
            let _ = w.insert_one(e, Health::new(max));
        }
        let need_dialogue: Vec<(Entity, Vec<DialogueNode>)> =
            w.query::<&MirrorDialogue>().without::<&Dialogue>().iter().map(|(e, m)| (e, m.0.clone())).collect();
        for (e, nodes) in need_dialogue {
            let _ = w.insert_one(e, Dialogue::new(nodes));
        }
        let need_anim: Vec<(Entity, MirrorAnimator)> =
            w.query::<&MirrorAnimator>().without::<&Animator>().iter().map(|(e, m)| (e, m.clone())).collect();
        for (e, m) in need_anim {
            let _ = w.insert_one(e, Animator { kind: m.kind, base_position: m.base_position, base_rotation: m.base_rotation, elapsed: 0.0 });
        }
        let need_emitter: Vec<(Entity, ParticleEmitterDef)> =
            w.query::<&MirrorEmitter>().without::<&ParticleEmitter>().iter().map(|(e, m)| (e, m.0)).collect();
        for (e, def) in need_emitter {
            let _ = w.insert_one(e, ParticleEmitter::new(def));
        }
        let need_brain: Vec<(Entity, Vec3)> = w
            .query::<(&MirrorBrain, &Transform)>()
            .without::<&CharacterBrain>()
            .iter()
            .map(|(e, (_, t))| (e, t.position))
            .collect();
        for (e, home) in need_brain {
            let _ = w.insert_one(e, CharacterBrain::new(home));
        }
        let need_spawner: Vec<Entity> = w.query::<&MirrorSpawnerState>().without::<&SpawnerState>().iter().map(|(e, _)| e).collect();
        for e in need_spawner {
            let _ = w.insert_one(e, SpawnerState::new());
        }
        let need_script: Vec<(Entity, PathBuf)> = w
            .query::<&LevelObjectMeta>()
            .without::<&BehaviorSlot>()
            .iter()
            .filter_map(|(e, m)| m.script_path.clone().map(|p| (e, p)))
            .collect();
        for (e, path) in need_script {
            if script_tried.insert(e) {
                self.attach_script_to_entity(e, &path);
            }
        }
    }

    /// Writes what the shell edited (Transform) back into the game's own
    /// authoritative fields.
    fn bridge_after_ui(&mut self, selected: &[Entity], events: &[ViewportEvent]) {
        let committed = events.iter().any(|e| matches!(e, ViewportEvent::TransformCommitted { .. }));

        // Level objects keep rotation as Euler degrees in their metadata. Animated ones
        // are moving every frame, so they are only re-based when a gizmo edit commits.
        let rotated: Vec<(Entity, Vec3)> = self
            .world
            .query::<(&Transform, &LevelObjectMeta)>()
            .without::<&Animator>()
            .iter()
            .filter(|(_, (t, m))| t.rotation.dot(euler_deg_to_quat(m.rotation_euler_deg)).abs() < 0.999_999)
            .map(|(e, (t, _))| (e, euler_deg_of(t.rotation)))
            .collect();
        for (e, euler) in rotated {
            if let Ok(mut m) = self.world.get::<&mut LevelObjectMeta>(e) {
                m.rotation_euler_deg = euler;
            }
        }
        if committed {
            for &e in selected {
                let pose = self.world.get::<&Transform>(e).ok().map(|t| (t.position, t.rotation));
                let (Some((position, rotation)), Ok(mut a)) = (pose, self.world.get::<&mut Animator>(e)) else { continue };
                a.base_position = position;
                a.base_rotation = rotation;
                drop(a);
                if let Ok(mut m) = self.world.get::<&mut LevelObjectMeta>(e) {
                    m.rotation_euler_deg = euler_deg_of(rotation);
                }
            }
        }

        // A rig root's world pose is derived from its local pose each frame.
        let roots: Vec<(Entity, Vec3, Vec3)> = self
            .world
            .query::<(&Transform, &RigPart)>()
            .with::<&RigRoot>()
            .iter()
            .filter(|(_, (t, p))| (t.position - p.local_position).length_squared() > 1e-8 || t.rotation.dot(p.local_rotation).abs() < 0.999_999)
            .map(|(e, (t, _))| (e, t.position, euler_deg_of(t.rotation)))
            .collect();
        for (e, position, euler) in roots {
            if let Ok(mut p) = self.world.get::<&mut RigPart>(e) {
                p.local_position = position;
                p.set_local_rotation_euler_deg(euler);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::editor::EditorState;

    fn meta(name: &str) -> LevelObjectMeta {
        LevelObjectMeta {
            name: name.into(),
            mesh_source: MeshSource::Primitive(PrimitiveKind::Cube),
            texture_path: None,
            rotation_euler_deg: Vec3::ZERO,
            script_path: None,
            level_transition: None,
            screen_effect: None,
            camera_shake: None,
        }
    }

    fn character(name: &str) -> CharacterMeta {
        CharacterMeta {
            name: name.into(),
            color: [1.0, 0.0, 0.0],
            texture_path: None,
            disposition: Disposition::Hostile,
            move_speed: 2.0,
            wander_radius: 1.0,
            sight_range: 5.0,
            damage: Some(3.0),
            attack_range: 1.0,
            attack_cooldown_secs: 1.0,
        }
    }

    fn game() -> Sandbox {
        Sandbox::new()
    }

    #[test]
    fn gizmo_rotation_reaches_level_metadata() {
        let mut g = game();
        let e = g.world.spawn((Transform::default(), meta("Box")));
        g.world.get::<&mut Transform>(e).unwrap().rotation = euler_deg_to_quat(Vec3::new(10.0, 35.0, -20.0));
        g.bridge_after_ui(&[], &[]);
        let m = g.world.get::<&LevelObjectMeta>(e).unwrap().rotation_euler_deg;
        let t = g.world.get::<&Transform>(e).unwrap().rotation;
        assert!(euler_deg_to_quat(m).dot(t).abs() > 0.99999, "saved rotation must reproduce the edited one");
    }

    #[test]
    fn animated_object_is_rebased_only_when_a_gizmo_edit_commits() {
        let mut g = game();
        let anim = Animator {
            kind: AnimationKind::Bob { axis: Vec3::Y, amplitude: 0.5, period_secs: 2.0 },
            base_position: Vec3::ZERO,
            base_rotation: Quat::IDENTITY,
            elapsed: 0.0,
        };
        let e = g.world.spawn((Transform::default(), meta("Bobber"), anim));
        g.world.get::<&mut Transform>(e).unwrap().position = Vec3::new(4.0, 0.0, 0.0);
        g.bridge_after_ui(&[e], &[]);
        assert_eq!(g.world.get::<&Animator>(e).unwrap().base_position, Vec3::ZERO);
        g.bridge_after_ui(&[e], &[ViewportEvent::TransformCommitted { label: "Move".into() }]);
        assert_eq!(g.world.get::<&Animator>(e).unwrap().base_position, Vec3::new(4.0, 0.0, 0.0));
    }

    #[test]
    fn rig_root_pose_is_written_back_to_its_local_pose() {
        let mut g = game();
        let part = RigPart { parent: None, local_position: Vec3::ZERO, local_rotation_euler_deg: Vec3::ZERO, local_rotation: Quat::IDENTITY };
        let e = g.world.spawn((Transform::default(), part, RigRoot { name: "r".into(), rig_path: PathBuf::new() }));
        g.world.get::<&mut Transform>(e).unwrap().position = Vec3::new(1.0, 2.0, 3.0);
        g.bridge_after_ui(&[e], &[]);
        assert_eq!(g.world.get::<&RigPart>(e).unwrap().local_position, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn names_follow_whichever_side_was_renamed() {
        let mut g = game();
        let mut b = Bridge::default();
        let e = g.world.spawn((Transform::default(), meta("Crate")));
        g.bridge_before_ui(&mut b);
        assert_eq!(g.world.get::<&Name>(e).unwrap().0, "Crate");
        // Hierarchy rename.
        g.world.get::<&mut Name>(e).unwrap().0 = "Barrel".into();
        g.bridge_before_ui(&mut b);
        assert_eq!(g.world.get::<&LevelObjectMeta>(e).unwrap().name, "Barrel");
        // Panel rename.
        g.world.get::<&mut LevelObjectMeta>(e).unwrap().name = "Pillar".into();
        g.bridge_before_ui(&mut b);
        assert_eq!(g.world.get::<&Name>(e).unwrap().0, "Pillar");
    }

    #[test]
    fn plane_bounds_are_pickable_and_rig_parts_locked() {
        let g = game();
        let mut cache = HashMap::new();
        let b = g.mesh_bounds(&mut cache, &MeshSource::Primitive(PrimitiveKind::Plane)).unwrap();
        assert_eq!(b.half_extents().x, 1.0);
        assert!(b.half_extents().y >= 0.02);
        let mut g = game();
        let part = RigPart { parent: None, local_position: Vec3::ZERO, local_rotation_euler_deg: Vec3::ZERO, local_rotation: Quat::IDENTITY };
        let e = g.world.spawn((Transform::default(), part));
        g.bridge_before_ui(&mut Bridge::default());
        assert!(g.world.get::<&Locked>(e).unwrap().0);
    }

    #[test]
    fn selection_maps_between_shell_and_game_panels() {
        let mut g = game();
        let mut b = Bridge::default();
        let mut sel = Selection::default();
        let obj = g.world.spawn((Transform::default(), meta("Box")));
        let npc = g.world.spawn((Transform::default(), character("Guard")));
        let rig = g.world.spawn((Transform::default(), RigRoot { name: "r".into(), rig_path: PathBuf::new() }));
        sel.add(obj);
        sel.add(npc);
        g.sync_selection(&mut b, &mut sel);
        assert_eq!(g.selected_entity, Some(obj));
        assert_eq!(g.selected_character, Some(npc));
        // The game's rig list picks something: the shell follows.
        g.selected_rig = Some(rig);
        g.sync_selection(&mut b, &mut sel);
        assert!(sel.contains(rig));
        // Shell clears: the game follows.
        sel.clear();
        g.sync_selection(&mut b, &mut sel);
        assert!(g.selected_entity.is_none() && g.selected_character.is_none() && g.selected_rig.is_none());
    }

    #[test]
    fn shell_duplicate_and_undo_delete_rebuild_non_clone_components() {
        let mut g = game();
        let mut b = Bridge::default();
        let npc = g.world.spawn((
            Transform::from_position(Vec3::new(5.0, 0.0, 0.0)),
            character("Guard"),
            CharacterBrain::new(Vec3::new(5.0, 0.0, 0.0)),
            Health::new(40.0),
            Dialogue::new(vec![DialogueNode { text: "hi".into(), choices: Vec::new() }]),
        ));
        let spinner = g.world.spawn((
            Transform::default(),
            meta("Spinner"),
            Animator { kind: AnimationKind::Orbit { axis: Vec3::Y, speed_deg_per_sec: 90.0 }, base_position: Vec3::ZERO, base_rotation: Quat::IDENTITY, elapsed: 0.0 },
            ParticleEmitter::new(ParticleEmitterDef::default()),
        ));
        g.bridge_before_ui(&mut b);

        let mut state = EditorState::with_registry(build_registry());
        state.selection.add(npc);
        state.selection.add(spinner);
        let copies = state.duplicate_selection(&mut g.world);
        assert_eq!(copies.len(), 2);
        g.bridge_before_ui(&mut b);
        let copy_npc = copies.iter().copied().find(|&c| g.world.satisfies::<&CharacterMeta>(c).unwrap()).unwrap();
        let copy_obj = copies.iter().copied().find(|&c| g.world.satisfies::<&LevelObjectMeta>(c).unwrap()).unwrap();
        assert_eq!(g.world.get::<&Health>(copy_npc).unwrap().max, 40.0);
        assert!(g.world.satisfies::<&CharacterBrain>(copy_npc).unwrap());
        assert_eq!(g.world.get::<&Dialogue>(copy_npc).unwrap().nodes.len(), 1);
        assert!(g.world.satisfies::<&Animator>(copy_obj).unwrap());
        assert!(g.world.satisfies::<&ParticleEmitter>(copy_obj).unwrap());

        // Delete + undo (shell) restores the original with its game components.
        state.selection.clear();
        state.selection.add(npc);
        assert_eq!(state.delete_selection(&mut g.world), 1);
        assert!(state.undo(&mut g.world));
        g.bridge_before_ui(&mut b);
        let restored = g.world.query::<&CharacterMeta>().iter().filter(|(_, m)| m.name == "Guard").map(|(e, _)| e).collect::<Vec<_>>();
        assert!(!restored.is_empty());
        for e in restored {
            assert!(g.world.satisfies::<&Health>(e).unwrap());
            assert!(g.world.satisfies::<&CharacterBrain>(e).unwrap());
        }
    }

    fn io(levels_dir: &Path) -> (GameSceneIo, SharedIo) {
        let shared: SharedIo = Rc::new(RefCell::new(IoShared { levels_dir: levels_dir.to_path_buf(), ..Default::default() }));
        (GameSceneIo { shared: shared.clone(), current: None, dirty: false }, shared)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("walkaround_docked_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_menu_queues_level_work_against_the_levels_folder() {
        let dir = temp_dir("io");
        let mut world = World::new();
        let (mut scene_io, shared) = io(&dir);

        scene_io.save(&world, Path::new("room.ron")).unwrap();
        assert_eq!(scene_io.current_path(), Some(dir.join("room.ron").as_path()), "relative names land in levels/");
        assert!(scene_io.save(&world, Path::new("")).is_err(), "no file stem, no save");

        engine::level::save_to_file(&Level { name: "room".into(), ..Level::default() }, &dir.join("room.ron")).unwrap();
        scene_io.open(Path::new("room.ron"), &mut world).unwrap();
        assert!(scene_io.open(Path::new("missing.ron"), &mut world).is_err());
        scene_io.new_scene(&mut world).unwrap();
        assert!(scene_io.current_path().is_none());

        let ops = std::mem::take(&mut shared.borrow_mut().ops);
        assert!(matches!(ops.as_slice(), [IoOp::Save(_), IoOp::Open(l, _), IoOp::New] if l.name == "room"));

        // The game switching levels behind the menu's back, and the shell's dirty report.
        shared.borrow_mut().current_update = Some(Some(dir.join("other.ron")));
        scene_io.sync_undo_dirty(true);
        assert_eq!(scene_io.current_path(), Some(dir.join("other.ron").as_path()));
        assert!(scene_io.is_dirty());
        assert_eq!(scene_io.title(), "other.ron*");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_the_level_named_after_the_file_and_caches_it() {
        let dir = temp_dir("save");
        let mut g = game();
        let mut t = Transform::from_position(Vec3::new(1.0, 2.0, 3.0));
        t.scale = Vec3::splat(2.0);
        g.world.spawn((t, meta("Crate")));
        let path = dir.join("my_room.ron");
        g.io_save(&path).unwrap();
        let saved = engine::level::load_from_file(&path).unwrap();
        assert_eq!(saved.name, "my_room");
        assert_eq!(saved.objects.len(), 1);
        assert_eq!(saved.objects[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(g.current_level_name, "my_room");
        assert!(g.levels.iter().any(|l| l.name == "my_room"));
        // Saving again replaces the cache entry instead of adding a second one.
        g.io_save(&path).unwrap();
        assert_eq!(g.levels.iter().filter(|l| l.name == "my_room").count(), 1);
        assert!(g.io_save(Path::new("")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn asset_drops_are_images_and_levels_only() {
        assert_eq!(asset_kind(Path::new("a/b.PNG")), AssetKind::Image);
        assert_eq!(asset_kind(Path::new("a/b.jpeg")), AssetKind::Image);
        assert_eq!(asset_kind(Path::new("levels/x.ron")), AssetKind::Level);
        assert_eq!(asset_kind(Path::new("music/x.wav")), AssetKind::Other);
        assert_eq!(asset_kind(Path::new("noext")), AssetKind::Other);
    }
}
