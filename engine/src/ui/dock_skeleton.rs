//! Dock layout model for the editor shell: tab identities, layout presets,
//! RON (de)serialisation and sanity-repair of saved layouts. No egui context
//! needed, so everything here is unit-testable headless; the drawing lives in
//! `editor::shell`.

use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};

use crate::ui::theme::EditorTheme;

/// A dockable panel. `Host` tabs are supplied by the embedding game (the
/// sandbox adds its level/render panels this way) and identified by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditorTab {
    Hierarchy,
    Inspector,
    Viewport,
    Assets,
    Console,
    Profiler,
    UiDesigner,
    Settings,
    Host(String),
}

impl EditorTab {
    /// Built-in tabs, in View-menu order.
    pub const BUILTIN: [EditorTab; 8] = [
        EditorTab::Hierarchy,
        EditorTab::Inspector,
        EditorTab::Viewport,
        EditorTab::Assets,
        EditorTab::Console,
        EditorTab::Profiler,
        EditorTab::UiDesigner,
        EditorTab::Settings,
    ];

    pub fn title(&self) -> String {
        match self {
            Self::Hierarchy => "Hierarchy".into(),
            Self::Inspector => "Inspector".into(),
            Self::Viewport => "Viewport".into(),
            Self::Assets => "Assets".into(),
            Self::Console => "Console".into(),
            Self::Profiler => "Profiler".into(),
            Self::UiDesigner => "UI Designer".into(),
            Self::Settings => "Settings".into(),
            Self::Host(name) => name.clone(),
        }
    }

    /// Tabs that open as a floating window rather than into the main layout.
    pub fn opens_floating(&self) -> bool {
        matches!(self, Self::Settings)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayoutPreset {
    Default,
    Tall,
    Minimal,
}

impl LayoutPreset {
    pub const ALL: [LayoutPreset; 3] = [Self::Default, Self::Tall, Self::Minimal];

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Tall => "Tall",
            Self::Minimal => "Minimal",
        }
    }
}

/// Builds a preset. `host_tabs` are placed next to the Inspector so game
/// panels are visible without hunting for them.
///
/// * **Default** — Hierarchy left, Viewport centre, Inspector right,
///   Console/Assets/Profiler along the bottom.
/// * **Tall** — for portrait/tall monitors: Viewport on top, everything else
///   in a wide band beneath.
/// * **Minimal** — Viewport plus one tabbed side panel.
pub fn build_layout(preset: LayoutPreset, host_tabs: &[String]) -> DockState<EditorTab> {
    let host: Vec<EditorTab> = host_tabs.iter().cloned().map(EditorTab::Host).collect();
    let mut state = DockState::new(vec![EditorTab::Viewport]);
    let tree = state.main_surface_mut();
    match preset {
        LayoutPreset::Default => {
            let [centre, _left] = tree.split_left(NodeIndex::root(), 0.18, vec![EditorTab::Hierarchy]);
            let mut right = vec![EditorTab::Inspector];
            right.extend(host);
            let [centre, _right] = tree.split_right(centre, 0.76, right);
            tree.split_below(
                centre,
                0.72,
                vec![EditorTab::Console, EditorTab::Assets, EditorTab::Profiler],
            );
        }
        LayoutPreset::Tall => {
            let [_top, bottom] = tree.split_below(
                NodeIndex::root(),
                0.58,
                vec![EditorTab::Hierarchy, EditorTab::Console, EditorTab::Assets, EditorTab::Profiler],
            );
            let mut right = vec![EditorTab::Inspector];
            right.extend(host);
            tree.split_right(bottom, 0.45, right);
        }
        LayoutPreset::Minimal => {
            let mut side = vec![EditorTab::Hierarchy, EditorTab::Inspector];
            side.extend(host);
            side.push(EditorTab::Console);
            tree.split_right(NodeIndex::root(), 0.78, side);
        }
    }
    state
}

pub fn layout_to_ron(state: &DockState<EditorTab>) -> Result<String, String> {
    ron::ser::to_string_pretty(state, ron::ser::PrettyConfig::default()).map_err(|e| e.to_string())
}

pub fn layout_from_ron(text: &str) -> Result<DockState<EditorTab>, String> {
    ron::from_str(text).map_err(|e| e.to_string())
}

pub fn contains_tab(state: &DockState<EditorTab>, tab: &EditorTab) -> bool {
    state.find_tab(tab).is_some()
}

pub fn tab_count(state: &DockState<EditorTab>) -> usize {
    state.iter_all_tabs().count()
}

/// Repairs a deserialised layout: drops host tabs the game no longer
/// provides. Returns `false` (caller should fall back to a preset) when the
/// layout has no Viewport — an editor without one is unusable.
pub fn sanitize_layout(state: &mut DockState<EditorTab>, host_tabs: &[String]) -> bool {
    // `retain_tabs` mis-balances nested splits in egui_dock 0.14, so remove
    // one tab at a time through `remove_tab`.
    let stale: Vec<EditorTab> = state
        .iter_all_tabs()
        .filter_map(|(_, t)| match t {
            EditorTab::Host(n) if !host_tabs.contains(n) => Some(t.clone()),
            _ => None,
        })
        .collect();
    for t in stale {
        if let Some(loc) = state.find_tab(&t) {
            state.remove_tab(loc);
        }
    }
    contains_tab(state, &EditorTab::Viewport)
}

/// Opens `tab` if it is closed (floating for window-like tabs, otherwise next
/// to the focused tab); closes it if open. Returns the new open state.
/// The Viewport is never closed — there'd be nothing to see the scene in.
pub fn toggle_tab(state: &mut DockState<EditorTab>, tab: &EditorTab) -> bool {
    match state.find_tab(tab) {
        Some(location) => {
            if *tab != EditorTab::Viewport {
                state.remove_tab(location);
                false
            } else {
                true
            }
        }
        None => {
            open_tab(state, tab);
            true
        }
    }
}

/// Opens `tab` if closed, and focuses it either way.
pub fn open_tab(state: &mut DockState<EditorTab>, tab: &EditorTab) {
    match state.find_tab(tab) {
        Some(location) => {
            let _ = state.set_active_tab(location);
        }
        None if tab.opens_floating() => {
            state.add_window(vec![tab.clone()]);
        }
        None => state.push_to_focused_leaf(tab.clone()),
    }
}

/// egui_dock look derived from the editor theme.
pub fn dock_style(theme: &EditorTheme, egui_style: &egui::Style) -> egui_dock::Style {
    let mut s = egui_dock::Style::from_egui(egui_style);
    let header = theme.panel_header_color();
    let accent = theme.accent_color();
    s.tab_bar.bg_fill = header;
    s.tab_bar.hline_color = theme.border_color();
    s.tab_bar.height = 24.0;
    s.main_surface_border_stroke = egui::Stroke::new(1.0_f32, theme.border_color());
    s.separator.color_idle = theme.border_color();
    s.separator.color_hovered = accent;
    s.separator.color_dragged = accent;
    s.tab.active.bg_fill = theme.panel_color();
    s.tab.active.text_color = theme.foreground_color();
    s.tab.active.outline_color = theme.border_color();
    s.tab.inactive.bg_fill = header;
    s.tab.inactive.text_color = theme.muted_color();
    s.tab.focused.bg_fill = theme.panel_color();
    s.tab.focused.text_color = accent;
    s.tab.hovered.bg_fill = theme.panel_color().linear_multiply(1.15);
    s.tab.hovered.text_color = theme.foreground_color();
    s.tab.tab_body.bg_fill = theme.panel_color();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(state: &DockState<EditorTab>) -> Vec<String> {
        let mut v: Vec<String> = state.iter_all_tabs().map(|(_, t)| t.title()).collect();
        v.sort();
        v
    }

    #[test]
    fn presets_contain_expected_tabs() {
        let host = vec!["Level".to_string()];
        let d = build_layout(LayoutPreset::Default, &host);
        assert_eq!(
            names(&d),
            ["Assets", "Console", "Hierarchy", "Inspector", "Level", "Profiler", "Viewport"]
        );
        let m = build_layout(LayoutPreset::Minimal, &[]);
        assert_eq!(names(&m), ["Console", "Hierarchy", "Inspector", "Viewport"]);
        let t = build_layout(LayoutPreset::Tall, &host);
        assert!(contains_tab(&t, &EditorTab::Viewport) && contains_tab(&t, &EditorTab::Host("Level".into())));
        for p in LayoutPreset::ALL {
            assert!(contains_tab(&build_layout(p, &[]), &EditorTab::Viewport), "{p:?}");
        }
    }

    #[test]
    fn presets_differ() {
        let a = layout_to_ron(&build_layout(LayoutPreset::Default, &[])).unwrap();
        let b = layout_to_ron(&build_layout(LayoutPreset::Tall, &[])).unwrap();
        let c = layout_to_ron(&build_layout(LayoutPreset::Minimal, &[])).unwrap();
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn layout_ron_roundtrip_is_lossless() {
        let host = vec!["Render".to_string()];
        for p in LayoutPreset::ALL {
            let l = build_layout(p, &host);
            let text = layout_to_ron(&l).unwrap();
            let back = layout_from_ron(&text).unwrap();
            assert_eq!(layout_to_ron(&back).unwrap(), text, "{p:?}");
            assert_eq!(names(&back), names(&l));
        }
    }

    #[test]
    fn garbage_layout_is_an_error_not_a_panic() {
        assert!(layout_from_ron("(not a layout").is_err());
        assert!(layout_from_ron("").is_err());
    }

    #[test]
    fn sanitize_drops_unknown_host_tabs_and_requires_viewport() {
        let mut l = build_layout(LayoutPreset::Default, &["Gone".to_string(), "Kept".to_string()]);
        assert!(sanitize_layout(&mut l, &["Kept".to_string()]));
        assert!(!contains_tab(&l, &EditorTab::Host("Gone".into())));
        assert!(contains_tab(&l, &EditorTab::Host("Kept".into())));
        let mut no_vp = DockState::new(vec![EditorTab::Console]);
        assert!(!sanitize_layout(&mut no_vp, &[]));
    }

    #[test]
    fn toggle_opens_closes_but_keeps_viewport() {
        let mut l = build_layout(LayoutPreset::Minimal, &[]);
        assert!(!contains_tab(&l, &EditorTab::Profiler));
        assert!(toggle_tab(&mut l, &EditorTab::Profiler));
        assert!(contains_tab(&l, &EditorTab::Profiler));
        assert!(!toggle_tab(&mut l, &EditorTab::Profiler));
        assert!(!contains_tab(&l, &EditorTab::Profiler));
        assert!(toggle_tab(&mut l, &EditorTab::Viewport));
        assert!(contains_tab(&l, &EditorTab::Viewport));
        // Settings opens as a floating window (extra surface).
        assert!(toggle_tab(&mut l, &EditorTab::Settings));
        assert!(l.surfaces_count() > 1);
    }

    #[test]
    fn dock_style_follows_theme() {
        let mut t = EditorTheme::builtin_default();
        t.panel_header = [1.0, 0.0, 0.0];
        let s = dock_style(&t, &egui::Style::default());
        assert_eq!(s.tab_bar.bg_fill, t.panel_header_color());
    }
}
