//! Keyboard / gamepad focus navigation between buttons, sliders and toggles.
//!
//! Feed it abstract [`NavInput`]s (map D-pad/arrow keys/Tab/Enter to them in
//! the game). Directional moves pick the nearest focusable in that direction
//! using the solved layout; Next/Prev follow document order and wrap.

use super::bindings::{resolve_enabled, resolve_range, resolve_checked, UiBindings, UiEvent, UiValue};
use super::layout::LayoutResult;
use super::model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavInput {
    Up,
    Down,
    Left,
    Right,
    /// Tab / R1.
    Next,
    /// Shift-Tab / L1.
    Prev,
    /// Enter / A button.
    Accept,
}

/// Which widget currently has focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiFocus {
    pub focused: Option<NodeId>,
}

/// Visible, enabled, focusable nodes in document order.
pub fn focusable_nodes(doc: &UiDoc, layout: &LayoutResult, bindings: Option<&UiBindings>) -> Vec<NodeId> {
    let mut out = Vec::new();
    doc.root.walk(&mut |n| {
        if n.kind.is_focusable() && layout.entry(n.id).is_some() && resolve_enabled(n, bindings) {
            out.push(n.id);
        }
    });
    out
}

impl UiFocus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.focused = None;
    }

    /// Apply one input. Returns an event when the input activated or
    /// changed a widget (button click, toggle, slider step).
    pub fn handle(&mut self, input: NavInput, doc: &UiDoc, layout: &LayoutResult, bindings: Option<&UiBindings>) -> Option<UiEvent> {
        let list = focusable_nodes(doc, layout, bindings);
        if list.is_empty() {
            self.focused = None;
            return None;
        }
        // Focus on something that vanished (hidden, disabled) is dropped.
        if let Some(f) = self.focused {
            if !list.contains(&f) {
                self.focused = None;
            }
        }
        let Some(current) = self.focused else {
            // First input only establishes focus.
            match input {
                NavInput::Accept => {}
                NavInput::Prev => self.focused = list.last().copied(),
                _ => self.focused = list.first().copied(),
            }
            return None;
        };

        match input {
            NavInput::Next | NavInput::Prev => {
                let pos = list.iter().position(|&i| i == current).unwrap_or(0);
                let n = list.len();
                let next = if input == NavInput::Next { (pos + 1) % n } else { (pos + n - 1) % n };
                self.focused = Some(list[next]);
                None
            }
            NavInput::Accept => activate(doc, current, bindings),
            NavInput::Left | NavInput::Right if matches!(doc.find(current).map(|n| &n.kind), Some(NodeKind::Slider { .. })) => {
                let dir = if input == NavInput::Right { 1.0 } else { -1.0 };
                step_slider(doc, current, dir, bindings)
            }
            dir => {
                if let Some(target) = nearest_in_direction(dir, current, &list, layout) {
                    self.focused = Some(target);
                }
                None
            }
        }
    }
}

fn activate(doc: &UiDoc, id: NodeId, bindings: Option<&UiBindings>) -> Option<UiEvent> {
    let node = doc.find(id)?;
    match &node.kind {
        NodeKind::Button { on_click: Some(name), .. } => Some(UiEvent { name: name.clone(), node: id, value: None }),
        NodeKind::Toggle { on_toggle: Some(name), .. } => {
            Some(UiEvent { name: name.clone(), node: id, value: Some(UiValue::Bool(!resolve_checked(node, bindings))) })
        }
        _ => None,
    }
}

/// One keyboard step on a slider: a tenth of its range.
fn step_slider(doc: &UiDoc, id: NodeId, dir: f32, bindings: Option<&UiBindings>) -> Option<UiEvent> {
    let node = doc.find(id)?;
    let NodeKind::Slider { on_change: Some(name), .. } = &node.kind else { return None };
    let (value, min, max) = resolve_range(node, bindings);
    let new = (value + dir * (max - min) / 10.0).clamp(min.min(max), max.max(min));
    Some(UiEvent { name: name.clone(), node: id, value: Some(UiValue::Float(new)) })
}

/// Nearest candidate whose center lies in `dir` from the current center.
/// Score = distance along the direction + 2 × sideways drift, so a button
/// straight below beats a slightly nearer one far to the side.
fn nearest_in_direction(dir: NavInput, current: NodeId, candidates: &[NodeId], layout: &LayoutResult) -> Option<NodeId> {
    let from = layout.rect(current)?.center();
    let mut best: Option<(f32, NodeId)> = None;
    for &id in candidates {
        if id == current {
            continue;
        }
        let Some(rect) = layout.rect(id) else { continue };
        let c = rect.center();
        let (dx, dy) = (c[0] - from[0], c[1] - from[1]);
        let (along, side) = match dir {
            NavInput::Right => (dx, dy.abs()),
            NavInput::Left => (-dx, dy.abs()),
            NavInput::Down => (dy, dx.abs()),
            NavInput::Up => (-dy, dx.abs()),
            _ => continue,
        };
        if along <= 0.5 {
            continue;
        }
        let score = along + 2.0 * side;
        if best.map_or(true, |(s, _)| score < s) {
            best = Some((score, id));
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::layout::{solve, LayoutParams};

    fn button(name: &str, event: &str, x: f32, y: f32) -> UiNode {
        UiNode::new(NodeKind::Button { text: name.into(), on_click: Some(event.into()) })
            .with_name(name)
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(30.0))
            .at([0.0, 0.0], [x, y])
    }

    fn setup(children: Vec<UiNode>) -> (UiDoc, LayoutResult) {
        let mut root = UiNode::new(NodeKind::Panel).with_name("root");
        root.children = children;
        let doc = UiDoc::new("t", root);
        let layout = solve(&doc, &LayoutParams::new([800.0, 600.0]));
        (doc, layout)
    }

    fn name(doc: &UiDoc, id: Option<NodeId>) -> String {
        id.map(|i| doc.find(i).unwrap().name.clone()).unwrap_or_default()
    }

    fn menu() -> (UiDoc, LayoutResult) {
        setup(vec![button("a", "ea", 10.0, 10.0), button("b", "eb", 10.0, 60.0), button("c", "ec", 10.0, 110.0)])
    }

    #[test]
    fn first_input_establishes_focus() {
        let (d, l) = menu();
        let mut f = UiFocus::new();
        assert!(f.handle(NavInput::Down, &d, &l, None).is_none());
        assert_eq!(name(&d, f.focused), "a");
        let mut g = UiFocus::new();
        g.handle(NavInput::Prev, &d, &l, None);
        assert_eq!(name(&d, g.focused), "c");
        let mut h = UiFocus::new();
        h.handle(NavInput::Accept, &d, &l, None);
        assert_eq!(h.focused, None, "accept with no focus does nothing");
    }

    #[test]
    fn up_down_moves_and_stops_at_ends() {
        let (d, l) = menu();
        let mut f = UiFocus::new();
        f.handle(NavInput::Down, &d, &l, None);
        f.handle(NavInput::Down, &d, &l, None);
        assert_eq!(name(&d, f.focused), "b");
        f.handle(NavInput::Down, &d, &l, None);
        f.handle(NavInput::Down, &d, &l, None);
        assert_eq!(name(&d, f.focused), "c", "no wrap on directional moves");
        f.handle(NavInput::Up, &d, &l, None);
        assert_eq!(name(&d, f.focused), "b");
        f.handle(NavInput::Left, &d, &l, None);
        assert_eq!(name(&d, f.focused), "b", "nothing to the left");
    }

    #[test]
    fn next_prev_wrap_in_document_order() {
        let (d, l) = menu();
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None); // → a
        f.handle(NavInput::Prev, &d, &l, None); // wraps → c
        assert_eq!(name(&d, f.focused), "c");
        f.handle(NavInput::Next, &d, &l, None);
        assert_eq!(name(&d, f.focused), "a");
    }

    #[test]
    fn grid_navigation_prefers_aligned_neighbour() {
        let (d, l) = setup(vec![
            button("tl", "e", 10.0, 10.0),
            button("tr", "e", 300.0, 10.0),
            button("bl", "e", 10.0, 100.0),
            button("br", "e", 300.0, 100.0),
        ]);
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None);
        f.handle(NavInput::Right, &d, &l, None);
        assert_eq!(name(&d, f.focused), "tr");
        f.handle(NavInput::Down, &d, &l, None);
        assert_eq!(name(&d, f.focused), "br");
        f.handle(NavInput::Left, &d, &l, None);
        assert_eq!(name(&d, f.focused), "bl");
        f.handle(NavInput::Up, &d, &l, None);
        assert_eq!(name(&d, f.focused), "tl");
    }

    #[test]
    fn skips_hidden_and_disabled() {
        let mut hidden = button("hidden", "e", 10.0, 60.0);
        hidden.visible = false;
        let mut disabled = button("disabled", "e", 10.0, 110.0);
        disabled.enabled = false;
        let (d, l) = setup(vec![button("a", "e", 10.0, 10.0), hidden, disabled, button("d", "e", 10.0, 160.0)]);
        assert_eq!(focusable_nodes(&d, &l, None).len(), 2);
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None);
        f.handle(NavInput::Down, &d, &l, None);
        assert_eq!(name(&d, f.focused), "d");
    }

    #[test]
    fn disabled_via_binding_drops_focus() {
        let (d, _) = setup(vec![button("a", "e", 10.0, 10.0).with_binding("enabled", "on"), button("b", "e", 10.0, 60.0)]);
        let mut b = UiBindings::new();
        b.set_bool("on", true);
        let l = solve(&d, &LayoutParams::new([800.0, 600.0]).with_bindings(&b));
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, Some(&b));
        assert_eq!(name(&d, f.focused), "a");
        b.set_bool("on", false);
        f.handle(NavInput::Next, &d, &l, Some(&b));
        // focus on 'a' was dropped; the input re-establishes focus on the first remaining widget
        assert_eq!(name(&d, f.focused), "b");
    }

    #[test]
    fn accept_emits_button_event() {
        let (d, l) = menu();
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None);
        f.handle(NavInput::Down, &d, &l, None);
        let e = f.handle(NavInput::Accept, &d, &l, None).unwrap();
        assert_eq!((e.name.as_str(), e.value), ("eb", None));
        assert_eq!(e.node, f.focused.unwrap());
    }

    #[test]
    fn toggle_accept_flips_value() {
        let t = UiNode::new(NodeKind::Toggle { text: "Mute".into(), checked: true, on_toggle: Some("mute".into()) })
            .with_name("t")
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(20.0))
            .at([0.0, 0.0], [0.0, 0.0]);
        let (d, l) = setup(vec![t]);
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None);
        let e = f.handle(NavInput::Accept, &d, &l, None).unwrap();
        assert_eq!(e.value, Some(UiValue::Bool(false)));
    }

    #[test]
    fn slider_left_right_steps_and_clamps() {
        let s = UiNode::new(NodeKind::Slider { value: 0.95, min: 0.0, max: 1.0, on_change: Some("volume".into()) })
            .with_name("vol")
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(20.0))
            .at([0.0, 0.0], [0.0, 0.0]);
        let (d, l) = setup(vec![s, button("after", "e", 0.0, 60.0)]);
        let mut f = UiFocus::new();
        f.handle(NavInput::Next, &d, &l, None);
        let e = f.handle(NavInput::Right, &d, &l, None).unwrap();
        assert_eq!(e.value, Some(UiValue::Float(1.0)), "clamped to max");
        let e = f.handle(NavInput::Left, &d, &l, None).unwrap();
        match e.value {
            Some(UiValue::Float(v)) => assert!((v - 0.85).abs() < 1e-5),
            other => panic!("{other:?}"),
        }
        assert_eq!(name(&d, f.focused), "vol", "left/right do not leave a slider");
        f.handle(NavInput::Down, &d, &l, None);
        assert_eq!(name(&d, f.focused), "after");
    }

    #[test]
    fn empty_ui_has_no_focus() {
        let (d, l) = setup(vec![]);
        let mut f = UiFocus { focused: Some(5) };
        assert!(f.handle(NavInput::Next, &d, &l, None).is_none());
        assert_eq!(f.focused, None);
    }
}
