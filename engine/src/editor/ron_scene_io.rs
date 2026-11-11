//! [`RonSceneIo`]: the File-menu backend over [`SceneDocument`] and the
//! editor [`Registry`] (versioned RON scenes, unknown-component
//! preservation, dirty flag, autosave + crash recovery).
//!
//! * **Dirty** is `forced || undo_dirty`. The shell feeds `undo_dirty` from
//!   `UndoStack::is_dirty` (any executed command, undo or redo that leaves the
//!   stack away from its saved point), so undoing back to the saved state is
//!   clean again. `forced` is for states with no undo-stack counterpart: a
//!   recovered autosave, or [`RonSceneIo::mark_dirty`].
//! * **Legacy level files** (`level::Level` RON) open through
//!   [`level_compat`]; the scene remembers it came from one and
//!   [`SceneIo::save_confirmation`] asks before it is written in the scene
//!   format.
//! * **Autosave** goes to `<scene>.autosave` ([`scene::autosave_path`]); a
//!   clean save, New/Open of another scene or [`SceneIo::close_clean`] remove it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hecs::World;

use super::inspector::Registry;
use super::level_compat;
use super::prefab::{self, SharedLibrary};
use super::scene::{self, Recovery, Scene, SceneDocument};
use super::scene_io::SceneIo;
use crate::level::Level;

/// Where the open scene came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneOrigin {
    /// Created with File > New and not saved yet.
    New,
    /// A native scene file.
    Scene,
    /// A legacy `level::Level` file converted on load.
    LegacyLevel,
}

pub struct RonSceneIo {
    doc: SceneDocument,
    origin: SceneOrigin,
    forced_dirty: bool,
    undo_dirty: bool,
    prefabs: Option<SharedLibrary>,
    messages: Vec<String>,
    /// After a failed autosave, don't retry before this (no per-frame spam).
    autosave_retry_at: Option<Instant>,
}

impl Default for RonSceneIo {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads a scene file, falling back to the legacy level format.
/// `Scene`'s serde ignores unknown keys, so a level would "parse" as an empty
/// scene; `Level` has a required `objects` field and rejects scenes, so it is
/// tried first (same order as [`level_compat::load_scene_or_level`]).
pub fn read_scene_any(path: &Path) -> Result<(Scene, SceneOrigin), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    if let Ok(level) = ron::from_str::<Level>(&text) {
        let scene = level_compat::level_to_scene(&level).map_err(|e| format!("open {}: {e}", path.display()))?;
        return Ok((scene, SceneOrigin::LegacyLevel));
    }
    Scene::from_ron(&text).map(|s| (s, SceneOrigin::Scene)).map_err(|e| format!("open {}: {e}", path.display()))
}

fn sniff_origin(path: &Path) -> SceneOrigin {
    match std::fs::read_to_string(path) {
        Ok(t) if ron::from_str::<Level>(&t).is_ok() => SceneOrigin::LegacyLevel,
        _ => SceneOrigin::Scene,
    }
}

impl RonSceneIo {
    pub fn new() -> Self {
        Self {
            doc: SceneDocument::new("untitled"),
            origin: SceneOrigin::New,
            forced_dirty: false,
            undo_dirty: false,
            prefabs: None,
            messages: Vec::new(),
            autosave_retry_at: None,
        }
    }

    pub fn document(&self) -> &SceneDocument {
        &self.doc
    }
    pub fn document_mut(&mut self) -> &mut SceneDocument {
        &mut self.doc
    }
    pub fn origin(&self) -> SceneOrigin {
        self.origin
    }
    pub fn set_autosave_interval(&mut self, d: Duration) {
        self.doc.autosave_interval = d;
    }

    /// Marks the scene dirty for a change that isn't an undoable command.
    pub fn mark_dirty(&mut self) {
        self.forced_dirty = true;
        self.refresh();
    }

    fn refresh(&mut self) {
        self.doc.set_dirty(self.forced_dirty || self.undo_dirty);
    }

    fn refresh_overrides(&self, world: &mut World, reg: &Registry) {
        if let Some(lib) = &self.prefabs {
            prefab::update_overrides(world, reg, &mut lib.borrow_mut());
        }
    }

    /// Replaces the document after a successful load.
    fn install(&mut self, path: &Path, scene: Scene, origin: SceneOrigin, dirty: bool, warnings: Vec<String>) {
        let interval = self.doc.autosave_interval;
        let mut doc = SceneDocument::new(&scene.name);
        if doc.name.is_empty() {
            doc.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into());
        }
        doc.settings = scene.settings;
        doc.path = Some(path.to_path_buf());
        doc.autosave_interval = interval;
        self.doc = doc;
        self.origin = origin;
        self.forced_dirty = dirty;
        self.undo_dirty = false;
        self.autosave_retry_at = None;
        self.messages.extend(warnings);
        self.refresh();
    }

    fn load_from(
        &mut self,
        path: &Path,
        read: Result<(Scene, SceneOrigin), String>,
        origin_override: Option<SceneOrigin>,
        dirty: bool,
        world: &mut World,
        reg: &Registry,
    ) -> Result<(), String> {
        let (scene, origin) = read?;
        // The old document is going away: its autosave must not be offered later.
        self.doc.close_clean();
        let res = scene.load_replace(world, reg);
        self.install(path, scene, origin_override.unwrap_or(origin), dirty, res.report.warnings);
        Ok(())
    }
}

impl SceneIo for RonSceneIo {
    fn new_scene(&mut self, world: &mut World) -> Result<(), String> {
        self.new_scene_with(world, &scene::scene_registry())
    }

    fn open(&mut self, path: &Path, world: &mut World) -> Result<(), String> {
        self.open_with(path, world, &scene::scene_registry())
    }

    /// Saving needs the registry and a mutable world (entity ids are
    /// assigned on first save); use [`SceneIo::save_with`].
    fn save(&mut self, _world: &World, _path: &Path) -> Result<(), String> {
        Err("RonSceneIo saves through save_with (needs the editor registry and a mutable world)".into())
    }

    fn current_path(&self) -> Option<&Path> {
        self.doc.path.as_deref()
    }

    fn new_scene_with(&mut self, world: &mut World, _registry: &Registry) -> Result<(), String> {
        self.doc.close_clean();
        world.clear();
        let interval = self.doc.autosave_interval;
        self.doc = SceneDocument::new("untitled");
        self.doc.autosave_interval = interval;
        self.origin = SceneOrigin::New;
        self.forced_dirty = false;
        self.undo_dirty = false;
        self.autosave_retry_at = None;
        self.refresh();
        Ok(())
    }

    fn open_with(&mut self, path: &Path, world: &mut World, registry: &Registry) -> Result<(), String> {
        self.load_from(path, read_scene_any(path), None, false, world, registry)
    }

    fn save_with(&mut self, world: &mut World, registry: &Registry, path: &Path) -> Result<(), String> {
        self.refresh_overrides(world, registry);
        self.doc.save_as(path, world, registry).map_err(|e| format!("save {}: {e}", path.display()))?;
        self.origin = SceneOrigin::Scene;
        self.forced_dirty = false;
        self.autosave_retry_at = None;
        // The shell marks the undo stack saved right after; until then the
        // stack may still report dirty, so don't claim clean here.
        self.refresh();
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.forced_dirty || self.undo_dirty
    }

    fn sync_undo_dirty(&mut self, undo_dirty: bool) {
        self.undo_dirty = undo_dirty;
        self.refresh();
    }

    fn save_confirmation(&self, path: &Path) -> Option<String> {
        if self.origin != SceneOrigin::LegacyLevel {
            return None;
        }
        let src = self.doc.path.as_deref();
        Some(if src == Some(path) {
            format!(
                "{} is a legacy level file. Saving overwrites it with the new Scene format, which games that still load it as a level cannot read.",
                path.display()
            )
        } else {
            format!(
                "The open scene was converted from a legacy level file. {} will be written in the new Scene format.",
                path.display()
            )
        })
    }

    fn recovery_for(&self, path: &Path) -> Option<Recovery> {
        scene::detect_recovery(path)
    }

    fn open_recovery_with(&mut self, path: &Path, world: &mut World, registry: &Registry) -> Result<(), String> {
        let auto = scene::autosave_path(path);
        let read = Scene::load(&auto)
            .map(|s| (s, SceneOrigin::Scene))
            .map_err(|e| format!("recover {}: {e}", auto.display()));
        // The autosave is always scene-format; whether the real file is a
        // legacy level decides the save confirmation.
        let origin = if path.exists() { sniff_origin(path) } else { SceneOrigin::Scene };
        self.load_from(path, read, Some(origin), true, world, registry)
    }

    fn discard_autosave(&mut self, path: &Path) {
        scene::discard_autosave(path);
    }

    fn autosave_tick(&mut self, world: &mut World, registry: &Registry, now: Instant) -> Result<bool, String> {
        if self.autosave_retry_at.is_some_and(|t| now < t) || !self.doc.autosave_due(now) {
            return Ok(false);
        }
        self.refresh_overrides(world, registry);
        match self.doc.autosave_if_due(now, world, registry) {
            Ok(wrote) => {
                self.autosave_retry_at = None;
                Ok(wrote)
            }
            Err(e) => {
                self.autosave_retry_at = Some(now + self.doc.autosave_interval);
                Err(format!("autosave failed: {e}"))
            }
        }
    }

    fn close_clean(&mut self) {
        self.doc.close_clean();
    }

    fn set_prefab_library(&mut self, lib: Option<SharedLibrary>) {
        self.prefabs = lib;
    }

    fn take_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }
}

/// Resolves what the user typed in a path prompt: absolute paths and the
/// no-project case are taken as is; otherwise relative to `<project>/scenes`.
pub fn resolve_scene_path(project_dir: Option<&Path>, typed: &str) -> PathBuf {
    let p = PathBuf::from(typed.trim());
    match project_dir {
        Some(dir) if p.is_relative() => dir.join("scenes").join(p),
        _ => p,
    }
}
