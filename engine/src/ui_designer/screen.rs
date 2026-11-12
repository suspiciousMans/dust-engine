//! A UI document in a running game: loaded from a `.ron` file at startup,
//! hot-reloaded when the file changes on disk, drawn each frame with the
//! game's [`UiBindings`], with clicks coming back as named [`UiEvent`]s.
//!
//! Typical game wiring (see `games/sandbox`):
//! ```ignore
//! // init
//! let hud = UiScreen::load("hud", assets.join("ui/hud.ron"), Some(examples::hud_doc()));
//! // every frame
//! hud.poll();                                   // one stat(); reloads on change
//! bindings.set_f32("player.health", hp);        // publish game state
//! let r = hud.show(ctx, &bindings, screen_rect); // draw + collect events
//! for e in r.events { match e.name.as_str() { "resume_game" => ..., _ => {} } }
//! ```
//! Nothing here touches GL; `show` only needs an `egui::Context`, so the
//! loader and the hot-reload logic are testable headlessly.

use std::path::{Path, PathBuf};

use super::bindings::UiBindings;
use super::files::{load_doc, UiDocWatcher};
use super::model::UiDoc;
use super::render::{draw_ui_tree_with, DrawParams, UiFrameResult};

/// What [`UiScreen::poll`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum ReloadStatus {
    /// File unchanged (or gone): nothing to do.
    Unchanged,
    /// The file changed and the new document is now live.
    Reloaded,
    /// The file changed but did not parse; the previous document stays live.
    Failed(String),
}

/// Where the live document came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenSource {
    File,
    /// The file was missing/invalid at startup; the built-in fallback is live
    /// until the file becomes valid.
    Fallback,
}

pub struct UiScreen {
    name: String,
    doc: UiDoc,
    watcher: UiDocWatcher,
    source: ScreenSource,
    last_error: Option<String>,
    reloads: u32,
}

impl UiScreen {
    /// Load `path`. Never fails: a missing or broken file falls back to
    /// `fallback` (or an empty document) and the problem is in
    /// [`Self::last_error`] and the log. The file is watched either way, so
    /// creating or fixing it later takes effect without a restart.
    pub fn load(name: &str, path: impl Into<PathBuf>, fallback: Option<UiDoc>) -> UiScreen {
        let path = path.into();
        let watcher = UiDocWatcher::watch(path.clone());
        match load_doc(&path) {
            Ok(doc) => UiScreen { name: name.into(), doc, watcher, source: ScreenSource::File, last_error: None, reloads: 0 },
            Err(err) => {
                log::warn!("ui screen '{name}': {err}; using the built-in fallback");
                let doc = fallback.unwrap_or_else(|| UiDoc::new(name, super::model::UiNode::new(super::model::NodeKind::Panel)));
                UiScreen { name: name.into(), doc, watcher, source: ScreenSource::Fallback, last_error: Some(err), reloads: 0 }
            }
        }
    }

    /// Check the file (one `stat`) and swap in the new document if it
    /// changed. Call once per frame.
    pub fn poll(&mut self) -> ReloadStatus {
        match self.watcher.poll() {
            None => ReloadStatus::Unchanged,
            Some(Ok(doc)) => {
                log::info!("ui screen '{}': reloaded {}", self.name, self.watcher.path().display());
                self.doc = doc;
                self.source = ScreenSource::File;
                self.last_error = None;
                self.reloads += 1;
                ReloadStatus::Reloaded
            }
            Some(Err(err)) => {
                log::warn!("ui screen '{}': {err}; keeping the previous document", self.name);
                self.last_error = Some(err.clone());
                ReloadStatus::Failed(err)
            }
        }
    }

    /// Draw the document over `area` (screen points; the docked editor
    /// passes its Viewport rect, a plain game passes the whole window) and
    /// return this frame's events/hover. Events carry the node's event name
    /// (`on_click`, `on_toggle`, `on_change`) for the game to act on.
    pub fn show(&self, ctx: &egui::Context, bindings: &UiBindings, area: egui::Rect) -> UiFrameResult {
        let mut params = DrawParams::new(area.size());
        params.origin = area.min;
        draw_ui_tree_with(ctx, &self.doc, bindings, &params)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn doc(&self) -> &UiDoc {
        &self.doc
    }
    pub fn path(&self) -> &Path {
        self.watcher.path()
    }
    pub fn source(&self) -> ScreenSource {
        self.source
    }
    /// Most recent load/parse error, cleared by the next successful load.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    /// How many hot reloads have been applied.
    pub fn reloads(&self) -> u32 {
        self.reloads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::files::{save_doc, temp_path};
    use crate::ui_designer::model::*;
    use egui::{Pos2, Vec2};
    use std::time::{Duration, SystemTime};

    fn doc_with_label(text: &str) -> UiDoc {
        UiDoc::new("t", UiNode::new(NodeKind::Panel).with_child(UiNode::new(NodeKind::Label { text: text.into() }).with_name("L")))
    }

    fn label_text(screen: &UiScreen) -> String {
        match &screen.doc().root.children[0].kind {
            NodeKind::Label { text } => text.clone(),
            other => panic!("not a label: {other:?}"),
        }
    }

    /// Filesystems may have coarse mtimes: push the timestamp forward.
    fn touch_later(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn loads_file_and_reports_source() {
        let path = temp_path("a.ron");
        save_doc(&doc_with_label("one"), &path).unwrap();
        let s = UiScreen::load("a", &path, Some(doc_with_label("fallback")));
        assert_eq!(label_text(&s), "one");
        assert_eq!(s.source(), ScreenSource::File);
        assert!(s.last_error().is_none());
    }

    #[test]
    fn missing_file_uses_fallback_then_picks_up_the_file_when_it_appears() {
        let path = temp_path("late.ron");
        let mut s = UiScreen::load("late", &path, Some(doc_with_label("fallback")));
        assert_eq!(label_text(&s), "fallback");
        assert_eq!(s.source(), ScreenSource::Fallback);
        assert!(s.last_error().unwrap().contains("late.ron"));
        assert_eq!(s.poll(), ReloadStatus::Unchanged, "still missing");

        save_doc(&doc_with_label("real"), &path).unwrap();
        assert_eq!(s.poll(), ReloadStatus::Reloaded);
        assert_eq!(label_text(&s), "real");
        assert_eq!(s.source(), ScreenSource::File);
        assert!(s.last_error().is_none());
        assert_eq!(s.reloads(), 1);
    }

    #[test]
    fn edit_on_disk_is_applied_once() {
        let path = temp_path("hot.ron");
        save_doc(&doc_with_label("v1"), &path).unwrap();
        let mut s = UiScreen::load("hot", &path, None);
        assert_eq!(s.poll(), ReloadStatus::Unchanged);

        save_doc(&doc_with_label("v2"), &path).unwrap();
        touch_later(&path, 5);
        assert_eq!(s.poll(), ReloadStatus::Reloaded);
        assert_eq!(label_text(&s), "v2");
        assert_eq!(s.poll(), ReloadStatus::Unchanged, "reported once");
        assert_eq!(s.reloads(), 1);
    }

    #[test]
    fn broken_edit_keeps_last_good_document_and_recovers() {
        let path = temp_path("broken.ron");
        save_doc(&doc_with_label("good"), &path).unwrap();
        let mut s = UiScreen::load("broken", &path, None);

        std::fs::write(&path, "(root: oops").unwrap();
        touch_later(&path, 3);
        assert!(matches!(s.poll(), ReloadStatus::Failed(e) if e.contains("broken.ron")));
        assert_eq!(label_text(&s), "good", "previous document stays live");
        assert!(s.last_error().is_some());
        assert_eq!(s.poll(), ReloadStatus::Unchanged, "the error is reported once, not every frame");

        save_doc(&doc_with_label("fixed"), &path).unwrap();
        touch_later(&path, 9);
        assert_eq!(s.poll(), ReloadStatus::Reloaded);
        assert_eq!(label_text(&s), "fixed");
        assert!(s.last_error().is_none());
    }

    #[test]
    fn invalid_file_at_startup_falls_back_and_recovers() {
        let path = temp_path("startbad.ron");
        std::fs::write(&path, "(((").unwrap();
        let mut s = UiScreen::load("startbad", &path, Some(doc_with_label("fallback")));
        assert_eq!(s.source(), ScreenSource::Fallback);
        assert_eq!(s.poll(), ReloadStatus::Unchanged, "the startup failure is not re-reported");
        save_doc(&doc_with_label("ok"), &path).unwrap();
        touch_later(&path, 4);
        assert_eq!(s.poll(), ReloadStatus::Reloaded);
        assert_eq!(label_text(&s), "ok");
    }

    #[test]
    fn deleting_the_file_keeps_the_live_document() {
        let path = temp_path("del.ron");
        save_doc(&doc_with_label("x"), &path).unwrap();
        let mut s = UiScreen::load("del", &path, None);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(s.poll(), ReloadStatus::Unchanged);
        assert_eq!(label_text(&s), "x");
    }

    // ---- drawing (headless egui, no GL) ----------------------------------

    fn button_doc() -> UiDoc {
        let b = UiNode::new(NodeKind::Button { text: "Go".into(), on_click: Some("go".into()) })
            .with_name("btn")
            .with_size(SizeMode::Fixed(100.0), SizeMode::Fixed(40.0))
            .at([0.0, 0.0], [10.0, 10.0]);
        UiDoc::new("screen_btn", UiNode::new(NodeKind::Panel).with_child(b))
    }

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>, screen: &UiScreen, area: egui::Rect) -> UiFrameResult {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut out = None;
        let _ = ctx.run(raw, |ctx| out = Some(screen.show(ctx, &UiBindings::new(), area)));
        out.unwrap()
    }

    #[test]
    fn show_draws_into_a_sub_rect_and_a_click_there_emits_the_event() {
        let path = temp_path("btn.ron");
        save_doc(&button_doc(), &path).unwrap();
        let screen = UiScreen::load("btn", &path, None);
        let ctx = egui::Context::default();
        // A 400x300 region offset by (200, 100), like the docked Viewport tab.
        let area = egui::Rect::from_min_size(Pos2::new(200.0, 100.0), Vec2::new(400.0, 300.0));
        let first = frame(&ctx, vec![], &screen, area);
        let id = screen.doc().root.children[0].id;
        let rect = first.layout.rect(id).expect("button laid out");
        assert_eq!((rect.x, rect.y), (10.0, 10.0), "layout is relative to the region");

        // Click at the button's on-screen position (region origin + layout rect).
        let pos = Pos2::new(200.0 + 10.0 + 50.0, 100.0 + 10.0 + 20.0);
        let btn = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let mut events = Vec::new();
        for evs in [vec![egui::Event::PointerMoved(pos)], vec![egui::Event::PointerMoved(pos)], vec![btn(true)], vec![btn(false)]] {
            events.extend(frame(&ctx, evs, &screen, area).events);
        }
        assert_eq!(events.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["go"]);

        // The same click outside the region's button does nothing.
        let ctx2 = egui::Context::default();
        let miss = Pos2::new(50.0, 50.0);
        let mbtn = |pressed| egui::Event::PointerButton { pos: miss, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        let mut events = Vec::new();
        for evs in [vec![egui::Event::PointerMoved(miss)], vec![egui::Event::PointerMoved(miss)], vec![mbtn(true)], vec![mbtn(false)]] {
            events.extend(frame(&ctx2, evs, &screen, area).events);
        }
        assert!(events.is_empty());
    }

    #[test]
    fn hot_reload_changes_what_is_drawn() {
        let path = temp_path("swap.ron");
        save_doc(&button_doc(), &path).unwrap();
        let mut screen = UiScreen::load("swap", &path, None);
        let ctx = egui::Context::default();
        let area = egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        assert_eq!(frame(&ctx, vec![], &screen, area).layout.entries.len(), 2);

        save_doc(&doc_with_label("only a label"), &path).unwrap();
        touch_later(&path, 6);
        assert_eq!(screen.poll(), ReloadStatus::Reloaded);
        let after = frame(&ctx, vec![], &screen, area);
        assert_eq!(after.layout.entries.len(), 2, "root + label");
        assert!(after.events.is_empty());
        assert!(matches!(screen.doc().root.children[0].kind, NodeKind::Label { .. }));
    }
}
