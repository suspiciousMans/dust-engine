//! The UI Designer tab: hosts the `ui_designer` egui panel and adds a
//! document picker over a configurable directory of `.ron` documents.
//!
//! The designer panel itself provides the live preview (its canvas lays out
//! and renders the document with the same solver the game uses), undo/redo
//! (toolbar and Ctrl+Z / Ctrl+Y), Load/Save by path, and hot reload.

use std::path::{Path, PathBuf};

use crate::ui_designer::{draw_ui_designer, examples, DesignerState, UiDoc};

/// Built-in starting points offered in the picker.
const EXAMPLES: [&str; 2] = ["pause_menu", "hud"];

pub struct DesignerTab {
    /// Designer state (document, selection, undo stacks, preview settings).
    pub state: DesignerState,
    dir: Option<PathBuf>,
    /// `.ron` files directly inside `dir`, sorted by name.
    docs: Vec<PathBuf>,
    /// Text box for "Save as" (file stem, no extension).
    save_name: String,
    scanned: bool,
}

impl Default for DesignerTab {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DesignerTab {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let mut t = Self { state: DesignerState::default(), dir: None, docs: Vec::new(), save_name: String::new(), scanned: false };
        t.set_dir(dir);
        t
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Points the picker at another directory (or none) and rescans it.
    pub fn set_dir(&mut self, dir: Option<PathBuf>) {
        self.dir = dir;
        self.refresh();
    }

    /// Rescans the directory for `.ron` documents.
    pub fn refresh(&mut self) {
        self.docs.clear();
        if let Some(dir) = &self.dir {
            if let Ok(rd) = std::fs::read_dir(dir) {
                self.docs = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ron")))
                    .collect();
                self.docs.sort();
            }
        }
        self.scanned = true;
    }

    /// Documents found in the directory.
    pub fn documents(&self) -> &[PathBuf] {
        &self.docs
    }

    /// Opens a document from the directory (undoable in the designer).
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let r = self.state.load(path);
        if r.is_ok() {
            self.save_name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        }
        r
    }

    /// Replaces the document with a built-in example (undoable). It is
    /// unsaved: Save writes it to `<dir>/<name>.ron` if a directory is set.
    pub fn open_example(&mut self, name: &str) -> bool {
        let doc: UiDoc = match name {
            "pause_menu" => examples::pause_menu_doc(),
            "hud" => examples::hud_doc(),
            _ => return false,
        };
        self.state.checkpoint();
        self.state.doc = doc;
        self.state.selected = None;
        self.state.dirty = true;
        self.state.path_text = match &self.dir {
            Some(d) => d.join(format!("{name}.ron")).display().to_string(),
            None => format!("ui/{name}.ron"),
        };
        self.save_name = name.to_string();
        self.state.status = format!("Opened example {name} (not saved yet)");
        true
    }

    /// Saves the current document as `<dir>/<name>.ron` and rescans.
    pub fn save_as_name(&mut self, name: &str) -> Result<PathBuf, String> {
        let Some(dir) = &self.dir else { return Err("no UI document directory configured".into()) };
        let name = name.trim().trim_end_matches(".ron");
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(format!("invalid document name {name:?}"));
        }
        let path = dir.join(format!("{name}.ron"));
        self.state.save_as(&path)?;
        self.refresh();
        Ok(path)
    }

    /// Draws the tab: the picker bar on top, the designer below.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        if !self.scanned {
            self.refresh();
        }
        egui::TopBottomPanel::top("designer_tab_docs").show_inside(ui, |ui| self.picker_bar(ui));
        draw_ui_designer(ui, &mut self.state);
    }

    fn picker_bar(&mut self, ui: &mut egui::Ui) {
        let mut open: Option<PathBuf> = None;
        let mut example: Option<&'static str> = None;
        let mut save_as = false;
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label("Document");
            let current = if self.state.path_text.is_empty() { "—".to_string() } else { file_label(Path::new(&self.state.path_text)) };
            egui::ComboBox::from_id_salt("designer_tab_picker").selected_text(current).width(220.0).show_ui(ui, |ui| {
                ui.weak("Examples");
                for name in EXAMPLES {
                    if ui.selectable_label(false, name).clicked() {
                        example = Some(name);
                    }
                }
                ui.separator();
                match &self.dir {
                    None => {
                        ui.weak("No document folder configured");
                    }
                    Some(_) if self.docs.is_empty() => {
                        ui.weak("No .ron documents in the folder yet");
                    }
                    Some(_) => {
                        ui.weak("Documents");
                        for p in &self.docs {
                            let here = Path::new(&self.state.path_text) == p.as_path();
                            if ui.selectable_label(here, file_label(p)).clicked() {
                                open = Some(p.clone());
                            }
                        }
                    }
                }
            });
            if ui.small_button("⟳").on_hover_text("Rescan the folder").clicked() {
                refresh = true;
            }
            ui.separator();
            ui.add_enabled_ui(self.dir.is_some(), |ui| {
                ui.label("Save as");
                ui.add(egui::TextEdit::singleline(&mut self.save_name).desired_width(120.0).hint_text("name"));
                if ui.button("Save to folder").clicked() {
                    save_as = true;
                }
            });
            match &self.dir {
                Some(d) => ui.weak(d.display().to_string()),
                None => ui.weak("No document folder configured: set one with EditorRuntime::set_ui_dir. Load/Save below still take any path."),
            };
        });
        if let Some(p) = open {
            let _ = self.open(&p);
        }
        if let Some(name) = example {
            self.open_example(name);
        }
        if refresh {
            self.refresh();
        }
        if save_as {
            let name = self.save_name.clone();
            if let Err(e) = self.save_as_name(&name) {
                self.state.status = e;
            }
        }
    }
}

fn file_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Rect, Vec2};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("designer_tab_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn frame(ctx: &egui::Context, tab: &mut DesignerTab) {
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1500.0, 900.0))), ..Default::default() };
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| tab.draw(ui));
        });
    }

    #[test]
    fn empty_state_draws_without_directory() {
        let ctx = egui::Context::default();
        let mut tab = DesignerTab::default();
        assert!(tab.dir().is_none() && tab.documents().is_empty());
        frame(&ctx, &mut tab);
        frame(&ctx, &mut tab);
        assert!(tab.save_as_name("x").is_err(), "no directory => cannot save to folder");
    }

    #[test]
    fn missing_directory_is_an_empty_list_not_a_panic() {
        let ctx = egui::Context::default();
        let mut tab = DesignerTab::new(Some(PathBuf::from("/definitely/not/here")));
        assert!(tab.documents().is_empty());
        frame(&ctx, &mut tab);
    }

    #[test]
    fn draws_example_documents() {
        let ctx = egui::Context::default();
        let mut tab = DesignerTab::default();
        for name in EXAMPLES {
            assert!(tab.open_example(name));
            assert!(tab.state.doc.root.subtree_size() > 1, "{name}");
            frame(&ctx, &mut tab);
            frame(&ctx, &mut tab);
        }
        assert!(!tab.open_example("nope"));
        // Opening an example is undoable.
        let before = tab.state.undo_depth();
        assert!(before >= 2);
        assert!(tab.state.undo());
        assert_eq!(tab.state.doc.name, examples::pause_menu_doc().name);
    }

    #[test]
    fn save_and_load_roundtrip_through_a_directory() {
        let dir = temp_dir("roundtrip");
        let ctx = egui::Context::default();
        let mut tab = DesignerTab::new(Some(dir.clone()));
        assert!(tab.documents().is_empty());

        assert!(tab.open_example("hud"));
        let saved_doc = tab.state.doc.clone();
        let path = tab.save_as_name("my_hud").unwrap();
        assert_eq!(path, dir.join("my_hud.ron"));
        assert!(!tab.state.dirty);
        assert_eq!(tab.documents(), &[dir.join("my_hud.ron")]);

        // Edit, then load the file back: the saved content returns.
        tab.state.add_node("Button").unwrap();
        assert_ne!(tab.state.doc, saved_doc);
        tab.open(&path).unwrap();
        assert_eq!(tab.state.doc, saved_doc);
        frame(&ctx, &mut tab);

        // Undo the load → the edited version comes back.
        assert!(tab.state.undo());
        assert_ne!(tab.state.doc, saved_doc);

        // A second document shows up in the picker after a rescan.
        assert!(tab.open_example("pause_menu"));
        tab.save_as_name("pause.ron").unwrap();
        assert_eq!(tab.documents().len(), 2);
        frame(&ctx, &mut tab);

        // Bad names are rejected and write nothing.
        assert!(tab.save_as_name("../evil").is_err());
        assert!(tab.save_as_name("   ").is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The point of `EditorRuntime::set_ui_dir`: the tab lists the game's
    /// documents, and saving an edit there reaches a `UiScreen` the running
    /// game holds on the same file.
    #[test]
    fn saving_in_the_tab_hot_reloads_the_games_screen() {
        use crate::ui_designer::{NodeKind, ReloadStatus, UiScreen};
        let dir = temp_dir("game_screen");
        let hud_path = dir.join("hud.ron");
        crate::ui_designer::save_doc(&examples::hud_doc(), &hud_path).unwrap();
        crate::ui_designer::save_doc(&examples::pause_menu_doc(), &dir.join("pause_menu.ron")).unwrap();

        let mut screen = UiScreen::load("hud", &hud_path, None);
        let mut tab = DesignerTab::new(Some(dir.clone()));
        assert_eq!(tab.documents(), &[hud_path.clone(), dir.join("pause_menu.ron")], "both game documents are listed");
        tab.open(&hud_path).unwrap();
        assert_eq!(tab.state.doc, *screen.doc());
        assert_eq!(screen.poll(), ReloadStatus::Unchanged);

        // Edit the title label's text in the tab and save (the toolbar's Save).
        let mut edited = false;
        tab.state.checkpoint();
        let mut doc = tab.state.doc.clone();
        fn retext(n: &mut crate::ui_designer::UiNode, done: &mut bool) {
            if n.name == "Title" {
                n.kind = NodeKind::Label { text: "edited in the tab".into() };
                *done = true;
            }
            for c in &mut n.children {
                retext(c, done);
            }
        }
        retext(&mut doc.root, &mut edited);
        assert!(edited);
        tab.state.doc = doc;
        tab.state.save().unwrap();
        // Coarse filesystem timestamps: make sure the save is seen as newer.
        let f = std::fs::OpenOptions::new().write(true).open(&hud_path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        drop(f);

        assert_eq!(screen.poll(), ReloadStatus::Reloaded);
        assert_eq!(screen.doc(), &tab.state.doc);
        let mut text = String::new();
        screen.doc().root.walk(&mut |n| {
            if n.name == "Title" {
                if let NodeKind::Label { text: t } = &n.kind {
                    text = t.clone();
                }
            }
        });
        assert_eq!(text, "edited in the tab");
        let _ = std::fs::remove_dir_all(dir);
    }
}
