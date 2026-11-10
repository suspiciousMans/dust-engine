//! The UI document data model: a tree of [`UiNode`]s plus document-level
//! settings. Everything here is plain serde data (RON on disk) with no GL or
//! egui dependency.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::style::StyleSheet;

/// Identifies a node inside one [`UiDoc`]. Unique per document; `0` means
/// "not assigned yet" (hand-written RON files may omit ids — they are
/// assigned by [`UiDoc::normalize`] on load).
pub type NodeId = u32;

/// Per-side distances (margin, padding, safe area, 9-slice borders).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Insets {
    pub const ZERO: Insets = Insets { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };

    pub fn uniform(v: f32) -> Self {
        Insets { left: v, top: v, right: v, bottom: v }
    }
    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Insets { left, top, right, bottom }
    }
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
    /// Start side of an axis (0 = x → left, 1 = y → top).
    pub fn start(&self, axis: usize) -> f32 {
        if axis == 0 { self.left } else { self.top }
    }
    /// End side of an axis (0 = x → right, 1 = y → bottom).
    pub fn end(&self, axis: usize) -> f32 {
        if axis == 0 { self.right } else { self.bottom }
    }
    pub fn along(&self, axis: usize) -> f32 {
        self.start(axis) + self.end(axis)
    }
    /// Scale every side (used to convert between design units and points).
    pub fn scaled(&self, f: f32) -> Insets {
        Insets::new(self.left * f, self.top * f, self.right * f, self.bottom * f)
    }
}

/// How a node's size along one axis is decided.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum SizeMode {
    /// Exactly this many design units.
    Fixed(f32),
    /// Free layout: stretch between the anchors (plus offsets).
    /// Stack/grid layout: take a share of the leftover space
    /// (see [`UiNode::fill_weight`]).
    Fill,
    /// Size of the content (text, children) plus padding.
    Fit,
}

impl Default for SizeMode {
    fn default() -> Self {
        SizeMode::Fill
    }
}

/// Alignment used by stacks for the main axis (leftover space) and the
/// cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    /// Cross axis only: `Fit` children are stretched to the full cross size.
    Stretch,
}

fn one() -> f32 {
    1.0
}
fn default_native() -> [f32; 2] {
    [64.0, 64.0]
}
fn default_columns() -> u32 {
    2
}

/// What a node is, plus the properties specific to that kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Plain rectangle: background/border from style, holds free-positioned children.
    Panel,
    Label {
        #[serde(default)]
        text: String,
    },
    Button {
        #[serde(default)]
        text: String,
        /// Event name pushed to the [`super::UiEventQueue`] when clicked.
        #[serde(default)]
        on_click: Option<String>,
    },
    Image {
        /// Asset path; the host supplies the actual texture (see `ImageProvider`).
        #[serde(default)]
        path: String,
        /// `Some(borders)` = draw as a 9-slice with these source-pixel borders.
        #[serde(default)]
        nine_slice: Option<Insets>,
        /// Size used by `Fit` sizing (usually the texture's pixel size).
        #[serde(default = "default_native")]
        native_size: [f32; 2],
    },
    ProgressBar {
        #[serde(default)]
        value: f32,
        #[serde(default)]
        min: f32,
        #[serde(default = "one")]
        max: f32,
    },
    Slider {
        #[serde(default)]
        value: f32,
        #[serde(default)]
        min: f32,
        #[serde(default = "one")]
        max: f32,
        /// Event emitted with the new value when changed.
        #[serde(default)]
        on_change: Option<String>,
    },
    Toggle {
        #[serde(default)]
        text: String,
        #[serde(default)]
        checked: bool,
        /// Event emitted with the new bool value when toggled.
        #[serde(default)]
        on_toggle: Option<String>,
    },
    /// Invisible; takes up space (use `Fill` to push siblings apart).
    Spacer,
    HStack {
        #[serde(default)]
        spacing: f32,
        #[serde(default)]
        main_align: Align,
        #[serde(default)]
        cross_align: Align,
    },
    VStack {
        #[serde(default)]
        spacing: f32,
        #[serde(default)]
        main_align: Align,
        #[serde(default)]
        cross_align: Align,
    },
    Grid {
        #[serde(default = "default_columns")]
        columns: u32,
        #[serde(default)]
        spacing: [f32; 2],
    },
}

impl NodeKind {
    /// Names of all kinds, in the order shown in the "add node" menu.
    pub const ALL_NAMES: [&'static str; 11] = [
        "Panel", "Label", "Button", "Image", "ProgressBar", "Slider", "Toggle", "Spacer", "HStack", "VStack", "Grid",
    ];

    /// A kind with default properties, by name (see [`Self::ALL_NAMES`]).
    pub fn default_for(name: &str) -> Option<NodeKind> {
        Some(match name {
            "Panel" => NodeKind::Panel,
            "Label" => NodeKind::Label { text: "Label".into() },
            "Button" => NodeKind::Button { text: "Button".into(), on_click: None },
            "Image" => NodeKind::Image { path: String::new(), nine_slice: None, native_size: default_native() },
            "ProgressBar" => NodeKind::ProgressBar { value: 0.5, min: 0.0, max: 1.0 },
            "Slider" => NodeKind::Slider { value: 0.5, min: 0.0, max: 1.0, on_change: None },
            "Toggle" => NodeKind::Toggle { text: "Toggle".into(), checked: false, on_toggle: None },
            "Spacer" => NodeKind::Spacer,
            "HStack" => NodeKind::HStack { spacing: 4.0, main_align: Align::Start, cross_align: Align::Start },
            "VStack" => NodeKind::VStack { spacing: 4.0, main_align: Align::Start, cross_align: Align::Start },
            "Grid" => NodeKind::Grid { columns: 2, spacing: [4.0, 4.0] },
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            NodeKind::Panel => "Panel",
            NodeKind::Label { .. } => "Label",
            NodeKind::Button { .. } => "Button",
            NodeKind::Image { .. } => "Image",
            NodeKind::ProgressBar { .. } => "ProgressBar",
            NodeKind::Slider { .. } => "Slider",
            NodeKind::Toggle { .. } => "Toggle",
            NodeKind::Spacer => "Spacer",
            NodeKind::HStack { .. } => "HStack",
            NodeKind::VStack { .. } => "VStack",
            NodeKind::Grid { .. } => "Grid",
        }
    }

    /// Can hold children.
    pub fn is_container(&self) -> bool {
        matches!(self, NodeKind::Panel | NodeKind::HStack { .. } | NodeKind::VStack { .. } | NodeKind::Grid { .. })
    }

    /// Can receive keyboard/gamepad focus and be activated.
    pub fn is_focusable(&self) -> bool {
        matches!(self, NodeKind::Button { .. } | NodeKind::Slider { .. } | NodeKind::Toggle { .. })
    }

    /// True when the layout of children is controlled by this node
    /// (children's anchors/offsets are ignored).
    pub fn controls_child_layout(&self) -> bool {
        matches!(self, NodeKind::HStack { .. } | NodeKind::VStack { .. } | NodeKind::Grid { .. })
    }
}

/// One element of the UI tree.
///
/// **Free layout** (child of a `Panel` or any non-stack node), per axis:
/// the parent's content rect (shrunk by this node's `margin`) is split by
/// `anchor_min`/`anchor_max` (normalized 0..1) into an anchor interval
/// `[A0, A1]`. Then
/// * `Fill`: the node spans `[A0 + offset_min, A1 + offset_max]`
///   (Unity-style stretch; with a point anchor this means size = max − min);
/// * `Fixed(s)` / `Fit`: the node has that size and its `pivot` point sits at
///   `lerp(A0, A1, pivot) + offset_min` (`offset_max` is unused).
///
/// **Stack/grid layout**: anchors and offsets are ignored; `margin` is outer
/// spacing and the size modes drive the stack algorithm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiNode {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub anchor_min: [f32; 2],
    pub anchor_max: [f32; 2],
    pub offset_min: [f32; 2],
    pub offset_max: [f32; 2],
    pub pivot: [f32; 2],
    pub width: SizeMode,
    pub height: SizeMode,
    /// Share of leftover stack space when the size mode is `Fill`.
    pub fill_weight: f32,
    pub margin: Insets,
    pub padding: Insets,
    /// Draw order among siblings (higher = in front). Ties keep tree order.
    pub z: i32,
    /// Invisible nodes (and their subtree) take no layout space and are not drawn.
    pub visible: bool,
    /// Disabled widgets are drawn with the `disabled` style and ignore input.
    pub enabled: bool,
    /// Own opacity; multiplied down the tree.
    pub opacity: f32,
    /// Style class names, applied in order (later wins).
    pub classes: Vec<String>,
    /// Property name → binding. See `bindings.rs` for the supported properties.
    pub bindings: BTreeMap<String, String>,
    pub children: Vec<UiNode>,
}

impl Default for UiNode {
    fn default() -> Self {
        UiNode {
            id: 0,
            name: String::new(),
            kind: NodeKind::Panel,
            anchor_min: [0.0, 0.0],
            anchor_max: [1.0, 1.0],
            offset_min: [0.0, 0.0],
            offset_max: [0.0, 0.0],
            pivot: [0.5, 0.5],
            width: SizeMode::Fill,
            height: SizeMode::Fill,
            fill_weight: 1.0,
            margin: Insets::ZERO,
            padding: Insets::ZERO,
            z: 0,
            visible: true,
            enabled: true,
            opacity: 1.0,
            classes: Vec::new(),
            bindings: BTreeMap::new(),
            children: Vec::new(),
        }
    }
}

impl UiNode {
    pub fn new(kind: NodeKind) -> Self {
        UiNode { name: kind.name().to_string(), kind, ..Default::default() }
    }

    /// A node as the designer creates it: a sensible visible default size
    /// (fixed, top-left, 120×32 or per-kind) with padding for text widgets.
    pub fn new_default(kind: NodeKind) -> Self {
        let mut n = UiNode::new(kind);
        n.anchor_min = [0.0, 0.0];
        n.anchor_max = [0.0, 0.0];
        n.pivot = [0.0, 0.0];
        n.offset_min = [16.0, 16.0];
        match n.kind {
            NodeKind::Button { .. } => {
                n.padding = Insets::new(8.0, 4.0, 8.0, 4.0);
                n.width = SizeMode::Fit;
                n.height = SizeMode::Fit;
                n.classes = vec!["button".into()];
            }
            NodeKind::Label { .. } => {
                n.width = SizeMode::Fit;
                n.height = SizeMode::Fit;
            }
            NodeKind::Toggle { .. } => {
                n.width = SizeMode::Fit;
                n.height = SizeMode::Fit;
            }
            NodeKind::ProgressBar { .. } => {
                n.width = SizeMode::Fixed(160.0);
                n.height = SizeMode::Fixed(16.0);
            }
            NodeKind::Slider { .. } => {
                n.width = SizeMode::Fixed(160.0);
                n.height = SizeMode::Fixed(20.0);
            }
            NodeKind::Spacer => {
                n.width = SizeMode::Fixed(16.0);
                n.height = SizeMode::Fixed(16.0);
            }
            NodeKind::Image { .. } => {
                n.width = SizeMode::Fixed(64.0);
                n.height = SizeMode::Fixed(64.0);
            }
            NodeKind::HStack { .. } | NodeKind::VStack { .. } | NodeKind::Grid { .. } => {
                n.width = SizeMode::Fit;
                n.height = SizeMode::Fit;
            }
            NodeKind::Panel => {
                n.width = SizeMode::Fixed(200.0);
                n.height = SizeMode::Fixed(120.0);
                n.padding = Insets::uniform(8.0);
                n.classes = vec!["panel".into()];
            }
        }
        n
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }
    pub fn with_child(mut self, child: UiNode) -> Self {
        self.children.push(child);
        self
    }
    pub fn with_size(mut self, width: SizeMode, height: SizeMode) -> Self {
        self.width = width;
        self.height = height;
        self
    }
    pub fn with_class(mut self, class: &str) -> Self {
        self.classes.push(class.to_string());
        self
    }
    pub fn with_binding(mut self, property: &str, value_name: &str) -> Self {
        self.bindings.insert(property.to_string(), value_name.to_string());
        self
    }
    /// Point anchor at `anchor` (0..1 each axis), pivot at the same spot,
    /// position offset `offset` from it.
    pub fn at(mut self, anchor: [f32; 2], offset: [f32; 2]) -> Self {
        self.anchor_min = anchor;
        self.anchor_max = anchor;
        self.pivot = anchor;
        self.offset_min = offset;
        self
    }

    pub fn size_mode(&self, axis: usize) -> SizeMode {
        if axis == 0 { self.width } else { self.height }
    }

    /// Visit this node and all descendants, parents first.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a UiNode)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }

    fn walk_mut(&mut self, f: &mut dyn FnMut(&mut UiNode)) {
        f(self);
        for c in &mut self.children {
            c.walk_mut(f);
        }
    }

    pub fn subtree_size(&self) -> usize {
        1 + self.children.iter().map(|c| c.subtree_size()).sum::<usize>()
    }
}

/// How the UI scales with the screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScaleMode {
    /// 1 design unit = 1 point (times the user UI scale).
    Constant,
    /// Scale so a `reference` resolution fills the screen. `match_height`
    /// 0.0 = match width, 1.0 = match height, in between blends
    /// logarithmically (like Unity's CanvasScaler), so 16:9 and 4:3 both
    /// look right without distortion.
    ScaleWithScreen { reference: [f32; 2], match_height: f32 },
}

impl Default for ScaleMode {
    fn default() -> Self {
        ScaleMode::Constant
    }
}

impl ScaleMode {
    /// Scale factor for a viewport size (in points).
    pub fn scale_for(&self, viewport: [f32; 2]) -> f32 {
        match self {
            ScaleMode::Constant => 1.0,
            ScaleMode::ScaleWithScreen { reference, match_height } => {
                if reference[0] <= 0.0 || reference[1] <= 0.0 || viewport[0] <= 0.0 || viewport[1] <= 0.0 {
                    return 1.0;
                }
                let lw = (viewport[0] / reference[0]).ln();
                let lh = (viewport[1] / reference[1]).ln();
                let t = match_height.clamp(0.0, 1.0);
                (lw * (1.0 - t) + lh * t).exp()
            }
        }
    }
}

/// A whole UI screen (HUD, pause menu, ...): tree + styles + settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiDoc {
    pub name: String,
    pub scale_mode: ScaleMode,
    /// Lay the root out inside the platform safe area (notches, TV overscan).
    pub respect_safe_area: bool,
    pub styles: StyleSheet,
    pub root: UiNode,
    next_id: NodeId,
}

impl Default for UiDoc {
    fn default() -> Self {
        let mut root = UiNode::new(NodeKind::Panel);
        root.name = "Root".into();
        let mut doc = UiDoc {
            name: "Untitled".into(),
            scale_mode: ScaleMode::Constant,
            respect_safe_area: false,
            styles: StyleSheet::default_theme(),
            root,
            next_id: 1,
        };
        doc.normalize();
        doc
    }
}

impl UiDoc {
    pub fn new(name: &str, root: UiNode) -> Self {
        let mut doc = UiDoc { name: name.into(), root, ..UiDoc::default() };
        doc.normalize();
        doc
    }

    /// Give every node a unique non-zero id (keeping existing unique ones).
    /// Called after loading; safe to call any time.
    pub fn normalize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut max_id = 0;
        let mut need_fix = Vec::new();
        // First pass: find the max id and which nodes need a fresh one.
        let mut index = 0usize;
        self.root.walk(&mut |n| {
            if n.id == 0 || !seen.insert(n.id) {
                need_fix.push(index);
            } else {
                max_id = max_id.max(n.id);
            }
            index += 1;
        });
        let mut next = max_id.max(self.next_id.saturating_sub(1)) + 1;
        let mut index = 0usize;
        let mut fix_iter = need_fix.into_iter().peekable();
        self.root.walk_mut(&mut |n| {
            if fix_iter.peek() == Some(&index) {
                fix_iter.next();
                n.id = next;
                next += 1;
            }
            index += 1;
        });
        self.next_id = next;
    }

    /// Fresh id for a node about to be inserted.
    pub fn alloc_id(&mut self) -> NodeId {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    pub fn find(&self, id: NodeId) -> Option<&UiNode> {
        fn rec(n: &UiNode, id: NodeId) -> Option<&UiNode> {
            if n.id == id {
                return Some(n);
            }
            n.children.iter().find_map(|c| rec(c, id))
        }
        rec(&self.root, id)
    }

    pub fn find_mut(&mut self, id: NodeId) -> Option<&mut UiNode> {
        fn rec(n: &mut UiNode, id: NodeId) -> Option<&mut UiNode> {
            if n.id == id {
                return Some(n);
            }
            n.children.iter_mut().find_map(|c| rec(c, id))
        }
        rec(&mut self.root, id)
    }

    pub fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        let mut result = None;
        self.root.walk(&mut |n| {
            if n.children.iter().any(|c| c.id == id) {
                result = Some(n.id);
            }
        });
        result
    }

    /// Index of `id` among its parent's children.
    pub fn index_in_parent(&self, id: NodeId) -> Option<usize> {
        let parent = self.find(self.parent_of(id)?)?;
        parent.children.iter().position(|c| c.id == id)
    }

    /// Append `node` (with fresh ids for its whole subtree) under `parent`,
    /// at `index` (or the end). Returns the new node's id.
    pub fn insert(&mut self, parent: NodeId, index: Option<usize>, mut node: UiNode) -> Option<NodeId> {
        self.find(parent)?;
        self.assign_fresh_ids(&mut node);
        let id = node.id;
        let p = self.find_mut(parent)?;
        let at = index.unwrap_or(p.children.len()).min(p.children.len());
        p.children.insert(at, node);
        Some(id)
    }

    fn assign_fresh_ids(&mut self, node: &mut UiNode) {
        node.id = self.alloc_id();
        for c in &mut node.children {
            self.assign_fresh_ids(c);
        }
    }

    /// Remove a node (never the root). Returns it.
    pub fn remove(&mut self, id: NodeId) -> Option<UiNode> {
        if id == self.root.id {
            return None;
        }
        let parent = self.parent_of(id)?;
        let p = self.find_mut(parent)?;
        let pos = p.children.iter().position(|c| c.id == id)?;
        Some(p.children.remove(pos))
    }

    /// Copy a node right after itself; returns the copy's id. Copy gets a
    /// "name copy" style unique name only when it would collide.
    pub fn duplicate(&mut self, id: NodeId) -> Option<NodeId> {
        if id == self.root.id {
            return None;
        }
        let parent = self.parent_of(id)?;
        let index = self.index_in_parent(id)?;
        let mut copy = self.find(id)?.clone();
        copy.name = format!("{} copy", copy.name);
        self.insert(parent, Some(index + 1), copy)
    }

    /// Move a node up (-1) or down (+1) among its siblings. Returns whether it moved.
    pub fn reorder(&mut self, id: NodeId, delta: i32) -> bool {
        let Some(parent) = self.parent_of(id) else { return false };
        let Some(index) = self.index_in_parent(id) else { return false };
        let Some(p) = self.find_mut(parent) else { return false };
        let new = index as i32 + delta;
        if new < 0 || new as usize >= p.children.len() {
            return false;
        }
        p.children.swap(index, new as usize);
        true
    }

    /// Move `id` to become a child of `new_parent` (at the end). Refuses to
    /// move a node into itself or its own descendants.
    pub fn reparent(&mut self, id: NodeId, new_parent: NodeId) -> bool {
        if id == self.root.id || id == new_parent {
            return false;
        }
        if let Some(n) = self.find(id) {
            if n.find_in_subtree(new_parent) {
                return false;
            }
        }
        if self.find(new_parent).is_none() {
            return false;
        }
        let Some(node) = self.remove(id) else { return false };
        if let Some(p) = self.find_mut(new_parent) {
            p.children.push(node);
            true
        } else {
            false
        }
    }

    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
    }

    pub fn from_ron(text: &str) -> Result<UiDoc, ron::error::SpannedError> {
        let mut doc: UiDoc = ron::from_str(text)?;
        doc.normalize();
        Ok(doc)
    }
}

impl UiNode {
    fn find_in_subtree(&self, id: NodeId) -> bool {
        self.id == id || self.children.iter().any(|c| c.find_in_subtree(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UiDoc {
        let root = UiNode::new(NodeKind::Panel)
            .with_name("root")
            .with_child(UiNode::new(NodeKind::Label { text: "hi".into() }).with_name("a"))
            .with_child(
                UiNode::new(NodeKind::VStack { spacing: 2.0, main_align: Align::Start, cross_align: Align::Start })
                    .with_name("stack")
                    .with_child(UiNode::new(NodeKind::Spacer).with_name("s1")),
            );
        UiDoc::new("t", root)
    }

    #[test]
    fn ids_unique_after_normalize() {
        let doc = sample();
        let mut ids = Vec::new();
        doc.root.walk(&mut |n| ids.push(n.id));
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        assert!(!ids.contains(&0));
    }

    #[test]
    fn normalize_fixes_duplicates_keeps_unique() {
        let mut doc = sample();
        let a = doc.root.children[0].id;
        doc.root.children[1].id = a; // duplicate
        doc.normalize();
        assert_eq!(doc.root.children[0].id, a, "first holder keeps id");
        assert_ne!(doc.root.children[1].id, a);
    }

    #[test]
    fn ron_roundtrip() {
        let doc = sample();
        let text = doc.to_ron().unwrap();
        let back = UiDoc::from_ron(&text).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn minimal_ron_uses_defaults() {
        let doc = UiDoc::from_ron(r#"(root: (kind: Label(text: "x"), children: []))"#).unwrap();
        assert_eq!(doc.root.width, SizeMode::Fill);
        assert!(doc.root.visible);
        assert_eq!(doc.root.opacity, 1.0);
        assert_ne!(doc.root.id, 0);
    }

    #[test]
    fn insert_remove_duplicate_reorder() {
        let mut doc = sample();
        let root = doc.root.id;
        let new = doc.insert(root, None, UiNode::new_default(NodeKind::default_for("Button").unwrap())).unwrap();
        assert_eq!(doc.parent_of(new), Some(root));
        let dup = doc.duplicate(new).unwrap();
        assert_ne!(dup, new);
        assert_eq!(doc.index_in_parent(dup), Some(doc.index_in_parent(new).unwrap() + 1));
        assert!(doc.reorder(dup, -1));
        assert_eq!(doc.index_in_parent(dup), doc.index_in_parent(new).map(|i| i - 1));
        assert!(doc.remove(dup).is_some());
        assert!(doc.find(dup).is_none());
        assert!(doc.remove(root).is_none(), "root is not removable");
        assert!(doc.duplicate(root).is_none());
    }

    #[test]
    fn duplicate_subtree_gets_fresh_ids() {
        let mut doc = sample();
        let stack = doc.root.children[1].id;
        let dup = doc.duplicate(stack).unwrap();
        let mut ids = Vec::new();
        doc.root.walk(&mut |n| ids.push(n.id));
        let mut s = ids.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), ids.len());
        assert_eq!(doc.find(dup).unwrap().children.len(), 1);
    }

    #[test]
    fn reparent_refuses_cycles() {
        let mut doc = sample();
        let stack = doc.root.children[1].id;
        let spacer = doc.root.children[1].children[0].id;
        assert!(!doc.reparent(stack, spacer));
        assert!(!doc.reparent(stack, stack));
        let label = doc.root.children[0].id;
        assert!(doc.reparent(label, stack));
        assert_eq!(doc.parent_of(label), Some(stack));
    }

    #[test]
    fn scale_with_screen_matches_width_or_height() {
        let m = ScaleMode::ScaleWithScreen { reference: [1920.0, 1080.0], match_height: 0.0 };
        assert!((m.scale_for([960.0, 540.0]) - 0.5).abs() < 1e-5);
        // 4:3 at 1024x768: match width gives 0.533
        assert!((m.scale_for([1024.0, 768.0]) - 1024.0 / 1920.0).abs() < 1e-5);
        let h = ScaleMode::ScaleWithScreen { reference: [1920.0, 1080.0], match_height: 1.0 };
        assert!((h.scale_for([1024.0, 768.0]) - 768.0 / 1080.0).abs() < 1e-5);
        assert_eq!(ScaleMode::Constant.scale_for([10.0, 10.0]), 1.0);
    }
}
