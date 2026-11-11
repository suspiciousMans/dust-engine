//! Headless tests for the shell <-> scene/prefab wiring: RonSceneIo, dirty
//! tracking, prompts, autosave recovery, legacy level files and the prefab
//! commands. No GL, no SDL.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use glam::Vec3;
use hecs::{Entity, World};

use super::command::SetFieldCommand;
use super::hierarchy::{self, spawn_object, PrimitiveShape};
use super::inspector::Value;
use super::prefab::{self, PrefabInstance, PrefabMember};
use super::prefab_ui;
use super::ron_scene_io::{read_scene_any, RonSceneIo, SceneOrigin};
use super::scene::{self, scene_registry, testutil::TempDir, RawRon, SceneValue, UnknownComponents};
use super::scene_io::SceneIo;
use super::scene_prompts::{Choice, Confirm, PendingAction};
use super::shell::{EditorShell, ShellEvent, ShellHost};
use super::state::EditorState;
use crate::ecs::{Light, LightKind, Transform};
use crate::level::{self, Level, LevelObject, MeshSource, PrimitiveKind};

struct H;
impl ShellHost for H {}

struct Fixture {
    ctx: egui::Context,
    shell: EditorShell,
    editor: EditorState,
    world: World,
}

impl Fixture {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            shell: EditorShell::new(vec![]),
            editor: EditorState::with_registry(scene_registry()),
            world: World::new(),
        }
    }

    /// One headless UI frame (this is also what syncs dirty state / autosave).
    fn frame(&mut self) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0))),
            ..Default::default()
        };
        let Self { ctx, shell, editor, world } = self;
        let _ = ctx.run(input, |ctx| {
            shell.ui(ctx, editor, world, &mut H);
        });
    }

    fn spawn_cmd(&mut self) -> Entity {
        self.editor.create_empty(&mut self.world, None).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.world.iter().map(|r| hierarchy::display_name(&self.world, r.entity())).collect();
        v.sort();
        v
    }
}

fn find(w: &World, name: &str) -> Entity {
    w.iter().map(|r| r.entity()).find(|e| hierarchy::display_name(w, *e) == name).unwrap_or_else(|| panic!("no entity {name}"))
}

fn set_mtime(path: &Path, t: SystemTime) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

// ------------------------------------------------------------------ RonSceneIo

#[test]
fn save_open_roundtrip_keeps_settings_and_unknown_components() {
    let dir = TempDir::new("rsio-rt");
    let path = dir.path().join("s.scene.ron");
    let reg = scene_registry();
    let mut w = World::new();
    let a = spawn_object(&mut w, "Crate", None, Some(PrimitiveShape::Cube), Vec3::new(1.0, 2.0, 3.0));
    spawn_object(&mut w, "Lid", Some(a), Some(PrimitiveShape::Plane), Vec3::new(0.0, 0.5, 0.0));
    let mut unknown = UnknownComponents::default();
    unknown.fields.insert("Mystery".into(), [("power".to_string(), SceneValue::F32(9.0))].into_iter().collect());
    unknown.raw.insert("Blob".into(), RawRon::from_text("(a: 1,\n b: [2, 3])"));
    w.insert_one(a, unknown.clone()).unwrap();

    let mut io = RonSceneIo::new();
    io.document_mut().settings.insert("physics".into(), RawRon::from_text("(gravity: 9.5)"));
    assert_eq!(io.origin(), SceneOrigin::New);
    io.save_with(&mut w, &reg, &path).unwrap();
    assert_eq!(io.current_path(), Some(path.as_path()));
    assert_eq!(io.origin(), SceneOrigin::Scene);
    let first = std::fs::read_to_string(&path).unwrap();

    let mut io2 = RonSceneIo::new();
    let mut w2 = World::new();
    w2.spawn((hierarchy::Name("stale".into()),));
    io2.open_with(&path, &mut w2, &reg).unwrap();
    assert_eq!(w2.len(), 2, "open replaces the world");
    assert_eq!(io2.document().settings.get("physics"), Some(&RawRon::from_text("(gravity: 9.5)")));
    let crate_e = find(&w2, "Crate");
    assert_eq!(w2.get::<&Transform>(crate_e).unwrap().position, Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(*w2.get::<&UnknownComponents>(crate_e).unwrap(), unknown);
    assert_eq!(hierarchy::parent_of(&w2, find(&w2, "Lid")), Some(crate_e));
    assert!(!io2.is_dirty());

    // Saving what we loaded reproduces the file byte for byte (ids, settings, unknowns).
    let path2 = dir.path().join("again.scene.ron");
    io2.save_with(&mut w2, &reg, &path2).unwrap();
    assert_eq!(std::fs::read_to_string(&path2).unwrap(), first);
    let msgs = io2.take_messages();
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert!(msgs[0].contains("Mystery"), "only the unknown component is reported: {msgs:?}");
    assert!(io2.take_messages().is_empty(), "messages are drained");
}

#[test]
fn legacy_save_is_refused_and_new_resets_document() {
    let reg = scene_registry();
    let mut io = RonSceneIo::new();
    let mut w = World::new();
    assert!(io.save(&w, Path::new("x.ron")).is_err());
    io.document_mut().settings.insert("k".into(), RawRon::from_text("1"));
    spawn_object(&mut w, "a", None, None, Vec3::ZERO);
    io.new_scene_with(&mut w, &reg).unwrap();
    assert_eq!(w.len(), 0);
    assert!(io.document().settings.is_empty());
    assert!(io.current_path().is_none());
    assert_eq!(io.title(), "untitled");
    assert!(io.revert_with(&mut w, &reg).is_err(), "nothing to revert to");
}

#[test]
fn newer_format_files_are_refused_with_a_message() {
    let dir = TempDir::new("rsio-new");
    let path = dir.path().join("future.ron");
    std::fs::write(&path, "(format_version: 999, name: \"x\", entities: [])").unwrap();
    let mut io = RonSceneIo::new();
    let mut w = World::new();
    let err = io.open_with(&path, &mut w, &scene_registry()).unwrap_err();
    assert!(err.contains("999"), "{err}");
}

// ------------------------------------------------------------------ dirty flag

#[test]
fn dirty_flag_follows_the_undo_stack() {
    let dir = TempDir::new("dirty");
    let path = dir.path().join("d.scene.ron");
    let mut f = Fixture::new();
    f.frame();
    assert!(!f.shell.is_scene_dirty());
    assert_eq!(f.shell.scene_title(), "untitled");

    f.spawn_cmd();
    f.frame();
    assert!(f.shell.is_scene_dirty(), "a command makes the scene dirty");
    assert_eq!(f.shell.scene_title(), "untitled*");

    assert!(f.shell.save_scene(&mut f.editor, &mut f.world, &path));
    assert!(!f.shell.is_scene_dirty());
    assert_eq!(f.shell.scene_title(), "d.scene.ron");
    assert_eq!(f.shell.window_title("Dust Editor"), "d.scene.ron - Dust Editor");

    f.spawn_cmd();
    f.frame();
    assert_eq!(f.shell.scene_title(), "d.scene.ron*");
    f.editor.undo(&mut f.world);
    f.frame();
    assert!(!f.shell.is_scene_dirty(), "undo back to the saved state is clean");
    f.editor.redo(&mut f.world);
    f.frame();
    assert!(f.shell.is_scene_dirty(), "redo is an edit");
    f.editor.undo(&mut f.world);
    f.editor.undo(&mut f.world); // before the saved point
    f.frame();
    assert!(f.shell.is_scene_dirty());
    f.editor.redo(&mut f.world);
    f.frame();
    assert!(!f.shell.is_scene_dirty());

    // New scene resets everything
    f.shell.new_scene(&mut f.editor, &mut f.world);
    assert!(!f.shell.is_scene_dirty());
    assert_eq!(f.shell.scene_title(), "untitled");
}

// ------------------------------------------------------------------ prompts

#[test]
fn unsaved_changes_prompt_on_new_open_and_quit() {
    let dir = TempDir::new("prompts");
    let saved = dir.path().join("saved.scene.ron");
    let other = dir.path().join("other.scene.ron");
    let mut f = Fixture::new();
    // two scene files on disk
    f.spawn_cmd();
    assert!(f.shell.save_scene(&mut f.editor, &mut f.world, &other));
    f.shell.new_scene(&mut f.editor, &mut f.world);
    f.spawn_cmd();
    f.spawn_cmd();
    assert!(f.shell.save_scene(&mut f.editor, &mut f.world, &saved));
    let _ = f.shell.take_events();

    // clean -> no prompt
    f.shell.request_new_scene(&mut f.editor, &mut f.world);
    assert!(f.shell.pending_confirm().is_none());
    assert_eq!(f.world.len(), 0);

    // dirty -> prompt; Cancel keeps everything
    f.spawn_cmd();
    f.shell.request_new_scene(&mut f.editor, &mut f.world);
    assert_eq!(f.shell.pending_confirm(), Some(&Confirm::Unsaved(PendingAction::NewScene)));
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Cancel);
    assert!(f.shell.pending_confirm().is_none());
    assert_eq!(f.world.len(), 1);

    // Don't Save -> proceeds
    f.shell.request_new_scene(&mut f.editor, &mut f.world);
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Secondary);
    assert_eq!(f.world.len(), 0);
    assert!(!f.shell.is_scene_dirty());

    // Open while dirty: Save writes the (untitled -> needs a path) ... use a saved scene instead
    f.shell.open_scene(&mut f.editor, &mut f.world, &saved);
    assert_eq!(f.world.len(), 2);
    f.spawn_cmd();
    f.shell.request_open_scene(&mut f.editor, &mut f.world, &other);
    assert_eq!(f.shell.pending_confirm(), Some(&Confirm::Unsaved(PendingAction::OpenScene(other.clone()))));
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Primary); // Save, then open
    assert_eq!(f.world.len(), 1, "other.scene.ron has one entity");
    assert_eq!(f.shell.scene_io.current_path(), Some(other.as_path()));
    let (back, _) = read_scene_any(&saved).unwrap();
    assert_eq!(back.entities.len(), 3, "the save before opening wrote the third entity");

    // Quit
    let _ = f.shell.take_events();
    f.spawn_cmd();
    assert!(!f.shell.request_quit(&mut f.editor, &mut f.world));
    assert!(!f.shell.take_events().contains(&ShellEvent::QuitApproved));
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Cancel);
    assert!(!f.shell.take_events().contains(&ShellEvent::QuitApproved));
    assert!(!f.shell.request_quit(&mut f.editor, &mut f.world));
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Secondary);
    assert!(f.shell.take_events().contains(&ShellEvent::QuitApproved));
    // clean -> immediate
    f.shell.open_scene(&mut f.editor, &mut f.world, &other);
    assert!(f.shell.request_quit(&mut f.editor, &mut f.world));
    assert!(f.shell.take_events().contains(&ShellEvent::QuitApproved));
}

#[test]
fn untitled_scene_save_prompt_continues_the_pending_action() {
    let mut f = Fixture::new();
    f.spawn_cmd();
    f.shell.request_new_scene(&mut f.editor, &mut f.world);
    // No path: choosing Save has to ask for one instead of dropping the action.
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Primary);
    assert!(f.shell.pending_confirm().is_none());
    assert_eq!(f.world.len(), 1, "nothing happened yet; the path prompt is up");
    assert!(f.shell.is_scene_dirty());
}

// ------------------------------------------------------------------ autosave

#[test]
fn autosave_is_paced_and_removed_on_save() {
    let dir = TempDir::new("autosave");
    let path = dir.path().join("a.scene.ron");
    let reg = scene_registry();
    let mut w = World::new();
    spawn_object(&mut w, "a", None, None, Vec3::ZERO);
    let mut io = RonSceneIo::new();
    let t0 = Instant::now();
    // untitled scenes have no path to put an autosave next to
    io.sync_undo_dirty(true);
    assert_eq!(io.autosave_tick(&mut w, &reg, t0), Ok(false));
    io.save_with(&mut w, &reg, &path).unwrap();
    io.sync_undo_dirty(false);
    assert_eq!(io.autosave_tick(&mut w, &reg, t0), Ok(false), "clean scenes are not autosaved");
    io.sync_undo_dirty(true);
    assert_eq!(io.autosave_tick(&mut w, &reg, t0), Ok(true));
    let auto = scene::autosave_path(&path);
    assert!(auto.exists());
    assert_eq!(io.autosave_tick(&mut w, &reg, t0 + Duration::from_secs(10)), Ok(false), "paced");
    assert_eq!(io.autosave_tick(&mut w, &reg, t0 + Duration::from_secs(61)), Ok(true));
    io.save_with(&mut w, &reg, &path).unwrap();
    assert!(!auto.exists(), "a save removes the autosave");
}

#[test]
fn autosave_failure_backs_off_instead_of_retrying_every_frame() {
    let dir = TempDir::new("autosave-fail");
    let path = dir.path().join("a.scene.ron");
    let reg = scene_registry();
    let mut w = World::new();
    let mut io = RonSceneIo::new();
    io.save_with(&mut w, &reg, &path).unwrap();
    io.sync_undo_dirty(true);
    // a directory where the autosave file should go makes the write fail
    std::fs::create_dir_all(scene::autosave_path(&path)).unwrap();
    let t0 = Instant::now();
    assert!(io.autosave_tick(&mut w, &reg, t0).is_err());
    assert_eq!(io.autosave_tick(&mut w, &reg, t0 + Duration::from_secs(1)), Ok(false));
}

#[test]
fn recovery_is_offered_only_for_a_newer_autosave() {
    let dir = TempDir::new("recovery");
    let path = dir.path().join("r.scene.ron");
    let mut f = Fixture::new();
    f.spawn_cmd();
    assert!(f.shell.save_scene(&mut f.editor, &mut f.world, &path));
    let io = RonSceneIo::new();
    assert!(io.recovery_for(&path).is_none(), "no autosave");

    // The editor "crashes" after more work: write an autosave of the 2-entity state.
    f.spawn_cmd();
    f.shell.scene_io.sync_undo_dirty(true);
    let auto = scene::autosave_path(&path);
    assert_eq!(f.shell.scene_io.autosave_tick(&mut f.world, &f.editor.registry, Instant::now()), Ok(true));
    let saved_at = mtime(&path);
    set_mtime(&auto, saved_at - Duration::from_secs(5));
    assert!(io.recovery_for(&path).is_none(), "older autosave is stale");
    set_mtime(&auto, saved_at + Duration::from_secs(5));
    let rec = io.recovery_for(&path).expect("newer autosave");
    assert_eq!(rec.autosave, auto);
    // a lone autosave with no scene file is recoverable too
    let lone = dir.path().join("lone.scene.ron");
    std::fs::copy(&auto, scene::autosave_path(&lone)).unwrap();
    assert!(io.recovery_for(&lone).is_some());

    // Shell: opening offers it; Recover loads the autosave and stays dirty.
    let mut g = Fixture::new();
    g.shell.request_open_scene(&mut g.editor, &mut g.world, &path);
    assert_eq!(g.shell.pending_confirm(), Some(&Confirm::Recover(path.clone())));
    assert_eq!(g.world.len(), 0, "nothing opened until answered");
    g.shell.resolve_confirm(&mut g.editor, &mut g.world, Choice::Primary);
    assert_eq!(g.world.len(), 2, "autosave content");
    assert!(g.shell.is_scene_dirty(), "recovered state is unsaved");
    assert_eq!(g.shell.scene_title(), "r.scene.ron*");
    g.frame();
    assert!(g.shell.is_scene_dirty(), "still dirty after the per-frame undo sync");
    assert!(g.shell.save_scene(&mut g.editor, &mut g.world, &path));
    assert!(!g.shell.is_scene_dirty());
    assert!(!auto.exists());
    assert_eq!(read_scene_any(&path).unwrap().0.entities.len(), 2);

    // "Open saved file" discards the autosave.
    std::fs::write(&auto, "garbage").unwrap();
    set_mtime(&auto, mtime(&path) + Duration::from_secs(5));
    let mut h = Fixture::new();
    h.shell.request_open_scene(&mut h.editor, &mut h.world, &path);
    h.shell.resolve_confirm(&mut h.editor, &mut h.world, Choice::Secondary);
    assert_eq!(h.world.len(), 2);
    assert!(!auto.exists());
    assert!(!h.shell.is_scene_dirty());

    // Cancel does nothing.
    std::fs::copy(&path, &auto).unwrap();
    set_mtime(&auto, mtime(&path) + Duration::from_secs(5));
    let mut k = Fixture::new();
    k.shell.request_open_scene(&mut k.editor, &mut k.world, &path);
    k.shell.resolve_confirm(&mut k.editor, &mut k.world, Choice::Cancel);
    assert_eq!(k.world.len(), 0);
    assert!(auto.exists());
}

// ------------------------------------------------------------------ legacy levels

fn write_level(path: &Path) {
    let lvl = Level {
        name: "old".into(),
        objects: vec![
            LevelObject {
                name: "floor".into(),
                mesh: MeshSource::Primitive(PrimitiveKind::Plane),
                texture_path: None,
                position: [0.0, -1.0, 0.0],
                rotation_euler_deg: [0.0; 3],
                scale: [10.0, 1.0, 10.0],
                is_dynamic: true,
                is_trigger: false,
                animation: None,
                script: None,
                class: None,
                level_transition: None,
                screen_effect: None,
                camera_shake: None,
            },
            LevelObject {
                name: "box".into(),
                mesh: MeshSource::Primitive(PrimitiveKind::Cube),
                texture_path: None,
                position: [2.0, 0.5, 1.0],
                rotation_euler_deg: [0.0; 3],
                scale: [1.0; 3],
                is_dynamic: false,
                is_trigger: false,
                animation: None,
                script: None,
                class: None,
                level_transition: None,
                screen_effect: None,
                camera_shake: None,
            },
        ],
        ..Default::default()
    };
    level::save_to_file(&lvl, path).unwrap();
}

#[test]
fn level_files_open_via_level_compat_and_save_as_scene_after_confirmation() {
    let dir = TempDir::new("legacy");
    let lvl = dir.path().join("old.ron");
    write_level(&lvl);
    let original = std::fs::read_to_string(&lvl).unwrap();
    assert_eq!(read_scene_any(&lvl).unwrap().1, SceneOrigin::LegacyLevel);

    let mut f = Fixture::new();
    f.shell.open_scene(&mut f.editor, &mut f.world, &lvl);
    assert_eq!(f.names(), ["box", "floor"]);
    assert_eq!(f.world.get::<&Transform>(find(&f.world, "box")).unwrap().position, Vec3::new(2.0, 0.5, 1.0));
    assert!(!f.shell.is_scene_dirty());
    // level-only data survives as raw/unknown payload
    let floor = find(&f.world, "floor");
    assert!(f.world.get::<&UnknownComponents>(floor).unwrap().raw["LevelObject"].text().contains("is_dynamic: true"));

    // Save asks first and leaves the level file untouched until answered.
    f.shell.save_current(&mut f.editor, &mut f.world);
    assert!(matches!(f.shell.pending_confirm(), Some(Confirm::LevelSave { path, .. }) if *path == lvl));
    assert_eq!(std::fs::read_to_string(&lvl).unwrap(), original);
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Cancel);
    assert_eq!(std::fs::read_to_string(&lvl).unwrap(), original);

    // Save As to a new path asks too; confirm -> scene format, original kept.
    let out = dir.path().join("new.scene.ron");
    f.shell.save_scene_as(&mut f.editor, &mut f.world, &out);
    assert!(matches!(f.shell.pending_confirm(), Some(Confirm::LevelSave { .. })));
    assert!(!out.exists());
    f.shell.resolve_confirm(&mut f.editor, &mut f.world, Choice::Primary);
    assert_eq!(std::fs::read_to_string(&lvl).unwrap(), original);
    let (scene, origin) = read_scene_any(&out).unwrap();
    assert_eq!(origin, SceneOrigin::Scene);
    assert_eq!(scene.entities.len(), 2);
    assert!(scene.settings.contains_key("physics"), "level settings carried over");

    // Now it is a scene: further saves don't ask, and it reopens.
    f.shell.save_current(&mut f.editor, &mut f.world);
    assert!(f.shell.pending_confirm().is_none());
    let mut g = Fixture::new();
    g.shell.open_scene(&mut g.editor, &mut g.world, &out);
    assert_eq!(g.names(), ["box", "floor"]);
}

#[test]
fn overwriting_a_level_in_place_needs_the_same_confirmation() {
    let dir = TempDir::new("legacy2");
    let lvl = dir.path().join("old.ron");
    write_level(&lvl);
    let mut io = RonSceneIo::new();
    let mut w = World::new();
    io.open_with(&lvl, &mut w, &scene_registry()).unwrap();
    assert_eq!(io.origin(), SceneOrigin::LegacyLevel);
    assert!(io.save_confirmation(&lvl).unwrap().contains("overwrites"));
    io.save_with(&mut w, &scene_registry(), &lvl).unwrap();
    assert!(io.save_confirmation(&lvl).is_none());
    assert_eq!(read_scene_any(&lvl).unwrap().1, SceneOrigin::Scene);
}

// ------------------------------------------------------------------ prefabs

fn lamp(w: &mut World) -> Entity {
    let root = spawn_object(w, "Lamp", None, Some(PrimitiveShape::Cube), Vec3::new(1.0, 0.0, 0.0));
    let bulb = spawn_object(w, "Bulb", Some(root), None, Vec3::new(0.0, 2.0, 0.0));
    w.insert_one(bulb, Light { color: Vec3::ONE, intensity: 1.0, kind: LightKind::Point { range: 5.0 }, is_static: false }).unwrap();
    root
}

fn intensity(w: &World, e: Entity) -> f32 {
    w.get::<&Light>(e).unwrap().intensity
}

fn range(w: &World, e: Entity) -> f32 {
    match w.get::<&Light>(e).unwrap().kind {
        LightKind::Point { range } => range,
        _ => panic!("not a point light"),
    }
}

fn bulb_of(w: &World, root: Entity) -> Entity {
    hierarchy::subtree(w, root).into_iter().find(|e| hierarchy::display_name(w, *e) == "Bulb").unwrap()
}

fn set_intensity(f: &mut Fixture, bulb: Entity, v: f32) {
    f.editor.exec(&mut f.world, SetFieldCommand::new(vec![bulb], "Light", "intensity", Value::F32(v))).unwrap();
    f.editor.undo.seal();
}

/// Project with a prefab "lamp" instantiated twice: returns (fixture, dir, a, b).
fn prefab_fixture() -> (Fixture, TempDir, Entity, Entity) {
    let dir = TempDir::new("prefabs");
    let mut f = Fixture::new();
    f.shell.set_project_dir(Some(dir.path().to_path_buf()));
    f.frame(); // wires the library into the editor state
    let root = lamp(&mut f.world);
    let a = prefab_ui::create_prefab(&mut f.editor, &mut f.world, root, "props/lamp").unwrap();
    assert_eq!(a, root);
    let b = prefab_ui::instantiate_prefab(&mut f.editor, &mut f.world, "props/lamp.prefab.ron", None).unwrap();
    (f, dir, a, b)
}

#[test]
fn prefab_is_disabled_without_a_project_dir() {
    let mut f = Fixture::new();
    f.frame();
    let root = lamp(&mut f.world);
    assert!(f.editor.prefab_ui.library().is_none());
    assert_eq!(prefab_ui::create_prefab(&mut f.editor, &mut f.world, root, "x").unwrap_err(), prefab_ui::NO_PROJECT_HINT);
    assert!(prefab_ui::instantiate_prefab(&mut f.editor, &mut f.world, "x.prefab.ron", None).is_err());
    assert!(prefab_ui::apply(&mut f.editor, &mut f.world, root).is_err());
}

#[test]
fn prefab_name_normalisation() {
    assert_eq!(prefab_ui::normalize_source("lamp").unwrap(), "lamp.prefab.ron");
    assert_eq!(prefab_ui::normalize_source(" props\\lamp.prefab.ron ").unwrap(), "props/lamp.prefab.ron");
    assert!(prefab_ui::normalize_source("../evil").is_err());
    assert!(prefab_ui::normalize_source("  ").is_err());
}

#[test]
fn create_instantiate_edit_apply_propagates_to_the_other_instance() {
    let (mut f, dir, a, b) = prefab_fixture();
    let (ba, bb) = (bulb_of(&f.world, a), bulb_of(&f.world, b));
    assert!(dir.path().join("prefabs/props/lamp.prefab.ron").exists());
    assert_eq!(f.world.get::<&PrefabInstance>(b).unwrap().source, "props/lamp.prefab.ron");
    assert!(f.shell.is_scene_dirty() || f.editor.undo.is_dirty());

    set_intensity(&mut f, bb, 7.0);
    assert_eq!(intensity(&f.world, ba), 1.0);
    prefab_ui::apply(&mut f.editor, &mut f.world, b).unwrap();
    assert_eq!(intensity(&f.world, ba), 7.0, "second instance picked up the applied change");
    assert_eq!(intensity(&f.world, bb), 7.0);
    let lib = f.editor.prefab_ui.library().unwrap();
    let saved = lib.borrow_mut().get("props/lamp.prefab.ron").unwrap();
    let light = saved.entities.iter().find(|e| e.name == "Bulb").unwrap();
    assert_eq!(light.field("Light", "intensity"), Some(&SceneValue::F32(7.0)));
    assert!(f.world.get::<&PrefabInstance>(b).unwrap().overrides.iter().all(|o| o.field != "intensity"));

    // Undo puts back the other instance AND the prefab file; redo reapplies.
    assert!(f.editor.undo(&mut f.world));
    assert_eq!(intensity(&f.world, ba), 1.0);
    assert_eq!(intensity(&f.world, bb), 7.0);
    let saved = lib.borrow_mut().get("props/lamp.prefab.ron").unwrap();
    assert_eq!(saved.entities.iter().find(|e| e.name == "Bulb").unwrap().field("Light", "intensity"), Some(&SceneValue::F32(1.0)));
    assert!(f.editor.redo(&mut f.world));
    assert_eq!(intensity(&f.world, ba), 7.0);
    let saved = lib.borrow_mut().get("props/lamp.prefab.ron").unwrap();
    assert_eq!(saved.entities.iter().find(|e| e.name == "Bulb").unwrap().field("Light", "intensity"), Some(&SceneValue::F32(7.0)));
}

#[test]
fn revert_field_and_revert_all_are_undoable() {
    let (mut f, _dir, _a, b) = prefab_fixture();
    let bb = bulb_of(&f.world, b);
    set_intensity(&mut f, bb, 4.0);
    f.editor.exec(&mut f.world, SetFieldCommand::new(vec![bb], "Light", "range", Value::F32(9.0))).unwrap();
    f.editor.undo.seal();
    let target = f.world.get::<&PrefabMember>(bb).unwrap().0.last().unwrap().id;

    prefab_ui::revert_field(&mut f.editor, &mut f.world, b, target, "Light", "intensity").unwrap();
    assert_eq!(intensity(&f.world, bb), 1.0, "field reverted");
    assert_eq!(range(&f.world, bb), 9.0, "other overrides stay");
    // reverting a field that isn't overridden is an error and records nothing
    let depth = f.editor.undo.undo_len();
    assert!(prefab_ui::revert_field(&mut f.editor, &mut f.world, b, target, "Light", "intensity").is_err());
    assert_eq!(f.editor.undo.undo_len(), depth);

    assert!(f.editor.undo(&mut f.world));
    assert_eq!(intensity(&f.world, bb), 4.0);
    assert!(f.editor.redo(&mut f.world));
    assert_eq!(intensity(&f.world, bb), 1.0);

    prefab_ui::revert_all(&mut f.editor, &mut f.world, bb).unwrap(); // member resolves to its instance
    assert_eq!(range(&f.world, bb), 5.0);
    assert!(f.editor.undo(&mut f.world));
    assert_eq!(range(&f.world, bb), 9.0);
}

#[test]
fn unpack_detaches_and_undo_restores_the_instance() {
    let (mut f, _dir, _a, b) = prefab_fixture();
    let bb = bulb_of(&f.world, b);
    let before = f.world.len();
    prefab_ui::unpack(&mut f.editor, &mut f.world, b, false).unwrap();
    assert!(f.world.get::<&PrefabInstance>(b).is_err());
    assert!(f.world.get::<&PrefabMember>(bb).is_err());
    assert_eq!(f.world.len(), before);
    assert!(prefab_ui::unpack(&mut f.editor, &mut f.world, b, false).is_err(), "no longer an instance");

    assert!(f.editor.undo(&mut f.world));
    assert!(f.world.get::<&PrefabInstance>(b).is_ok());
    assert_eq!(prefab::members_of(&f.world, b).len(), 2);
    assert!(f.editor.redo(&mut f.world));
    assert!(f.world.get::<&PrefabInstance>(b).is_err());
}

#[test]
fn create_prefab_undo_removes_the_file_and_the_wrapper() {
    let (mut f, dir, _a, b) = prefab_fixture();
    let file = dir.path().join("prefabs/props/lamp2.prefab.ron");
    let before = f.world.len();
    // b is already an instance -> create wraps it
    let w = prefab_ui::create_prefab(&mut f.editor, &mut f.world, b, "props/lamp2").unwrap();
    assert_ne!(w, b, "wrapper root");
    assert!(file.exists());
    assert_eq!(f.world.len(), before + 1);
    assert_eq!(hierarchy::parent_of(&f.world, b), Some(w));
    // creating over an existing prefab is refused and leaves no trace
    let depth = f.editor.undo.undo_len();
    assert!(prefab_ui::create_prefab(&mut f.editor, &mut f.world, b, "props/lamp2").is_err());
    assert_eq!(f.editor.undo.undo_len(), depth);

    assert!(f.editor.undo(&mut f.world));
    assert!(!file.exists());
    assert!(!f.world.contains(w));
    assert_eq!(f.world.len(), before);
    assert_eq!(hierarchy::parent_of(&f.world, b), None);
    assert!(f.world.get::<&PrefabInstance>(b).is_ok(), "still an instance of the first prefab");
    assert!(f.editor.redo(&mut f.world));
    assert!(file.exists());
    assert_eq!(hierarchy::parent_of(&f.world, b), Some(w));
    assert!(f.world.get::<&PrefabInstance>(w).is_ok());
}

#[test]
fn instances_survive_scene_save_and_open_with_overrides() {
    let (mut f, dir, _a, b) = prefab_fixture();
    let bb = bulb_of(&f.world, b);
    set_intensity(&mut f, bb, 3.0);
    let path = dir.path().join("scenes/lamps.scene.ron");
    assert!(f.shell.save_scene(&mut f.editor, &mut f.world, &path));
    let mut g = Fixture::new();
    g.shell.open_scene(&mut g.editor, &mut g.world, &path);
    let roots = prefab::instance_roots(&g.world);
    assert_eq!(roots.len(), 2);
    let overridden = roots.iter().filter(|r| prefab::instance_of(&g.world, **r).unwrap().overrides.iter().any(|o| o.field == "intensity")).count();
    assert_eq!(overridden, 1, "override recorded at save time");
}

// ------------------------------------------------------------------ headless UI

#[test]
fn file_menu_and_prefab_ui_draw_without_panicking() {
    let (mut f, _dir, _a, b) = prefab_fixture();
    let bb = bulb_of(&f.world, b);
    set_intensity(&mut f, bb, 5.0);
    f.editor.selection.select_only(bb);

    let Fixture { ctx, shell, editor, world } = &mut f;
    for with_project in [true, false] {
        shell.set_project_dir(if with_project { editor.prefab_ui.root().map(|r| r.parent().unwrap().to_path_buf()) } else { None });
        editor.prefab_ui.set_root(shell.prefabs_dir());
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    shell.file_menu(ui, editor, world);
                    super::ui::draw_inspector(ui, editor, world);
                    super::ui::draw_hierarchy(ui, editor, world);
                });
            });
        }
        if with_project {
            assert!(editor.prefab_ui.field_override("Light", "intensity").is_some(), "inspector flagged the overridden field");
            assert!(editor.prefab_ui.field_override("Light", "range").is_none());
            assert!(!editor.prefab_ui.overrides_of_selected().is_empty());
        }
    }

    // name prompt + prompts draw too
    let root = lamp(world);
    editor.prefab_ui.naming = Some(prefab_ui::NameDialog { root, buffer: "Lamp.prefab.ron".into(), error: Some("boom".into()) });
    shell.request_new_scene(editor, world); // dirty -> confirm window
    assert!(shell.pending_confirm().is_some());
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        shell.ui(ctx, editor, world, &mut H);
        egui::CentralPanel::default().show(ctx, |ui| super::ui::draw_hierarchy(ui, editor, world));
    });
    assert!(editor.prefab_ui.naming.is_some(), "dialog stays open until answered");
}

#[test]
fn multi_selection_clears_the_override_cache() {
    let (mut f, _dir, a, b) = prefab_fixture();
    let bb = bulb_of(&f.world, b);
    set_intensity(&mut f, bb, 5.0);
    f.editor.selection.select_only(bb);
    let Fixture { ctx, editor, world, .. } = &mut f;
    let draw = |editor: &mut EditorState, world: &mut World| {
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| super::ui::draw_inspector(ui, editor, world));
        });
    };
    draw(editor, world);
    assert!(editor.prefab_ui.field_override("Light", "intensity").is_some());
    editor.selection.select_all([bb, a]);
    draw(editor, world);
    assert!(editor.prefab_ui.field_override("Light", "intensity").is_none());
}
