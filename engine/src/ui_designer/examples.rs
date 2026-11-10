//! The two shipped example documents (`engine/assets/ui/*.ron`) plus the
//! glue that feeds them from the engine's existing `HudState` and maps
//! their events back onto `PauseMenuAction`.

use crate::hud::HudState;
use crate::ui::PauseMenuAction;

use super::bindings::{UiBindings, UiEvent};
use super::model::UiDoc;

pub const PAUSE_MENU_RON: &str = include_str!("../../assets/ui/pause_menu.ron");
pub const HUD_RON: &str = include_str!("../../assets/ui/hud.ron");

/// Bar names the shipped `hud.ron` has slots for.
pub const HUD_BAR_NAMES: [&str; 2] = ["health", "stamina"];
/// Toast slots in `hud.ron`.
pub const HUD_TOAST_SLOTS: usize = 3;

pub fn pause_menu_doc() -> UiDoc {
    UiDoc::from_ron(PAUSE_MENU_RON).expect("assets/ui/pause_menu.ron is valid")
}

pub fn hud_doc() -> UiDoc {
    UiDoc::from_ron(HUD_RON).expect("assets/ui/hud.ron is valid")
}

/// Set the bindings `pause_menu.ron` reads.
pub fn publish_pause_menu(bindings: &mut UiBindings, editor_available: bool) {
    bindings.set_bool("editor_available", editor_available);
}

/// Translate a UI event from `pause_menu.ron` into the engine's action enum.
pub fn pause_action_from_event(event: &UiEvent) -> Option<PauseMenuAction> {
    match event.name.as_str() {
        "resume_game" => Some(PauseMenuAction::Resume),
        "exit_to_editor" => Some(PauseMenuAction::ExitToEditor),
        "restart_level" => Some(PauseMenuAction::RestartLevel),
        "quit_game" => Some(PauseMenuAction::QuitGame),
        _ => None,
    }
}

/// Publish `HudState` as the `hud.*` values `hud.ron` binds to:
/// `hud.title`, `hud.has_title`, `hud.bar.<name>` (0..1), `hud.bar.<name>_pct`,
/// `hud.bar.<name>.visible`, and `hud.toast.<i>.{text,alpha,visible}`.
/// Bar names are lower-cased ("Health" → `hud.bar.health`).
pub fn publish_hud(bindings: &mut UiBindings, hud: &HudState) {
    bindings.remove_prefix("hud.");
    bindings.set_bool("hud.has_title", hud.title.is_some());
    bindings.set_text("hud.title", hud.title.as_deref().unwrap_or(""));
    for name in HUD_BAR_NAMES {
        bindings.set_bool(&format!("hud.bar.{name}.visible"), false);
    }
    for (name, fraction) in &hud.bars {
        let key = name.to_lowercase();
        bindings.set_f32(&format!("hud.bar.{key}"), *fraction);
        bindings.set_f32(&format!("hud.bar.{key}_pct"), fraction * 100.0);
        bindings.set_bool(&format!("hud.bar.{key}.visible"), true);
    }
    for slot in 0..HUD_TOAST_SLOTS {
        let toast = hud.toasts().get(slot);
        bindings.set_bool(&format!("hud.toast.{slot}.visible"), toast.is_some());
        bindings.set_text(&format!("hud.toast.{slot}.text"), toast.map_or("", |t| t.0.as_str()));
        // Same fade as the old draw_hud: alpha = min(remaining, 1).
        bindings.set_f32(&format!("hud.toast.{slot}.alpha"), toast.map_or(0.0, |t| t.1.min(1.0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::layout::{solve, LayoutParams, LayoutResult, Rect};
    use crate::ui_designer::model::*;

    fn find(doc: &UiDoc, name: &str) -> NodeId {
        let mut id = None;
        doc.root.walk(&mut |n| {
            if n.name == name {
                id = Some(n.id);
            }
        });
        id.unwrap_or_else(|| panic!("no node named {name}"))
    }

    fn rect(doc: &UiDoc, l: &LayoutResult, name: &str) -> Rect {
        l.rect(find(doc, name)).unwrap_or_else(|| panic!("{name} not visible"))
    }

    fn visible(doc: &UiDoc, l: &LayoutResult, name: &str) -> bool {
        l.rect(find(doc, name)).is_some()
    }

    #[test]
    fn shipped_documents_parse_and_have_valid_styles() {
        for doc in [pause_menu_doc(), hud_doc()] {
            assert!(doc.styles.validate().is_empty(), "{}: {:?}", doc.name, doc.styles.validate());
            let mut ids = std::collections::HashSet::new();
            doc.root.walk(&mut |n| {
                assert!(ids.insert(n.id), "duplicate id in {}", doc.name);
            });
            // They survive a save/load cycle unchanged.
            assert_eq!(UiDoc::from_ron(&doc.to_ron().unwrap()).unwrap(), doc);
        }
    }

    #[test]
    fn pause_menu_layout_in_editor_mode() {
        let doc = pause_menu_doc();
        let mut b = UiBindings::new();
        publish_pause_menu(&mut b, true);
        let l = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));
        assert!(visible(&doc, &l, "ExitToEditorButton"));
        assert!(!visible(&doc, &l, "RestartLevelButton"));

        // Fullscreen dim, window centred on screen.
        let screen = rect(&doc, &l, "Dim");
        assert_eq!((screen.w, screen.h), (1280.0, 720.0));
        let win = rect(&doc, &l, "Window");
        assert!((win.center()[0] - 640.0).abs() < 0.01 && (win.center()[1] - 360.0).abs() < 0.01);
        assert!(win.w >= 180.0, "old window had min_width 180, got {}", win.w);

        // Buttons stacked top to bottom in the original order, all inside the window,
        // each as wide as the window content.
        let order = ["ResumeButton", "ExitToEditorButton", "QuitButton"];
        let rects: Vec<Rect> = order.iter().map(|n| rect(&doc, &l, n)).collect();
        for pair in rects.windows(2) {
            assert!(pair[0].bottom() <= pair[1].y, "{pair:?} overlap");
        }
        for r in &rects {
            assert!(r.x >= win.x && r.right() <= win.right() && r.y >= win.y && r.bottom() <= win.bottom());
            assert!((r.w - (win.w - 24.0 * l.scale)).abs() < 0.01, "button fills window content width");
        }
        // Title above the first button.
        assert!(rect(&doc, &l, "Title").bottom() <= rects[0].y);
    }

    #[test]
    fn pause_menu_swaps_middle_button_in_shipped_mode() {
        let doc = pause_menu_doc();
        let mut b = UiBindings::new();
        publish_pause_menu(&mut b, false);
        let l = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));
        assert!(!visible(&doc, &l, "ExitToEditorButton"));
        assert!(visible(&doc, &l, "RestartLevelButton"));
        // Same button slot, so the window is the same height either way.
        let mut editor = UiBindings::new();
        publish_pause_menu(&mut editor, true);
        let l2 = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&editor));
        assert!((rect(&doc, &l, "Window").h - rect(&doc, &l2, "Window").h).abs() < 0.01);
    }

    #[test]
    fn pause_menu_stays_centred_at_4_3_and_scales_with_screen() {
        let doc = pause_menu_doc();
        let mut b = UiBindings::new();
        publish_pause_menu(&mut b, true);
        for (w, h) in [(1920.0, 1080.0), (1024.0, 768.0), (800.0, 600.0)] {
            let l = solve(&doc, &LayoutParams::new([w, h]).with_bindings(&b));
            let win = rect(&doc, &l, "Window");
            assert!((win.center()[0] - w / 2.0).abs() < 0.01 && (win.center()[1] - h / 2.0).abs() < 0.01, "{w}x{h}: {win:?}");
            assert!(win.right() <= w && win.bottom() <= h);
        }
        let big = solve(&doc, &LayoutParams::new([2560.0, 1440.0]).with_bindings(&b));
        let small = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));
        assert!((rect(&doc, &big, "Window").w / rect(&doc, &small, "Window").w - 2.0).abs() < 1e-3);
    }

    #[test]
    fn pause_menu_events_map_to_actions() {
        let doc = pause_menu_doc();
        let mut seen = Vec::new();
        doc.root.walk(&mut |n| {
            if let NodeKind::Button { on_click: Some(name), .. } = &n.kind {
                seen.push(pause_action_from_event(&UiEvent { name: name.clone(), node: n.id, value: None }));
            }
        });
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|a| a.is_some()), "every button event maps to an action");
        assert!(pause_action_from_event(&UiEvent { name: "other".into(), node: 1, value: None }).is_none());
    }

    #[test]
    fn hud_anchored_top_left_with_bars_and_toasts() {
        let doc = hud_doc();
        let mut hud = HudState::default();
        hud.title = Some("Level 1".into());
        hud.set_bar("Health", 0.75);
        hud.set_bar("Stamina", 0.5);
        hud.show_toast("Picked up key", 3.0);
        let mut b = UiBindings::new();
        publish_hud(&mut b, &hud);
        let l = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));

        let stack = rect(&doc, &l, "TopLeft");
        assert_eq!((stack.x, stack.y, stack.w), (12.0, 12.0, 160.0), "12px offset, bar_width 160");
        let health = rect(&doc, &l, "HealthBar");
        assert_eq!((health.x, health.w), (12.0, 160.0));
        assert!(rect(&doc, &l, "Title").bottom() <= health.y);
        assert!(health.bottom() <= rect(&doc, &l, "StaminaBar").y);
        assert!(visible(&doc, &l, "Toast0"));
        assert!(!visible(&doc, &l, "Toast1"));
        // The toast is below the bars.
        assert!(rect(&doc, &l, "StaminaBar").bottom() <= rect(&doc, &l, "Toast0").y);

        // Bound values reach the nodes.
        let bar = doc.find(find(&doc, "HealthBar")).unwrap();
        assert!((crate::ui_designer::resolve_fraction(bar, Some(&b)) - 0.75).abs() < 1e-6);
        assert_eq!(crate::ui_designer::resolve_text(bar, Some(&b)), "Health: 75%");
        let toast = doc.find(find(&doc, "Toast0")).unwrap();
        assert_eq!(crate::ui_designer::resolve_text(toast, Some(&b)), "Picked up key");
        assert_eq!(crate::ui_designer::resolve_opacity(toast, Some(&b)), 1.0);
    }

    #[test]
    fn hud_hides_missing_title_bars_and_fades_toasts() {
        let doc = hud_doc();
        let mut hud = HudState::default();
        hud.set_bar("Health", 1.0);
        hud.show_toast("fading", 0.25);
        let mut b = UiBindings::new();
        publish_hud(&mut b, &hud);
        let l = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));
        assert!(!visible(&doc, &l, "Title"), "no title set");
        assert!(visible(&doc, &l, "HealthBar"));
        assert!(!visible(&doc, &l, "StaminaBar"));
        // Health bar moves up to where the title would have been.
        assert_eq!(rect(&doc, &l, "HealthBar").y, 12.0);
        let toast = doc.find(find(&doc, "Toast0")).unwrap();
        assert_eq!(crate::ui_designer::resolve_opacity(toast, Some(&b)), 0.25);

        // Republishing after the toast expired clears old values.
        hud.tick(1.0);
        publish_hud(&mut b, &hud);
        let l = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b));
        assert!(!visible(&doc, &l, "Toast0"));
    }

    #[test]
    fn hud_pinned_to_corner_at_every_resolution_and_respects_safe_area() {
        let doc = hud_doc();
        let b = UiBindings::new();
        let wide = solve(&doc, &LayoutParams::new([1920.0, 1080.0]).with_bindings(&b));
        let tall43 = solve(&doc, &LayoutParams::new([1024.0, 768.0]).with_bindings(&b));
        // Scale factors differ (match 0.5), but both stay at scale * 12 from the corner.
        let s_wide = rect(&doc, &wide, "TopLeft");
        assert!((s_wide.x - 12.0 * wide.scale).abs() < 1e-3 && (s_wide.y - 12.0 * wide.scale).abs() < 1e-3);
        let s43 = rect(&doc, &tall43, "TopLeft");
        assert!((s43.x - 12.0 * tall43.scale).abs() < 1e-3);
        assert!(wide.scale > tall43.scale);

        let safe = Insets::new(30.0, 20.0, 30.0, 20.0);
        let with_safe = solve(&doc, &LayoutParams::new([1280.0, 720.0]).with_bindings(&b).with_safe_area(safe));
        let r = rect(&doc, &with_safe, "TopLeft");
        assert!((r.x - 42.0).abs() < 1e-3 && (r.y - 32.0).abs() < 1e-3, "{r:?}");
    }
}
