//! The sandbox's HUD and pause menu. Both are RON documents under
//! `assets/ui/` (designed in the editor's UI Designer tab); this module only
//! loads them, publishes game state as bindings, draws them through the
//! `ui_designer` renderer and turns button/toggle events into [`UiAction`]s.
//! Editing either file while the game runs hot-reloads it.

use std::path::{Path, PathBuf};

use engine::hud::HudState;
use engine::ui_designer::examples::{self, publish_hud, publish_pause_menu};
use engine::ui_designer::{ReloadStatus, UiBindings, UiEvent, UiScreen, UiValue};

/// What the player asked for through the pause menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiAction {
    Resume,
    ExitToEditor,
    RestartLevel,
    Quit,
    /// "Show frame stats" toggle; carries the new state.
    SetShowStats(bool),
}

/// Event name (`on_click` / `on_toggle` in the RON) -> game action.
pub fn action_from_event(event: &UiEvent) -> Option<UiAction> {
    match event.name.as_str() {
        "resume_game" => Some(UiAction::Resume),
        "exit_to_editor" => Some(UiAction::ExitToEditor),
        "restart_level" => Some(UiAction::RestartLevel),
        "quit_game" => Some(UiAction::Quit),
        "toggle_stats" => Some(UiAction::SetShowStats(event.value.as_ref().is_some_and(UiValue::as_bool))),
        _ => None,
    }
}

/// Everything the HUD/pause documents may bind to, gathered by the game
/// each frame.
pub struct UiSnapshot<'a> {
    pub hud: &'a HudState,
    pub level_name: &'a str,
    pub dt: f32,
    /// Hosting/joined: show the network chip with `net_status`.
    pub net_active: bool,
    pub net_status: &'a str,
    pub editor_available: bool,
    pub paused: bool,
    /// Drawn inside the docked editor's Viewport tab (no Esc/F3 hints there).
    pub docked: bool,
}

pub struct GameUi {
    pub hud: UiScreen,
    pub pause: UiScreen,
    bindings: UiBindings,
    pub show_stats: bool,
    fps: f32,
}

/// Directory holding the sandbox's UI documents.
pub fn ui_dir(asset_root: &Path) -> PathBuf {
    asset_root.join("assets").join("ui")
}

impl GameUi {
    pub fn new(asset_root: &Path) -> Self {
        let dir = ui_dir(asset_root);
        // The engine's example documents are only a fallback for a missing or
        // broken file; the shipped game files live in `assets/ui`.
        GameUi {
            hud: UiScreen::load("hud", dir.join("hud.ron"), Some(examples::hud_doc())),
            pause: UiScreen::load("pause_menu", dir.join("pause_menu.ron"), Some(examples::pause_menu_doc())),
            bindings: UiBindings::new(),
            show_stats: false,
            fps: 0.0,
        }
    }

    /// Hot reload (one `stat` per document) then publish this frame's values.
    pub fn update(&mut self, s: &UiSnapshot) {
        for (name, status) in [("hud", self.hud.poll()), ("pause_menu", self.pause.poll())] {
            if let ReloadStatus::Failed(err) = status {
                log::warn!("{name}.ron not reloaded: {err}");
            }
        }
        publish_hud(&mut self.bindings, s.hud);
        publish_pause_menu(&mut self.bindings, s.editor_available);
        // Bar turns red when health is low (`class` binding in hud.ron).
        let low = s.hud.bars.iter().any(|(n, f)| n.eq_ignore_ascii_case("health") && *f < 0.3);
        self.bindings.set_text("hud.bar.health.class", if low { "bar_low" } else { "" });

        let b = &mut self.bindings;
        let inst = if s.dt > 1e-5 { 1.0 / s.dt } else { 0.0 };
        self.fps = if self.fps == 0.0 { inst } else { self.fps + (inst - self.fps) * 0.1 };
        b.set_bool("hud.show_stats", self.show_stats);
        b.set_f32("perf.fps", self.fps);
        b.set_f32("perf.ms", s.dt * 1000.0);
        b.set_text("level.name", s.level_name);
        b.set_bool("net.active", s.net_active);
        b.set_text("net.status", s.net_status);
        b.set_bool("play.show_hint", !s.docked);
        b.set_text("play.esc_hint", if s.paused { "Esc to resume" } else { "Esc to pause" });
        b.set_text("play.f3_hint", if s.editor_available { " \u{b7} F3 stop" } else { "" });
    }

    /// Draw the HUD over `area`. It has no widgets, so no events.
    pub fn draw_hud(&self, ctx: &egui::Context, area: egui::Rect) {
        self.hud.show(ctx, &self.bindings, area);
    }

    /// Draw the pause menu over `area`; the actions the player triggered.
    pub fn draw_pause(&self, ctx: &egui::Context, area: egui::Rect) -> Vec<UiAction> {
        self.pause.show(ctx, &self.bindings, area).events.iter().filter_map(action_from_event).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, value: Option<UiValue>) -> UiEvent {
        UiEvent { name: name.into(), node: 1, value }
    }

    #[test]
    fn events_map_to_actions() {
        assert_eq!(action_from_event(&ev("resume_game", None)), Some(UiAction::Resume));
        assert_eq!(action_from_event(&ev("exit_to_editor", None)), Some(UiAction::ExitToEditor));
        assert_eq!(action_from_event(&ev("restart_level", None)), Some(UiAction::RestartLevel));
        assert_eq!(action_from_event(&ev("quit_game", None)), Some(UiAction::Quit));
        assert_eq!(action_from_event(&ev("toggle_stats", Some(UiValue::Bool(true)))), Some(UiAction::SetShowStats(true)));
        assert_eq!(action_from_event(&ev("toggle_stats", Some(UiValue::Bool(false)))), Some(UiAction::SetShowStats(false)));
        assert_eq!(action_from_event(&ev("nope", None)), None);
    }

    #[test]
    fn every_event_in_the_shipped_pause_menu_is_handled() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let doc = engine::ui_designer::load_doc(&ui_dir(root).join("pause_menu.ron")).expect("pause_menu.ron parses");
        let mut names = Vec::new();
        doc.root.walk(&mut |n| match &n.kind {
            engine::ui_designer::NodeKind::Button { on_click: Some(e), .. } => names.push(e.clone()),
            engine::ui_designer::NodeKind::Toggle { on_toggle: Some(e), .. } => names.push(e.clone()),
            _ => {}
        });
        assert_eq!(names.len(), 5, "{names:?}");
        for n in names {
            assert!(action_from_event(&ev(&n, Some(UiValue::Bool(true)))).is_some(), "unhandled event {n}");
        }
    }

    #[test]
    fn shipped_hud_parses_and_binds_published_names() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ui = GameUi::new(root);
        assert_eq!(ui.hud.source(), engine::ui_designer::ScreenSource::File, "{:?}", ui.hud.last_error());
        assert_eq!(ui.pause.source(), engine::ui_designer::ScreenSource::File, "{:?}", ui.pause.last_error());
    }
}
