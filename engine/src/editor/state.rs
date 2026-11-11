//! `EditorState`: everything the editor UI owns apart from the `World`.

use std::collections::HashSet;

use egui::{Key, Modifiers};
use hecs::{Entity, World};

use super::command::*;
use super::hierarchy::{self, is_locked, PrimitiveShape};
use super::inspector::Registry;
use super::selection::Selection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EditorMode {
    #[default]
    Edit,
    Play,
    Pause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Move,
    Rotate,
    Scale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeymapPreset {
    #[default]
    Unity,
    Blender,
}

impl KeymapPreset {
    pub const ALL: [KeymapPreset; 2] = [Self::Unity, Self::Blender];
    pub fn label(self) -> &'static str {
        match self {
            Self::Unity => "Unity-like",
            Self::Blender => "Blender-like",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorAction {
    Undo,
    Redo,
    Delete,
    Duplicate,
    SelectAll,
    DeselectAll,
    Rename,
    SetTool(Tool),
}

/// Maps a key press to an action for `preset`.
///
/// Unity-like: W/E/R tools, Ctrl+D duplicate, Delete, Ctrl+A, F2 rename.
/// Blender-like: G/R/S tools (grab/rotate/scale — selects the tool here, it
/// is not a modal transform), X or Delete, Shift+D, A, Alt+A deselect.
/// Both: Ctrl+Z undo, Ctrl+Shift+Z / Ctrl+Y redo.
pub fn resolve_key(preset: KeymapPreset, key: Key, m: Modifiers) -> Option<EditorAction> {
    let ctrl = m.ctrl || m.command;
    if ctrl && !m.alt {
        match (key, m.shift) {
            (Key::Z, false) => return Some(EditorAction::Undo),
            (Key::Z, true) | (Key::Y, false) => return Some(EditorAction::Redo),
            _ => {}
        }
    }
    if key == Key::F2 && !ctrl && !m.alt {
        return Some(EditorAction::Rename);
    }
    let bare = !ctrl && !m.alt && !m.shift;
    match preset {
        KeymapPreset::Unity => match key {
            Key::Delete if bare => Some(EditorAction::Delete),
            Key::D if ctrl && !m.alt && !m.shift => Some(EditorAction::Duplicate),
            Key::A if ctrl && !m.alt && !m.shift => Some(EditorAction::SelectAll),
            Key::W if bare => Some(EditorAction::SetTool(Tool::Move)),
            Key::E if bare => Some(EditorAction::SetTool(Tool::Rotate)),
            Key::R if bare => Some(EditorAction::SetTool(Tool::Scale)),
            _ => None,
        },
        KeymapPreset::Blender => match key {
            Key::X | Key::Delete if bare => Some(EditorAction::Delete),
            Key::D if m.shift && !ctrl && !m.alt => Some(EditorAction::Duplicate),
            Key::A if bare => Some(EditorAction::SelectAll),
            Key::A if m.alt && !ctrl && !m.shift => Some(EditorAction::DeselectAll),
            Key::G if bare => Some(EditorAction::SetTool(Tool::Move)),
            Key::R if bare => Some(EditorAction::SetTool(Tool::Rotate)),
            Key::S if bare => Some(EditorAction::SetTool(Tool::Scale)),
            _ => None,
        },
    }
}

/// In-progress rename in the hierarchy.
#[derive(Clone, Debug)]
pub struct RenameState {
    pub entity: Entity,
    pub buffer: String,
    pub focus_requested: bool,
}

pub struct EditorState {
    pub selection: Selection,
    pub undo: UndoStack,
    pub registry: Registry,
    pub mode: EditorMode,
    pub keymap: KeymapPreset,
    pub tool: Tool,
    /// Hierarchy search text (case-insensitive substring of the name).
    pub search: String,
    pub rename: Option<RenameState>,
    /// Hierarchy nodes the user collapsed (default: expanded).
    pub collapsed: HashSet<Entity>,
    /// "Add Component" popup filter text.
    pub add_component_filter: String,
    /// A merge-able edit is in flight while the pointer is held; seal when released.
    pub(crate) merge_pending: bool,
    /// Last command error, for a status bar / toast.
    pub last_error: Option<String>,
    /// Prefab library handle, name prompt and inspector override cache.
    pub prefab_ui: super::prefab_ui::PrefabUiState,
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorState {
    pub fn new() -> Self {
        Self::with_registry(Registry::with_defaults())
    }

    pub fn with_registry(registry: Registry) -> Self {
        Self {
            selection: Selection::default(),
            undo: UndoStack::default(),
            registry,
            mode: EditorMode::Edit,
            keymap: KeymapPreset::default(),
            tool: Tool::default(),
            search: String::new(),
            rename: None,
            collapsed: HashSet::new(),
            add_component_filter: String::new(),
            merge_pending: false,
            last_error: None,
            prefab_ui: Default::default(),
        }
    }

    // ---- play mode (state machine only; scene snapshot/restore is the host's job)

    pub fn play(&mut self) {
        if self.mode == EditorMode::Edit {
            self.mode = EditorMode::Play;
            self.undo.seal();
        }
    }
    /// Play <-> Pause. No-op in Edit.
    pub fn toggle_pause(&mut self) {
        self.mode = match self.mode {
            EditorMode::Play => EditorMode::Pause,
            EditorMode::Pause => EditorMode::Play,
            EditorMode::Edit => EditorMode::Edit,
        };
    }
    pub fn stop(&mut self) {
        self.mode = EditorMode::Edit;
        self.undo.seal();
    }
    pub fn is_editing(&self) -> bool {
        self.mode == EditorMode::Edit
    }

    // ---- commands

    /// Runs a command through the undo stack; errors are stored in `last_error`.
    pub fn exec(&mut self, world: &mut World, cmd: impl Command + 'static) -> Result<(), String> {
        let mut ctx = Ctx { world, registry: &self.registry };
        let r = self.undo.execute(Box::new(cmd), &mut ctx);
        self.last_error = r.as_ref().err().cloned();
        r
    }

    pub fn undo(&mut self, world: &mut World) -> bool {
        let mut ctx = Ctx { world, registry: &self.registry };
        let ok = self.undo.undo(&mut ctx);
        self.after_history_change(world);
        ok
    }

    pub fn redo(&mut self, world: &mut World) -> bool {
        let mut ctx = Ctx { world, registry: &self.registry };
        let ok = self.undo.redo(&mut ctx);
        self.after_history_change(world);
        ok
    }

    fn after_history_change(&mut self, world: &World) {
        self.selection.prune(world);
        if self.rename.as_ref().map(|r| !world.contains(r.entity)).unwrap_or(false) {
            self.rename = None;
        }
        self.merge_pending = false;
    }

    /// Runs `f` as one undo step named `label`.
    pub fn transaction<R>(&mut self, label: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        self.undo.begin_transaction(label);
        let r = f(self);
        self.undo.end_transaction();
        r
    }

    /// Creates an object (selecting it) and returns it.
    pub fn spawn(&mut self, world: &mut World, spec: SpawnSpec) -> Option<Entity> {
        let (cmd, handle) = SpawnCommand::new(spec);
        self.exec(world, cmd).ok()?;
        let e = handle.get()?;
        self.selection.select_only(e);
        Some(e)
    }

    pub fn create_empty(&mut self, world: &mut World, parent: Option<Entity>) -> Option<Entity> {
        self.spawn(world, SpawnSpec::empty("GameObject", parent))
    }

    pub fn create_primitive(
        &mut self,
        world: &mut World,
        shape: PrimitiveShape,
        parent: Option<Entity>,
    ) -> Option<Entity> {
        self.spawn(world, SpawnSpec::primitive(shape, parent))
    }

    /// Deletes selected entities (and their subtrees) as one undo step.
    /// Locked entities are skipped. Returns how many roots were deleted.
    pub fn delete_selection(&mut self, world: &mut World) -> usize {
        let targets: Vec<Entity> = self
            .selection
            .top_level(world)
            .into_iter()
            .filter(|e| !is_locked(world, *e))
            .collect();
        if targets.is_empty() {
            return 0;
        }
        let label = if targets.len() == 1 {
            format!("Delete {}", hierarchy::display_name(world, targets[0]))
        } else {
            format!("Delete {} objects", targets.len())
        };
        let n = self.transaction(&label, |s| {
            targets.iter().filter(|e| s.exec(world, DeleteCommand::new(**e)).is_ok()).count()
        });
        self.selection.prune(world);
        n
    }

    /// Duplicates selected entities as one undo step and selects the copies.
    pub fn duplicate_selection(&mut self, world: &mut World) -> Vec<Entity> {
        let targets = self.selection.top_level(world);
        if targets.is_empty() {
            return Vec::new();
        }
        let copies = self.transaction("Duplicate", |s| {
            let mut out = Vec::new();
            for t in &targets {
                let (cmd, h) = DuplicateCommand::new(*t);
                if s.exec(world, cmd).is_ok() {
                    out.extend(h.get());
                }
            }
            out
        });
        if !copies.is_empty() {
            self.selection.select_all(copies.iter().copied());
        }
        copies
    }

    pub fn select_all(&mut self, world: &World) {
        let all: Vec<Entity> = world.iter().map(|e| e.entity()).collect();
        // Use hierarchy order so "primary" is the last visible row.
        let mut ordered = Vec::new();
        for r in hierarchy::roots(world) {
            ordered.extend(hierarchy::subtree(world, r));
        }
        debug_assert_eq!(ordered.len(), all.len());
        self.selection.select_all(ordered);
    }

    pub fn begin_rename(&mut self, world: &World, e: Entity) {
        if world.contains(e) && !is_locked(world, e) {
            self.rename = Some(RenameState {
                entity: e,
                buffer: hierarchy::display_name(world, e),
                focus_requested: false,
            });
        }
    }

    /// Applies a keymap action. Returns false if it was a no-op.
    pub fn perform(&mut self, world: &mut World, action: EditorAction) -> bool {
        match action {
            EditorAction::Undo => self.undo(world),
            EditorAction::Redo => self.redo(world),
            EditorAction::Delete => self.delete_selection(world) > 0,
            EditorAction::Duplicate => !self.duplicate_selection(world).is_empty(),
            EditorAction::SelectAll => {
                self.select_all(world);
                true
            }
            EditorAction::DeselectAll => {
                self.selection.clear();
                true
            }
            EditorAction::Rename => match self.selection.primary() {
                Some(p) => {
                    self.begin_rename(world, p);
                    self.rename.is_some()
                }
                None => false,
            },
            EditorAction::SetTool(t) => {
                self.tool = t;
                true
            }
        }
    }

    /// Resolves `key` with the current preset and performs it.
    /// Returns the action if one matched.
    pub fn handle_key(&mut self, world: &mut World, key: Key, mods: Modifiers) -> Option<EditorAction> {
        let a = resolve_key(self.keymap, key, mods)?;
        self.perform(world, a);
        Some(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::hierarchy::*;

    const NONE: Modifiers = Modifiers::NONE;

    fn ctrl() -> Modifiers {
        Modifiers::CTRL
    }
    fn ctrl_shift() -> Modifiers {
        Modifiers { ctrl: true, shift: true, ..Modifiers::NONE }
    }
    fn shift() -> Modifiers {
        Modifiers::SHIFT
    }

    #[test]
    fn unity_keymap() {
        use KeymapPreset::Unity as U;
        assert_eq!(resolve_key(U, Key::W, NONE), Some(EditorAction::SetTool(Tool::Move)));
        assert_eq!(resolve_key(U, Key::E, NONE), Some(EditorAction::SetTool(Tool::Rotate)));
        assert_eq!(resolve_key(U, Key::R, NONE), Some(EditorAction::SetTool(Tool::Scale)));
        assert_eq!(resolve_key(U, Key::D, ctrl()), Some(EditorAction::Duplicate));
        assert_eq!(resolve_key(U, Key::Delete, NONE), Some(EditorAction::Delete));
        assert_eq!(resolve_key(U, Key::A, ctrl()), Some(EditorAction::SelectAll));
        assert_eq!(resolve_key(U, Key::F2, NONE), Some(EditorAction::Rename));
        // Blender keys do nothing here
        assert_eq!(resolve_key(U, Key::X, NONE), None);
        assert_eq!(resolve_key(U, Key::G, NONE), None);
        assert_eq!(resolve_key(U, Key::A, NONE), None);
        assert_eq!(resolve_key(U, Key::D, shift()), None);
    }

    #[test]
    fn blender_keymap() {
        use KeymapPreset::Blender as B;
        assert_eq!(resolve_key(B, Key::G, NONE), Some(EditorAction::SetTool(Tool::Move)));
        assert_eq!(resolve_key(B, Key::R, NONE), Some(EditorAction::SetTool(Tool::Rotate)));
        assert_eq!(resolve_key(B, Key::S, NONE), Some(EditorAction::SetTool(Tool::Scale)));
        assert_eq!(resolve_key(B, Key::X, NONE), Some(EditorAction::Delete));
        assert_eq!(resolve_key(B, Key::Delete, NONE), Some(EditorAction::Delete));
        assert_eq!(resolve_key(B, Key::D, shift()), Some(EditorAction::Duplicate));
        assert_eq!(resolve_key(B, Key::A, NONE), Some(EditorAction::SelectAll));
        assert_eq!(resolve_key(B, Key::A, Modifiers::ALT), Some(EditorAction::DeselectAll));
        assert_eq!(resolve_key(B, Key::D, ctrl()), None);
        assert_eq!(resolve_key(B, Key::W, NONE), None);
        // Ctrl+S must not trigger "scale"
        assert_eq!(resolve_key(B, Key::S, ctrl()), None);
    }

    #[test]
    fn undo_redo_keys_work_in_both_presets() {
        for p in KeymapPreset::ALL {
            assert_eq!(resolve_key(p, Key::Z, ctrl()), Some(EditorAction::Undo));
            assert_eq!(resolve_key(p, Key::Z, ctrl_shift()), Some(EditorAction::Redo));
            assert_eq!(resolve_key(p, Key::Y, ctrl()), Some(EditorAction::Redo));
            assert_eq!(resolve_key(p, Key::Z, NONE), None);
        }
        assert_eq!(resolve_key(KeymapPreset::Unity, Key::Z, Modifiers::COMMAND), Some(EditorAction::Undo));
    }

    #[test]
    fn mode_state_machine() {
        let mut s = EditorState::new();
        assert!(s.is_editing());
        s.toggle_pause();
        assert_eq!(s.mode, EditorMode::Edit, "pause is a no-op in edit mode");
        s.play();
        assert_eq!(s.mode, EditorMode::Play);
        s.toggle_pause();
        assert_eq!(s.mode, EditorMode::Pause);
        s.toggle_pause();
        assert_eq!(s.mode, EditorMode::Play);
        s.stop();
        assert_eq!(s.mode, EditorMode::Edit);
    }

    #[test]
    fn spawn_selects_and_ctrl_z_removes() {
        let mut w = World::new();
        let mut s = EditorState::new();
        let e = s.create_primitive(&mut w, PrimitiveShape::Sphere, None).unwrap();
        assert_eq!(s.selection.primary(), Some(e));
        assert_eq!(w.get::<&Primitive>(e).unwrap().0, PrimitiveShape::Sphere);
        assert_eq!(s.handle_key(&mut w, Key::Z, ctrl()), Some(EditorAction::Undo));
        assert!(!w.contains(e));
        assert!(s.selection.is_empty(), "selection pruned");
        s.handle_key(&mut w, Key::Z, ctrl_shift());
        assert!(w.contains(e));
    }

    #[test]
    fn delete_selection_is_one_undo_step_and_skips_nested_and_locked() {
        let mut w = World::new();
        let mut s = EditorState::new();
        let a = s.create_empty(&mut w, None).unwrap();
        let b = s.create_empty(&mut w, Some(a)).unwrap();
        let c = s.create_empty(&mut w, None).unwrap();
        let locked = s.create_empty(&mut w, None).unwrap();
        w.insert_one(locked, Locked(true)).unwrap();
        for e in [a, b, c, locked] {
            s.selection.add(e);
        }
        let before = s.undo.undo_len();
        assert_eq!(s.delete_selection(&mut w), 2); // a (with b inside) and c
        assert_eq!(s.undo.undo_len(), before + 1);
        assert!(!w.contains(a) && !w.contains(b) && !w.contains(c) && w.contains(locked));
        s.undo(&mut w);
        assert!(w.contains(a) && w.contains(b) && w.contains(c));
        assert_eq!(children(&w, a), vec![b]);
    }

    #[test]
    fn blender_x_and_shift_d_drive_selection() {
        let mut w = World::new();
        let mut s = EditorState::new();
        s.keymap = KeymapPreset::Blender;
        let a = s.create_empty(&mut w, None).unwrap();
        s.handle_key(&mut w, Key::D, shift());
        assert_eq!(w.len(), 2);
        let copy = s.selection.primary().unwrap();
        assert_ne!(copy, a, "copy becomes the selection");
        s.handle_key(&mut w, Key::X, NONE);
        assert_eq!(w.len(), 1);
        assert!(w.contains(a));
        s.handle_key(&mut w, Key::A, NONE);
        assert!(s.selection.contains(a));
    }

    #[test]
    fn duplicate_multi_select_is_one_step() {
        let mut w = World::new();
        let mut s = EditorState::new();
        let a = s.create_empty(&mut w, None).unwrap();
        let b = s.create_empty(&mut w, None).unwrap();
        s.selection.add(a);
        s.selection.add(b);
        let n = s.undo.undo_len();
        let copies = s.duplicate_selection(&mut w);
        assert_eq!(copies.len(), 2);
        assert_eq!(s.undo.undo_len(), n + 1);
        assert_eq!(s.selection.len(), 2);
        s.undo(&mut w);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn rename_action_starts_rename_for_primary() {
        let mut w = World::new();
        let mut s = EditorState::new();
        assert!(!s.perform(&mut w, EditorAction::Rename));
        let e = s.create_empty(&mut w, None).unwrap();
        s.handle_key(&mut w, Key::F2, NONE);
        let r = s.rename.as_ref().unwrap();
        assert_eq!((r.entity, r.buffer.as_str()), (e, "GameObject"));
    }

    #[test]
    fn dirty_flag_follows_edits() {
        let mut w = World::new();
        let mut s = EditorState::new();
        assert!(!s.undo.is_dirty());
        s.create_empty(&mut w, None);
        assert!(s.undo.is_dirty());
        s.undo.mark_saved();
        assert!(!s.undo.is_dirty());
    }

    #[test]
    fn select_all_covers_every_entity() {
        let mut w = World::new();
        let mut s = EditorState::new();
        let a = s.create_empty(&mut w, None).unwrap();
        s.create_empty(&mut w, Some(a));
        s.create_empty(&mut w, None);
        s.perform(&mut w, EditorAction::SelectAll);
        assert_eq!(s.selection.len(), 3);
        s.perform(&mut w, EditorAction::DeselectAll);
        assert!(s.selection.is_empty());
    }
}
