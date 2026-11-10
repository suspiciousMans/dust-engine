//! Designer document state and editing operations (no egui): selection,
//! add/delete/duplicate, drag move/resize, anchor presets, undo/redo,
//! save/load and hot reload.
//!
//! Undo is a plain stack of whole-document snapshots (`UiDoc: Clone`). The
//! editor-core Command stack can adapt by wrapping [`DesignerState::snapshot`]
//! / [`DesignerState::restore`]: a command stores the before/after snapshots.

use std::path::{Path, PathBuf};

use super::bindings::UiBindings;
use super::focus::UiFocus;
use super::files::{save_doc, UiDocWatcher};
use super::layout::{apply_anchor_preset, AnchorAxis, LayoutResult, Rect};
use super::model::*;

/// Preview resolutions offered by the designer (name, width, height).
pub const RESOLUTIONS: &[(&str, [f32; 2])] = &[
    ("1920×1080 (16:9)", [1920.0, 1080.0]),
    ("1280×720 (16:9)", [1280.0, 720.0]),
    ("2560×1440 (16:9)", [2560.0, 1440.0]),
    ("2560×1080 (21:9)", [2560.0, 1080.0]),
    ("1024×768 (4:3)", [1024.0, 768.0]),
    ("800×600 (4:3)", [800.0, 600.0]),
    ("1280×800 (16:10)", [1280.0, 800.0]),
    ("1080×1920 (portrait)", [1080.0, 1920.0]),
];

const MAX_UNDO: usize = 200;

/// The eight resize handles around the selected node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    pub const ALL: [Handle; 8] = [Handle::NW, Handle::N, Handle::NE, Handle::W, Handle::E, Handle::SW, Handle::S, Handle::SE];

    /// (moves left edge, moves right edge, moves top edge, moves bottom edge)
    fn edges(self) -> (bool, bool, bool, bool) {
        match self {
            Handle::N => (false, false, true, false),
            Handle::S => (false, false, false, true),
            Handle::E => (false, true, false, false),
            Handle::W => (true, false, false, false),
            Handle::NE => (false, true, true, false),
            Handle::NW => (true, false, true, false),
            Handle::SE => (false, true, false, true),
            Handle::SW => (true, false, false, true),
        }
    }

    /// Position of the handle on a rect, as fractions (0, 0.5, 1).
    pub fn fraction(self) -> [f32; 2] {
        let (l, r, t, b) = self.edges();
        let x = if l { 0.0 } else if r { 1.0 } else { 0.5 };
        let y = if t { 0.0 } else if b { 1.0 } else { 0.5 };
        [x, y]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragMode {
    Move,
    Resize(Handle),
}

/// An in-progress drag: the node as it was at the start plus what we need to
/// turn screen deltas into design-unit deltas.
#[derive(Clone, Debug)]
struct DragState {
    id: NodeId,
    mode: DragMode,
    start: UiNode,
    start_rect: Rect, // design units
    scale: f32,
}

/// Result of checking the watched file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadOutcome {
    Nothing,
    Reloaded,
    /// File changed on disk but we have unsaved edits; nothing was replaced.
    Conflict,
    Failed(String),
}

pub struct DesignerState {
    pub doc: UiDoc,
    pub selected: Option<NodeId>,
    /// Index into [`RESOLUTIONS`], or `None` to use `custom_resolution`.
    pub resolution: Option<usize>,
    pub custom_resolution: [f32; 2],
    /// Canvas zoom multiplier on top of "fit to window".
    pub zoom: f32,
    /// Extra user UI scale applied in the preview.
    pub ui_scale: f32,
    /// Show/simulate a 5% safe-area border in the preview.
    pub preview_safe_area: bool,
    /// Canvas acts like the running game: hover/click fire events into `event_log`.
    pub preview_interactive: bool,
    /// Round dragged offsets to multiples of this (0 = off).
    pub snap: f32,
    /// Sample values for bindings in the preview.
    pub bindings: UiBindings,
    pub status: String,
    pub event_log: Vec<String>,
    pub path_text: String,
    pub dirty: bool,
    /// Style class being edited in the style editor.
    pub style_class: Option<String>,
    /// Text buffers for the "add" boxes in the panel.
    pub new_class_name: String,
    pub new_binding_name: String,
    /// Focus cursor used by the interactive preview (arrow keys / Tab / Enter).
    pub preview_focus: UiFocus,
    /// Panel bookkeeping: last hot-reload poll time (seconds) and canvas drag start.
    pub last_poll: f64,
    pub canvas_drag_start: Option<[f32; 2]>,
    /// Where the canvas drew the viewport last frame: (screen origin, pixels per point).
    pub canvas_view: Option<([f32; 2], f32)>,

    undo: Vec<UiDoc>,
    redo: Vec<UiDoc>,
    edit_group_open: bool,
    drag: Option<DragState>,
    watcher: Option<UiDocWatcher>,
}

impl Default for DesignerState {
    fn default() -> Self {
        DesignerState::new(UiDoc::default())
    }
}

impl DesignerState {
    pub fn new(doc: UiDoc) -> Self {
        DesignerState {
            doc,
            selected: None,
            resolution: Some(0),
            custom_resolution: [1280.0, 720.0],
            zoom: 1.0,
            ui_scale: 1.0,
            preview_safe_area: false,
            preview_interactive: false,
            snap: 0.0,
            bindings: UiBindings::new(),
            status: String::new(),
            event_log: Vec::new(),
            path_text: "ui/untitled.ron".into(),
            dirty: false,
            style_class: None,
            new_class_name: String::new(),
            new_binding_name: String::new(),
            preview_focus: UiFocus::new(),
            last_poll: 0.0,
            canvas_drag_start: None,
            canvas_view: None,
            undo: Vec::new(),
            redo: Vec::new(),
            edit_group_open: false,
            drag: None,
            watcher: None,
        }
    }

    pub fn viewport(&self) -> [f32; 2] {
        match self.resolution {
            Some(i) => RESOLUTIONS[i.min(RESOLUTIONS.len() - 1)].1,
            None => self.custom_resolution,
        }
    }

    /// Simulated safe-area insets for the preview, in points.
    pub fn safe_area(&self) -> Insets {
        if !self.preview_safe_area {
            return Insets::ZERO;
        }
        let [w, h] = self.viewport();
        Insets::new(w * 0.05, h * 0.05, w * 0.05, h * 0.05)
    }

    // ---- snapshots / undo ----------------------------------------------------

    /// A clonable copy of the whole document (for external command stacks).
    pub fn snapshot(&self) -> UiDoc {
        self.doc.clone()
    }

    /// Replace the document with `doc` without touching undo history
    /// (the caller owns that). Drops the selection if the node vanished.
    pub fn restore(&mut self, doc: UiDoc) {
        self.doc = doc;
        if self.selected.map_or(false, |id| self.doc.find(id).is_none()) {
            self.selected = None;
        }
        self.drag = None;
        self.dirty = true;
    }

    /// Remember the current document as an undo step.
    pub fn checkpoint(&mut self) {
        self.push_undo(self.doc.clone());
        self.dirty = true;
    }

    fn push_undo(&mut self, snapshot: UiDoc) {
        self.undo.push(snapshot);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        let current = std::mem::replace(&mut self.doc, prev);
        self.redo.push(current);
        self.after_history_jump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        let current = std::mem::replace(&mut self.doc, next);
        self.undo.push(current);
        self.after_history_jump();
        true
    }

    fn after_history_jump(&mut self) {
        if self.selected.map_or(false, |id| self.doc.find(id).is_none()) {
            self.selected = None;
        }
        self.drag = None;
        self.edit_group_open = false;
        self.dirty = true;
    }

    /// For UI code that edits `self.doc` directly (inspector widgets): call
    /// with a clone taken *before* the edits. Edits made while the pointer is
    /// held (dragging a number field) merge into a single undo step.
    pub fn record_change(&mut self, before: UiDoc, pointer_down: bool) {
        if !(self.edit_group_open && pointer_down) {
            self.push_undo(before);
        }
        self.edit_group_open = pointer_down;
        self.dirty = true;
    }

    /// Call on frames with no edit so the next edit starts a new undo step.
    pub fn end_edit_group_if_idle(&mut self, pointer_down: bool) {
        if !pointer_down {
            self.edit_group_open = false;
        }
    }

    // ---- structure edits -----------------------------------------------------

    pub fn select(&mut self, id: Option<NodeId>) {
        self.selected = id.filter(|&i| self.doc.find(i).is_some());
    }

    /// Where a new node should go: inside the selection if it is a
    /// container, else next to it, else under the root.
    fn insertion_parent(&self) -> (NodeId, Option<usize>) {
        if let Some(sel) = self.selected.and_then(|s| self.doc.find(s)) {
            if sel.kind.is_container() {
                return (sel.id, None);
            }
            if let (Some(parent), Some(index)) = (self.doc.parent_of(sel.id), self.doc.index_in_parent(sel.id)) {
                return (parent, Some(index + 1));
            }
        }
        (self.doc.root.id, None)
    }

    /// Add a default node of the named kind (see [`NodeKind::ALL_NAMES`]).
    pub fn add_node(&mut self, kind_name: &str) -> Option<NodeId> {
        let kind = NodeKind::default_for(kind_name)?;
        let (parent, index) = self.insertion_parent();
        self.checkpoint();
        let id = self.doc.insert(parent, index, UiNode::new_default(kind))?;
        self.selected = Some(id);
        Some(id)
    }

    pub fn delete_selected(&mut self) -> bool {
        let Some(id) = self.selected else { return false };
        if id == self.doc.root.id {
            self.status = "The root node can't be deleted".into();
            return false;
        }
        // Select a neighbour so keyboard workflows continue smoothly.
        let parent = self.doc.parent_of(id);
        let index = self.doc.index_in_parent(id).unwrap_or(0);
        self.checkpoint();
        self.doc.remove(id);
        self.selected = parent.and_then(|p| {
            let kids = &self.doc.find(p)?.children;
            kids.get(index).or_else(|| kids.last()).map(|n| n.id).or(Some(p))
        });
        true
    }

    pub fn duplicate_selected(&mut self) -> Option<NodeId> {
        let id = self.selected?;
        if id == self.doc.root.id {
            return None;
        }
        self.checkpoint();
        let copy = self.doc.duplicate(id)?;
        self.selected = Some(copy);
        Some(copy)
    }

    /// Move the selection up (-1) / down (+1) among its siblings (changes draw/stack order).
    pub fn reorder_selected(&mut self, delta: i32) -> bool {
        let Some(id) = self.selected else { return false };
        let before = self.doc.clone();
        if self.doc.reorder(id, delta) {
            self.push_undo(before);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn reparent_selected(&mut self, new_parent: NodeId) -> bool {
        let Some(id) = self.selected else { return false };
        let before = self.doc.clone();
        if self.doc.reparent(id, new_parent) {
            self.push_undo(before);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    // ---- placement edits (need a solved layout) --------------------------------

    /// Is the node positioned by anchors (true) or by its parent's stack/grid?
    pub fn is_free_positioned(&self, id: NodeId) -> bool {
        match self.doc.parent_of(id).and_then(|p| self.doc.find(p)) {
            Some(parent) => !parent.kind.controls_child_layout(),
            None => true, // root
        }
    }

    /// Re-anchor the selection to a preset, keeping its on-screen rect.
    pub fn apply_preset(&mut self, h: AnchorAxis, v: AnchorAxis, layout: &LayoutResult) -> bool {
        let Some(id) = self.selected else { return false };
        if !self.is_free_positioned(id) {
            self.status = "Anchors are ignored: this node is positioned by its stack/grid parent".into();
            return false;
        }
        let Some(entry) = layout.entry(id) else { return false };
        let parent_content = match entry.parent.and_then(|p| layout.entry(p)) {
            Some(p) => p.content,
            None => Rect::new(0.0, 0.0, layout.viewport[0], layout.viewport[1]),
        };
        let inv = 1.0 / layout.scale;
        let (rect, parent_rect) = (entry.rect.scaled(inv), parent_content.scaled(inv));
        self.checkpoint();
        match self.doc.find_mut(id) {
            Some(node) => {
                apply_anchor_preset(node, h, v, rect, parent_rect);
                true
            }
            None => false,
        }
    }

    /// Start dragging the selected node. One undo step covers the whole drag.
    pub fn begin_drag(&mut self, mode: DragMode, layout: &LayoutResult) -> bool {
        let Some(id) = self.selected else { return false };
        if !self.is_free_positioned(id) {
            self.status = "This node is positioned by its stack/grid parent — move it in the outline instead".into();
            return false;
        }
        let (Some(node), Some(entry)) = (self.doc.find(id), layout.entry(id)) else { return false };
        let drag = DragState { id, mode, start: node.clone(), start_rect: entry.rect.scaled(1.0 / layout.scale), scale: layout.scale };
        self.checkpoint();
        self.drag = Some(drag);
        true
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Apply the *total* pointer movement since `begin_drag`, in screen
    /// points at 1:1 zoom (the canvas divides by its zoom first).
    pub fn update_drag(&mut self, total_delta: [f32; 2]) {
        let Some(drag) = self.drag.clone() else { return };
        let snap = |v: f32| if self.snap > 0.0 { (v / self.snap).round() * self.snap } else { v };
        let d = [snap(total_delta[0] / drag.scale), snap(total_delta[1] / drag.scale)];
        let mut node = drag.start.clone();
        match drag.mode {
            DragMode::Move => {
                for a in 0..2 {
                    node.offset_min[a] += d[a];
                    node.offset_max[a] += d[a];
                }
            }
            DragMode::Resize(handle) => {
                let (left, right, top, bottom) = handle.edges();
                if left {
                    resize_edge(&mut node, 0, false, d[0], drag.start_rect.w);
                }
                if right {
                    resize_edge(&mut node, 0, true, d[0], drag.start_rect.w);
                }
                if top {
                    resize_edge(&mut node, 1, false, d[1], drag.start_rect.h);
                }
                if bottom {
                    resize_edge(&mut node, 1, true, d[1], drag.start_rect.h);
                }
            }
        }
        if let Some(target) = self.doc.find_mut(drag.id) {
            copy_placement(&node, target);
        }
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// Abort the drag and put the node back.
    pub fn cancel_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            if let Some(target) = self.doc.find_mut(drag.id) {
                copy_placement(&drag.start, target);
            }
            // Drop the checkpoint begin_drag made.
            self.undo.pop();
        }
    }

    // ---- files -----------------------------------------------------------------

    pub fn save(&mut self) -> Result<(), String> {
        let path = PathBuf::from(self.path_text.trim());
        self.save_as(&path)
    }

    pub fn save_as(&mut self, path: &Path) -> Result<(), String> {
        let result = save_doc(&self.doc, path);
        match &result {
            Ok(()) => {
                self.path_text = path.display().to_string();
                self.watcher = Some(UiDocWatcher::watch(path));
                self.dirty = false;
                self.status = format!("Saved {}", path.display());
            }
            Err(e) => self.status = e.clone(),
        }
        result
    }

    /// Load a file as a new document. Undoable (the old document is kept as an undo step).
    pub fn load(&mut self, path: &Path) -> Result<(), String> {
        match UiDocWatcher::open(path) {
            Ok((watcher, doc)) => {
                self.push_undo(self.doc.clone());
                self.doc = doc;
                self.selected = None;
                self.path_text = path.display().to_string();
                self.watcher = Some(watcher);
                self.dirty = false;
                self.status = format!("Loaded {}", path.display());
                Ok(())
            }
            Err(e) => {
                self.status = e.clone();
                Err(e)
            }
        }
    }

    /// Hot reload: if the watched file changed on disk, load it — unless
    /// there are unsaved edits, which are never overwritten silently.
    pub fn check_reload(&mut self) -> ReloadOutcome {
        let Some(watcher) = &mut self.watcher else { return ReloadOutcome::Nothing };
        if self.dirty {
            if watcher.changed() {
                watcher.mark_seen();
                self.status = "File changed on disk, but you have unsaved edits — Save to overwrite or Load to discard yours".into();
                return ReloadOutcome::Conflict;
            }
            return ReloadOutcome::Nothing;
        }
        match watcher.poll() {
            None => ReloadOutcome::Nothing,
            Some(Ok(doc)) => {
                self.push_undo(self.doc.clone());
                self.doc = doc;
                if self.selected.map_or(false, |id| self.doc.find(id).is_none()) {
                    self.selected = None;
                }
                self.dirty = false;
                self.status = "Reloaded from disk".into();
                ReloadOutcome::Reloaded
            }
            Some(Err(e)) => {
                self.status = format!("Reload failed: {e}");
                ReloadOutcome::Failed(e)
            }
        }
    }
}

/// Copy only the placement fields (anchors, offsets, pivot, size modes).
fn copy_placement(from: &UiNode, to: &mut UiNode) {
    to.anchor_min = from.anchor_min;
    to.anchor_max = from.anchor_max;
    to.offset_min = from.offset_min;
    to.offset_max = from.offset_max;
    to.pivot = from.pivot;
    to.width = from.width;
    to.height = from.height;
}

/// Move one edge of a node by `d` design units along `axis`, keeping the
/// opposite edge fixed on screen. `cur_size` is the node's current size on
/// that axis (needed to turn `Fit` into a fixed size).
///
/// * `Fill` (stretch): the edge's offset changes.
/// * `Fixed`/`Fit`: becomes `Fixed(size ± d)`; the offset shifts by `d × (1 − pivot)`
///   (start edge) or `d × pivot` (end edge) so the other edge doesn't move.
/// Sizes never go below 1 unit.
pub fn resize_edge(node: &mut UiNode, axis: usize, end: bool, d: f32, cur_size: f32) {
    match node.size_mode(axis) {
        SizeMode::Fill => {
            if end {
                node.offset_max[axis] += d;
            } else {
                node.offset_min[axis] += d;
            }
        }
        mode => {
            let size = match mode {
                SizeMode::Fixed(s) => s,
                _ => cur_size,
            };
            let new_size = if end { size + d } else { size - d };
            let d = if new_size < 1.0 { if end { 1.0 - size } else { size - 1.0 } } else { d };
            let new_size = new_size.max(1.0);
            let pivot = node.pivot[axis];
            node.offset_min[axis] += if end { d * pivot } else { d * (1.0 - pivot) };
            if axis == 0 { node.width = SizeMode::Fixed(new_size) } else { node.height = SizeMode::Fixed(new_size) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::files::temp_path;
    use crate::ui_designer::layout::{solve, LayoutParams};
    use std::time::{Duration, SystemTime};

    fn state_with_panel() -> (DesignerState, NodeId) {
        let mut s = DesignerState::default();
        s.doc.root.name = "root".into();
        let id = s.add_node("Panel").unwrap();
        s.doc.find_mut(id).unwrap().name = "p".into();
        (s, id)
    }

    fn lay(s: &DesignerState) -> LayoutResult {
        solve(&s.doc, &LayoutParams::new([800.0, 600.0]))
    }

    fn rect(s: &DesignerState, id: NodeId) -> Rect {
        lay(s).rect(id).unwrap()
    }

    fn approx(a: Rect, x: f32, y: f32, w: f32, h: f32) {
        let ok = |p: f32, q: f32| (p - q).abs() < 1e-3;
        assert!(ok(a.x, x) && ok(a.y, y) && ok(a.w, w) && ok(a.h, h), "expected ({x},{y},{w},{h}) got {a:?}");
    }

    #[test]
    fn add_goes_inside_selected_container_else_next_to_it() {
        let (mut s, panel) = state_with_panel();
        let label = s.add_node("Label").unwrap();
        assert_eq!(s.doc.parent_of(label), Some(panel), "panel was selected → child");
        let button = s.add_node("Button").unwrap();
        assert_eq!(s.doc.parent_of(button), Some(panel), "label selected → sibling after it");
        assert_eq!(s.doc.index_in_parent(button), Some(1));
        assert_eq!(s.selected, Some(button));
        assert!(s.add_node("Nonsense").is_none());
    }

    #[test]
    fn delete_duplicate_and_root_protection() {
        let (mut s, panel) = state_with_panel();
        let copy = s.duplicate_selected().unwrap();
        assert_ne!(copy, panel);
        assert_eq!(s.selected, Some(copy));
        assert!(s.delete_selected());
        assert!(s.doc.find(copy).is_none());
        assert_eq!(s.selected, Some(panel), "selection moves to a neighbour");
        s.select(Some(s.doc.root.id));
        assert!(!s.delete_selected());
        assert!(s.duplicate_selected().is_none());
    }

    #[test]
    fn undo_redo_walks_history_and_clears_redo_on_new_edit() {
        let (mut s, _) = state_with_panel();
        let after_panel = s.snapshot();
        s.add_node("Label");
        assert!(s.can_undo());
        assert!(s.undo());
        assert_eq!(s.doc, after_panel);
        assert!(s.can_redo());
        assert!(s.redo());
        assert_eq!(s.doc.root.children[0].children.len(), 1);
        s.undo();
        s.add_node("Button"); // new edit invalidates redo
        assert!(!s.can_redo());
        while s.undo() {}
        assert_eq!(s.doc.root.children.len(), 0, "all the way back to the empty doc");
        assert!(!s.undo());
    }

    #[test]
    fn undo_drops_selection_of_vanished_node() {
        let (mut s, _) = state_with_panel();
        let label = s.add_node("Label").unwrap();
        s.undo();
        assert_ne!(s.selected, Some(label));
        assert!(s.selected.map_or(true, |id| s.doc.find(id).is_some()));
    }

    #[test]
    fn record_change_merges_while_pointer_held() {
        let (mut s, panel) = state_with_panel();
        let depth = s.undo_depth();
        for i in 0..5 {
            let before = s.snapshot();
            s.doc.find_mut(panel).unwrap().opacity = 0.1 * i as f32;
            s.record_change(before, true);
        }
        assert_eq!(s.undo_depth(), depth + 1, "one drag = one undo step");
        s.end_edit_group_if_idle(false);
        let before = s.snapshot();
        s.doc.find_mut(panel).unwrap().opacity = 0.9;
        s.record_change(before, false);
        assert_eq!(s.undo_depth(), depth + 2);
        s.undo();
        assert!((s.doc.find(panel).unwrap().opacity - 0.4).abs() < 1e-5, "undo returns to the value before the last edit");
        s.undo();
        assert_eq!(s.doc.find(panel).unwrap().opacity, 1.0);
    }

    #[test]
    fn move_drag_shifts_rect_and_is_one_undo_step() {
        let (mut s, p) = state_with_panel(); // fixed 200x120 at (16,16)
        let l = lay(&s);
        let depth = s.undo_depth();
        s.select(Some(p));
        assert!(s.begin_drag(DragMode::Move, &l));
        s.update_drag([10.0, 5.0]);
        s.update_drag([30.0, 20.0]); // total, not incremental
        s.end_drag();
        approx(rect(&s, p), 46.0, 36.0, 200.0, 120.0);
        assert_eq!(s.undo_depth(), depth + 1);
        s.undo();
        approx(rect(&s, p), 16.0, 16.0, 200.0, 120.0);
    }

    #[test]
    fn move_drag_respects_snap_and_scale() {
        let (mut s, p) = state_with_panel();
        s.doc.scale_mode = ScaleMode::ScaleWithScreen { reference: [400.0, 300.0], match_height: 0.0 }; // scale 2 at 800x600
        s.snap = 8.0;
        let l = lay(&s);
        s.begin_drag(DragMode::Move, &l);
        s.update_drag([40.0, 0.0]); // 40 points = 20 design units → snapped to 24
        s.end_drag();
        assert_eq!(s.doc.find(p).unwrap().offset_min, [16.0 + 24.0, 16.0]);
    }

    #[test]
    fn cancel_drag_restores_and_drops_undo_step() {
        let (mut s, p) = state_with_panel();
        let depth = s.undo_depth();
        let l = lay(&s);
        s.begin_drag(DragMode::Move, &l);
        s.update_drag([100.0, 100.0]);
        s.cancel_drag();
        approx(rect(&s, p), 16.0, 16.0, 200.0, 120.0);
        assert_eq!(s.undo_depth(), depth);
    }

    #[test]
    fn resize_every_handle_moves_only_its_edges() {
        for pivot in [[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]] {
            for handle in Handle::ALL {
                let (mut s, p) = state_with_panel();
                s.doc.find_mut(p).unwrap().pivot = pivot;
                // keep the same on-screen rect regardless of pivot
                let (px, py) = (16.0 + pivot[0] * 200.0, 16.0 + pivot[1] * 120.0);
                s.doc.find_mut(p).unwrap().offset_min = [px, py];
                let before = rect(&s, p);
                approx(before, 16.0, 16.0, 200.0, 120.0);
                let l = lay(&s);
                s.begin_drag(DragMode::Resize(handle), &l);
                s.update_drag([10.0, 6.0]);
                s.end_drag();
                let after = rect(&s, p);
                let (left, right, top, bottom) = handle.edges();
                let (dl, dr, dt, db) = (after.x - before.x, after.right() - before.right(), after.y - before.y, after.bottom() - before.bottom());
                let exp = |moved: bool, d: f32| if moved { d } else { 0.0 };
                let ctx = format!("{handle:?} pivot {pivot:?} → {after:?}");
                assert!((dl - exp(left, 10.0)).abs() < 1e-3, "left edge: {ctx}");
                assert!((dr - exp(right, 10.0)).abs() < 1e-3, "right edge: {ctx}");
                assert!((dt - exp(top, 6.0)).abs() < 1e-3, "top edge: {ctx}");
                assert!((db - exp(bottom, 6.0)).abs() < 1e-3, "bottom edge: {ctx}");
            }
        }
    }

    #[test]
    fn resize_stretch_node_changes_offsets_and_fit_becomes_fixed() {
        let (mut s, p) = state_with_panel();
        {
            let n = s.doc.find_mut(p).unwrap();
            n.anchor_min = [0.0, 0.0];
            n.anchor_max = [1.0, 1.0];
            n.width = SizeMode::Fill;
            n.height = SizeMode::Fit;
            n.offset_min = [10.0, 10.0];
            n.offset_max = [-10.0, 0.0];
        }
        let before = rect(&s, p);
        let l = lay(&s);
        s.begin_drag(DragMode::Resize(Handle::SE), &l);
        s.update_drag([-20.0, 15.0]);
        s.end_drag();
        let n = s.doc.find(p).unwrap();
        assert_eq!(n.offset_max[0], -30.0, "stretch axis edits offset_max");
        assert_eq!(n.height, SizeMode::Fixed(before.h + 15.0), "Fit axis becomes Fixed");
    }

    #[test]
    fn resize_never_collapses_below_one_unit() {
        let mut n = UiNode::new_default(NodeKind::Panel);
        n.width = SizeMode::Fixed(20.0);
        resize_edge(&mut n, 0, true, -500.0, 20.0);
        assert_eq!(n.width, SizeMode::Fixed(1.0));
        let mut m = UiNode::new_default(NodeKind::Panel);
        m.width = SizeMode::Fixed(20.0);
        m.pivot = [0.0, 0.0];
        resize_edge(&mut m, 0, false, 500.0, 20.0);
        assert_eq!(m.width, SizeMode::Fixed(1.0));
        assert_eq!(m.offset_min[0], 16.0 + 19.0, "left edge stops 1 unit before the right edge");
    }

    #[test]
    fn stack_children_cannot_be_dragged_or_anchored() {
        let (mut s, p) = state_with_panel();
        s.select(Some(p));
        let stack = s.add_node("VStack").unwrap();
        let btn = s.add_node("Button").unwrap();
        assert_eq!(s.doc.parent_of(btn), Some(stack));
        let l = lay(&s);
        assert!(!s.begin_drag(DragMode::Move, &l));
        assert!(!s.apply_preset(AnchorAxis::End, AnchorAxis::End, &l));
        assert!(s.status.contains("stack"));
    }

    #[test]
    fn preset_via_state_keeps_rect_and_is_undoable() {
        let (mut s, p) = state_with_panel();
        let before = rect(&s, p);
        let l = lay(&s);
        assert!(s.apply_preset(AnchorAxis::End, AnchorAxis::Middle, &l));
        approx(rect(&s, p), before.x, before.y, before.w, before.h);
        assert_eq!(s.doc.find(p).unwrap().anchor_min, [1.0, 0.5]);
        s.undo();
        assert_eq!(s.doc.find(p).unwrap().anchor_min, [0.0, 0.0]);
    }

    #[test]
    fn preset_inside_nested_panel_uses_parent_content_rect() {
        let (mut s, outer) = state_with_panel(); // outer: fixed 200x120 at (16,16), padding 8
        let inner = s.add_node("Panel").unwrap();
        s.doc.find_mut(inner).unwrap().width = SizeMode::Fixed(40.0);
        s.doc.find_mut(inner).unwrap().height = SizeMode::Fixed(20.0);
        let before = rect(&s, inner);
        let l = lay(&s);
        s.apply_preset(AnchorAxis::End, AnchorAxis::End, &l);
        approx(rect(&s, inner), before.x, before.y, before.w, before.h);
        // grows with the parent: widen outer by 100 → inner moves 100 right
        s.doc.find_mut(outer).unwrap().width = SizeMode::Fixed(300.0);
        approx(rect(&s, inner), before.x + 100.0, before.y, before.w, before.h);
    }

    #[test]
    fn save_load_and_undo_of_load() {
        let (mut s, _) = state_with_panel();
        let path = temp_path("d.ron");
        s.path_text = path.display().to_string();
        s.save().unwrap();
        assert!(!s.dirty);
        let saved = s.snapshot();
        s.add_node("Label");
        assert!(s.dirty);
        s.load(&path).unwrap();
        assert_eq!(s.doc, saved);
        assert!(!s.dirty);
        s.undo();
        assert_eq!(s.doc.find(s.doc.root.children[0].id).unwrap().children.len(), 1, "load can be undone");
    }

    #[test]
    fn hot_reload_replaces_clean_document_and_undo_restores() {
        let (mut s, _) = state_with_panel();
        let path = temp_path("h.ron");
        s.save_as(&path).unwrap();
        assert_eq!(s.check_reload(), ReloadOutcome::Nothing);

        // An "external editor" writes a different doc.
        let mut other = s.snapshot();
        other.name = "from_disk".into();
        save_doc(&other, &path).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(20)).unwrap();

        assert_eq!(s.check_reload(), ReloadOutcome::Reloaded);
        assert_eq!(s.doc.name, "from_disk");
        assert!(s.undo());
        assert_ne!(s.doc.name, "from_disk");
    }

    #[test]
    fn hot_reload_does_not_clobber_unsaved_edits() {
        let (mut s, _) = state_with_panel();
        let path = temp_path("c.ron");
        s.save_as(&path).unwrap();
        s.add_node("Label"); // unsaved edit
        let mut other = s.snapshot();
        other.name = "from_disk".into();
        save_doc(&other, &path).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(30)).unwrap();
        let mine = s.snapshot();
        assert_eq!(s.check_reload(), ReloadOutcome::Conflict);
        assert_eq!(s.doc, mine);
        assert_eq!(s.check_reload(), ReloadOutcome::Nothing, "conflict is reported once");
    }

    #[test]
    fn snapshot_restore_roundtrip_for_external_command_stack() {
        let (mut s, p) = state_with_panel();
        let before = s.snapshot();
        s.doc.find_mut(p).unwrap().visible = false;
        let after = s.snapshot();
        s.restore(before.clone());
        assert!(s.doc.find(p).unwrap().visible);
        s.restore(after);
        assert!(!s.doc.find(p).unwrap().visible);
    }

    #[test]
    fn reorder_and_reparent_are_undoable() {
        let (mut s, panel) = state_with_panel();
        let a = s.add_node("Label").unwrap();
        s.select(Some(panel));
        let b = s.add_node("Button").unwrap();
        // a, b under panel
        s.select(Some(b));
        assert!(s.reorder_selected(-1));
        assert_eq!(s.doc.find(panel).unwrap().children[0].id, b);
        s.undo();
        assert_eq!(s.doc.find(panel).unwrap().children[0].id, a);
        s.select(Some(a));
        assert!(!s.reparent_selected(a), "cannot parent to itself");
        assert!(s.reparent_selected(s.doc.root.id));
        assert_eq!(s.doc.parent_of(a), Some(s.doc.root.id));
        s.undo();
        assert_eq!(s.doc.parent_of(a), Some(panel));
    }

    #[test]
    fn viewport_presets_and_safe_area() {
        let mut s = DesignerState::default();
        assert_eq!(s.viewport(), [1920.0, 1080.0]);
        s.resolution = RESOLUTIONS.iter().position(|(_, r)| *r == [1024.0, 768.0]);
        assert_eq!(s.viewport(), [1024.0, 768.0]);
        s.resolution = None;
        s.custom_resolution = [333.0, 222.0];
        assert_eq!(s.viewport(), [333.0, 222.0]);
        assert_eq!(s.safe_area(), Insets::ZERO);
        s.preview_safe_area = true;
        assert!((s.safe_area().left - 16.65).abs() < 1e-3);
    }
}
