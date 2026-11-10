//! Pure-Rust layout solver: [`UiDoc`] + viewport → absolute rectangles.
//! No GL, no egui. See [`UiNode`] for the layout rules.
//!
//! Units: the solver works in "design units" (`viewport / scale`), and the
//! returned rects are in points (design units × scale), so they can be drawn
//! directly at the viewport origin.

use std::collections::HashMap;

use super::bindings::{resolve_text, resolve_visible, UiBindings};
use super::model::*;
use super::style::WidgetState;

/// Axis-aligned rectangle (x/y = top-left).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> [f32; 2] {
        [self.x + self.w * 0.5, self.y + self.h * 0.5]
    }
    pub fn contains(&self, p: [f32; 2]) -> bool {
        p[0] >= self.x && p[0] < self.right() && p[1] >= self.y && p[1] < self.bottom()
    }
    pub fn origin(&self, axis: usize) -> f32 {
        if axis == 0 { self.x } else { self.y }
    }
    pub fn size(&self, axis: usize) -> f32 {
        if axis == 0 { self.w } else { self.h }
    }
    /// Shrink by insets; width/height never go negative.
    pub fn inset(&self, i: Insets) -> Rect {
        Rect::new(self.x + i.left, self.y + i.top, (self.w - i.horizontal()).max(0.0), (self.h - i.vertical()).max(0.0))
    }
    pub fn scaled(&self, f: f32) -> Rect {
        Rect::new(self.x * f, self.y * f, self.w * f, self.h * f)
    }
    /// Build from per-axis (origin, size) pairs; `a` is the first axis index.
    fn from_axes(a: usize, a_origin: f32, a_size: f32, b_origin: f32, b_size: f32) -> Rect {
        if a == 0 {
            Rect::new(a_origin, b_origin, a_size, b_size)
        } else {
            Rect::new(b_origin, a_origin, b_size, a_size)
        }
    }
}

/// Measures text for `Fit` sizing. The default is a rough approximation;
/// the egui renderer can supply real font metrics.
pub trait TextMeasure {
    fn measure(&self, text: &str, font_size: f32) -> [f32; 2];
}

/// ~0.55 em per character, 1.25 em per line. Good enough for previews and tests.
pub struct ApproxTextMeasure;

impl TextMeasure for ApproxTextMeasure {
    fn measure(&self, text: &str, font_size: f32) -> [f32; 2] {
        let lines = text.lines().count().max(1);
        let longest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        [longest as f32 * font_size * 0.55, lines as f32 * font_size * 1.25]
    }
}

pub struct LayoutParams<'a> {
    /// Screen size in points.
    pub viewport: [f32; 2],
    /// Extra user scale (accessibility "UI size", or DPI-derived), multiplied
    /// with the document's [`ScaleMode`].
    pub ui_scale: f32,
    /// Unsafe border in points (notch, overscan); used when `doc.respect_safe_area`.
    pub safe_area: Insets,
    pub bindings: Option<&'a UiBindings>,
    pub measure: &'a dyn TextMeasure,
}

impl<'a> LayoutParams<'a> {
    pub fn new(viewport: [f32; 2]) -> Self {
        LayoutParams { viewport, ui_scale: 1.0, safe_area: Insets::ZERO, bindings: None, measure: &ApproxTextMeasure }
    }
    pub fn with_bindings(mut self, b: &'a UiBindings) -> Self {
        self.bindings = Some(b);
        self
    }
    pub fn with_ui_scale(mut self, s: f32) -> Self {
        self.ui_scale = s;
        self
    }
    pub fn with_safe_area(mut self, i: Insets) -> Self {
        self.safe_area = i;
        self
    }
    pub fn with_measure(mut self, m: &'a dyn TextMeasure) -> Self {
        self.measure = m;
        self
    }
}

/// Resolved placement of one visible node.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutEntry {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub depth: u32,
    /// Absolute rect in points.
    pub rect: Rect,
    /// `rect` minus padding, in points (where children are laid out).
    pub content: Rect,
}

#[derive(Clone, Debug, Default)]
pub struct LayoutResult {
    /// Total scale from design units to points.
    pub scale: f32,
    pub viewport: [f32; 2],
    /// Visible nodes in draw order (parents before children; siblings by z).
    pub entries: Vec<LayoutEntry>,
    index: HashMap<NodeId, usize>,
}

impl LayoutResult {
    pub fn entry(&self, id: NodeId) -> Option<&LayoutEntry> {
        self.index.get(&id).map(|&i| &self.entries[i])
    }
    pub fn rect(&self, id: NodeId) -> Option<Rect> {
        self.entry(id).map(|e| e.rect)
    }
    /// Topmost visible node under `p` for which `accept` is true.
    pub fn hit_test_where(&self, p: [f32; 2], accept: &dyn Fn(NodeId) -> bool) -> Option<NodeId> {
        self.entries.iter().rev().find(|e| e.rect.contains(p) && accept(e.id)).map(|e| e.id)
    }
    pub fn hit_test(&self, p: [f32; 2]) -> Option<NodeId> {
        self.hit_test_where(p, &|_| true)
    }
}

/// Resolve `doc` for the given viewport.
pub fn solve(doc: &UiDoc, params: &LayoutParams) -> LayoutResult {
    let user = if params.ui_scale > 0.0 { params.ui_scale } else { 1.0 };
    let scale = (doc.scale_mode.scale_for(params.viewport) * user).max(0.0001);
    let design = Rect::new(0.0, 0.0, params.viewport[0] / scale, params.viewport[1] / scale);
    let safe = if doc.respect_safe_area { params.safe_area.scaled(1.0 / scale) } else { Insets::ZERO };
    let area = design.inset(safe);

    let mut solver = Solver { doc, p: params, entries: Vec::new() };
    if resolve_visible(&doc.root, params.bindings) {
        let root_rect = solver.free_rect(&doc.root, area);
        solver.place(&doc.root, root_rect, None, 0);
    }

    let mut result = LayoutResult { scale, viewport: params.viewport, entries: solver.entries, index: HashMap::new() };
    for e in &mut result.entries {
        e.rect = e.rect.scaled(scale);
        e.content = e.content.scaled(scale);
    }
    result.index = result.entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
    result
}

struct Solver<'a> {
    doc: &'a UiDoc,
    p: &'a LayoutParams<'a>,
    entries: Vec<LayoutEntry>,
}

const DEFAULT_BAR_SIZE: [f32; 2] = [120.0, 16.0];
const DEFAULT_SLIDER_SIZE: [f32; 2] = [120.0, 20.0];
const TOGGLE_BOX: f32 = 20.0;

impl<'a> Solver<'a> {
    fn is_visible(&self, n: &UiNode) -> bool {
        resolve_visible(n, self.p.bindings)
    }

    /// Record `node` at `rect`, then lay out and record its visible children.
    fn place(&mut self, node: &UiNode, rect: Rect, parent: Option<NodeId>, depth: u32) {
        let content = rect.inset(node.padding);
        self.entries.push(LayoutEntry { id: node.id, parent, depth, rect, content });

        let kids: Vec<&UiNode> = node.children.iter().filter(|c| self.is_visible(c)).collect();
        let rects = self.child_rects(node, &kids, content);
        let mut order: Vec<usize> = (0..kids.len()).collect();
        order.sort_by_key(|&i| kids[i].z); // stable: ties keep tree order
        for i in order {
            self.place(kids[i], rects[i], Some(node.id), depth + 1);
        }
    }

    fn child_rects(&self, node: &UiNode, kids: &[&UiNode], content: Rect) -> Vec<Rect> {
        match &node.kind {
            NodeKind::HStack { spacing, main_align, cross_align } => self.stack_rects(kids, content, 0, *spacing, *main_align, *cross_align),
            NodeKind::VStack { spacing, main_align, cross_align } => self.stack_rects(kids, content, 1, *spacing, *main_align, *cross_align),
            NodeKind::Grid { columns, spacing } => self.grid_rects(kids, content, *columns, *spacing),
            _ => kids.iter().map(|k| self.free_rect(k, content)).collect(),
        }
    }

    // ---- free (anchor) layout -------------------------------------------------

    fn free_rect(&self, node: &UiNode, parent: Rect) -> Rect {
        let area = parent.inset(node.margin);
        let fit = self.intrinsic(node);
        let mut origin = [0.0; 2];
        let mut size = [0.0; 2];
        for axis in 0..2 {
            let i0 = area.origin(axis);
            let len = area.size(axis);
            let a0 = i0 + len * node.anchor_min[axis];
            let a1 = i0 + len * node.anchor_max[axis];
            match node.size_mode(axis) {
                SizeMode::Fill => {
                    origin[axis] = a0 + node.offset_min[axis];
                    size[axis] = (a1 + node.offset_max[axis] - origin[axis]).max(0.0);
                }
                mode => {
                    let s = match mode {
                        SizeMode::Fixed(s) => s.max(0.0),
                        _ => fit[axis],
                    };
                    let pivot = node.pivot[axis];
                    let reference = a0 + (a1 - a0) * pivot + node.offset_min[axis];
                    origin[axis] = reference - pivot * s;
                    size[axis] = s;
                }
            }
        }
        Rect::new(origin[0], origin[1], size[0], size[1])
    }

    // ---- stacks -----------------------------------------------------------------

    /// `m` = main axis (0 = HStack, 1 = VStack).
    fn stack_rects(&self, kids: &[&UiNode], content: Rect, m: usize, spacing: f32, main_align: Align, cross_align: Align) -> Vec<Rect> {
        let n = kids.len();
        if n == 0 {
            return Vec::new();
        }
        let c = 1 - m;
        let mut used = spacing * (n - 1) as f32;
        let mut weights = 0.0;
        for k in kids {
            used += k.margin.along(m);
            match self.preferred(k, m) {
                Some(s) => used += s,
                None => weights += k.fill_weight.max(0.0),
            }
        }
        let leftover = (content.size(m) - used).max(0.0);
        let shift = if weights > 0.0 {
            0.0
        } else {
            match main_align {
                Align::Center => leftover * 0.5,
                Align::End => leftover,
                _ => 0.0,
            }
        };

        let mut pos = content.origin(m) + shift;
        let mut out = Vec::with_capacity(n);
        for k in kids {
            let main_size = match self.preferred(k, m) {
                Some(s) => s,
                None if weights > 0.0 => leftover * k.fill_weight.max(0.0) / weights,
                None => 0.0,
            };
            pos += k.margin.start(m);

            let avail = (content.size(c) - k.margin.along(c)).max(0.0);
            let cross_size = match (k.size_mode(c), cross_align) {
                (SizeMode::Fill, _) | (SizeMode::Fit, Align::Stretch) => avail,
                _ => self.preferred(k, c).unwrap_or(avail),
            };
            let cross_off = match cross_align {
                Align::Center => (avail - cross_size) * 0.5,
                Align::End => avail - cross_size,
                _ => 0.0,
            };
            let cross_pos = content.origin(c) + k.margin.start(c) + cross_off;
            out.push(Rect::from_axes(m, pos, main_size, cross_pos, cross_size));
            pos += main_size + k.margin.end(m) + spacing;
        }
        out
    }

    // ---- grid -------------------------------------------------------------------

    fn grid_rects(&self, kids: &[&UiNode], content: Rect, columns: u32, spacing: [f32; 2]) -> Vec<Rect> {
        let cols = columns.max(1) as usize;
        let n = kids.len();
        if n == 0 {
            return Vec::new();
        }
        let rows = (n + cols - 1) / cols;
        let cell_w = (content.w - spacing[0] * (cols - 1) as f32).max(0.0) / cols as f32;

        // Row height: tallest fixed/fit child; rows with only Fill children share the rest.
        let row_heights: Vec<Option<f32>> = (0..rows)
            .map(|r| {
                kids[r * cols..((r + 1) * cols).min(n)]
                    .iter()
                    .filter_map(|k| self.preferred(k, 1).map(|h| h + k.margin.vertical()))
                    .fold(None, |acc: Option<f32>, h| Some(acc.map_or(h, |a| a.max(h))))
            })
            .collect();
        let fixed_total: f32 = row_heights.iter().flatten().sum::<f32>() + spacing[1] * (rows - 1) as f32;
        let fill_rows = row_heights.iter().filter(|h| h.is_none()).count();
        let fill_h = if fill_rows > 0 { (content.h - fixed_total).max(0.0) / fill_rows as f32 } else { 0.0 };

        let mut out = Vec::with_capacity(n);
        let mut y = content.y;
        for r in 0..rows {
            let row_h = row_heights[r].unwrap_or(fill_h);
            for col in 0..cols {
                let i = r * cols + col;
                if i >= n {
                    break;
                }
                let k = kids[i];
                let cell = Rect::new(content.x + col as f32 * (cell_w + spacing[0]), y, cell_w, row_h).inset(k.margin);
                let w = if k.size_mode(0) == SizeMode::Fill { cell.w } else { self.preferred(k, 0).unwrap_or(cell.w) };
                let h = if k.size_mode(1) == SizeMode::Fill { cell.h } else { self.preferred(k, 1).unwrap_or(cell.h) };
                out.push(Rect::new(cell.x, cell.y, w, h));
            }
            y += row_h + spacing[1];
        }
        out
    }

    // ---- measuring ----------------------------------------------------------------

    /// Size along `axis` if it doesn't depend on leftover space.
    fn preferred(&self, node: &UiNode, axis: usize) -> Option<f32> {
        match node.size_mode(axis) {
            SizeMode::Fixed(s) => Some(s.max(0.0)),
            SizeMode::Fit => Some(self.intrinsic(node)[axis]),
            SizeMode::Fill => None,
        }
    }

    fn text_size(&self, node: &UiNode) -> [f32; 2] {
        let style = self.doc.styles.resolve(node.kind.name(), &node.classes, WidgetState::default());
        self.p.measure.measure(&resolve_text(node, self.p.bindings), style.font_size)
    }

    /// Content size plus padding (margin not included). Children with `Fill`
    /// size contribute nothing; free children's offsets are ignored.
    fn intrinsic(&self, node: &UiNode) -> [f32; 2] {
        let pad = [node.padding.horizontal(), node.padding.vertical()];
        let content = match &node.kind {
            NodeKind::Label { .. } | NodeKind::Button { .. } => self.text_size(node),
            NodeKind::Toggle { .. } => {
                let t = self.text_size(node);
                [t[0] + TOGGLE_BOX + 6.0, t[1].max(TOGGLE_BOX)]
            }
            NodeKind::Image { native_size, .. } => *native_size,
            NodeKind::ProgressBar { .. } => DEFAULT_BAR_SIZE,
            NodeKind::Slider { .. } => DEFAULT_SLIDER_SIZE,
            NodeKind::Spacer => [0.0, 0.0],
            NodeKind::Panel => self.children_union(node),
            NodeKind::HStack { spacing, .. } => self.children_sum(node, 0, *spacing),
            NodeKind::VStack { spacing, .. } => self.children_sum(node, 1, *spacing),
            NodeKind::Grid { columns, spacing } => self.grid_intrinsic(node, *columns, *spacing),
        };
        [content[0] + pad[0], content[1] + pad[1]]
    }

    fn outer(&self, child: &UiNode, axis: usize) -> f32 {
        self.preferred(child, axis).unwrap_or(0.0) + child.margin.along(axis)
    }

    fn visible_children<'n>(&self, node: &'n UiNode) -> Vec<&'n UiNode> {
        node.children.iter().filter(|c| self.is_visible(c)).collect()
    }

    fn children_union(&self, node: &UiNode) -> [f32; 2] {
        let kids = self.visible_children(node);
        let mut size = [0.0f32; 2];
        for k in kids {
            for a in 0..2 {
                size[a] = size[a].max(self.outer(k, a));
            }
        }
        size
    }

    /// Stack along `m`: sum of main sizes + spacing; max of cross sizes.
    fn children_sum(&self, node: &UiNode, m: usize, spacing: f32) -> [f32; 2] {
        let kids = self.visible_children(node);
        let mut size = [0.0f32; 2];
        for k in &kids {
            size[m] += self.outer(k, m);
            size[1 - m] = size[1 - m].max(self.outer(k, 1 - m));
        }
        if kids.len() > 1 {
            size[m] += spacing * (kids.len() - 1) as f32;
        }
        size
    }

    fn grid_intrinsic(&self, node: &UiNode, columns: u32, spacing: [f32; 2]) -> [f32; 2] {
        let kids = self.visible_children(node);
        let cols = columns.max(1) as usize;
        if kids.is_empty() {
            return [0.0, 0.0];
        }
        let rows = (kids.len() + cols - 1) / cols;
        let mut col_w = vec![0.0f32; cols];
        let mut row_h = vec![0.0f32; rows];
        for (i, k) in kids.iter().enumerate() {
            col_w[i % cols] = col_w[i % cols].max(self.outer(k, 0));
            row_h[i / cols] = row_h[i / cols].max(self.outer(k, 1));
        }
        [
            col_w.iter().sum::<f32>() + spacing[0] * (cols - 1) as f32,
            row_h.iter().sum::<f32>() + spacing[1] * (rows - 1) as f32,
        ]
    }
}

// ---- helpers for editing tools ------------------------------------------------

/// Anchor preset along one axis (Unity-style 4×4 grid: 3 positions + stretch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorAxis {
    Start,
    Middle,
    End,
    Stretch,
}

/// Re-anchor `node` to a preset while keeping its on-screen rect unchanged.
///
/// `current` is the node's rect and `parent_content` its parent's content
/// rect, both in design units (points ÷ `LayoutResult::scale`).
pub fn apply_anchor_preset(node: &mut UiNode, h: AnchorAxis, v: AnchorAxis, current: Rect, parent_content: Rect) {
    for (axis, preset) in [(0usize, h), (1usize, v)] {
        let (a0, a1, pivot) = match preset {
            AnchorAxis::Start => (0.0, 0.0, 0.0),
            AnchorAxis::Middle => (0.5, 0.5, 0.5),
            AnchorAxis::End => (1.0, 1.0, 1.0),
            AnchorAxis::Stretch => (0.0, 1.0, 0.5),
        };
        node.anchor_min[axis] = a0;
        node.anchor_max[axis] = a1;
        node.pivot[axis] = pivot;
        let size = current.size(axis);
        let mode = node.size_mode(axis);
        let new_mode = match (preset, mode) {
            (AnchorAxis::Stretch, _) => SizeMode::Fill,
            (_, SizeMode::Fill) => SizeMode::Fixed(size),
            (_, other) => other,
        };
        if axis == 0 { node.width = new_mode } else { node.height = new_mode }
        set_offsets_for_rect(node, axis, current, parent_content);
    }
}

/// Compute `offset_min/max` on one axis so the node covers `target`
/// given its current anchors, pivot and size mode.
pub fn set_offsets_for_rect(node: &mut UiNode, axis: usize, target: Rect, parent_content: Rect) {
    let area_origin = parent_content.origin(axis) + node.margin.start(axis);
    let area_len = (parent_content.size(axis) - node.margin.along(axis)).max(0.0);
    let a0 = area_origin + area_len * node.anchor_min[axis];
    let a1 = area_origin + area_len * node.anchor_max[axis];
    let t0 = target.origin(axis);
    let t1 = t0 + target.size(axis);
    match node.size_mode(axis) {
        SizeMode::Fill => {
            node.offset_min[axis] = t0 - a0;
            node.offset_max[axis] = t1 - a1;
        }
        _ => {
            let pivot = node.pivot[axis];
            let reference = a0 + (a1 - a0) * pivot;
            node.offset_min[axis] = t0 + pivot * target.size(axis) - reference;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 units per char wide, `font_size` tall: easy arithmetic.
    struct TenPerChar;
    impl TextMeasure for TenPerChar {
        fn measure(&self, text: &str, font_size: f32) -> [f32; 2] {
            [text.chars().count() as f32 * 10.0, font_size]
        }
    }

    fn doc(root: UiNode) -> UiDoc {
        UiDoc::new("t", root)
    }

    fn solve_at(d: &UiDoc, w: f32, h: f32) -> LayoutResult {
        solve(d, &LayoutParams::new([w, h]).with_measure(&TenPerChar))
    }

    fn rect_of(d: &UiDoc, r: &LayoutResult, name: &str) -> Rect {
        let mut id = None;
        d.root.walk(&mut |n| {
            if n.name == name {
                id = Some(n.id);
            }
        });
        r.rect(id.unwrap_or_else(|| panic!("no node {name}"))).unwrap_or_else(|| panic!("{name} not laid out"))
    }

    fn assert_rect(actual: Rect, x: f32, y: f32, w: f32, h: f32) {
        let ok = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(ok(actual.x, x) && ok(actual.y, y) && ok(actual.w, w) && ok(actual.h, h), "expected ({x},{y},{w},{h}) got {actual:?}");
    }

    fn fixed(w: f32, h: f32) -> UiNode {
        UiNode::new(NodeKind::Panel).with_size(SizeMode::Fixed(w), SizeMode::Fixed(h))
    }

    fn vstack() -> UiNode {
        UiNode::new(NodeKind::VStack { spacing: 0.0, main_align: Align::Start, cross_align: Align::Start })
    }
    fn hstack() -> UiNode {
        UiNode::new(NodeKind::HStack { spacing: 0.0, main_align: Align::Start, cross_align: Align::Start })
    }
    fn set_spacing(n: &mut UiNode, s: f32) {
        match &mut n.kind {
            NodeKind::HStack { spacing, .. } | NodeKind::VStack { spacing, .. } => *spacing = s,
            _ => panic!(),
        }
    }

    #[test]
    fn root_fills_viewport() {
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root"));
        let r = solve_at(&d, 800.0, 600.0);
        assert_rect(rect_of(&d, &r, "root"), 0.0, 0.0, 800.0, 600.0);
    }

    #[test]
    fn point_anchors_center_and_corners() {
        let root = UiNode::new(NodeKind::Panel)
            .with_name("root")
            .with_child(fixed(100.0, 50.0).with_name("center").at([0.5, 0.5], [0.0, 0.0]))
            .with_child(fixed(40.0, 40.0).with_name("tl").at([0.0, 0.0], [10.0, 20.0]))
            .with_child(fixed(40.0, 40.0).with_name("br").at([1.0, 1.0], [-10.0, -20.0]));
        let d = doc(root);
        let r = solve_at(&d, 800.0, 600.0);
        assert_rect(rect_of(&d, &r, "center"), 350.0, 275.0, 100.0, 50.0);
        assert_rect(rect_of(&d, &r, "tl"), 10.0, 20.0, 40.0, 40.0);
        assert_rect(rect_of(&d, &r, "br"), 750.0, 540.0, 40.0, 40.0);
    }

    #[test]
    fn stretch_anchors_with_offsets_act_like_pixel_margins() {
        let mut child = UiNode::new(NodeKind::Panel).with_name("c");
        child.offset_min = [10.0, 20.0];
        child.offset_max = [-30.0, -40.0];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(child));
        let r = solve_at(&d, 800.0, 600.0);
        assert_rect(rect_of(&d, &r, "c"), 10.0, 20.0, 760.0, 540.0);
    }

    #[test]
    fn partial_anchors_and_margin() {
        let mut child = UiNode::new(NodeKind::Panel).with_name("c");
        child.anchor_min = [0.25, 0.0];
        child.anchor_max = [0.75, 1.0];
        child.margin = Insets::uniform(10.0);
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(child));
        let r = solve_at(&d, 800.0, 600.0);
        // area = 780x580 starting at 10 → x: 10+195=205 .. 10+585=595
        assert_rect(rect_of(&d, &r, "c"), 205.0, 10.0, 390.0, 580.0);
    }

    #[test]
    fn pivot_positions_fixed_node() {
        let mut n = fixed(100.0, 100.0).with_name("c");
        n.anchor_min = [0.0, 0.0];
        n.anchor_max = [0.0, 0.0];
        n.pivot = [1.0, 1.0];
        n.offset_min = [200.0, 200.0];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(n));
        let r = solve_at(&d, 800.0, 600.0);
        assert_rect(rect_of(&d, &r, "c"), 100.0, 100.0, 100.0, 100.0);
    }

    #[test]
    fn nested_panels_use_parent_content_rect() {
        let mut outer = fixed(400.0, 300.0).with_name("outer").at([0.5, 0.5], [0.0, 0.0]);
        outer.padding = Insets::uniform(20.0);
        let mut inner = UiNode::new(NodeKind::Panel).with_name("inner");
        inner.anchor_min = [0.5, 0.0];
        inner.anchor_max = [1.0, 0.5];
        outer.children.push(inner);
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(outer));
        let r = solve_at(&d, 800.0, 600.0);
        assert_rect(rect_of(&d, &r, "outer"), 200.0, 150.0, 400.0, 300.0);
        // content = (220,170,360,260); inner = right half of width, top half of height
        assert_rect(rect_of(&d, &r, "inner"), 400.0, 170.0, 180.0, 130.0);
    }

    #[test]
    fn resolution_change_16_9_vs_4_3() {
        let root = UiNode::new(NodeKind::Panel)
            .with_name("root")
            .with_child(fixed(100.0, 30.0).with_name("corner").at([1.0, 0.0], [-10.0, 10.0]))
            .with_child({
                let mut bar = UiNode::new(NodeKind::Panel).with_name("bar");
                bar.anchor_min = [0.1, 1.0];
                bar.anchor_max = [0.9, 1.0];
                bar.pivot = [0.5, 1.0];
                bar.height = SizeMode::Fixed(40.0);
                bar
            });
        let d = doc(root);
        let wide = solve_at(&d, 1920.0, 1080.0);
        let box43 = solve_at(&d, 1024.0, 768.0);
        assert_rect(rect_of(&d, &wide, "corner"), 1810.0, 10.0, 100.0, 30.0);
        assert_rect(rect_of(&d, &box43, "corner"), 914.0, 10.0, 100.0, 30.0);
        assert_rect(rect_of(&d, &wide, "bar"), 192.0, 1040.0, 1536.0, 40.0);
        assert_rect(rect_of(&d, &box43, "bar"), 102.4, 728.0, 819.2, 40.0);
    }

    #[test]
    fn scale_with_screen_keeps_proportions() {
        let mut d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(fixed(192.0, 108.0).with_name("c").at([0.0, 0.0], [0.0, 0.0])));
        d.scale_mode = ScaleMode::ScaleWithScreen { reference: [1920.0, 1080.0], match_height: 0.5 };
        let half = solve_at(&d, 960.0, 540.0);
        assert!((half.scale - 0.5).abs() < 1e-5);
        assert_rect(rect_of(&d, &half, "c"), 0.0, 0.0, 96.0, 54.0);
        // Root still covers the entire screen in points.
        assert_rect(rect_of(&d, &half, "root"), 0.0, 0.0, 960.0, 540.0);
    }

    #[test]
    fn ui_scale_multiplies_and_safe_area_insets_root() {
        let mut d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(fixed(50.0, 50.0).with_name("c").at([0.0, 0.0], [0.0, 0.0])));
        let r = solve(&d, &LayoutParams::new([800.0, 600.0]).with_ui_scale(2.0).with_measure(&TenPerChar));
        assert_rect(rect_of(&d, &r, "c"), 0.0, 0.0, 100.0, 100.0);
        assert_rect(rect_of(&d, &r, "root"), 0.0, 0.0, 800.0, 600.0);

        let safe = Insets::new(40.0, 20.0, 40.0, 30.0);
        let ignored = solve(&d, &LayoutParams::new([800.0, 600.0]).with_safe_area(safe));
        assert_rect(rect_of(&d, &ignored, "root"), 0.0, 0.0, 800.0, 600.0);
        d.respect_safe_area = true;
        let used = solve(&d, &LayoutParams::new([800.0, 600.0]).with_safe_area(safe));
        assert_rect(rect_of(&d, &used, "root"), 40.0, 20.0, 720.0, 550.0);
    }

    #[test]
    fn hstack_fixed_and_weighted_fill_with_spacing_and_margin() {
        let mut stack = hstack().with_name("s");
        set_spacing(&mut stack, 10.0);
        let mut a = fixed(100.0, 50.0).with_name("a");
        a.margin = Insets::new(5.0, 0.0, 5.0, 0.0);
        let mut b = UiNode::new(NodeKind::Panel).with_name("b").with_size(SizeMode::Fill, SizeMode::Fixed(20.0));
        b.fill_weight = 1.0;
        let mut c = UiNode::new(NodeKind::Panel).with_name("c").with_size(SizeMode::Fill, SizeMode::Fill);
        c.fill_weight = 2.0;
        stack.children = vec![a, b, c];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(stack));
        let r = solve_at(&d, 800.0, 600.0);
        // used = 100 + 10 (margins) + 20 (spacing) = 130; leftover 670 → b 223.33, c 446.67
        assert_rect(rect_of(&d, &r, "a"), 5.0, 0.0, 100.0, 50.0);
        assert_rect(rect_of(&d, &r, "b"), 120.0, 0.0, 670.0 / 3.0, 20.0);
        assert_rect(rect_of(&d, &r, "c"), 120.0 + 670.0 / 3.0 + 10.0, 0.0, 670.0 * 2.0 / 3.0, 600.0);
    }

    #[test]
    fn vstack_fit_children_with_spacing() {
        let label = |name: &str, text: &str| UiNode::new(NodeKind::Label { text: text.into() }).with_name(name).with_size(SizeMode::Fit, SizeMode::Fit);
        let mut stack = vstack().with_name("s");
        set_spacing(&mut stack, 4.0);
        stack.children = vec![label("l1", "hello"), label("l2", "hi")];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(stack));
        let r = solve_at(&d, 200.0, 100.0);
        // font size default 14 → height 14; widths 50 and 20
        assert_rect(rect_of(&d, &r, "l1"), 0.0, 0.0, 50.0, 14.0);
        assert_rect(rect_of(&d, &r, "l2"), 0.0, 18.0, 20.0, 14.0);
    }

    #[test]
    fn vstack_alignment_variants() {
        let mk = |main: Align, cross: Align| {
            let mut s = UiNode::new(NodeKind::VStack { spacing: 0.0, main_align: main, cross_align: cross }).with_name("s");
            s.children = vec![fixed(40.0, 20.0).with_name("a")];
            doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(s))
        };
        let d = mk(Align::Center, Align::Center);
        assert_rect(rect_of(&d, &solve_at(&d, 200.0, 100.0), "a"), 80.0, 40.0, 40.0, 20.0);
        let d = mk(Align::End, Align::End);
        assert_rect(rect_of(&d, &solve_at(&d, 200.0, 100.0), "a"), 160.0, 80.0, 40.0, 20.0);
        // Stretch widens Fit children but leaves Fixed ones alone
        let mut s = UiNode::new(NodeKind::VStack { spacing: 0.0, main_align: Align::Start, cross_align: Align::Stretch }).with_name("s");
        s.children = vec![
            UiNode::new(NodeKind::Label { text: "x".into() }).with_name("fit").with_size(SizeMode::Fit, SizeMode::Fit),
            fixed(40.0, 20.0).with_name("fx"),
        ];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(s));
        let r = solve_at(&d, 200.0, 100.0);
        assert_rect(rect_of(&d, &r, "fit"), 0.0, 0.0, 200.0, 14.0);
        assert_rect(rect_of(&d, &r, "fx"), 0.0, 14.0, 40.0, 20.0);
    }

    #[test]
    fn grid_two_columns_rows_and_spacing() {
        let grid = UiNode::new(NodeKind::Grid { columns: 2, spacing: [10.0, 6.0] })
            .with_name("g")
            .with_child(fixed(0.0, 30.0).with_name("c0").with_size(SizeMode::Fill, SizeMode::Fixed(30.0)))
            .with_child(fixed(0.0, 20.0).with_name("c1").with_size(SizeMode::Fill, SizeMode::Fixed(20.0)))
            .with_child(fixed(50.0, 10.0).with_name("c2"));
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(grid));
        let r = solve_at(&d, 210.0, 200.0);
        // cell_w = (210-10)/2 = 100; row0 h = 30; row1 h = 10
        assert_rect(rect_of(&d, &r, "c0"), 0.0, 0.0, 100.0, 30.0);
        assert_rect(rect_of(&d, &r, "c1"), 110.0, 0.0, 100.0, 20.0);
        assert_rect(rect_of(&d, &r, "c2"), 0.0, 36.0, 50.0, 10.0);
    }

    #[test]
    fn grid_fit_size_from_cells() {
        let cell = |n: &str, w: f32, h: f32| fixed(w, h).with_name(n);
        let mut grid = UiNode::new(NodeKind::Grid { columns: 2, spacing: [4.0, 4.0] }).with_name("g").with_size(SizeMode::Fit, SizeMode::Fit);
        grid.anchor_max = [0.0, 0.0];
        grid.pivot = [0.0, 0.0];
        grid.children = vec![cell("a", 30.0, 10.0), cell("b", 50.0, 20.0), cell("c", 40.0, 5.0)];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(grid));
        let r = solve_at(&d, 400.0, 400.0);
        // col widths: max(30,40)=40, 50 → 40+4+50 = 94; rows: 20, 5 → 29
        assert_rect(rect_of(&d, &r, "g"), 0.0, 0.0, 94.0, 29.0);
    }

    #[test]
    fn fit_panel_wraps_nested_stack_plus_padding() {
        let btn = |n: &str, t: &str| {
            let mut b = UiNode::new(NodeKind::Button { text: t.into(), on_click: None }).with_name(n).with_size(SizeMode::Fill, SizeMode::Fit);
            b.padding = Insets::new(8.0, 4.0, 8.0, 4.0);
            b
        };
        let mut stack = vstack().with_name("stack").with_size(SizeMode::Fill, SizeMode::Fit);
        set_spacing(&mut stack, 6.0);
        stack.children = vec![btn("b1", "Resume"), btn("b2", "Quit")];
        let mut panel = UiNode::new(NodeKind::Panel).with_name("win").with_size(SizeMode::Fixed(200.0), SizeMode::Fit).at([0.5, 0.5], [0.0, 0.0]);
        panel.padding = Insets::uniform(10.0);
        panel.children = vec![stack];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(panel));
        let r = solve_at(&d, 800.0, 600.0);
        // buttons are 14+8 = 22 tall each; stack = 22+6+22 = 50; panel = 50+20 = 70
        assert_rect(rect_of(&d, &r, "win"), 300.0, 265.0, 200.0, 70.0);
        assert_rect(rect_of(&d, &r, "b1"), 310.0, 275.0, 180.0, 22.0);
        assert_rect(rect_of(&d, &r, "b2"), 310.0, 303.0, 180.0, 22.0);
    }

    #[test]
    fn invisible_nodes_take_no_space_and_are_omitted() {
        let mut hidden = fixed(100.0, 30.0).with_name("hidden");
        hidden.visible = false;
        let mut stack = vstack().with_name("s");
        stack.children = vec![hidden, fixed(100.0, 30.0).with_name("a")];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(stack));
        let r = solve_at(&d, 200.0, 200.0);
        assert_rect(rect_of(&d, &r, "a"), 0.0, 0.0, 100.0, 30.0);
        let hidden_id = d.root.children[0].children[0].id;
        assert!(r.rect(hidden_id).is_none());
    }

    #[test]
    fn visibility_binding_changes_layout() {
        let a = fixed(100.0, 30.0).with_name("a").with_binding("visible", "show_a");
        let b = fixed(100.0, 30.0).with_name("b");
        let mut stack = vstack().with_name("s");
        stack.children = vec![a, b];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(stack));
        let mut bind = UiBindings::new();
        bind.set_bool("show_a", true);
        let shown = solve(&d, &LayoutParams::new([200.0, 200.0]).with_bindings(&bind));
        assert_rect(rect_of(&d, &shown, "b"), 0.0, 30.0, 100.0, 30.0);
        bind.set_bool("show_a", false);
        let hidden = solve(&d, &LayoutParams::new([200.0, 200.0]).with_bindings(&bind));
        assert_rect(rect_of(&d, &hidden, "b"), 0.0, 0.0, 100.0, 30.0);
    }

    #[test]
    fn bound_text_changes_fit_width() {
        let label = UiNode::new(NodeKind::Label { text: "".into() }).with_name("l").with_size(SizeMode::Fit, SizeMode::Fit).at([0.0, 0.0], [0.0, 0.0]).with_binding("text", "{hp}/{max}");
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(label));
        let mut b = UiBindings::new();
        b.set_f32("hp", 5.0);
        b.set_f32("max", 10.0);
        let r1 = solve(&d, &LayoutParams::new([200.0, 200.0]).with_bindings(&b).with_measure(&TenPerChar));
        assert_rect(rect_of(&d, &r1, "l"), 0.0, 0.0, 40.0, 14.0);
        b.set_f32("hp", 100.0);
        let r2 = solve(&d, &LayoutParams::new([200.0, 200.0]).with_bindings(&b).with_measure(&TenPerChar));
        assert_rect(rect_of(&d, &r2, "l"), 0.0, 0.0, 60.0, 14.0);
    }

    #[test]
    fn z_order_sorts_siblings_and_hit_test_picks_topmost() {
        let mut low = fixed(100.0, 100.0).with_name("low").at([0.0, 0.0], [0.0, 0.0]);
        low.z = 5;
        let high = fixed(100.0, 100.0).with_name("high").at([0.0, 0.0], [50.0, 50.0]); // z 0, later in tree
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(low).with_child(high));
        let r = solve_at(&d, 400.0, 400.0);
        let names: Vec<String> = r.entries.iter().map(|e| d.find(e.id).unwrap().name.clone()).collect();
        assert_eq!(names, vec!["root", "high", "low"]);
        // overlap area (60,60) → "low" has the higher z so it is on top
        let hit = r.hit_test([60.0, 60.0]).unwrap();
        assert_eq!(d.find(hit).unwrap().name, "low");
        let hit = r.hit_test([140.0, 140.0]).unwrap();
        assert_eq!(d.find(hit).unwrap().name, "high");
        assert!(r.hit_test([390.0, 5.0]).map(|id| d.find(id).unwrap().name.clone()) == Some("root".into()));
    }

    #[test]
    fn entries_record_parent_depth_and_content() {
        let mut p = fixed(100.0, 100.0).with_name("p").at([0.0, 0.0], [0.0, 0.0]);
        p.padding = Insets::uniform(10.0);
        p.children = vec![fixed(10.0, 10.0).with_name("k").at([0.0, 0.0], [0.0, 0.0])];
        let d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(p));
        let r = solve_at(&d, 300.0, 300.0);
        let pid = d.root.children[0].id;
        let e = r.entry(pid).unwrap();
        assert_eq!((e.depth, e.parent), (1, Some(d.root.id)));
        assert_rect(e.content, 10.0, 10.0, 80.0, 80.0);
        assert_rect(rect_of(&d, &r, "k"), 10.0, 10.0, 10.0, 10.0);
    }

    #[test]
    fn preset_keeps_rect_and_roundtrips_through_solver() {
        // A free node at (100,60) size 80x40 in a 400x300 parent; try every preset.
        let presets = [AnchorAxis::Start, AnchorAxis::Middle, AnchorAxis::End, AnchorAxis::Stretch];
        for &h in &presets {
            for &v in &presets {
                let child = fixed(80.0, 40.0).with_name("c").at([0.0, 0.0], [100.0, 60.0]);
                let mut d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(child));
                let before = rect_of(&d, &solve_at(&d, 400.0, 300.0), "c");
                let cid = d.root.children[0].id;
                let parent = Rect::new(0.0, 0.0, 400.0, 300.0);
                apply_anchor_preset(d.find_mut(cid).unwrap(), h, v, before, parent);
                let after = rect_of(&d, &solve_at(&d, 400.0, 300.0), "c");
                assert_rect(after, before.x, before.y, before.w, before.h);
            }
        }
    }

    #[test]
    fn preset_pins_to_corner_when_resolution_changes() {
        let child = fixed(80.0, 40.0).with_name("c").at([0.0, 0.0], [100.0, 60.0]);
        let mut d = doc(UiNode::new(NodeKind::Panel).with_name("root").with_child(child));
        let cid = d.root.children[0].id;
        apply_anchor_preset(d.find_mut(cid).unwrap(), AnchorAxis::End, AnchorAxis::End, Rect::new(100.0, 60.0, 80.0, 40.0), Rect::new(0.0, 0.0, 400.0, 300.0));
        // Still 220 from the right and 200 from the bottom at any size.
        assert_rect(rect_of(&d, &solve_at(&d, 800.0, 600.0), "c"), 500.0, 360.0, 80.0, 40.0);
    }
}
