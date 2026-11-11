//! The docked editor: the engine's `EditorRuntime`, and the game's only editor.
//! F1, F2, F4 (and `DUST_EDITOR=1` at startup) open it in debug builds; F4
//! closes it again. Release builds never create it.
//!
//! What the shell does here: viewport tab (the game's own scene, post-FX
//! included), editor camera in Edit/Pause, click/box selection, move/rotate/
//! scale gizmos, hierarchy, inspector, shell undo/redo and Ctrl+D/Del, the
//! Assets tab (rooted at the game's asset dir; a dropped image textures the
//! object under the cursor, a dropped level `.ron` loads it), the UI designer
//! (`<asset root>/ui`) and the File menu over `levels/*.ron` ([`GameSceneIo`]).
//!
//! What stays game-side:
//! * Play/Stop. The shell's registry snapshot only knows registered
//!   components, so it is replaced by `NullSnapshot` and Play/Stop drive the
//!   game's own `enter_play_mode`/`exit_play_mode` (a full `Level` snapshot,
//!   which also covers rigs, characters and spawners).
//! * The game panels: `Level (F2)` (rigs, classes, characters, spawners,
//!   emitters, music, textures, scripts; its own level-snapshot undo, labelled
//!   in the tab), `Render (F1)` (shader profiles, render params, physics,
//!   camera feel) and `Multiplayer` (plus the appearance window).
//!
//! Bridge: level objects are hecs entities with `Transform`, so the shell can
//! pick them. [`bridge_world`] adds the missing editor components
//! (`Name`, `LocalBounds`) every frame, keeps names in sync with the level
//! metadata, and restores runtime-only components that the shell's
//! clone-based Duplicate / Undo-Delete cannot copy (animators, emitters,
//! brains...), from small clonable shadow components.
//!
//! Everything here is reachable only when `editor_available()` is true.

use engine::editor::bounds::{Aabb, LocalBounds};
use engine::editor::hierarchy::{Primitive, PrimitiveShape};
use engine::editor::runtime::EditorRuntime;
use engine::editor::scene_io::{NullSnapshot, SceneIo};
use engine::editor::shell::{PerfSnapshot, ShellEvent, ShellHost};
use engine::editor::state::EditorMode as ShellMode;
use engine::editor::{Locked, Name, Registry};
use engine::glam::EulerRot;
use engine::ui::EditorTab;
use std::cell::RefCell;
use std::rc::Rc;

use super::*;

const LEVEL_TAB: &str = "Level (F2)";
const RENDER_TAB: &str = "Render (F1)";
const NET_TAB: &str = "Multiplayer";

/// (object, character, spawner, rig) as the F2 panel tracks them.
type GameSel = (Option<Entity>, Option<Entity>, Option<Entity>, Option<Entity>);

#[derive(Default)]
pub struct DockedState {
    rt: Option<EditorRuntime>,
    /// Authoritative open flag (`rt.open` is a mirror; `rt` is taken out of
    /// the struct while a frame is drawn).
    pub open: bool,
    /// Set by `apply_level`: every entity id is stale.
    pub world_rebuilt: bool,
    /// Play only: mouse captured by the game (relative mode).
    captured: bool,
    seen: HashSet<Entity>,
    bounds_cache: HashMap<String, Option<Aabb>>,
    last_shell_sel: Option<Entity>,
    last_game_sel: GameSel,
    show_net: bool,
    drawable: (u32, u32),
    picked_texture: Option<PathBuf>,
    net_actions: (Option<engine::ui::MultiplayerAction>, Option<engine::ui::AppearanceAction>),
    /// File-menu requests from [`GameSceneIo`], applied after the UI pass.
    scene: Rc<RefCell<SceneBridge>>,
    /// Window close while the editor is open: ask the shell first (unsaved
    /// changes prompt) instead of quitting at once.
    quit_requested: bool,
}

// ------------------------------------------------------------ File menu

enum SceneOp {
    New,
    Open { level: Level, path: PathBuf },
    Save(PathBuf),
}

/// State shared between [`GameSceneIo`] (boxed inside the shell, inside the
/// runtime that is taken out of `Sandbox` while the UI runs) and the game.
#[derive(Default)]
struct SceneBridge {
    ops: Vec<SceneOp>,
    /// File of the open level; `None` for a new or never-saved one.
    current: Option<PathBuf>,
    /// Level name `current` was derived from (re-derived when it changes,
    /// e.g. after the Level tab's Load list switched levels).
    named: Option<String>,
}

/// File > New/Open/Save/Save As/Revert over `levels/*.ron`, using the game's
/// own `Level` format. Opening and saving need the game (GPU, rigs, music...),
/// so they are queued here and run by [`Sandbox::docked_scene_ops`] right
/// after the UI pass; parse errors and unwritable paths are still reported
/// by the shell immediately.
struct GameSceneIo {
    levels_dir: PathBuf,
    shared: Rc<RefCell<SceneBridge>>,
    path: Option<PathBuf>,
    undo_dirty: bool,
}

impl GameSceneIo {
    /// A bare name or relative path means a file in `levels/`; `.ron` is added.
    fn resolve(&self, path: &Path) -> PathBuf {
        let mut p = if path.is_absolute() || path.starts_with(&self.levels_dir) {
            path.to_path_buf()
        } else {
            self.levels_dir.join(path)
        };
        if p.extension().is_none() {
            p.set_extension("ron");
        }
        p
    }
}

impl SceneIo for GameSceneIo {
    fn new_scene(&mut self, _: &mut World) -> Result<(), String> {
        let mut shared = self.shared.borrow_mut();
        shared.ops.push(SceneOp::New);
        shared.current = None;
        shared.named = Some("untitled".into());
        self.path = None;
        Ok(())
    }

    fn open(&mut self, path: &Path, _: &mut World) -> Result<(), String> {
        let path = self.resolve(path);
        let level = engine::level::load_from_file(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut shared = self.shared.borrow_mut();
        shared.named = Some(level.name.clone());
        shared.current = Some(path.clone());
        shared.ops.push(SceneOp::Open { level, path: path.clone() });
        self.path = Some(path);
        Ok(())
    }

    fn save(&mut self, _: &World, path: &Path) -> Result<(), String> {
        let path = self.resolve(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("save {}: {e}", path.display()))?;
        }
        let mut shared = self.shared.borrow_mut();
        shared.ops.push(SceneOp::Save(path.clone()));
        shared.current = Some(path.clone());
        shared.named = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        self.path = Some(path);
        Ok(())
    }

    fn current_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn is_dirty(&self) -> bool {
        self.undo_dirty
    }

    /// Called by the shell every frame: also where the game's idea of the
    /// open file (set by the Level tab's Load list) is picked up.
    fn sync_undo_dirty(&mut self, undo_dirty: bool) {
        self.undo_dirty = undo_dirty;
        self.path = self.shared.borrow().current.clone();
    }
}

// ------------------------------------------------------------------ bridge

/// Last name the hierarchy and the level metadata agreed on, so a change on
/// either side can be told apart.
#[derive(Clone)]
struct BridgedName(String);
/// Clonable shadows of components the shell's Duplicate / Undo-Delete cannot
/// clone (`Animator`, `ParticleEmitter`, ...); see [`bridge_world`].
#[derive(Clone, Copy)]
struct BridgedAnim(AnimationSpec);
#[derive(Clone, Copy)]
struct BridgedEmitter(ParticleEmitterDef);
#[derive(Clone)]
struct BridgedCharacter(CharacterInstance);

/// Registry for the shell: everything on a level entity that is `Clone`, so
/// Duplicate and Undo-of-Delete keep the entity whole.
fn editor_registry() -> Registry {
    let mut r = Registry::with_defaults();
    r.register_cloneable::<LevelObjectMeta>();
    r.register_cloneable::<MeshRenderer>();
    r.register_cloneable::<Collider>();
    r.register_cloneable::<RigidBody>();
    r.register_cloneable::<ClassMember>();
    r.register_cloneable::<MushroomPickup>();
    r.register_cloneable::<CharacterMeta>();
    r.register_cloneable::<SpawnerConfig>();
    r.register_cloneable::<LocalBounds>();
    r.register_cloneable::<BridgedName>();
    r.register_cloneable::<BridgedAnim>();
    r.register_cloneable::<BridgedEmitter>();
    r.register_cloneable::<BridgedCharacter>();
    r
}

/// `CharacterInstance` for a live character entity (the saved form).
pub fn character_instance_of(world: &World, entity: Entity) -> Option<CharacterInstance> {
    let mut query = world.query_one::<(&Transform, &CharacterMeta)>(entity).ok()?;
    let (transform, meta) = query.get()?;
    let max_health = world.get::<&Health>(entity).ok().map(|health| health.max);
    let dialogue_nodes = world.get::<&Dialogue>(entity).map(|d| d.nodes.clone()).unwrap_or_default();
    Some(CharacterInstance {
        name: meta.name.clone(),
        position: transform.position.to_array(),
        scale: transform.scale.to_array(),
        color: meta.color,
        texture_path: meta.texture_path.clone(),
        disposition: meta.disposition,
        move_speed: meta.move_speed,
        wander_radius: meta.wander_radius,
        sight_range: meta.sight_range,
        max_health,
        damage: meta.damage,
        attack_range: meta.attack_range,
        attack_cooldown_secs: meta.attack_cooldown_secs,
        dialogue_nodes,
    })
}

fn anim_spec(animator: &Animator) -> AnimationSpec {
    match animator.kind {
        AnimationKind::Orbit { axis, speed_deg_per_sec } => AnimationSpec::Orbit { axis: axis.to_array(), speed_deg_per_sec },
        AnimationKind::Bob { axis, amplitude, period_secs } => AnimationSpec::Bob { axis: axis.to_array(), amplitude, period_secs },
    }
}

fn anim_kind(spec: AnimationSpec) -> AnimationKind {
    match spec {
        AnimationSpec::Orbit { axis, speed_deg_per_sec } => AnimationKind::Orbit { axis: Vec3::from(axis), speed_deg_per_sec },
        AnimationSpec::Bob { axis, amplitude, period_secs } => AnimationKind::Bob { axis: Vec3::from(axis), amplitude, period_secs },
    }
}

/// Reconciles the hierarchy `Name` with the owner's own name field.
/// Returns `Some(new)` when the hierarchy renamed it (owner must adopt).
fn sync_name(world: &mut World, e: Entity, owner: &str) -> Option<String> {
    let Some(name) = world.get::<&Name>(e).ok().map(|n| n.0.clone()) else {
        let _ = world.insert(e, (Name(owner.to_string()), BridgedName(owner.to_string())));
        return None;
    };
    let bridged = world.get::<&BridgedName>(e).ok().map(|b| b.0.clone()).unwrap_or_else(|| owner.to_string());
    if name != bridged {
        let _ = world.insert_one(e, BridgedName(name.clone()));
        return Some(name);
    }
    if name != owner {
        if let Ok(mut n) = world.get::<&mut Name>(e) {
            n.0 = owner.to_string();
        }
    }
    if bridged != owner {
        let _ = world.insert_one(e, BridgedName(owner.to_string()));
    }
    None
}

#[derive(Default)]
pub struct BridgeReport {
    /// Restored entities whose script has to be re-attached (needs file IO).
    pub needs_script: Vec<Entity>,
}

/// Per-frame bridge between the game's entities and the shell. `edit` is
/// true while the scene is not simulating (Edit/Pause): only then are the
/// shadows refreshed and missing runtime components restored. `seen` holds
/// the entities already bridged; an entity not in it is new to the editor
/// (spawned, duplicated, or restored by undo) and gets healed once.
pub fn bridge_world(
    world: &mut World,
    seen: &mut HashSet<Entity>,
    bounds_of: &mut dyn FnMut(&MeshSource) -> Option<Aabb>,
    edit: bool,
) -> BridgeReport {
    seen.retain(|e| world.contains(*e));
    let mut report = BridgeReport::default();

    // Level objects (meshes, lights, particle emitters).
    let objects: Vec<Entity> = world.query::<&LevelObjectMeta>().iter().map(|(e, _)| e).collect();
    for e in objects {
        let fresh = !seen.contains(&e);
        let owner = world.get::<&LevelObjectMeta>(e).map(|m| m.name.clone()).unwrap_or_default();
        if let Some(renamed) = sync_name(world, e, &owner) {
            if let Ok(mut meta) = world.get::<&mut LevelObjectMeta>(e) {
                meta.name = renamed;
            }
        }
        if world.get::<&LocalBounds>(e).is_err() && world.get::<&MeshRenderer>(e).is_ok() {
            let source = world.get::<&LevelObjectMeta>(e).map(|m| m.mesh_source.clone());
            if let Some(b) = source.ok().and_then(|s| bounds_of(&s)) {
                let _ = world.insert_one(e, LocalBounds(b));
            }
        }
        if edit {
            if fresh {
                if world.get::<&Animator>(e).is_err() {
                    let spec = world.get::<&BridgedAnim>(e).map(|s| s.0);
                    let pose = world.get::<&Transform>(e).map(|t| (t.position, t.rotation));
                    if let (Ok(spec), Ok((position, rotation))) = (spec, pose) {
                        let _ = world.insert_one(
                            e,
                            Animator { kind: anim_kind(spec), base_position: position, base_rotation: rotation, elapsed: 0.0 },
                        );
                    }
                }
                if world.get::<&ParticleEmitter>(e).is_err() {
                    if let Ok(def) = world.get::<&BridgedEmitter>(e).map(|s| s.0) {
                        let _ = world.insert_one(e, ParticleEmitter::new(def));
                    }
                }
                let has_script = world.get::<&LevelObjectMeta>(e).is_ok_and(|m| m.script_path.is_some());
                if has_script && world.get::<&BehaviorSlot>(e).is_err() {
                    report.needs_script.push(e);
                }
            }
            let spec = world.get::<&Animator>(e).ok().map(|a| anim_spec(&a));
            if let Some(spec) = spec {
                let _ = world.insert_one(e, BridgedAnim(spec));
            }
            let def = world.get::<&ParticleEmitter>(e).ok().map(|p| p.def);
            if let Some(def) = def {
                let _ = world.insert_one(e, BridgedEmitter(def));
            }
        }
        seen.insert(e);
    }

    // Characters.
    let characters: Vec<Entity> = world.query::<&CharacterMeta>().iter().map(|(e, _)| e).collect();
    for e in characters {
        let fresh = !seen.contains(&e);
        let owner = world.get::<&CharacterMeta>(e).map(|m| m.name.clone()).unwrap_or_default();
        if let Some(renamed) = sync_name(world, e, &owner) {
            if let Ok(mut meta) = world.get::<&mut CharacterMeta>(e) {
                meta.name = renamed;
            }
        }
        if edit {
            if fresh {
                let shadow = world.get::<&BridgedCharacter>(e).map(|b| b.0.clone());
                let position = world.get::<&Transform>(e).map(|t| t.position);
                if let (Ok(instance), Ok(position)) = (shadow, position) {
                    if world.get::<&CharacterBrain>(e).is_err() {
                        let _ = world.insert_one(e, CharacterBrain::new(position));
                    }
                    let missing_health = world.get::<&Health>(e).is_err();
                    if let (Some(max), true) = (instance.max_health, missing_health) {
                        let _ = world.insert_one(e, Health::new(max));
                    }
                    if !instance.dialogue_nodes.is_empty() && world.get::<&Dialogue>(e).is_err() {
                        let _ = world.insert_one(e, Dialogue::new(instance.dialogue_nodes));
                    }
                }
            }
            if let Some(instance) = character_instance_of(world, e) {
                let _ = world.insert_one(e, BridgedCharacter(instance));
            }
        }
        seen.insert(e);
    }

    // Spawners.
    let spawners: Vec<Entity> = world.query::<&SpawnerConfig>().iter().map(|(e, _)| e).collect();
    for e in spawners {
        let owner = world.get::<&SpawnerConfig>(e).map(|c| c.name.clone()).unwrap_or_default();
        if let Some(renamed) = sync_name(world, e, &owner) {
            if let Ok(mut config) = world.get::<&mut SpawnerConfig>(e) {
                config.name = renamed;
            }
        }
        if edit && !seen.contains(&e) && world.get::<&SpawnerState>(e).is_err() {
            let _ = world.insert_one(e, SpawnerState::new());
        }
        seen.insert(e);
    }

    // Rig parts are driven by `RigPart` (world pose is rewritten every frame),
    // so a Transform edit would be lost: show them, but keep them locked.
    let parts: Vec<(Entity, Option<String>)> = world
        .query::<&RigPart>()
        .iter()
        .map(|(e, _)| (e, world.get::<&RigRoot>(e).ok().map(|r| r.name.clone())))
        .collect();
    for (e, root_name) in parts {
        if world.get::<&Locked>(e).is_err() {
            let _ = world.insert_one(e, Locked(true));
        }
        if world.get::<&Name>(e).is_err() {
            let _ = world.insert_one(e, Name(root_name.unwrap_or_else(|| "Rig part".to_string())));
        }
    }

    let players: Vec<Entity> = world.query::<&PlayerController>().iter().map(|(e, _)| e).collect();
    for e in players {
        if world.get::<&Name>(e).is_err() {
            let _ = world.insert_one(e, Name("Player".to_string()));
        }
    }
    report
}

fn euler_deg_of(rotation: Quat) -> Vec3 {
    let (x, y, z) = rotation.to_euler(EulerRot::XYZ);
    Vec3::new(x.to_degrees(), y.to_degrees(), z.to_degrees())
}

/// Edit mode only: makes the components the level saver reads agree with the
/// `Transform` the gizmos edit (Euler metadata, animator base pose, collider
/// size). Call before anything that builds a `Level` from the world.
pub fn reconcile_edit(world: &mut World) {
    let objects: Vec<Entity> = world.query::<(&Transform, &LevelObjectMeta)>().iter().map(|(e, _)| e).collect();
    for e in objects {
        let Ok(t) = world.get::<&Transform>(e).map(|t| *t) else { continue };
        if let Ok(mut meta) = world.get::<&mut LevelObjectMeta>(e) {
            if euler_deg_to_quat(meta.rotation_euler_deg).angle_between(t.rotation) > 1e-3 {
                meta.rotation_euler_deg = euler_deg_of(t.rotation);
            }
            if let Ok(mut collider) = world.get::<&mut Collider>(e) {
                if let ColliderShape::Aabb { half_extents } = &mut collider.shape {
                    let want = collider_half_extents(&meta.mesh_source, t.scale.to_array());
                    if (*half_extents - want).abs().max_element() > 1e-4 {
                        *half_extents = want;
                    }
                }
            }
        }
        if let Ok(mut animator) = world.get::<&mut Animator>(e) {
            animator.base_position = t.position;
            animator.base_rotation = t.rotation;
        }
    }
}

/// Puts every animated object at its base pose (the docked Edit mode does
/// not run animators, so the pose on screen is the pose that gets saved).
pub fn snap_animators_to_base(world: &mut World) {
    for (_, (t, a)) in world.query_mut::<(&mut Transform, &Animator)>() {
        t.position = a.base_position;
        t.rotation = a.base_rotation;
    }
}

// ------------------------------------------------------------ Sandbox glue

impl Sandbox {
    pub(crate) fn docked_editing(&self) -> bool {
        self.docked.open && self.mode == EditorMode::Edit
    }

    /// Play-time input is ignored while the docked editor is open and the
    /// mouse is not captured by the game (typing in a panel must not walk).
    pub(crate) fn docked_input_blocked(&self) -> bool {
        self.docked.open && !self.docked.captured
    }

    pub(crate) fn init_docked(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let tabs = vec![LEVEL_TAB.to_string(), RENDER_TAB.to_string(), NET_TAB.to_string()];
        let mut rt = EditorRuntime::new(ctx.gl_arc(), editor_registry(), Some(self.asset_root.join(".editor")), tabs)?;
        rt.shell.snapshot = Box::new(NullSnapshot);
        rt.shell.scene_io = Box::new(GameSceneIo {
            levels_dir: self.levels_dir.clone(),
            shared: self.docked.scene.clone(),
            path: None,
            undo_dirty: false,
        });
        // Assets tab: the game's asset dir, so a path relative to it is what
        // `LevelObjectMeta::texture_path` and the level files already use.
        rt.set_asset_root(Some(self.asset_root.clone()));
        rt.set_ui_dir(Some(self.asset_root.join("ui")));
        self.docked.rt = Some(rt);
        // Same switch the physics sandbox uses: start with the editor open.
        if std::env::var_os("DUST_EDITOR").is_some() {
            self.set_docked_open(ctx, true);
        }
        Ok(())
    }

    /// Relative mouse mode follows `captured`. Set every call (SDL ignores a
    /// no-op) because `enter_play_mode`/`restart_level` switch it on by themselves.
    fn apply_mouse(&mut self, ctx: &mut Context) {
        let want = self.docked.captured && self.mode == EditorMode::Play && !self.paused;
        if !want {
            self.docked.captured = false;
        }
        ctx.platform.sdl.mouse().set_relative_mouse_mode(want);
    }

    /// The scene seen through the (idle) editor camera, for the closed editor
    /// in Edit mode.
    pub(crate) fn docked_closed_view(&self, aspect: f32) -> (Mat4, Mat4) {
        match self.docked.rt.as_ref() {
            Some(rt) => (rt.shell.camera.view_matrix(), rt.shell.camera.projection_matrix(aspect)),
            None => {
                let camera = engine::editor::camera::EditorCamera::default();
                (camera.view_matrix(), camera.projection_matrix(aspect))
            }
        }
    }

    fn set_docked_open(&mut self, ctx: &mut Context, open: bool) {
        if open == self.docked.open {
            return;
        }
        let Some(rt) = self.docked.rt.as_mut() else { return };
        self.docked.open = open;
        rt.open = open;
        log::info!("docked editor {}", if open { "opened" } else { "closed" });
        self.docked.captured = false;
        if open {
            rt.state.mode = match (self.mode, self.paused) {
                (EditorMode::Edit, _) => ShellMode::Edit,
                (EditorMode::Play, false) => ShellMode::Play,
                (EditorMode::Play, true) => ShellMode::Pause,
            };
            if self.mode == EditorMode::Edit {
                snap_animators_to_base(&mut self.world);
            }
            self.docked.seen.clear();
            ctx.platform.sdl.mouse().set_relative_mouse_mode(false);
        } else {
            rt.save_layout();
            rt.state.stop();
            // Hand the mouse back to the original editor's expectations.
            let relative = self.mode == EditorMode::Play && !self.paused;
            ctx.platform.sdl.mouse().set_relative_mouse_mode(relative);
        }
    }

    /// F4 toggles; while open, the docked editor owns input. Returns true if
    /// the event was consumed.
    pub(crate) fn docked_handle_event(&mut self, ctx: &mut Context, event: &Event) -> bool {
        if !editor_available() || self.docked.rt.is_none() {
            return false;
        }
        if let Event::KeyDown { keycode: Some(key), repeat: false, .. } = event {
            match *key {
                Keycode::F4 => {
                    self.set_docked_open(ctx, !self.docked.open);
                    return true;
                }
                // F1/F2 open the editor; once it is open F2 is the shell's Rename.
                Keycode::F1 | Keycode::F2 if !self.docked.open => {
                    self.set_docked_open(ctx, true);
                    return true;
                }
                _ => {}
            }
        }
        if !self.docked.open {
            return false;
        }
        if let Event::Quit { .. } = event {
            // The shell asks about unsaved changes first (see `render_docked_with`).
            ctx.should_quit = false;
            self.docked.quit_requested = true;
            return true;
        }
        let playing = self.mode == EditorMode::Play && !self.paused;
        let Some(rt) = self.docked.rt.as_mut() else { return false };
        rt.handle_event(event);
        let (hovered, typing) = (rt.shell.viewport.hovered, rt.wants_keyboard());
        match event {
            Event::KeyDown { keycode: Some(Keycode::F3), repeat: false, .. } => {
                rt.shell.toggle_play(&mut rt.state, &mut self.world);
                return true;
            }
            Event::KeyDown { keycode: Some(Keycode::Escape), repeat: false, .. } => {
                self.docked.captured = false;
                self.apply_mouse(ctx);
                return true;
            }
            // Shader profile shortcuts: Tab cycles (over the viewport only, Tab is
            // also egui's focus key), F5 saves the current profile.
            Event::KeyDown { keycode: Some(Keycode::Tab), repeat: false, .. } if hovered && !typing && !playing => {
                if let Some(cycler) = self.profiles.as_mut() {
                    let profile = cycler.next().clone();
                    if let Err(err) = self.apply_profile(ctx.gl(), ctx.drawable_size(), &profile) {
                        log::error!("failed to apply profile '{}': {err}", profile.name);
                    }
                }
                return true;
            }
            Event::KeyDown { keycode: Some(Keycode::F5), repeat: false, .. } if !typing && !playing => {
                self.save_current_profile();
                return true;
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, .. } if playing && !self.docked.captured && hovered => {
                self.docked.captured = true;
                self.apply_mouse(ctx);
                return true;
            }
            _ => {}
        }
        // Gameplay keys still reach the game handler while playing.
        let gameplay = match event {
            Event::KeyDown { keycode: Some(k), repeat: false, .. } => {
                matches!(*k, Keycode::E | Keycode::F9 | Keycode::F10) || number_key_index(*k).is_some()
            }
            Event::ControllerButtonDown { .. } => true,
            _ => false,
        };
        !(playing && gameplay && !typing)
    }

    /// Scene into the viewport tab, then the docked editor around it.
    pub(crate) fn render_docked(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let Some(mut rt) = self.docked.rt.take() else { return Ok(()) };
        let result = self.render_docked_with(ctx, &mut rt);
        self.docked.rt = Some(rt);
        result
    }

    fn render_docked_with(&mut self, ctx: &mut Context, rt: &mut EditorRuntime) -> anyhow::Result<()> {
        let drawable = ctx.drawable_size();
        self.docked.drawable = drawable;
        let gl = ctx.gl();

        self.docked_pre_frame(gl, rt);

        let size = rt.ensure_target(gl, drawable)?;
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let simulating = rt.is_simulating();
        // Play shows the game camera; Edit and Pause the editor camera.
        let (view, proj) = if simulating {
            let eye = self.player_eye_position();
            (self.fp_camera.view_matrix(eye), self.fp_camera.projection_matrix(aspect))
        } else {
            rt.view_proj()
        };
        let draw_calls = self.draw_world(gl, size, view, proj)?;
        let params = self.render_params;
        let (tint_color, tint_strength) = self.screen_effects.current();
        let renderer = self.renderer.as_mut().expect("renderer set up in init");
        if !simulating && rt.shell.prefs.show_grid {
            renderer.draw_grid(gl, proj * view, 80.0);
        }
        renderer.present_to(
            gl,
            rt.target.framebuffer(),
            size,
            &PostParams {
                color_levels: params.color_levels as f32,
                dither_strength: params.dither_strength,
                tint_color,
                tint_strength,
            },
        );

        if self.texture_asset_browser.is_open() {
            self.texture_asset_browser.ensure_thumbnails_loaded(gl, &mut rt.egui);
        }
        let dt = ctx.time.delta.max(1e-4);
        let perf = PerfSnapshot { fps: 1.0 / dt, frame_ms: dt * 1000.0, entities: self.world.len() as usize, draw_calls };
        let mut world = std::mem::take(&mut self.world);
        if std::mem::take(&mut self.docked.quit_requested) {
            rt.shell.request_quit(&mut rt.state, &mut world);
        }
        {
            let mut host = DockHost { game: self, gl, drawable, editing: rt.state.mode == ShellMode::Edit };
            rt.run_ui(gl, drawable, &mut world, &mut host, perf);
        }
        self.world = world;

        self.docked_after_ui(ctx, rt);
        Ok(())
    }

    /// Before drawing: editor components for new entities, name sync, healing.
    fn docked_pre_frame(&mut self, gl: &glow::Context, rt: &mut EditorRuntime) {
        let edit = !rt.is_simulating();
        if edit {
            self.promote_shell_primitives(gl);
        }
        let mut seen = std::mem::take(&mut self.docked.seen);
        let mut cache = std::mem::take(&mut self.docked.bounds_cache);
        let mut world = std::mem::take(&mut self.world);
        let report = {
            let me: &Sandbox = self;
            bridge_world(
                &mut world,
                &mut seen,
                &mut |source| {
                    *cache.entry(format!("{source:?}")).or_insert_with(|| {
                        let mut bounds = me.resolve_mesh_data(source).ok().and_then(|m| Aabb::of_mesh(&m))?;
                        // Flat meshes (planes) need some thickness to be pickable.
                        let half = bounds.half_extents().max(Vec3::splat(0.02));
                        bounds = Aabb::from_center_half(bounds.center(), half);
                        Some(bounds)
                    })
                },
                edit,
            )
        };
        self.world = world;
        self.docked.seen = seen;
        self.docked.bounds_cache = cache;
        for e in report.needs_script {
            if let Some(path) = self.world.get::<&LevelObjectMeta>(e).ok().and_then(|m| m.script_path.clone()) {
                self.attach_script_to_entity(e, &path);
            }
        }
        rt.state.selection.prune(&self.world);
    }

    /// Entities created by the shell's Create menu (`Primitive`, no mesh)
    /// become real level objects: cube/plane mesh, level metadata, collider.
    fn promote_shell_primitives(&mut self, gl: &glow::Context) {
        let found: Vec<(Entity, PrimitiveShape)> =
            self.world.query::<&Primitive>().without::<&LevelObjectMeta>().iter().map(|(e, p)| (e, p.0)).collect();
        for (e, shape) in found {
            let kind = if shape == PrimitiveShape::Plane { PrimitiveKind::Plane } else { PrimitiveKind::Cube };
            let source = MeshSource::Primitive(kind);
            let Ok(mesh) = self.resolve_mesh(gl, &source) else { continue };
            let texture = self.resolve_texture(gl, None);
            let Ok(t) = self.world.get::<&Transform>(e).map(|t| *t) else { continue };
            let name = self.world.get::<&Name>(e).map(|n| n.0.clone()).unwrap_or_else(|_| "Object".to_string());
            let half_extents = collider_half_extents(&source, t.scale.to_array());
            let _ = self.world.insert(
                e,
                (
                    MeshRenderer { mesh, texture: Some(texture) },
                    LevelObjectMeta {
                        name,
                        mesh_source: source,
                        texture_path: None,
                        rotation_euler_deg: euler_deg_of(t.rotation),
                        script_path: None,
                        level_transition: None,
                        screen_effect: None,
                        camera_shake: None,
                    },
                    Collider { shape: ColliderShape::Aabb { half_extents }, is_trigger: false },
                ),
            );
        }
    }

    fn game_sel(&self) -> GameSel {
        (self.selected_entity, self.selected_character, self.selected_spawner, self.selected_rig)
    }

    /// After drawing: shell events (Play/Pause/Stop), selection sync with the
    /// F2 panel, world-rebuilt handling, pending UI actions, mouse mode.
    fn docked_after_ui(&mut self, ctx: &mut Context, rt: &mut EditorRuntime) {
        for event in rt.shell.take_events() {
            match event {
                ShellEvent::PlayStarted => {
                    reconcile_edit(&mut self.world);
                    self.enter_play_mode(ctx);
                    self.docked.captured = false;
                }
                ShellEvent::Paused => self.paused = true,
                ShellEvent::Resumed => self.paused = false,
                ShellEvent::Stopped { .. } => {
                    self.exit_play_mode(ctx);
                    self.docked.captured = false;
                }
                ShellEvent::AssetDropped { path, target, .. } => self.docked_asset_dropped(ctx, rt, &path, target),
                ShellEvent::QuitApproved => ctx.should_quit = true,
                _ => {}
            }
        }
        self.docked_scene_ops(ctx, rt);
        // The game left Play by itself (e.g. restart): keep the shell in step.
        if self.mode == EditorMode::Edit && rt.state.mode != ShellMode::Edit {
            rt.state.stop();
        }
        if std::mem::take(&mut self.docked.world_rebuilt) {
            rt.state.selection.clear();
            rt.state.undo.clear();
            self.docked.seen.clear();
            self.docked.last_shell_sel = None;
            self.docked.last_game_sel = self.game_sel();
        }
        self.docked_sync_selection(rt);

        let picked = self.docked.picked_texture.take();
        let (multiplayer, appearance) = std::mem::take(&mut self.docked.net_actions);
        self.apply_ui_actions(ctx, picked, None, multiplayer, appearance);
        self.apply_mouse(ctx);
    }

    /// An asset released over the viewport or the hierarchy: an image textures
    /// the object (or character) under the cursor through the same code as the
    /// "Browse Texture..." button; a level file loads like the Load list does.
    fn docked_asset_dropped(&mut self, ctx: &mut Context, rt: &mut EditorRuntime, path: &Path, target: Option<Entity>) {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" => {
                let pick = target.and_then(|e| {
                    if self.world.satisfies::<(&MeshRenderer, &LevelObjectMeta)>(e).unwrap_or(false) {
                        Some(PendingTexturePick::ForEntity(e))
                    } else if self.world.satisfies::<(&MeshRenderer, &CharacterMeta)>(e).unwrap_or(false) {
                        Some(PendingTexturePick::ForCharacter(e))
                    } else {
                        None
                    }
                });
                let Some(pick) = pick else {
                    rt.shell.console.warn("drop an image onto an object or character to texture it");
                    return;
                };
                // Leave a texture pick the browser is waiting for untouched.
                let waiting = self.pending_texture_pick.replace(pick);
                self.push_undo_snapshot();
                let relative = engine::level::relativize(path, &self.asset_root);
                self.apply_ui_actions(ctx, Some(relative), None, None, None);
                self.pending_texture_pick = waiting;
            }
            "ron" => match engine::level::load_from_file(path) {
                Ok(level) => {
                    self.docked_open_level(ctx, level, path);
                    rt.shell.console.info(format!("loaded level {}", path.display()));
                }
                Err(err) => rt.shell.console.error(format!("{} is not a level: {err}", path.display())),
            },
            _ => rt.shell.console.info(format!("nothing to do with {} here", path.display())),
        }
    }

    /// Loads `level` as the open level (what the Level tab's Load list does)
    /// and makes `path` the file File > Save writes to.
    fn docked_open_level(&mut self, ctx: &mut Context, level: Level, path: &Path) {
        self.sync_current_level_into_cache();
        if let Err(err) = self.apply_level(ctx.gl(), &level) {
            log::error!("failed to load level '{}': {err}", level.name);
            return;
        }
        let mut shared = self.docked.scene.borrow_mut();
        shared.current = Some(path.to_path_buf());
        shared.named = Some(level.name.clone());
        drop(shared);
        self.cache_level(level);
    }

    fn cache_level(&mut self, level: Level) {
        match self.levels.iter_mut().find(|l| l.name == level.name) {
            Some(existing) => *existing = level,
            None => self.levels.push(level),
        }
    }

    /// File-menu requests queued by [`GameSceneIo`].
    fn docked_scene_ops(&mut self, ctx: &mut Context, rt: &mut EditorRuntime) {
        let ops = std::mem::take(&mut self.docked.scene.borrow_mut().ops);
        for op in ops {
            match op {
                SceneOp::New => {
                    let level = Level { name: "untitled".into(), ..Default::default() };
                    self.sync_current_level_into_cache();
                    if let Err(err) = self.apply_level(ctx.gl(), &level) {
                        rt.shell.console.error(format!("new level: {err}"));
                    }
                }
                SceneOp::Open { level, path } => self.docked_open_level(ctx, level, &path),
                SceneOp::Save(path) => {
                    if self.mode != EditorMode::Edit {
                        rt.shell.console.error("stop playing before saving the level");
                        continue;
                    }
                    reconcile_edit(&mut self.world);
                    let mut level = self.build_level_from_ecs();
                    if let Some(name) = path.file_stem() {
                        level.name = name.to_string_lossy().into_owned();
                    }
                    match engine::level::save_to_file(&level, &path) {
                        Ok(()) => {
                            self.current_level_name = level.name.clone();
                            self.cache_level(level);
                        }
                        Err(err) => rt.shell.console.error(format!("save {}: {err}", path.display())),
                    }
                }
            }
        }
        // The Level tab's Load list switched levels: follow its file.
        let mut shared = self.docked.scene.borrow_mut();
        if shared.named.as_deref() != Some(self.current_level_name.as_str()) {
            let file = self.levels_dir.join(format!("{}.ron", self.current_level_name));
            shared.current = file.is_file().then_some(file);
            shared.named = Some(self.current_level_name.clone());
        }
    }

    /// Shell selection -> F2 panel selection, and back when the panel's
    /// outliner was clicked.
    fn docked_sync_selection(&mut self, rt: &mut EditorRuntime) {
        let shell_primary = rt.state.selection.primary();
        let now = self.game_sel();
        if shell_primary != self.docked.last_shell_sel {
            self.selected_entity = None;
            self.selected_character = None;
            self.selected_spawner = None;
            self.selected_rig = None;
            for e in rt.state.selection.to_vec() {
                if self.world.satisfies::<&LevelObjectMeta>(e).unwrap_or(false) {
                    self.selected_entity = Some(e);
                } else if self.world.satisfies::<&CharacterMeta>(e).unwrap_or(false) {
                    self.selected_character = Some(e);
                } else if self.world.satisfies::<&SpawnerConfig>(e).unwrap_or(false) {
                    self.selected_spawner = Some(e);
                } else if self.world.satisfies::<&RigRoot>(e).unwrap_or(false) {
                    self.selected_rig = Some(e);
                }
            }
            self.docked.last_game_sel = self.game_sel();
        } else if now != self.docked.last_game_sel {
            let last = self.docked.last_game_sel;
            let changed = [(now.0, last.0), (now.1, last.1), (now.2, last.2), (now.3, last.3)]
                .into_iter()
                .find_map(|(n, l)| if n != l { n } else { None });
            if let Some(e) = changed.filter(|e| self.world.contains(*e)) {
                rt.state.selection.select_only(e);
            }
            self.docked.last_game_sel = now;
        }
        self.docked.last_shell_sel = rt.state.selection.primary();
    }

    /// Per-frame game UI drawn by the shell's overlay hook: HUD and dialogue
    /// choices while playing, texture browser, multiplayer/appearance windows.
    fn docked_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect) {
        if let Some(path) = self.texture_asset_browser.show(ctx, &self.asset_root) {
            self.docked.picked_texture = Some(path);
        }
        if self.mode == EditorMode::Play {
            // HUD, dialogue choices and the hint anchor to the scene image, not
            // the window, so they stay clear of the dock's toolbar and panels.
            let window = ctx.input(|i| i.screen_rect);
            ctx.input_mut(|i| i.screen_rect = viewport);
            draw_hud(ctx, &self.hud, &self.render_params.hud);
            self.draw_dialogue_choices(ctx);
            if !self.paused && !self.docked.captured {
                egui::Area::new("docked_capture_hint".into())
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label("Click the viewport to capture the mouse \u{b7} Esc releases it \u{b7} F3 stops");
                        });
                    });
            }
            ctx.input_mut(|i| i.screen_rect = window);
        }
        if self.docked.show_net || (self.mode == EditorMode::Play && self.paused) {
            self.docked.net_actions = self.draw_net_panels(ctx);
        }
    }

    /// Multiplayer and player-appearance windows (egui windows, drawn by the
    /// pause menu in the original editor and by the overlay hook here).
    pub(crate) fn draw_net_panels(
        &mut self,
        egui_ctx: &egui::Context,
    ) -> (Option<engine::ui::MultiplayerAction>, Option<engine::ui::AppearanceAction>) {
        // Every currently-loaded rig, by name, for the appearance panel's
        // picker list; the local player's own chosen appearance (if any),
        // resolved to that same name for highlighting the current
        // selection — see `find_rig_asset` for the path-to-name lookup.
        let appearance_rig_names: Vec<String> = self.rigs.iter().map(|rig| rig.name.clone()).collect();
        let current_appearance_name: Option<String> =
            self.local_appearance.as_deref().and_then(|path| self.find_rig_asset(path)).map(|rig| rig.name.clone());
        let net_status_text = self.net_status_text();
        let net_connected = matches!(self.net_mode, NetMode::Host(_))
            || matches!(&self.net_mode, NetMode::Client(client) if client.is_connected());
        // Host-only roster for the multiplayer panel's player list — a
        // client sees `&[]` (see `draw_multiplayer_panel`'s doc comment).
        let net_players: Vec<(NetId, String, bool)> = if let NetMode::Host(host) = &self.net_mode {
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
        let multiplayer = engine::ui::draw_multiplayer_panel(
            egui_ctx,
            &net_status_text,
            net_connected,
            self.net_reconnect_attempts_left > 0,
            &mut self.net_join_address,
            &mut self.net_player_name,
            &net_players,
        );
        let appearance =
            engine::ui::draw_appearance_panel(egui_ctx, current_appearance_name.as_deref(), &appearance_rig_names);
        (multiplayer, appearance)
    }

    /// Applies what the egui pass reported: a picked texture, pause-menu,
    /// multiplayer and appearance actions. Shared by both editors.
    pub(crate) fn apply_ui_actions(
        &mut self,
        ctx: &mut Context,
        picked_texture: Option<PathBuf>,
        pause_menu_action: Option<PauseMenuAction>,
        multiplayer_action: Option<engine::ui::MultiplayerAction>,
        appearance_action: Option<engine::ui::AppearanceAction>,
    ) {
        let gl = ctx.gl();
        if let Some(path) = picked_texture {
            match self.pending_texture_pick.take() {
                Some(PendingTexturePick::ForEntity(entity)) => {
                    match GpuTexture::load_from_file(gl, &self.asset_root.join(&path), TextureFilter::Nearest) {
                        Ok(texture) => {
                            let texture = Arc::new(texture);
                            if let Ok(mut query) =
                                self.world.query_one::<(&mut MeshRenderer, &mut LevelObjectMeta)>(entity)
                            {
                                if let Some((renderer, meta)) = query.get() {
                                    renderer.texture = Some(texture);
                                    meta.texture_path = Some(path.clone());
                                }
                            }
                            log::info!("assigned texture {path:?} via asset browser");
                        }
                        Err(err) => log::error!("failed to load texture {path:?}: {err}"),
                    }
                }
                Some(PendingTexturePick::ForClass(index)) => {
                    if let Some(class) = self.classes.get_mut(index) {
                        class.texture_path = Some(path);
                    }
                }
                Some(PendingTexturePick::ForCharacter(entity)) => {
                    match GpuTexture::load_from_file(gl, &self.asset_root.join(&path), TextureFilter::Nearest) {
                        Ok(texture) => {
                            let texture = Arc::new(texture);
                            if let Ok(mut query) =
                                self.world.query_one::<(&mut MeshRenderer, &mut CharacterMeta)>(entity)
                            {
                                if let Some((renderer, meta)) = query.get() {
                                    renderer.texture = Some(texture);
                                    meta.texture_path = Some(path.clone());
                                }
                            }
                            log::info!("assigned character texture {path:?} via asset browser");
                        }
                        Err(err) => log::error!("failed to load texture {path:?}: {err}"),
                    }
                }
                None => {}
            }
        }

        match pause_menu_action {
            Some(PauseMenuAction::Resume) => self.resume(ctx),
            Some(PauseMenuAction::ExitToEditor) => self.exit_play_mode(ctx),
            Some(PauseMenuAction::RestartLevel) => self.restart_level(ctx),
            Some(PauseMenuAction::QuitGame) => ctx.should_quit = true,
            None => {}
        }

        match multiplayer_action {
            Some(engine::ui::MultiplayerAction::Host) => self.start_hosting(),
            Some(engine::ui::MultiplayerAction::Join) => self.join_game(),
            Some(engine::ui::MultiplayerAction::Disconnect) => self.disconnect_net(),
            Some(engine::ui::MultiplayerAction::SetLevelSwitchPermission { net_id, allowed }) => {
                self.set_level_switch_permission(net_id, allowed);
            }
            Some(engine::ui::MultiplayerAction::CancelReconnect) => self.cancel_reconnect(),
            None => {}
        }

        match appearance_action {
            Some(engine::ui::AppearanceAction::ImportNewModel) => self.pick_new_appearance_model(),
            Some(engine::ui::AppearanceAction::SelectExisting(index)) => self.select_appearance(index),
            Some(engine::ui::AppearanceAction::ResetToDefault) => self.reset_appearance(),
            None => {}
        }
    }
}

// ------------------------------------------------------------- shell host

/// `ShellHost` for the game. The shell owns the `World` while the UI runs
/// (the game's copy is parked), so each hook swaps it back in for the
/// game-side panels, which read `self.world`.
struct DockHost<'a> {
    game: &'a mut Sandbox,
    gl: &'a glow::Context,
    drawable: (u32, u32),
    editing: bool,
}

impl ShellHost for DockHost<'_> {
    fn tab_ui(&mut self, tab: &EditorTab, world: &mut World, ui: &mut egui::Ui) -> bool {
        let EditorTab::Host(name) = tab else { return false };
        if ![LEVEL_TAB, RENDER_TAB, NET_TAB].contains(&name.as_str()) {
            return false;
        }
        std::mem::swap(&mut self.game.world, world);
        match name.as_str() {
            LEVEL_TAB if self.editing => {
                reconcile_edit(&mut self.game.world);
                egui::ScrollArea::vertical().show(ui, |ui| self.game.draw_level_editor_ui(ui, self.gl));
            }
            LEVEL_TAB => {
                ui.weak("Level tools are available in Edit mode. Press Stop first.");
            }
            RENDER_TAB => {
                egui::ScrollArea::vertical().show(ui, |ui| self.game.draw_shader_editor_ui(ui, self.gl, self.drawable));
            }
            _ => {
                ui.label(self.game.net_status_text());
                ui.checkbox(&mut self.game.docked.show_net, "Show multiplayer / appearance windows");
                ui.weak("They also open on their own while the game is paused.");
            }
        }
        std::mem::swap(&mut self.game.world, world);
        true
    }

    fn accepts_asset_drop(&mut self, path: &std::path::Path) -> bool {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
        matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "ron"))
    }

    fn viewport_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect, world: &mut World) {
        std::mem::swap(&mut self.game.world, world);
        self.game.docked_overlay(ctx, viewport);
        std::mem::swap(&mut self.game.world, world);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::editor::state::EditorState;

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

    fn bridge(world: &mut World, seen: &mut HashSet<Entity>) -> BridgeReport {
        bridge_world(world, seen, &mut |_| None, true)
    }

    fn spinner(world: &mut World) -> Entity {
        world.spawn((
            Transform::from_position(Vec3::new(1.0, 2.0, 3.0)),
            meta("Spinner"),
            Collider { shape: ColliderShape::Aabb { half_extents: Vec3::splat(0.5) }, is_trigger: false },
            Animator {
                kind: AnimationKind::Orbit { axis: Vec3::Y, speed_deg_per_sec: 90.0 },
                base_position: Vec3::new(1.0, 2.0, 3.0),
                base_rotation: Quat::IDENTITY,
                elapsed: 0.0,
            },
        ))
    }

    fn character(world: &mut World) -> Entity {
        let instance = CharacterInstance {
            name: "Grub".into(),
            position: [0.0, 1.0, 0.0],
            scale: [0.8, 1.6, 0.8],
            color: [0.8, 0.2, 0.2],
            texture_path: None,
            disposition: Disposition::Hostile,
            move_speed: 2.0,
            wander_radius: 2.0,
            sight_range: 6.0,
            max_health: Some(20.0),
            damage: Some(5.0),
            attack_range: 1.2,
            attack_cooldown_secs: 1.0,
            dialogue_nodes: Vec::new(),
        };
        let e = world.spawn((
            Transform { position: Vec3::Y, rotation: Quat::IDENTITY, scale: Vec3::new(0.8, 1.6, 0.8) },
            CharacterMeta {
                name: instance.name.clone(),
                color: instance.color,
                texture_path: None,
                disposition: instance.disposition,
                move_speed: 2.0,
                wander_radius: 2.0,
                sight_range: 6.0,
                damage: Some(5.0),
                attack_range: 1.2,
                attack_cooldown_secs: 1.0,
            },
            CharacterBrain::new(Vec3::Y),
        ));
        world.insert_one(e, Health::new(20.0)).unwrap();
        e
    }

    #[test]
    fn names_flow_both_ways() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let e = world.spawn((Transform::default(), meta("Crate")));
        bridge(&mut world, &mut seen);
        assert_eq!(world.get::<&Name>(e).unwrap().0, "Crate");

        // Renamed in the hierarchy -> level metadata follows.
        world.get::<&mut Name>(e).unwrap().0 = "Big Crate".into();
        bridge(&mut world, &mut seen);
        assert_eq!(world.get::<&LevelObjectMeta>(e).unwrap().name, "Big Crate");

        // Renamed in the F2 panel -> hierarchy follows.
        world.get::<&mut LevelObjectMeta>(e).unwrap().name = "Small Crate".into();
        bridge(&mut world, &mut seen);
        assert_eq!(world.get::<&Name>(e).unwrap().0, "Small Crate");
    }

    #[test]
    fn reconcile_pulls_gizmo_edits_into_saved_fields() {
        let mut world = World::new();
        let e = spinner(&mut world);
        {
            let mut t = world.get::<&mut Transform>(e).unwrap();
            t.position = Vec3::new(5.0, 0.0, 0.0);
            t.rotation = euler_deg_to_quat(Vec3::new(10.0, 20.0, 30.0));
            t.scale = Vec3::new(2.0, 2.0, 2.0);
        }
        reconcile_edit(&mut world);
        let meta = world.get::<&LevelObjectMeta>(e).unwrap();
        assert!((meta.rotation_euler_deg - Vec3::new(10.0, 20.0, 30.0)).abs().max_element() < 1e-2, "{:?}", meta.rotation_euler_deg);
        let animator = world.get::<&Animator>(e).unwrap();
        assert_eq!(animator.base_position, Vec3::new(5.0, 0.0, 0.0));
        let collider = world.get::<&Collider>(e).unwrap();
        assert_eq!(collider.shape, ColliderShape::Aabb { half_extents: Vec3::ONE });
        // An untouched object is left alone.
        drop((meta, animator, collider));
        let before = world.get::<&LevelObjectMeta>(e).unwrap().rotation_euler_deg;
        reconcile_edit(&mut world);
        assert_eq!(world.get::<&LevelObjectMeta>(e).unwrap().rotation_euler_deg, before);
    }

    #[test]
    fn snap_returns_animated_objects_to_base() {
        let mut world = World::new();
        let e = spinner(&mut world);
        world.get::<&mut Transform>(e).unwrap().position = Vec3::splat(9.0);
        snap_animators_to_base(&mut world);
        assert_eq!(world.get::<&Transform>(e).unwrap().position, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn shell_duplicate_and_undo_delete_keep_animator() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let mut editor = EditorState::with_registry(editor_registry());
        let e = spinner(&mut world);
        bridge(&mut world, &mut seen);
        assert!(world.get::<&BridgedAnim>(e).is_ok());

        // Duplicate: the clone has everything cloneable but no Animator...
        editor.selection.select_only(e);
        let copies = editor.duplicate_selection(&mut world);
        assert_eq!(copies.len(), 1);
        let copy = copies[0];
        assert!(world.get::<&Animator>(copy).is_err(), "Animator is not cloneable");
        assert!(world.get::<&LevelObjectMeta>(copy).is_ok() && world.get::<&Collider>(copy).is_ok());
        // ...which the bridge restores.
        bridge(&mut world, &mut seen);
        assert!(world.get::<&Animator>(copy).is_ok());
        assert_eq!(world.get::<&Name>(copy).map(|n| n.0.clone()).ok(), world.get::<&LevelObjectMeta>(copy).map(|m| m.name.clone()).ok());

        // Delete + undo.
        editor.selection.select_only(e);
        assert_eq!(editor.delete_selection(&mut world), 1);
        bridge(&mut world, &mut seen);
        assert!(editor.undo(&mut world));
        assert!(world.contains(e));
        assert!(world.get::<&Animator>(e).is_err());
        bridge(&mut world, &mut seen);
        let animator = world.get::<&Animator>(e).expect("animator restored after undo of delete");
        assert!(matches!(animator.kind, AnimationKind::Orbit { speed_deg_per_sec, .. } if speed_deg_per_sec == 90.0));
    }

    #[test]
    fn removing_a_component_in_the_panel_is_not_undone_by_the_bridge() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let e = spinner(&mut world);
        bridge(&mut world, &mut seen);
        // The F2 panel's "Remove animation" button.
        world.remove_one::<Animator>(e).unwrap();
        bridge(&mut world, &mut seen);
        assert!(world.get::<&Animator>(e).is_err(), "seen entities are not healed");
    }

    #[test]
    fn duplicated_character_gets_its_brain_and_health_back() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let mut editor = EditorState::with_registry(editor_registry());
        let e = character(&mut world);
        bridge(&mut world, &mut seen);
        editor.selection.select_only(e);
        let copy = editor.duplicate_selection(&mut world)[0];
        assert!(world.get::<&CharacterBrain>(copy).is_err() && world.get::<&Health>(copy).is_err());
        bridge(&mut world, &mut seen);
        assert!(world.get::<&CharacterBrain>(copy).is_ok());
        assert_eq!(world.get::<&Health>(copy).unwrap().max, 20.0);
        assert!(character_instance_of(&world, copy).is_some());
    }

    #[test]
    fn rig_parts_are_named_and_locked() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let part = world.spawn((
            Transform::default(),
            RigPart { parent: None, local_position: Vec3::ZERO, local_rotation_euler_deg: Vec3::ZERO, local_rotation: Quat::IDENTITY },
            RigRoot { name: "humanoid".into(), rig_path: PathBuf::from("rigs/humanoid.ron") },
        ));
        bridge(&mut world, &mut seen);
        assert!(world.get::<&Locked>(part).unwrap().0);
        assert_eq!(world.get::<&Name>(part).unwrap().0, "humanoid");
    }

    #[test]
    fn restored_scripted_object_is_reported_for_reattach() {
        let mut world = World::new();
        let mut seen = HashSet::new();
        let mut m = meta("Door");
        m.script_path = Some(PathBuf::from("scripts/door.pss"));
        let e = world.spawn((Transform::default(), m));
        let report = bridge(&mut world, &mut seen);
        assert_eq!(report.needs_script, vec![e]);
        let report = bridge(&mut world, &mut seen);
        assert!(report.needs_script.is_empty(), "only reported once");
    }

    fn scene_io(dir: &Path) -> (GameSceneIo, Rc<RefCell<SceneBridge>>) {
        let shared = Rc::new(RefCell::new(SceneBridge::default()));
        (GameSceneIo { levels_dir: dir.to_path_buf(), shared: shared.clone(), path: None, undo_dirty: false }, shared)
    }

    fn temp_levels_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mushroom_sceneio_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_menu_names_resolve_into_the_levels_dir() {
        let dir = temp_levels_dir("resolve");
        let (io, _) = scene_io(&dir);
        assert_eq!(io.resolve(Path::new("cave")), dir.join("cave.ron"));
        assert_eq!(io.resolve(Path::new("sub/cave.ron")), dir.join("sub/cave.ron"));
        assert_eq!(io.resolve(&dir.join("cave.ron")), dir.join("cave.ron"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_menu_open_save_new_queue_work_for_the_game() {
        let dir = temp_levels_dir("ops");
        let (mut io, shared) = scene_io(&dir);
        let mut world = World::new();
        // A level that does not parse is refused on the spot and queues nothing.
        assert!(io.open(Path::new("missing"), &mut world).is_err());
        assert!(shared.borrow().ops.is_empty());

        let level = Level { name: "cave".into(), ..Default::default() };
        engine::level::save_to_file(&level, &dir.join("cave.ron")).unwrap();
        io.open(Path::new("cave"), &mut world).unwrap();
        assert_eq!(io.current_path(), Some(dir.join("cave.ron").as_path()));
        assert!(matches!(&shared.borrow().ops[..], [SceneOp::Open { level, .. }] if level.name == "cave"));

        // Save As another name: the game is told, and the open file follows.
        io.save(&world, Path::new("grotto")).unwrap();
        assert!(matches!(shared.borrow().ops.last(), Some(SceneOp::Save(p)) if *p == dir.join("grotto.ron")));
        assert_eq!(shared.borrow().current, Some(dir.join("grotto.ron")));
        assert_eq!(shared.borrow().named.as_deref(), Some("grotto"));

        // Dirty comes from the shell's undo stack; the game's file wins over a stale path.
        assert!(!io.is_dirty());
        shared.borrow_mut().current = Some(dir.join("other.ron"));
        io.sync_undo_dirty(true);
        assert!(io.is_dirty());
        assert_eq!(io.current_path(), Some(dir.join("other.ron").as_path()));
        assert_eq!(io.title(), "other.ron*");

        io.new_scene(&mut world).unwrap();
        assert!(io.current_path().is_none() && matches!(shared.borrow().ops.last(), Some(SceneOp::New)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewport_picking_finds_level_objects_but_not_rig_parts() {
        use engine::editor::cam_matrices::Ray;
        use engine::editor::picking::pick_nearest;
        let mut world = World::new();
        let mut seen = HashSet::new();
        // A light-like level object (no mesh): picked by its icon sphere.
        let light = world.spawn((Transform::from_position(Vec3::new(0.0, 0.0, 5.0)), meta("Lamp")));
        // A rig part in front of it: locked, so clicks pass through.
        let part = world.spawn((
            Transform::from_position(Vec3::new(0.0, 0.0, 2.0)),
            RigPart { parent: None, local_position: Vec3::ZERO, local_rotation_euler_deg: Vec3::ZERO, local_rotation: Quat::IDENTITY },
        ));
        bridge(&mut world, &mut seen);
        let hit = pick_nearest(&world, &Ray::new(Vec3::ZERO, Vec3::Z)).expect("ray hits the lamp");
        assert_eq!(hit.entity, light);
        assert_ne!(hit.entity, part);
    }
}
