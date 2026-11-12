//! Data bindings (named values the game sets) and the event queue (what the
//! UI tells the game).
//!
//! Supported binding properties on a node (`node.bindings["prop"] = "..."`):
//! * `text`    – a format template, e.g. `"{player.health}/{player.max}"`
//!               (Label, Button, Toggle, ProgressBar overlay text).
//!               `{name:.1}` formats a number with 1 decimal. `{{`/`}}` escape braces.
//! * `value`, `min`, `max` – value name; for ProgressBar / Slider.
//! * `checked` – value name; for Toggle.
//! * `visible`, `enabled` – value name (truthy), optionally prefixed with `!` to negate.
//! * `opacity` – value name (number 0..1).
//! * `class`   – value name holding text; its whitespace-separated words are
//!               style classes added after the node's own `classes`, so a bar
//!               can turn red when the bound value is `"low"` and the stylesheet
//!               has a `low` class.

use std::collections::{HashMap, VecDeque};

use super::model::{NodeId, NodeKind, UiNode};

#[derive(Clone, Debug, PartialEq)]
pub enum UiValue {
    Float(f32),
    Bool(bool),
    Text(String),
}

impl UiValue {
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            UiValue::Float(f) => Some(*f),
            UiValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            UiValue::Text(t) => t.trim().parse().ok(),
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            UiValue::Float(f) => *f != 0.0,
            UiValue::Bool(b) => *b,
            UiValue::Text(t) => !t.is_empty() && t != "false" && t != "0",
        }
    }

    /// Text form; `precision` applies to floats only.
    pub fn display(&self, precision: Option<usize>) -> String {
        match self {
            UiValue::Float(f) => match precision {
                Some(p) => format!("{f:.p$}"),
                None if f.fract() == 0.0 => format!("{f:.0}"),
                None => {
                    let s = format!("{f:.2}");
                    s.trim_end_matches('0').trim_end_matches('.').to_string()
                }
            },
            UiValue::Bool(b) => b.to_string(),
            UiValue::Text(t) => t.clone(),
        }
    }
}

/// Named values the game publishes to the UI each frame, e.g. `player.health`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiBindings {
    values: HashMap<String, UiValue>,
}

impl UiBindings {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&mut self, name: &str, value: UiValue) {
        self.values.insert(name.to_string(), value);
    }
    pub fn set_f32(&mut self, name: &str, v: f32) {
        self.set(name, UiValue::Float(v));
    }
    pub fn set_bool(&mut self, name: &str, v: bool) {
        self.set(name, UiValue::Bool(v));
    }
    pub fn set_text(&mut self, name: &str, v: &str) {
        self.set(name, UiValue::Text(v.to_string()));
    }
    pub fn get(&self, name: &str) -> Option<&UiValue> {
        self.values.get(name)
    }
    pub fn get_f32(&self, name: &str) -> Option<f32> {
        self.get(name).and_then(|v| v.as_f32())
    }
    pub fn remove(&mut self, name: &str) {
        self.values.remove(name);
    }
    /// Remove every value whose name starts with `prefix` (e.g. `"hud."`).
    pub fn remove_prefix(&mut self, prefix: &str) {
        self.values.retain(|k, _| !k.starts_with(prefix));
    }
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.values.keys()
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Truthiness of `"name"` or `"!name"`. `None` if the value is unknown.
    pub fn eval_bool(&self, expr: &str) -> Option<bool> {
        let (negate, name) = match expr.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, expr.trim()),
        };
        self.get(name).map(|v| v.as_bool() != negate)
    }

    /// Fill `{name}` / `{name:.N}` placeholders in `template`. Unknown names
    /// are left as written so a typo is visible on screen.
    pub fn format(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '{' => {
                    let mut inner = String::new();
                    let mut closed = false;
                    for c2 in chars.by_ref() {
                        if c2 == '}' {
                            closed = true;
                            break;
                        }
                        inner.push(c2);
                    }
                    if !closed {
                        out.push('{');
                        out.push_str(&inner);
                        continue;
                    }
                    let (name, precision) = match inner.split_once(":.") {
                        Some((n, p)) => (n.trim(), p.trim().parse::<usize>().ok()),
                        None => (inner.trim(), None),
                    };
                    match self.get(name) {
                        Some(v) => out.push_str(&v.display(precision)),
                        None => {
                            out.push('{');
                            out.push_str(&inner);
                            out.push('}');
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }
}

// ---- resolving node properties through bindings ---------------------------

fn bound<'a>(node: &UiNode, prop: &str, b: Option<&'a UiBindings>) -> Option<&'a UiValue> {
    let name = node.bindings.get(prop)?;
    b?.get(name.trim())
}

/// Text to display, with the `text` binding (a template) applied.
pub fn resolve_text(node: &UiNode, b: Option<&UiBindings>) -> String {
    if let (Some(template), Some(b)) = (node.bindings.get("text"), b) {
        return b.format(template);
    }
    match &node.kind {
        NodeKind::Label { text } | NodeKind::Button { text, .. } | NodeKind::Toggle { text, .. } => text.clone(),
        _ => String::new(),
    }
}

/// `(value, min, max)` for ProgressBar/Slider with bindings applied.
pub fn resolve_range(node: &UiNode, b: Option<&UiBindings>) -> (f32, f32, f32) {
    let (value, min, max) = match &node.kind {
        NodeKind::ProgressBar { value, min, max } | NodeKind::Slider { value, min, max, .. } => (*value, *min, *max),
        _ => (0.0, 0.0, 1.0),
    };
    let get = |prop: &str, fallback: f32| bound(node, prop, b).and_then(|v| v.as_f32()).unwrap_or(fallback);
    (get("value", value), get("min", min), get("max", max))
}

/// ProgressBar/Slider position as 0..1.
pub fn resolve_fraction(node: &UiNode, b: Option<&UiBindings>) -> f32 {
    let (v, min, max) = resolve_range(node, b);
    if max <= min {
        return 0.0;
    }
    ((v - min) / (max - min)).clamp(0.0, 1.0)
}

pub fn resolve_checked(node: &UiNode, b: Option<&UiBindings>) -> bool {
    if let (Some(expr), Some(b)) = (node.bindings.get("checked"), b) {
        if let Some(v) = b.eval_bool(expr) {
            return v;
        }
    }
    matches!(node.kind, NodeKind::Toggle { checked: true, .. })
}

pub fn resolve_visible(node: &UiNode, b: Option<&UiBindings>) -> bool {
    if !node.visible {
        return false;
    }
    match (node.bindings.get("visible"), b) {
        (Some(expr), Some(b)) => b.eval_bool(expr).unwrap_or(true),
        _ => true,
    }
}

pub fn resolve_enabled(node: &UiNode, b: Option<&UiBindings>) -> bool {
    if !node.enabled {
        return false;
    }
    match (node.bindings.get("enabled"), b) {
        (Some(expr), Some(b)) => b.eval_bool(expr).unwrap_or(true),
        _ => true,
    }
}

pub fn resolve_opacity(node: &UiNode, b: Option<&UiBindings>) -> f32 {
    let o = bound(node, "opacity", b).and_then(|v| v.as_f32()).unwrap_or(node.opacity);
    o.clamp(0.0, 1.0)
}

/// The node's style classes plus those named by its `class` binding.
pub fn resolve_classes(node: &UiNode, b: Option<&UiBindings>) -> Vec<String> {
    let mut classes = node.classes.clone();
    if let Some(v) = bound(node, "class", b) {
        classes.extend(v.display(None).split_whitespace().map(String::from));
    }
    classes
}

// ---- events -----------------------------------------------------------------

/// Something the player did to the UI, for the game to act on.
#[derive(Clone, Debug, PartialEq)]
pub struct UiEvent {
    /// Event name from the node (`on_click`, `on_change`, `on_toggle`).
    pub name: String,
    pub node: NodeId,
    /// New slider value / toggle state, if the widget has one.
    pub value: Option<UiValue>,
}

/// FIFO queue the game polls: `while let Some(e) = queue.poll() { ... }`.
#[derive(Clone, Debug, Default)]
pub struct UiEventQueue {
    queue: VecDeque<UiEvent>,
}

impl UiEventQueue {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, e: UiEvent) {
        self.queue.push_back(e);
    }
    pub fn extend(&mut self, events: impl IntoIterator<Item = UiEvent>) {
        self.queue.extend(events);
    }
    pub fn poll(&mut self) -> Option<UiEvent> {
        self.queue.pop_front()
    }
    pub fn drain(&mut self) -> Vec<UiEvent> {
        self.queue.drain(..).collect()
    }
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> UiBindings {
        let mut b = UiBindings::new();
        b.set_f32("player.health", 75.0);
        b.set_f32("player.max", 100.0);
        b.set_f32("ratio", 0.33333);
        b.set_text("player.name", "Ayu");
        b.set_bool("paused", true);
        b
    }

    #[test]
    fn format_template() {
        let b = health();
        assert_eq!(b.format("{player.health}/{player.max}"), "75/100");
        assert_eq!(b.format("{player.name}: {ratio:.1}"), "Ayu: 0.3");
        assert_eq!(b.format("{ratio}"), "0.33");
        assert_eq!(b.format("{{literal}} {paused}"), "{literal} true");
    }

    #[test]
    fn format_unknown_and_unclosed_left_visible() {
        let b = health();
        assert_eq!(b.format("{nope}!"), "{nope}!");
        assert_eq!(b.format("a {oops"), "a {oops");
    }

    #[test]
    fn eval_bool_with_negation() {
        let b = health();
        assert_eq!(b.eval_bool("paused"), Some(true));
        assert_eq!(b.eval_bool("!paused"), Some(false));
        assert_eq!(b.eval_bool("unknown"), None);
        assert_eq!(b.eval_bool("player.health"), Some(true));
    }

    #[test]
    fn progress_bar_value_binding_with_range() {
        let bar = UiNode::new(NodeKind::ProgressBar { value: 0.0, min: 0.0, max: 1.0 })
            .with_binding("value", "player.health")
            .with_binding("max", "player.max");
        let b = health();
        assert_eq!(resolve_range(&bar, Some(&b)), (75.0, 0.0, 100.0));
        assert!((resolve_fraction(&bar, Some(&b)) - 0.75).abs() < 1e-6);
        // Without bindings (or missing values) the static value is used.
        assert_eq!(resolve_range(&bar, None), (0.0, 0.0, 1.0));
        assert_eq!(resolve_range(&bar, Some(&UiBindings::new())), (0.0, 0.0, 1.0));
    }

    #[test]
    fn fraction_clamps_and_handles_empty_range() {
        let mut b = UiBindings::new();
        b.set_f32("v", 500.0);
        let bar = UiNode::new(NodeKind::ProgressBar { value: 0.0, min: 0.0, max: 10.0 }).with_binding("value", "v");
        assert_eq!(resolve_fraction(&bar, Some(&b)), 1.0);
        let bad = UiNode::new(NodeKind::ProgressBar { value: 1.0, min: 5.0, max: 5.0 });
        assert_eq!(resolve_fraction(&bad, None), 0.0);
    }

    #[test]
    fn label_text_binding() {
        let label = UiNode::new(NodeKind::Label { text: "static".into() }).with_binding("text", "HP {player.health}");
        assert_eq!(resolve_text(&label, Some(&health())), "HP 75");
        assert_eq!(resolve_text(&label, None), "static");
    }

    #[test]
    fn visible_enabled_opacity_checked() {
        let b = health();
        let n = UiNode::new(NodeKind::Spacer).with_binding("visible", "!paused");
        assert!(!resolve_visible(&n, Some(&b)));
        assert!(resolve_visible(&n, None));
        let mut hidden = UiNode::new(NodeKind::Spacer);
        hidden.visible = false;
        assert!(!resolve_visible(&hidden, Some(&b)), "static false wins");
        let btn = UiNode::new(NodeKind::Button { text: "x".into(), on_click: None }).with_binding("enabled", "!paused");
        assert!(!resolve_enabled(&btn, Some(&b)));
        let fade = UiNode::new(NodeKind::Spacer).with_binding("opacity", "ratio");
        assert!((resolve_opacity(&fade, Some(&b)) - 0.33333).abs() < 1e-5);
        let tog = UiNode::new(NodeKind::Toggle { text: "".into(), checked: false, on_toggle: None }).with_binding("checked", "paused");
        assert!(resolve_checked(&tog, Some(&b)));
    }

    #[test]
    fn class_binding_appends_classes() {
        let mut b = UiBindings::new();
        b.set_text("bar.class", "low  pulse");
        let bar = UiNode::new(NodeKind::ProgressBar { value: 0.0, min: 0.0, max: 1.0 })
            .with_class("bar")
            .with_binding("class", "bar.class");
        assert_eq!(resolve_classes(&bar, Some(&b)), ["bar", "low", "pulse"]);
        assert_eq!(resolve_classes(&bar, None), ["bar"]);
        assert_eq!(resolve_classes(&bar, Some(&UiBindings::new())), ["bar"], "unknown value adds nothing");
    }

    #[test]
    fn event_queue_is_fifo() {
        let mut q = UiEventQueue::new();
        q.push(UiEvent { name: "a".into(), node: 1, value: None });
        q.push(UiEvent { name: "b".into(), node: 2, value: Some(UiValue::Bool(true)) });
        assert_eq!(q.len(), 2);
        assert_eq!(q.poll().unwrap().name, "a");
        assert_eq!(q.drain().len(), 1);
        assert!(q.poll().is_none() && q.is_empty());
    }
}
