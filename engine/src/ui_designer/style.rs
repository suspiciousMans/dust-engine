//! Named style classes with inheritance and interaction-state variants.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// RGBA, each channel 0..1, non-premultiplied.
pub type Color = [f32; 4];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// A partial set of style values: `None` means "not specified here".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleProps {
    pub bg_color: Option<Color>,
    pub text_color: Option<Color>,
    pub font_size: Option<f32>,
    pub border_width: Option<f32>,
    pub border_color: Option<Color>,
    pub corner_radius: Option<f32>,
    /// Progress-bar fill, slider handle, toggle check mark.
    pub accent_color: Option<Color>,
    pub text_align: Option<TextAlign>,
}

impl StyleProps {
    /// Overlay `over` on top of `self`: any value `over` specifies wins.
    pub fn overlay(&mut self, over: &StyleProps) {
        if over.bg_color.is_some() {
            self.bg_color = over.bg_color;
        }
        if over.text_color.is_some() {
            self.text_color = over.text_color;
        }
        if over.font_size.is_some() {
            self.font_size = over.font_size;
        }
        if over.border_width.is_some() {
            self.border_width = over.border_width;
        }
        if over.border_color.is_some() {
            self.border_color = over.border_color;
        }
        if over.corner_radius.is_some() {
            self.corner_radius = over.corner_radius;
        }
        if over.accent_color.is_some() {
            self.accent_color = over.accent_color;
        }
        if over.text_align.is_some() {
            self.text_align = over.text_align;
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == StyleProps::default()
    }
}

/// One named class: base values plus per-state overrides, optionally
/// inheriting everything from `parent`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleClass {
    pub parent: Option<String>,
    pub base: StyleProps,
    pub hover: StyleProps,
    pub pressed: StyleProps,
    pub disabled: StyleProps,
    pub focused: StyleProps,
}

/// Interaction state a widget is drawn in. Several can be true at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WidgetState {
    pub hovered: bool,
    pub pressed: bool,
    pub disabled: bool,
    pub focused: bool,
}

/// Fully resolved style with concrete values.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyle {
    pub bg_color: Color,
    pub text_color: Color,
    pub font_size: f32,
    pub border_width: f32,
    pub border_color: Color,
    pub corner_radius: f32,
    pub accent_color: Color,
    pub text_align: TextAlign,
}

impl Default for ResolvedStyle {
    fn default() -> Self {
        ResolvedStyle {
            bg_color: [0.0, 0.0, 0.0, 0.0],
            text_color: [1.0, 1.0, 1.0, 1.0],
            font_size: 14.0,
            border_width: 0.0,
            border_color: [1.0, 1.0, 1.0, 1.0],
            corner_radius: 0.0,
            accent_color: [0.3, 0.7, 0.3, 1.0],
            text_align: TextAlign::Left,
        }
    }
}

/// Max depth of a `parent` chain before it is treated as a cycle.
const MAX_INHERIT_DEPTH: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleSheet {
    pub classes: BTreeMap<String, StyleClass>,
}

impl StyleSheet {
    /// Chain of classes from the root ancestor down to `name` itself.
    /// Missing parents and cycles end the chain early.
    fn chain(&self, name: &str) -> Vec<&StyleClass> {
        let mut chain = Vec::new();
        let mut visited: Vec<&str> = Vec::new();
        let mut current = self.classes.get_key_value(name);
        while let Some((key, class)) = current {
            if visited.contains(&key.as_str()) || chain.len() >= MAX_INHERIT_DEPTH {
                break;
            }
            visited.push(key.as_str());
            chain.push(class);
            current = class.parent.as_deref().and_then(|p| self.classes.get_key_value(p));
        }
        chain.reverse();
        chain
    }

    /// Merged properties of one class for a state (inheritance applied:
    /// ancestors first, so descendants override). Per class the order is
    /// base < hover < focused < pressed < disabled.
    pub fn props_for_class(&self, name: &str, state: WidgetState) -> StyleProps {
        let mut out = StyleProps::default();
        for class in self.chain(name) {
            out.overlay(&class.base);
            if state.hovered {
                out.overlay(&class.hover);
            }
            if state.focused {
                out.overlay(&class.focused);
            }
            if state.pressed {
                out.overlay(&class.pressed);
            }
            if state.disabled {
                out.overlay(&class.disabled);
            }
        }
        out
    }

    /// Resolve the style for a node: the class named after its kind (e.g.
    /// "Button", if defined) first, then the node's own classes in order.
    pub fn resolve(&self, kind_name: &str, classes: &[String], state: WidgetState) -> ResolvedStyle {
        let mut merged = self.props_for_class(kind_name, state);
        for c in classes {
            merged.overlay(&self.props_for_class(c, state));
        }
        let d = ResolvedStyle::default();
        ResolvedStyle {
            bg_color: merged.bg_color.unwrap_or(d.bg_color),
            text_color: merged.text_color.unwrap_or(d.text_color),
            font_size: merged.font_size.unwrap_or(d.font_size),
            border_width: merged.border_width.unwrap_or(d.border_width),
            border_color: merged.border_color.unwrap_or(d.border_color),
            corner_radius: merged.corner_radius.unwrap_or(d.corner_radius),
            accent_color: merged.accent_color.unwrap_or(d.accent_color),
            text_align: merged.text_align.unwrap_or(d.text_align),
        }
    }

    /// Problems worth showing in the designer: unknown parents and cycles.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, class) in &self.classes {
            if let Some(p) = &class.parent {
                if !self.classes.contains_key(p) {
                    problems.push(format!("class '{name}': unknown parent '{p}'"));
                }
            }
            // Walk up; if we come back to ourselves it's a cycle.
            let mut cur = class.parent.as_deref();
            let mut steps = 0;
            while let Some(p) = cur {
                if p == name {
                    problems.push(format!("class '{name}': inheritance cycle"));
                    break;
                }
                steps += 1;
                if steps > MAX_INHERIT_DEPTH {
                    break;
                }
                cur = self.classes.get(p).and_then(|c| c.parent.as_deref());
            }
        }
        problems
    }

    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
    }

    pub fn from_ron(text: &str) -> Result<StyleSheet, ron::error::SpannedError> {
        ron::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color = [1.0, 0.0, 0.0, 1.0];
    const BLUE: Color = [0.0, 0.0, 1.0, 1.0];
    const GREEN: Color = [0.0, 1.0, 0.0, 1.0];

    fn sheet() -> StyleSheet {
        let mut s = StyleSheet::default();
        s.classes.insert(
            "button".into(),
            StyleClass {
                base: StyleProps { bg_color: Some(RED), font_size: Some(18.0), corner_radius: Some(4.0), ..Default::default() },
                hover: StyleProps { bg_color: Some(BLUE), ..Default::default() },
                pressed: StyleProps { bg_color: Some(GREEN), ..Default::default() },
                disabled: StyleProps { text_color: Some([0.5, 0.5, 0.5, 1.0]), ..Default::default() },
                ..Default::default()
            },
        );
        s.classes.insert(
            "danger".into(),
            StyleClass {
                parent: Some("button".into()),
                base: StyleProps { font_size: Some(24.0), ..Default::default() },
                hover: StyleProps { corner_radius: Some(0.0), ..Default::default() },
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn base_values_and_defaults() {
        let r = sheet().resolve("Label", &["button".into()], WidgetState::default());
        assert_eq!(r.bg_color, RED);
        assert_eq!(r.font_size, 18.0);
        assert_eq!(r.text_color, ResolvedStyle::default().text_color, "unspecified → default");
    }

    #[test]
    fn child_inherits_and_overrides() {
        let r = sheet().resolve("Button", &["danger".into()], WidgetState::default());
        assert_eq!(r.bg_color, RED, "inherited");
        assert_eq!(r.font_size, 24.0, "overridden");
    }

    #[test]
    fn child_inherits_parent_variants() {
        let hover = WidgetState { hovered: true, ..Default::default() };
        let r = sheet().resolve("Button", &["danger".into()], hover);
        assert_eq!(r.bg_color, BLUE, "parent's hover variant applies to the child");
        assert_eq!(r.corner_radius, 0.0, "child's hover variant applies");
    }

    #[test]
    fn state_priority_pressed_beats_hover() {
        let st = WidgetState { hovered: true, pressed: true, ..Default::default() };
        assert_eq!(sheet().resolve("Button", &["button".into()], st).bg_color, GREEN);
        let dis = WidgetState { disabled: true, ..Default::default() };
        assert_eq!(sheet().resolve("Button", &["button".into()], dis).text_color, [0.5, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn later_class_wins_and_kind_class_is_first() {
        let mut s = sheet();
        s.classes.insert("Button".into(), StyleClass { base: StyleProps { bg_color: Some(GREEN), font_size: Some(30.0), ..Default::default() }, ..Default::default() });
        let r = s.resolve("Button", &["button".into()], WidgetState::default());
        assert_eq!(r.bg_color, RED, "node class beats kind class");
        assert_eq!(r.font_size, 18.0);
        let r2 = s.resolve("Button", &[], WidgetState::default());
        assert_eq!(r2.font_size, 30.0);
    }

    #[test]
    fn cycles_and_missing_parents_are_safe() {
        let mut s = StyleSheet::default();
        s.classes.insert("a".into(), StyleClass { parent: Some("b".into()), base: StyleProps { font_size: Some(1.0), ..Default::default() }, ..Default::default() });
        s.classes.insert("b".into(), StyleClass { parent: Some("a".into()), ..Default::default() });
        s.classes.insert("c".into(), StyleClass { parent: Some("nope".into()), ..Default::default() });
        assert_eq!(s.resolve("X", &["a".into()], WidgetState::default()).font_size, 1.0);
        let problems = s.validate();
        assert!(problems.iter().any(|p| p.contains("cycle")));
        assert!(problems.iter().any(|p| p.contains("unknown parent 'nope'")));
        assert!(sheet().validate().is_empty());
    }

    #[test]
    fn ron_roundtrip_and_partial_file() {
        let s = sheet();
        let back = StyleSheet::from_ron(&s.to_ron().unwrap()).unwrap();
        assert_eq!(s, back);
        let hand = StyleSheet::from_ron(r#"(classes: {"x": (base: (font_size: Some(9.0)))})"#).unwrap();
        assert_eq!(hand.classes["x"].base.font_size, Some(9.0));
    }
}
