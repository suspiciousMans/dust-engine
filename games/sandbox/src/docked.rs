//! The sandbox's only editor: the engine's docked editor (`EditorRuntime`)
//! hosted around the game. F1, F2 and F4 (or `DUST_EDITOR=1`) open it; F4 also
//! closes it. Debug builds only (`editor_available()`).
//!
//! How the game's data meets the shell:
//! * Level objects are already hecs entities with `Transform`, so selection,
//!   gizmos, hierarchy and undo work on them directly. `sync_entities` adds the
//!   components the shell wants (`Name`, `LocalBounds`) and mirrors gizmo edits
//!   back into the game's own metadata (`LevelObjectMeta::rotation_euler_deg`,
//!   collider size).
//! * Play/Stop use the game's own `enter_play_mode`/`exit_play_mode` (a `Level`
//!   snapshot restored with `apply_level`), so the shell's snapshot is `NullSnapshot`.
//! * The game-specific panels are host tabs (`tab_ui`) running against the
//!   shell's world: Level (level files, music), Selected (details of the
//!   selection: objects, rigs, characters, spawners, scripts), Create (new
//!   objects, rigs, characters, spawners, classes) and Render (shader
//!   profiles, HUD, physics, camera feel). Their edits are covered by the
//!   level-snapshot undo shown at the top of Level / Selected / Create.
//! * File > New / Open / Save / Save As work on `levels/*.ron` through
//!   [`GameSceneIo`], which queues the operation for the game's own Level code
//!   (`apply_level`, `build_level_from_ecs`) to run once the frame's UI is done.
//! * The Assets tab browses the game's asset root; an image dropped on an
//!   object textures it, a `.ron` level dropped anywhere loads it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use engine::ai::{CharacterBrain, CharacterMeta, Dialogue, DialogueNode, Health, SpawnerConfig, SpawnerState};
use engine::animation::{AnimationKind, Animator};
use engine::app::Context;
use engine::ecs::{euler_deg_to_quat, Entity, LevelObjectMeta, Light, MeshRenderer, Transform, World};
use engine::editor::bounds::{local_bounds, Aabb, LocalBounds};
use engine::editor::hierarchy::PrimitiveShape;
use engine::editor::interaction::ViewportEvent;
use engine::editor::runtime::EditorRuntime;
use engine::editor::scene_io::{NullSnapshot, SceneIo};
use engine::editor::shell::{PerfSnapshot, ShellEvent, ShellHost};
use engine::editor::{EditorMode as ShellMode, EditorState, Locked, Name, Primitive, Registry};
use engine::glam::{EulerRot, Mat4, Quat, Vec3};
use engine::glow;
use engine::level::{Level, MeshSource, PrimitiveKind};
use engine::particles::{ParticleEmitter, ParticleEmitterDef};
use engine::physics::{Collider, ColliderShape, RigidBody};
use engine::renderer::PostParams;
use engine::rig::{RigPart, RigPartMeta};
use engine::sdl2::event::Event;
use engine::sdl2::keyboard::Keycode;
use engine::ui::{draw_hud, EditorTab};

use crate::{collider_half_extents, ClassMember, EditorMode, PendingTexturePick, Sandbox};

const TAB_LEVEL: &str = "Level";
const TAB_SELECTED: &str = "Selected";
const TAB_CREATE: &str = "Create";
const TAB_RENDER: &str = "Render";

/// Authorable state of components that are not `Clone` (so the shell's
/// duplicate / undo-delete cannot copy them). Kept up to date each Edit frame
/// and used to rebuild the live component on a copy.
#[derive(Clone, Default)]
struct Authored {
    emitter: Option<ParticleEmitterDef>,
    animator: Option<(AnimationKind, Vec3, Quat)>,
    max_health: Option<f32>,
    dialogue: Option<Vec<DialogueNode>>,
}

type LegacySelection = (Option<Entity>, Option<Entity>, Option<Entity>, Option<Entity>);

pub struct DockedEditor {
    rt: EditorRuntime,
    bridge: Bridge,
    /// State shared with the shell's [`GameSceneIo`].
    scene: Rc<RefCell<SceneShared>>,
}

/// Book-keeping for the game <-> shell bridge (no GL, so it is unit-testable).
#[derive(Default)]
struct Bridge {
    /// Last name both `Name` and the game's metadata agreed on, per entity.
    names: HashMap<Entity, String>,
    bounds: HashMap<String, Aabb>,
    script_attempted: HashSet<Entity>,
    seen_generation: u32,
    last_shell_selection: Vec<Entity>,
    last_legacy_selection: LegacySelection,
    /// `current_level_name` as of the last frame, to notice a level switch.
    level_name: String,
}

/// Work the host tabs queue while the shell owns the frame.
#[derive(Default)]
struct Pending {
    picked_texture: Option<std::path::PathBuf>,
    multiplayer: Option<engine::ui::MultiplayerAction>,
    appearance: Option<engine::ui::AppearanceAction>,
}

struct DockHost<'a> {
    game: &'a mut Sandbox,
    gl: &'a glow::Context,
    drawable: (u32, u32),
    pending: Pending,
}

impl DockHost<'_> {
    /// The game keeps its own `world`; the shell hands us the real one for the
    /// duration of a tab, so swap it in and back.
    fn with_world<R>(&mut self, world: &mut World, f: impl FnOnce(&mut Sandbox) -> R) -> R {
        std::mem::swap(&mut self.game.world, world);
        let r = f(self.game);
        std::mem::swap(&mut self.game.world, world);
        r
    }
}

impl ShellHost for DockHost<'_> {
    fn tab_ui(&mut self, tab: &EditorTab, world: &mut World, ui: &mut egui::Ui) -> bool {
        let EditorTab::Host(name) = tab else { return false };
        let (gl, drawable) = (self.gl, self.drawable);
        match name.as_str() {
            TAB_LEVEL => {
                self.with_world(world, |g| egui::ScrollArea::vertical().show(ui, |ui| g.draw_level_tab_ui(ui, gl)));
                true
            }
            TAB_SELECTED => {
                self.with_world(world, |g| egui::ScrollArea::vertical().show(ui, |ui| g.draw_selected_tab_ui(ui, gl)));
                true
            }
            TAB_CREATE => {
                self.with_world(world, |g| egui::ScrollArea::vertical().show(ui, |ui| g.draw_create_tab_ui(ui, gl)));
                true
            }
            TAB_RENDER => {
                self.with_world(world, |g| {
                    egui::ScrollArea::vertical().show(ui, |ui| g.draw_shader_editor_ui(ui, gl, drawable))
                });
                true
            }
            _ => false,
        }
    }

    fn selection_bounds(&mut self, world: &World, selection: &[Entity]) -> Option<(Vec3, Vec3)> {
        let mut acc: Option<Aabb> = None;
        for &e in selection {
            let Ok(t) = world.get::<&Transform>(e) else { continue };
            let local = local_bounds(world, e).unwrap_or_else(|| Aabb::from_center_half(Vec3::ZERO, Vec3::splat(0.3)));
            let b = local.transformed(&t.matrix());
            acc = Some(acc.map_or(b, |a| a.union(&b)));
        }
        acc.map(|b| (b.min, b.max))
    }

    fn accepts_asset_drop(&mut self, path: &Path) -> bool {
        is_image(path) || is_level_file(path)
    }

    fn viewport_overlay(&mut self, ctx: &egui::Context, viewport: egui::Rect, _world: &mut World) {
        let g = &mut *self.game;
        if let Some(path) = g.texture_asset_browser.show(ctx, &g.asset_root) {
            self.pending.picked_texture = Some(path);
        }
        if g.mode == EditorMode::Play {
            // The HUD and dialogue choices anchor to the Viewport tab, not the window.
            with_screen_rect(ctx, viewport, || {
                draw_hud(ctx, &g.hud, &g.render_params.hud);
                g.draw_dialogue_choices(ctx);
            });
            if g.paused {
                let (m, a) = g.draw_pause_side_panels(ctx);
                self.pending.multiplayer = m;
                self.pending.appearance = a;
            }
        }
    }
}

impl Sandbox {
    /// Builds the runtime. Called once from `init`, debug builds only.
    pub(crate) fn init_docked(&mut self, ctx: &mut Context) {
        let registry = editor_registry();
        let config_dir = self.asset_root.join(".editor");
        let host_tabs = vec![TAB_LEVEL.into(), TAB_SELECTED.into(), TAB_CREATE.into(), TAB_RENDER.into()];
        let mut rt = match EditorRuntime::new(ctx.gl_arc(), registry, Some(config_dir), host_tabs) {
            Ok(rt) => rt,
            Err(err) => {
                log::warn!("docked editor unavailable: {err}");
                return;
            }
        };
        // The game restores itself on Stop (`exit_play_mode`).
        rt.shell.snapshot = Box::new(NullSnapshot);
        // The Assets tab browses the asset root (the same root texture paths are relative to);
        // the UI Designer reads and writes `<root>/ui`.
        let ui_dir = self.asset_root.join("ui");
        if let Err(err) = std::fs::create_dir_all(&ui_dir) {
            log::warn!("UI document directory {ui_dir:?} unavailable: {err}");
        }
        rt.set_asset_root(Some(self.asset_root.clone()));
        rt.set_ui_dir(Some(ui_dir));
        // File menu: the game's own level files.
        let scene = Rc::new(RefCell::new(SceneShared::default()));
        rt.shell.scene_io = Box::new(GameSceneIo {
            shared: scene.clone(),
            asset_root: self.asset_root.clone(),
            levels_dir: self.levels_dir.clone(),
            current: self.current_level_file(),
            undo_dirty: false,
        });
        // The default pose sits at the origin, which is inside the exit trigger of the
        // default level; start further out so the level is visible.
        let mut camera = engine::editor::camera::EditorCamera::new(Vec3::new(0.0, 1.0, 0.0), 14.0);
        camera.yaw = 0.0;
        camera.pitch = 0.3;
        camera.far = 500.0;
        rt.shell.camera = camera;
        let bridge = Bridge { seen_generation: self.level_generation, ..Bridge::default() };
        self.docked = Some(DockedEditor { rt, bridge, scene });
        if std::env::var_os("DUST_EDITOR").is_some() {
            self.set_docked(ctx, true);
        }
    }

    fn set_docked(&mut self, ctx: &mut Context, open: bool) {
        if self.docked.is_none() || open == self.docked_open {
            return;
        }
        if open {
            if self.mode != EditorMode::Edit {
                log::info!("stop Play mode (F3) before opening the docked editor");
                return;
            }
            self.docked_open = true;
            self.docked_captured = false;
            let d = self.docked.as_mut().unwrap();
            d.rt.open = true;
            d.rt.state.mode = ShellMode::Edit;
        } else {
            if self.mode == EditorMode::Play {
                self.exit_play_mode(ctx);
            }
            self.docked_open = false;
            self.docked_captured = false;
            let d = self.docked.as_mut().unwrap();
            d.rt.state.stop();
            d.rt.open = false;
            d.rt.save_layout();
        }
        let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(false);
    }

    /// Brings a host tab to the front of the dock (re-adding it if it was closed).
    fn focus_tab(&mut self, name: &str) {
        let Some(d) = self.docked.as_mut() else { return };
        let tab = EditorTab::Host(name.to_string());
        match d.rt.shell.dock.find_tab(&tab) {
            Some(location) => d.rt.shell.dock.set_active_tab(location),
            None => d.rt.shell.dock.push_to_focused_leaf(tab),
        }
    }

    /// Returns true when the editor consumed the event (the game's own handler must skip it).
    pub(crate) fn handle_docked_event(&mut self, ctx: &mut Context, event: &Event) -> bool {
        let Some(d) = self.docked.as_mut() else { return false };
        d.rt.handle_event(event);
        let pressed = |key: Keycode| matches!(event, Event::KeyDown { keycode: Some(k), repeat: false, .. } if *k == key);
        if pressed(Keycode::F4) {
            self.set_docked(ctx, !self.docked_open);
            return true;
        }
        // F1 / F2 open the editor too (on the Render / Level tab); they never close it.
        if pressed(Keycode::F1) || pressed(Keycode::F2) {
            let tab = if pressed(Keycode::F1) { TAB_RENDER } else { TAB_LEVEL };
            self.set_docked(ctx, true);
            if self.docked_open {
                self.focus_tab(tab);
            }
            return true;
        }
        if !self.docked_open {
            return false;
        }
        // Closing the window goes through the shell so unsaved changes are asked about.
        if matches!(event, Event::Quit { .. }) {
            ctx.should_quit = false;
            let mut d = self.docked.take().expect("checked above");
            d.rt.shell.request_quit(&mut d.rt.state, &mut self.world);
            self.process_shell_events(ctx, &mut d);
            self.docked = Some(d);
            return true;
        }
        let playing = self.mode == EditorMode::Play && !self.paused;
        if pressed(Keycode::F3) {
            let d = self.docked.as_mut().unwrap();
            d.rt.shell.toggle_play(&mut d.rt.state, &mut self.world);
            return true;
        }
        if pressed(Keycode::Escape) {
            if playing {
                self.docked_captured = !self.docked_captured;
                let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(self.docked_captured);
            }
            return true;
        }
        // The UI owns keyboard and mouse unless Play has captured the mouse.
        let input_event = matches!(
            event,
            Event::KeyDown { .. }
                | Event::KeyUp { .. }
                | Event::TextInput { .. }
                | Event::MouseMotion { .. }
                | Event::MouseButtonDown { .. }
                | Event::MouseButtonUp { .. }
                | Event::MouseWheel { .. }
        );
        input_event && !(playing && self.docked_captured)
    }

    /// Docked Play without a captured mouse: stand still instead of reading input.
    pub(crate) fn idle_player(&mut self) {
        if let Some(player) = self.player_entity {
            if let Ok(mut body) = self.world.get::<&mut RigidBody>(player) {
                body.velocity.x = 0.0;
                body.velocity.z = 0.0;
            }
        }
    }

    pub(crate) fn render_docked(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let Some(mut d) = self.docked.take() else {
            self.docked_open = false;
            return Ok(());
        };
        let result = self.render_docked_inner(ctx, &mut d);
        self.docked = Some(d);
        result
    }

    fn render_docked_inner(&mut self, ctx: &mut Context, d: &mut DockedEditor) -> anyhow::Result<()> {
        let gl_arc = ctx.gl_arc();
        let gl = &*gl_arc;
        let drawable = ctx.drawable_size();

        self.process_shell_events(ctx, d);
        self.sync_generation(&mut d.rt.state, &mut d.bridge);
        self.sync_selection(&mut d.rt.state, &mut d.bridge);
        if d.bridge.level_name != self.current_level_name {
            d.bridge.level_name = self.current_level_name.clone();
            d.scene.borrow_mut().set_current = Some(self.current_level_file());
        }
        if d.rt.state.mode == ShellMode::Edit {
            self.sync_entities(&mut d.bridge, gl);
            self.track_snapshot_selection();
        }
        if self.texture_asset_browser.is_open() {
            self.texture_asset_browser.ensure_thumbnails_loaded(gl, &mut d.rt.egui);
        }

        let size = d.rt.ensure_target(gl, drawable)?;
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let simulating = d.rt.is_simulating();
        // Play shows the game camera, Edit/Pause the editor camera.
        let (view, proj) = if simulating {
            (self.fp_camera.view_matrix(self.player_eye_position()), self.fp_camera.projection_matrix(aspect))
        } else {
            d.rt.view_proj()
        };
        let draw_calls = self.draw_scene(gl, size, view, proj)?;
        let params = self.render_params;
        let (tint_color, tint_strength) = self.screen_effects.current();
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

        let dt = ctx.time.delta.max(1e-4);
        let mut world = std::mem::take(&mut self.world);
        let perf = PerfSnapshot { fps: 1.0 / dt, frame_ms: dt * 1000.0, entities: world.len() as usize, draw_calls };
        let mut host = DockHost { game: self, gl, drawable, pending: Pending::default() };
        let events = d.rt.run_ui(gl, drawable, &mut world, &mut host, perf);
        let pending = std::mem::take(&mut host.pending);
        drop(host);
        self.world = world;

        for event in &events {
            if matches!(event, ViewportEvent::TransformCommitted { .. }) {
                self.after_transform_commit(&d.rt.state);
            }
        }
        if let Some(path) = pending.picked_texture {
            self.apply_picked_texture(gl, path);
        }
        match pending.multiplayer {
            Some(engine::ui::MultiplayerAction::Host) => self.start_hosting(),
            Some(engine::ui::MultiplayerAction::Join) => self.join_game(),
            Some(engine::ui::MultiplayerAction::Disconnect) => self.disconnect_net(),
            Some(engine::ui::MultiplayerAction::SetLevelSwitchPermission { net_id, allowed }) => {
                self.set_level_switch_permission(net_id, allowed)
            }
            Some(engine::ui::MultiplayerAction::CancelReconnect) => self.cancel_reconnect(),
            None => {}
        }
        match pending.appearance {
            Some(engine::ui::AppearanceAction::ImportNewModel) => self.pick_new_appearance_model(),
            Some(engine::ui::AppearanceAction::SelectExisting(index)) => self.select_appearance(index),
            Some(engine::ui::AppearanceAction::ResetToDefault) => self.reset_appearance(),
            None => {}
        }
        self.apply_scene_requests(ctx.gl(), d);
        self.process_shell_events(ctx, d);
        Ok(())
    }

    /// Maps the shell's Play / Pause / Stop buttons onto the game's own modes.
    fn process_shell_events(&mut self, ctx: &mut Context, d: &mut DockedEditor) {
        for event in d.rt.shell.take_events() {
            match event {
                ShellEvent::PlayStarted => {
                    if self.mode == EditorMode::Edit {
                        self.sync_entities(&mut d.bridge, ctx.gl());
                        self.enter_play_mode(ctx);
                        self.docked_captured = true;
                    }
                }
                ShellEvent::Paused => {
                    if self.mode == EditorMode::Play {
                        self.pause(ctx);
                        self.docked_captured = false;
                    }
                }
                ShellEvent::Resumed => {
                    if self.mode == EditorMode::Play {
                        self.resume(ctx);
                        self.docked_captured = true;
                    }
                }
                ShellEvent::Stopped { .. } => {
                    self.exit_play_mode(ctx);
                    self.docked_captured = false;
                }
                ShellEvent::AssetDropped { path, target, .. } => self.asset_dropped(ctx.gl(), d, path, target),
                ShellEvent::QuitApproved => ctx.should_quit = true,
                // File menu results are applied by `apply_scene_requests`; double-clicking an
                // asset has no game meaning yet.
                ShellEvent::SceneNew | ShellEvent::SceneOpened(_) | ShellEvent::SceneSaved(_) | ShellEvent::AssetOpened(_) => {}
            }
        }
    }

    /// Every entity id is stale after `apply_level` (level load, legacy undo,
    /// Stop): drop what refers to them.
    fn sync_generation(&mut self, state: &mut EditorState, b: &mut Bridge) {
        if b.seen_generation == self.level_generation {
            return;
        }
        b.seen_generation = self.level_generation;
        state.selection.clear();
        state.undo.clear();
        b.names.clear();
        b.script_attempted.clear();
        b.last_shell_selection.clear();
        b.last_legacy_selection = (None, None, None, None);
    }

    /// Keeps the shell's selection and the Selected tab's `selected_*` fields pointing
    /// at the same entity, whichever side changed.
    fn sync_selection(&mut self, state: &mut EditorState, b: &mut Bridge) {
        let shell = state.selection.to_vec();
        let legacy = (self.selected_entity, self.selected_rig, self.selected_character, self.selected_spawner);
        if shell != b.last_shell_selection {
            self.selected_entity = None;
            self.selected_rig = None;
            self.selected_character = None;
            self.selected_spawner = None;
            for &e in &shell {
                if !self.world.contains(e) {
                    continue;
                }
                if self.world.get::<&CharacterMeta>(e).is_ok() {
                    self.selected_character = Some(e);
                } else if self.world.get::<&SpawnerConfig>(e).is_ok() {
                    self.selected_spawner = Some(e);
                } else if self.world.get::<&crate::RigRoot>(e).is_ok() {
                    self.selected_rig = Some(e);
                } else if self.world.get::<&LevelObjectMeta>(e).is_ok() {
                    self.selected_entity = Some(e);
                }
            }
        } else if legacy != b.last_legacy_selection {
            let pick = [
                (legacy.0, b.last_legacy_selection.0),
                (legacy.1, b.last_legacy_selection.1),
                (legacy.2, b.last_legacy_selection.2),
                (legacy.3, b.last_legacy_selection.3),
            ]
            .into_iter()
            .find(|(now, before)| now != before && now.is_some())
            .and_then(|(now, _)| now);
            if let Some(e) = pick {
                state.selection.select_only(e);
            }
        }
        b.last_shell_selection = state.selection.to_vec();
        b.last_legacy_selection = (self.selected_entity, self.selected_rig, self.selected_character, self.selected_spawner);
    }

    fn mesh_bounds(&self, bridge: &mut Bridge, source: &MeshSource) -> Aabb {
        let key = format!("{source:?}");
        if let Some(b) = bridge.bounds.get(&key) {
            return *b;
        }
        let b = self
            .resolve_mesh_data(source)
            .ok()
            .and_then(|m| Aabb::of_mesh(&m))
            .unwrap_or_else(|| Aabb::from_center_half(Vec3::ZERO, Vec3::splat(0.5)));
        bridge.bounds.insert(key, b);
        b
    }

    /// The bridge between the game's entities and what the shell needs. Runs in Edit mode.
    fn sync_entities(&mut self, d: &mut Bridge, gl: &glow::Context) {
        self.adopt_primitives(gl);
        self.sync_components(d);
    }

    fn adopt_primitives(&mut self, gl: &glow::Context) {
        // 1. Shell-created cubes/planes become real level objects (mesh, collider, saved).
        let new_primitives: Vec<(Entity, PrimitiveKind)> = self
            .world
            .query::<&Primitive>()
            .without::<&LevelObjectMeta>()
            .without::<&MeshRenderer>()
            .iter()
            .filter_map(|(e, p)| match p.0 {
                PrimitiveShape::Cube => Some((e, PrimitiveKind::Cube)),
                PrimitiveShape::Plane => Some((e, PrimitiveKind::Plane)),
                _ => None, // the sandbox has no sphere/capsule mesh to show
            })
            .collect();
        for (e, kind) in new_primitives {
            let source = MeshSource::Primitive(kind);
            let Ok(mesh) = self.resolve_mesh(gl, &source) else { continue };
            let texture = self.resolve_texture(gl, None);
            let (name, rotation, scale) = {
                let Ok(t) = self.world.get::<&Transform>(e) else { continue };
                let name = self.world.get::<&Name>(e).map(|n| n.0.clone()).unwrap_or_else(|_| "Cube".into());
                (name, t.rotation, t.scale)
            };
            let half_extents = collider_half_extents(&source, scale.to_array());
            let meta = LevelObjectMeta {
                name,
                mesh_source: source,
                texture_path: None,
                rotation_euler_deg: euler_deg(rotation),
                script_path: None,
                level_transition: None,
                screen_effect: None,
                camera_shake: None,
            };
            let _ = self.world.remove_one::<Primitive>(e);
            let _ = self.world.insert(
                e,
                (
                    MeshRenderer { mesh, texture: Some(texture) },
                    meta,
                    Collider { shape: ColliderShape::Aabb { half_extents }, is_trigger: false },
                ),
            );
        }

    }

    /// Everything in the bridge that needs no GL.
    fn sync_components(&mut self, d: &mut Bridge) {
        // 2. Names: `Name` for the hierarchy, the game's own metadata name for saving.
        let mut metas: Vec<(Entity, String)> = Vec::new();
        metas.extend(self.world.query::<&LevelObjectMeta>().iter().map(|(e, m)| (e, m.name.clone())));
        metas.extend(self.world.query::<&CharacterMeta>().iter().map(|(e, m)| (e, m.name.clone())));
        metas.extend(self.world.query::<&SpawnerConfig>().iter().map(|(e, m)| (e, m.name.clone())));
        metas.extend(self.world.query::<&crate::RigRoot>().iter().map(|(e, m)| (e, m.name.clone())));
        for (e, meta_name) in metas {
            let shell_name = self.world.get::<&Name>(e).ok().map(|n| n.0.clone());
            match (shell_name, d.names.get(&e).cloned()) {
                (None, _) => {
                    let _ = self.world.insert_one(e, Name(meta_name.clone()));
                    d.names.insert(e, meta_name);
                }
                // Not seen before but already named: a copy made by the shell, its name wins.
                (Some(n), None) => {
                    self.set_meta_name(e, &n);
                    d.names.insert(e, n);
                }
                (Some(n), Some(last)) if n != last => {
                    self.set_meta_name(e, &n);
                    d.names.insert(e, n);
                }
                (Some(_), Some(last)) if meta_name != last => {
                    if let Ok(mut n) = self.world.get::<&mut Name>(e) {
                        n.0 = meta_name.clone();
                    }
                    d.names.insert(e, meta_name);
                }
                _ => {}
            }
        }

        // 3. Rig parts are driven by the rig system each frame: show them, don't let the shell edit them.
        let rig_entities: Vec<Entity> = self.world.query::<&RigPart>().without::<&Locked>().iter().map(|(e, _)| e).collect();
        for e in rig_entities {
            let _ = self.world.insert_one(e, Locked(true));
        }

        let part_names: Vec<(Entity, String)> = self
            .world
            .query::<&engine::rig::Rig>()
            .iter()
            .flat_map(|(_, rig)| rig.parts_by_name.iter().map(|(n, e)| (*e, n.clone())).collect::<Vec<_>>())
            .collect();
        for (e, name) in part_names {
            if self.world.get::<&Name>(e).is_err() {
                let _ = self.world.insert_one(e, Name(name));
            }
        }

        // 4. Pick bounds for meshes (GpuMesh keeps no vertices; unit cube otherwise).
        let unbounded: Vec<(Entity, MeshSource)> = self
            .world
            .query::<(&MeshRenderer, Option<&LevelObjectMeta>, Option<&RigPartMeta>, Option<&CharacterMeta>)>()
            .without::<&LocalBounds>()
            .iter()
            .filter_map(|(e, (_, level, rig, character))| {
                let source = level
                    .map(|m| m.mesh_source.clone())
                    .or_else(|| rig.map(|m| m.mesh_source.clone()))
                    .or_else(|| character.map(|_| MeshSource::Primitive(PrimitiveKind::Cube)))?;
                Some((e, source))
            })
            .collect();
        for (e, source) in unbounded {
            let b = self.mesh_bounds(d, &source);
            let _ = self.world.insert_one(e, LocalBounds(b));
        }

        // 5. Gizmo edits -> the game's own copies of the same data.
        for (_, (t, meta, collider, animator)) in
            self.world.query::<(&Transform, &mut LevelObjectMeta, Option<&mut Collider>, Option<&Animator>)>().iter()
        {
            // An animated object's live rotation is mid-motion, not authored.
            if animator.is_none() && euler_deg_to_quat(meta.rotation_euler_deg).angle_between(t.rotation) > 1e-3 {
                meta.rotation_euler_deg = euler_deg(t.rotation);
            }
            if let Some(c) = collider {
                if let ColliderShape::Aabb { half_extents } = &mut c.shape {
                    let want = collider_half_extents(&meta.mesh_source, t.scale.to_array());
                    if (*half_extents - want).abs().max_element() > 1e-4 {
                        *half_extents = want;
                    }
                }
            }
        }

        // 6. Non-Clone components: refresh the authored mirror, rebuild what a copy lacks.
        self.sync_authored();

        // 7. Scripts live in a non-Clone `BehaviorSlot`; re-attach on copies.
        let scriptless: Vec<(Entity, std::path::PathBuf)> = self
            .world
            .query::<&LevelObjectMeta>()
            .without::<&engine::behavior::BehaviorSlot>()
            .iter()
            .filter_map(|(e, m)| m.script_path.clone().map(|p| (e, p)))
            .collect();
        for (e, path) in scriptless {
            if d.script_attempted.insert(e) {
                self.attach_script_to_entity(e, &path);
            }
        }
    }

    fn set_meta_name(&mut self, e: Entity, name: &str) {
        if let Ok(mut m) = self.world.get::<&mut LevelObjectMeta>(e) {
            m.name = name.to_string();
        } else if let Ok(mut m) = self.world.get::<&mut CharacterMeta>(e) {
            m.name = name.to_string();
        } else if let Ok(mut m) = self.world.get::<&mut SpawnerConfig>(e) {
            m.name = name.to_string();
        } else if let Ok(mut m) = self.world.get::<&mut crate::RigRoot>(e) {
            m.name = name.to_string();
        }
    }

    fn sync_authored(&mut self) {
        // Rebuild live components a copy / undo-delete left out.
        let restore: Vec<(Entity, Authored)> = self
            .world
            .query::<&Authored>()
            .iter()
            .map(|(e, a)| (e, a.clone()))
            .collect();
        for (e, a) in restore {
            if let Some(def) = a.emitter {
                if self.world.get::<&ParticleEmitter>(e).is_err() {
                    let _ = self.world.insert_one(e, ParticleEmitter::new(def));
                }
            }
            if let Some((kind, base_position, base_rotation)) = a.animator {
                if self.world.get::<&Animator>(e).is_err() {
                    let _ = self.world.insert_one(e, Animator { kind, base_position, base_rotation, elapsed: 0.0 });
                }
            }
            if let Some(max) = a.max_health {
                if self.world.get::<&Health>(e).is_err() {
                    let _ = self.world.insert_one(e, Health::new(max));
                }
            }
            if let Some(nodes) = a.dialogue {
                if self.world.get::<&Dialogue>(e).is_err() {
                    let _ = self.world.insert_one(e, Dialogue::new(nodes));
                }
            }
        }
        let characters: Vec<(Entity, Vec3)> = self
            .world
            .query::<(&CharacterMeta, &Transform)>()
            .without::<&CharacterBrain>()
            .iter()
            .map(|(e, (_, t))| (e, t.position))
            .collect();
        for (e, position) in characters {
            let _ = self.world.insert_one(e, CharacterBrain::new(position));
        }
        let spawners: Vec<Entity> = self.world.query::<&SpawnerConfig>().without::<&SpawnerState>().iter().map(|(e, _)| e).collect();
        for e in spawners {
            let _ = self.world.insert_one(e, SpawnerState::new());
        }

        // Refresh the mirror from the live components.
        let live: Vec<(Entity, Authored)> = self
            .world
            .query::<(Option<&ParticleEmitter>, Option<&Animator>, Option<&Health>, Option<&Dialogue>)>()
            .iter()
            .filter(|(_, (p, a, h, dl))| p.is_some() || a.is_some() || h.is_some() || dl.is_some())
            .map(|(e, (p, a, h, dl))| {
                (
                    e,
                    Authored {
                        emitter: p.map(|p| p.def),
                        animator: a.map(|a| (a.kind, a.base_position, a.base_rotation)),
                        max_health: h.map(|h| h.max),
                        dialogue: dl.map(|dl| dl.nodes.clone()),
                    },
                )
            })
            .collect();
        for (e, authored) in live {
            // Only entities the game authored (level objects, characters), not
            // transient bursts or the player.
            if self.world.get::<&LevelObjectMeta>(e).is_ok() || self.world.get::<&CharacterMeta>(e).is_ok() {
                let _ = self.world.insert_one(e, authored);
            }
        }
    }

    /// An animated object's `Animator` re-derives its pose from `base_*`; make
    /// the pose the gizmo just committed the new base.
    fn after_transform_commit(&mut self, state: &EditorState) {
        for e in state.selection.to_vec() {
            let Ok((position, rotation)) = self.world.get::<&Transform>(e).map(|t| (t.position, t.rotation)) else { continue };
            if let Ok(mut a) = self.world.get::<&mut Animator>(e) {
                a.base_position = position;
                a.base_rotation = rotation;
            }
        }
    }
}

/// Runs `f` with egui's screen rect temporarily set to `rect`, so `Area`s anchored
/// to the screen (HUD, dialogue choices) anchor to the Viewport tab instead.
fn with_screen_rect(ctx: &egui::Context, rect: egui::Rect, f: impl FnOnce()) {
    let previous = ctx.input(|i| i.screen_rect);
    ctx.input_mut(|i| i.screen_rect = rect);
    f();
    ctx.input_mut(|i| i.screen_rect = previous);
}

fn has_extension(path: &Path, wanted: &[&str]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| wanted.iter().any(|w| e.eq_ignore_ascii_case(w)))
}

fn is_image(path: &Path) -> bool {
    has_extension(path, &["png", "jpg", "jpeg"])
}

fn is_level_file(path: &Path) -> bool {
    has_extension(path, &["ron"])
}

// ---------------------------------------------------------------- File menu

/// A File-menu operation waiting for the game to run it (the shell only has the
/// world during its UI; loading and saving a level needs the whole game and GL).
#[derive(Debug, PartialEq)]
enum SceneRequest {
    New,
    Open(PathBuf),
    Save(PathBuf),
}

#[derive(Default)]
struct SceneShared {
    queue: Vec<SceneRequest>,
    /// The game changed which file is "the current scene" (level switch); picked up on
    /// the shell's next per-frame sync.
    set_current: Option<Option<PathBuf>>,
}

/// File > New / Open / Save / Save As over the game's own `levels/*.ron`.
///
/// The methods validate what they can, queue a [`SceneRequest`] and return `Ok`; the
/// game runs the queue right after the shell's UI (`Sandbox::apply_scene_requests`) with
/// its Level code, and reports failures in the console. Relative paths: a bare name
/// (`foo`, `foo.ron`) means `<root>/levels/foo.ron`, anything with a folder is relative
/// to the asset root.
struct GameSceneIo {
    shared: Rc<RefCell<SceneShared>>,
    asset_root: PathBuf,
    levels_dir: PathBuf,
    current: Option<PathBuf>,
    undo_dirty: bool,
}

impl GameSceneIo {
    fn resolve(&self, typed: &Path) -> PathBuf {
        let mut path = if typed.is_absolute() {
            typed.to_path_buf()
        } else if typed.components().count() <= 1 {
            self.levels_dir.join(typed)
        } else {
            self.asset_root.join(typed)
        };
        if path.extension().is_none() {
            path.set_extension("ron");
        }
        path
    }
}

impl SceneIo for GameSceneIo {
    fn new_scene(&mut self, _world: &mut World) -> Result<(), String> {
        self.shared.borrow_mut().queue.push(SceneRequest::New);
        self.current = None;
        Ok(())
    }

    fn open(&mut self, path: &Path, _world: &mut World) -> Result<(), String> {
        let path = self.resolve(path);
        // Parse now so a bad file is refused before the shell clears its selection and undo.
        engine::level::load_from_file(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        self.shared.borrow_mut().queue.push(SceneRequest::Open(path.clone()));
        self.current = Some(path);
        Ok(())
    }

    fn save(&mut self, _world: &World, path: &Path) -> Result<(), String> {
        let path = self.resolve(path);
        match path.parent() {
            Some(dir) if !dir.is_dir() => return Err(format!("save {}: folder {} does not exist", path.display(), dir.display())),
            _ => {}
        }
        self.shared.borrow_mut().queue.push(SceneRequest::Save(path.clone()));
        self.current = Some(path);
        Ok(())
    }

    fn current_path(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    fn is_dirty(&self) -> bool {
        self.undo_dirty
    }

    fn sync_undo_dirty(&mut self, undo_dirty: bool) {
        self.undo_dirty = undo_dirty;
        if let Some(current) = self.shared.borrow_mut().set_current.take() {
            self.current = current;
        }
    }
}

impl Sandbox {
    /// `levels/<current level>.ron`, if that file exists.
    fn current_level_file(&self) -> Option<PathBuf> {
        let path = self.levels_dir.join(format!("{}.ron", self.current_level_name));
        path.is_file().then_some(path)
    }

    /// Loads a level file as the current level (File > Open, a dropped `.ron`).
    fn open_level_file(&mut self, gl: &glow::Context, path: &Path) -> anyhow::Result<()> {
        let level = engine::level::load_from_file(path)?;
        self.sync_current_level_into_cache();
        self.apply_level(gl, &level)?;
        self.cache_level(level);
        Ok(())
    }

    /// Writes the current world as the level file `path` (File > Save / Save As); the level
    /// is named after the file.
    fn save_level_file(&mut self, path: &Path) -> anyhow::Result<()> {
        let mut level = self.build_level_from_ecs();
        let stem = path.file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty());
        level.name = stem.ok_or_else(|| anyhow::anyhow!("{path:?} has no file name"))?.to_string();
        engine::level::save_to_file(&level, path)?;
        log::info!("saved level '{}' to {path:?}", level.name);
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

    /// Runs the File-menu operations queued by [`GameSceneIo`] during the shell's UI.
    fn apply_scene_requests(&mut self, gl: &glow::Context, d: &mut DockedEditor) {
        let requests = std::mem::take(&mut d.scene.borrow_mut().queue);
        for request in requests {
            let result = match &request {
                SceneRequest::New => {
                    self.sync_current_level_into_cache();
                    let empty = Level { name: "untitled".into(), ..Level::default() };
                    self.apply_level(gl, &empty)
                }
                SceneRequest::Open(path) => self.open_level_file(gl, path),
                SceneRequest::Save(path) => self.save_level_file(path),
            };
            if let Err(err) = result {
                log::error!("{request:?}: {err}");
                d.rt.shell.console.error(format!("{request:?} failed: {err}"));
            }
            // The file the shell now calls current was chosen by the request, not by the level name.
            d.bridge.level_name = self.current_level_name.clone();
        }
    }

    /// An asset dragged from the Assets tab and released over the viewport or hierarchy.
    fn asset_dropped(&mut self, gl: &glow::Context, d: &mut DockedEditor, path: PathBuf, target: Option<Entity>) {
        if is_level_file(&path) {
            // Through the shell, so unsaved changes are asked about first.
            d.rt.shell.request_open_scene(&mut d.rt.state, &mut self.world, &path);
        } else if is_image(&path) {
            let Some(entity) = target.filter(|e| self.world.contains(*e)) else {
                d.rt.shell.console.warn("drop the image onto an object to texture it");
                return;
            };
            let pick = if self.world.get::<&CharacterMeta>(entity).is_ok() {
                PendingTexturePick::ForCharacter(entity)
            } else if self.world.satisfies::<(&LevelObjectMeta, &MeshRenderer)>(entity).unwrap_or(false) {
                PendingTexturePick::ForEntity(entity)
            } else {
                d.rt.shell.console.warn("that object cannot take a texture");
                return;
            };
            self.push_undo_snapshot();
            self.pending_texture_pick = Some(pick);
            self.apply_picked_texture(gl, engine::level::relativize(&path, &self.asset_root));
        }
    }

    /// Camera for Edit mode while the dock is closed (the scene is only shown): the editor camera.
    pub(crate) fn closed_editor_view_proj(&self, aspect: f32) -> (Mat4, Mat4) {
        match &self.docked {
            Some(d) => (d.rt.shell.camera.view_matrix(), d.rt.shell.camera.projection_matrix(aspect)),
            None => (self.fp_camera.view_matrix(self.player_eye_position()), self.fp_camera.projection_matrix(aspect)),
        }
    }
}

fn euler_deg(q: Quat) -> Vec3 {
    let (x, y, z) = q.to_euler(EulerRot::XYZ);
    Vec3::new(x.to_degrees(), y.to_degrees(), z.to_degrees())
}

/// Components the shell must be able to copy (duplicate, undo of delete).
fn editor_registry() -> Registry {
    let mut registry = Registry::with_defaults();
    registry.register_cloneable::<MeshRenderer>();
    registry.register_cloneable::<LevelObjectMeta>();
    registry.register_cloneable::<LocalBounds>();
    registry.register_cloneable::<Collider>();
    registry.register_cloneable::<RigidBody>();
    registry.register_cloneable::<Light>();
    registry.register_cloneable::<ClassMember>();
    registry.register_cloneable::<CharacterMeta>();
    registry.register_cloneable::<SpawnerConfig>();
    registry.register_cloneable::<Authored>();
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::ai::Disposition;
    use engine::editor::EditorAction;

    fn level_object(name: &str, at: Vec3) -> (Transform, LevelObjectMeta, Collider) {
        let source = MeshSource::Primitive(PrimitiveKind::Cube);
        (
            Transform { position: at, rotation: Quat::IDENTITY, scale: Vec3::ONE },
            LevelObjectMeta {
                name: name.into(),
                mesh_source: source.clone(),
                texture_path: None,
                rotation_euler_deg: Vec3::ZERO,
                script_path: None,
                level_transition: None,
                screen_effect: None,
                camera_shake: None,
            },
            Collider { shape: ColliderShape::Aabb { half_extents: collider_half_extents(&source, [1.0; 3]) }, is_trigger: false },
        )
    }

    #[test]
    fn names_rotation_and_collider_follow_shell_edits() {
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let e = game.world.spawn(level_object("Crate", Vec3::ZERO));
        game.sync_components(&mut bridge);
        assert_eq!(game.world.get::<&Name>(e).unwrap().0, "Crate", "hierarchy name comes from the metadata");

        // Rename in the hierarchy -> metadata (what gets saved).
        game.world.get::<&mut Name>(e).unwrap().0 = "Renamed".into();
        // Gizmo rotate and scale.
        {
            let mut t = game.world.get::<&mut Transform>(e).unwrap();
            t.rotation = euler_deg_to_quat(Vec3::new(0.0, 90.0, 0.0));
            t.scale = Vec3::new(2.0, 1.0, 4.0);
        }
        game.sync_components(&mut bridge);
        let meta = game.world.get::<&LevelObjectMeta>(e).unwrap();
        assert_eq!(meta.name, "Renamed");
        assert!((meta.rotation_euler_deg.y - 90.0).abs() < 0.01, "got {:?}", meta.rotation_euler_deg);
        let ColliderShape::Aabb { half_extents } = game.world.get::<&Collider>(e).unwrap().shape else { panic!() };
        assert!((half_extents - Vec3::new(1.0, 0.5, 2.0)).abs().max_element() < 1e-4, "got {half_extents:?}");
        drop(meta);

        // The F2 panel renaming the metadata flows back to the hierarchy.
        game.world.get::<&mut LevelObjectMeta>(e).unwrap().name = "FromF2".into();
        game.sync_components(&mut bridge);
        assert_eq!(game.world.get::<&Name>(e).unwrap().0, "FromF2");
    }

    #[test]
    fn duplicate_and_undo_delete_keep_non_clone_components() {
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let mut state = EditorState::with_registry(editor_registry());
        let def = ParticleEmitterDef { rate_per_sec: 42.0, ..ParticleEmitterDef::default() };
        let (t, m, _) = level_object("Sparks", Vec3::new(1.0, 2.0, 3.0));
        let emitter = game.world.spawn((t, m, ParticleEmitter::new(def)));
        let (t, m, c) = level_object("Spinner", Vec3::ZERO);
        let animated = game.world.spawn((
            t,
            m,
            c,
            Animator { kind: AnimationKind::Orbit { axis: Vec3::Y, speed_deg_per_sec: 30.0 }, base_position: Vec3::ZERO, base_rotation: Quat::IDENTITY, elapsed: 0.0 },
        ));
        game.sync_components(&mut bridge);

        state.selection.select_only(emitter);
        state.selection.add(animated);
        state.perform(&mut game.world, EditorAction::Duplicate);
        game.sync_components(&mut bridge);
        let copies = state.selection.to_vec();
        assert_eq!(copies.len(), 2);
        for c in &copies {
            assert!(game.world.get::<&Name>(*c).is_ok());
            assert!(game.world.get::<&LevelObjectMeta>(*c).is_ok(), "metadata cloned so the copy is saved");
        }
        let copied_emitter = copies.iter().find(|c| game.world.get::<&ParticleEmitter>(**c).is_ok()).expect("emitter rebuilt on the copy");
        assert_eq!(game.world.get::<&ParticleEmitter>(*copied_emitter).unwrap().def.rate_per_sec, 42.0);
        assert!(copies.iter().any(|c| game.world.get::<&Animator>(*c).is_ok()), "animator rebuilt on the copy");

        // Delete + undo brings the object back with its emitter.
        state.selection.select_only(emitter);
        state.perform(&mut game.world, EditorAction::Delete);
        assert!(!game.world.contains(emitter));
        state.perform(&mut game.world, EditorAction::Undo);
        game.sync_components(&mut bridge);
        assert!(game.world.contains(emitter));
        assert_eq!(game.world.get::<&ParticleEmitter>(emitter).unwrap().def.rate_per_sec, 42.0);
    }

    #[test]
    fn characters_and_spawners_get_their_runtime_state_back() {
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let mut state = EditorState::with_registry(editor_registry());
        let guard = game.world.spawn((
            Transform::from_position(Vec3::new(0.0, 1.0, 0.0)),
            CharacterMeta {
                name: "Guard".into(),
                color: [1.0, 0.0, 0.0],
                texture_path: None,
                disposition: Disposition::Hostile,
                move_speed: 2.0,
                wander_radius: 1.0,
                sight_range: 5.0,
                damage: Some(3.0),
                attack_range: 1.0,
                attack_cooldown_secs: 1.0,
            },
            CharacterBrain::new(Vec3::new(0.0, 1.0, 0.0)),
            Health::new(30.0),
        ));
        game.sync_components(&mut bridge);
        state.selection.select_only(guard);
        state.perform(&mut game.world, EditorAction::Duplicate);
        game.sync_components(&mut bridge);
        let copy = state.selection.primary().unwrap();
        assert_ne!(copy, guard);
        assert!(game.world.get::<&CharacterBrain>(copy).is_ok());
        assert_eq!(game.world.get::<&Health>(copy).unwrap().max, 30.0);
    }

    #[test]
    fn selection_is_mirrored_both_ways() {
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let mut state = EditorState::with_registry(editor_registry());
        let a = game.world.spawn(level_object("A", Vec3::ZERO));
        let b = game.world.spawn(level_object("B", Vec3::X));
        game.sync_selection(&mut state, &mut bridge);

        state.selection.select_only(a);
        game.sync_selection(&mut state, &mut bridge);
        assert_eq!(game.selected_entity, Some(a), "viewport pick shows up in the F2 panel");

        game.selected_entity = Some(b); // click in the F2 outliner
        game.sync_selection(&mut state, &mut bridge);
        assert_eq!(state.selection.primary(), Some(b), "F2 pick selects in the shell");
        game.sync_selection(&mut state, &mut bridge);
        assert_eq!(game.selected_entity, Some(b), "stable when nothing changed");
    }

    #[test]
    fn stale_state_is_dropped_when_the_level_is_reapplied() {
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let mut state = EditorState::with_registry(editor_registry());
        let a = game.world.spawn(level_object("A", Vec3::ZERO));
        state.selection.select_only(a);
        state.perform(&mut game.world, EditorAction::Duplicate);
        assert!(state.undo.undo_len() > 0);
        game.level_generation += 1; // what `apply_level` does
        game.sync_generation(&mut state, &mut bridge);
        assert!(state.selection.is_empty());
        assert_eq!(state.undo.undo_len(), 0);
    }

    #[test]
    fn rays_pick_bridged_level_objects_and_skip_locked_rig_parts() {
        use engine::editor::picking::pick_nearest;
        use engine::editor::cam_matrices::Ray;
        let mut game = Sandbox::new();
        let mut bridge = Bridge::default();
        let near = game.world.spawn(level_object("Near", Vec3::new(0.0, 0.0, 0.0)));
        let far = game.world.spawn(level_object("Far", Vec3::new(0.0, 0.0, -5.0)));
        let part = game.world.spawn((
            Transform::from_position(Vec3::new(0.0, 0.0, 3.0)),
            RigPart { parent: None, local_position: Vec3::ZERO, local_rotation_euler_deg: Vec3::ZERO, local_rotation: Quat::IDENTITY },
        ));
        game.sync_components(&mut bridge);
        let ray = Ray::new(Vec3::new(0.0, 0.0, 10.0), Vec3::NEG_Z);
        let hit = pick_nearest(&game.world, &ray).expect("hit");
        assert_ne!(hit.entity, part, "locked rig parts are not pickable");
        assert_eq!(hit.entity, near);
        game.world.despawn(near).unwrap();
        assert_eq!(pick_nearest(&game.world, &ray).unwrap().entity, far);
    }
}
