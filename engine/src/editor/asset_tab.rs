//! The Assets tab: browse a folder, search, select, open (double click) and
//! drag assets onto the viewport / hierarchy.
//!
//! Engine-agnostic: no GL here. Thumbnails come from an
//! [`AssetBrowserState`], whose two-phase loading means the *owner* (the
//! `EditorRuntime`) must call `browser.ensure_thumbnails_loaded` before
//! `EguiState::run` each frame; drawing only requests and reads them.
//!
//! Drag and drop: a tile being dragged sets an [`AssetDrag`] payload with
//! egui's `DragAndDrop`; the shell turns a release over the viewport or the
//! hierarchy into a `ShellEvent::AssetDropped` (see `asset_drop`).

use std::path::{Path, PathBuf};

use egui::{Color32, Pos2, Rect, Sense, Vec2};

use crate::ui::AssetBrowserState;

/// Drag-and-drop payload carried by a tile while an asset is dragged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetDrag {
    /// The asset root joined with the asset's relative path.
    pub path: PathBuf,
}

/// True while an asset (not an entity) is being dragged.
pub fn is_dragging_asset(ctx: &egui::Context) -> bool {
    egui::DragAndDrop::has_payload_of_type::<AssetDrag>(ctx)
}

/// Upper bound on scanned files, so pointing the root at `/` doesn't hang.
const MAX_FILES: usize = 20_000;
const MAX_DEPTH: usize = 12;
/// Tiles drawn per frame; more matches ask the user to refine the search.
const MAX_TILES: usize = 1500;

const TILE: f32 = 84.0;
const THUMB: f32 = 64.0;

#[derive(Default)]
pub struct AssetsPanel {
    root: Option<PathBuf>,
    /// Thumbnail cache. The runtime calls `ensure_thumbnails_loaded` on this.
    pub browser: AssetBrowserState,
    /// Every file under the root, relative to it, sorted.
    files: Vec<PathBuf>,
    /// Folder being viewed, relative to the root (empty = the root).
    cwd: PathBuf,
    pub search: String,
    selected: Option<PathBuf>,
    opened: Vec<PathBuf>,
    scanned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Dir(PathBuf),
    File(PathBuf),
}

impl AssetsPanel {
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut p = Self::default();
        p.set_root(root);
        p
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Points the panel at another folder (or none) and rescans.
    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.root = root;
        self.cwd = PathBuf::new();
        self.selected = None;
        self.refresh();
    }

    /// Rescans the root; failed thumbnails are retried.
    pub fn refresh(&mut self) {
        self.browser.forget_failures();
        self.files.clear();
        if let Some(root) = &self.root {
            let mut found = Vec::new();
            scan(root, Path::new(""), 0, &mut found);
            found.sort();
            self.files = found;
        }
        self.scanned = true;
        if !self.dir_exists(&self.cwd) {
            self.cwd = PathBuf::new();
        }
    }

    /// Files found by the last scan, relative to the root.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The selected asset (absolute: root joined with its relative path).
    pub fn selected(&self) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        self.selected.as_ref().map(|r| root.join(r))
    }

    /// Selects `rel` (relative to the root).
    pub fn select(&mut self, rel: Option<PathBuf>) {
        self.selected = rel;
    }

    /// Assets double-clicked since the last call (absolute paths).
    pub fn take_opened(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.opened)
    }

    fn dir_exists(&self, dir: &Path) -> bool {
        dir.as_os_str().is_empty() || self.files.iter().any(|f| f.starts_with(dir))
    }

    /// What the grid shows: with a search, every matching file in the whole
    /// tree; otherwise the sub-folders and files of the current folder.
    fn entries(&self) -> Vec<Entry> {
        let q = self.search.trim().to_lowercase();
        if !q.is_empty() {
            return self
                .files
                .iter()
                .filter(|f| f.to_string_lossy().to_lowercase().contains(&q))
                .cloned()
                .map(Entry::File)
                .collect();
        }
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut files = Vec::new();
        for f in &self.files {
            let Ok(rest) = f.strip_prefix(&self.cwd) else { continue };
            let mut comps = rest.components();
            let first = comps.next();
            if comps.next().is_some() {
                if let Some(c) = first {
                    let d = self.cwd.join(c.as_os_str());
                    if dirs.last() != Some(&d) {
                        dirs.push(d);
                    }
                }
            } else {
                files.push(f.clone());
            }
        }
        dirs.dedup();
        dirs.into_iter().map(Entry::Dir).chain(files.into_iter().map(Entry::File)).collect()
    }

    /// Draws the tab.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let Some(root) = self.root.clone() else {
            empty_state(ui, "No asset folder configured", "The game sets one with EditorRuntime::set_asset_root.");
            return;
        };
        if !self.scanned {
            self.refresh();
        }

        ui.horizontal(|ui| {
            let w = (ui.available_width() - 90.0).max(60.0);
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search assets…").desired_width(w));
            if !self.search.is_empty() && ui.small_button("✖").on_hover_text("Clear search").clicked() {
                self.search.clear();
            }
            if ui.button("Refresh").on_hover_text(root.display().to_string()).clicked() {
                self.refresh();
            }
        });
        let searching = !self.search.trim().is_empty();
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.cwd.as_os_str().is_empty() && !searching, egui::Button::new("⬆ Up")).clicked() {
                self.cwd.pop();
            }
            let here = if searching {
                "search results".to_string()
            } else if self.cwd.as_os_str().is_empty() {
                "/".to_string()
            } else {
                format!("/{}", self.cwd.display())
            };
            ui.weak(here);
        });
        ui.separator();

        if self.files.is_empty() {
            empty_state(ui, "No files found", &format!("Nothing under {}", root.display()));
            return;
        }
        let entries = self.entries();
        if entries.is_empty() {
            ui.weak(if searching { "No assets match the search." } else { "This folder is empty." });
            return;
        }

        let total = entries.len();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(6.0);
                for entry in entries.iter().take(MAX_TILES) {
                    self.tile(ui, &root, entry);
                }
            });
            if total > MAX_TILES {
                ui.weak(format!("… {} more. Refine the search to see them.", total - MAX_TILES));
            }
        });
    }

    fn tile(&mut self, ui: &mut egui::Ui, root: &Path, entry: &Entry) {
        let (rel, is_dir) = match entry {
            Entry::Dir(p) => (p, true),
            Entry::File(p) => (p, false),
        };
        let name = rel.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let sense = if is_dir { Sense::click() } else { Sense::click_and_drag() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(TILE, TILE + 18.0), sense);
        if !ui.is_rect_visible(rect) {
            return;
        }
        let selected = self.selected.as_deref() == Some(rel.as_path());
        let visuals = ui.visuals();
        if selected {
            ui.painter().rect_filled(rect, 4.0, visuals.selection.bg_fill);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
        }
        let thumb = Rect::from_min_size(Pos2::new(rect.center().x - THUMB / 2.0, rect.top() + 2.0), Vec2::splat(THUMB));
        if is_dir {
            paint_folder(ui, thumb);
        } else {
            let tex = if AssetBrowserState::is_thumbnailable(rel) { self.browser.thumbnail_or_request(&root.join(rel)) } else { None };
            match tex {
                Some(id) => {
                    ui.painter().image(id, thumb, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                None => paint_badge(ui, thumb, rel),
            }
        }
        let font = egui::FontId::proportional(11.0);
        let label = ellipsize(&name, 13);
        ui.painter().text(
            Pos2::new(rect.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            font,
            visuals_text(ui, selected),
        );
        let resp = resp.on_hover_text(rel.display().to_string());

        if resp.clicked() {
            self.selected = Some(rel.clone());
        }
        if resp.double_clicked() {
            if is_dir {
                self.cwd = rel.clone();
                self.selected = None;
            } else {
                self.opened.push(root.join(rel));
            }
        }
        if !is_dir && resp.drag_started() {
            self.selected = Some(rel.clone());
            egui::DragAndDrop::set_payload(ui.ctx(), AssetDrag { path: root.join(rel) });
        }
    }
}

/// Floating label that follows the pointer while an asset is dragged.
pub fn paint_drag_ghost(ctx: &egui::Context) {
    let Some(drag) = egui::DragAndDrop::payload::<AssetDrag>(ctx) else { return };
    let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else { return };
    let name = drag.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("asset_drag_ghost"));
    let painter = ctx.layer_painter(layer);
    let galley = painter.layout_no_wrap(name, egui::FontId::proportional(12.0), Color32::WHITE);
    let rect = Rect::from_min_size(pos + Vec2::new(12.0, 12.0), galley.size() + Vec2::new(10.0, 6.0));
    painter.rect_filled(rect, 4.0, Color32::from_black_alpha(200));
    painter.galley(rect.min + Vec2::new(5.0, 3.0), galley, Color32::WHITE);
}

fn empty_state(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.strong(title);
        ui.weak(hint);
    });
}

fn visuals_text(ui: &egui::Ui, selected: bool) -> Color32 {
    if selected {
        ui.visuals().selection.stroke.color
    } else {
        ui.visuals().text_color()
    }
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{head}…")
    }
}

fn paint_folder(ui: &egui::Ui, r: Rect) {
    let col = Color32::from_rgb(214, 170, 70);
    let p = ui.painter();
    let tab = Rect::from_min_size(r.min + Vec2::new(4.0, 10.0), Vec2::new(r.width() * 0.4, 8.0));
    p.rect_filled(tab, 2.0, col.gamma_multiply(0.8));
    let body = Rect::from_min_max(r.min + Vec2::new(4.0, 16.0), r.max - Vec2::new(4.0, 8.0));
    p.rect_filled(body, 3.0, col);
}

/// Category colour and short caption for a file type.
pub fn file_kind(path: &Path) -> (Color32, String) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let col = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "tga" | "gif" | "webp" => Color32::from_rgb(80, 150, 90),
        "glb" | "gltf" | "obj" | "fbx" => Color32::from_rgb(90, 130, 200),
        "wav" | "ogg" | "mp3" | "flac" => Color32::from_rgb(180, 100, 160),
        "ron" | "json" | "toml" | "yaml" | "yml" => Color32::from_rgb(200, 130, 70),
        "rs" | "lua" | "rhai" | "js" | "py" => Color32::from_rgb(190, 90, 80),
        _ => Color32::from_gray(110),
    };
    let caption = if ext.is_empty() { "FILE".to_string() } else { ext.to_uppercase() };
    (col, caption)
}

fn paint_badge(ui: &egui::Ui, r: Rect, path: &Path) {
    let (col, caption) = file_kind(path);
    let body = Rect::from_min_max(r.min + Vec2::new(10.0, 2.0), r.max - Vec2::new(10.0, 2.0));
    ui.painter().rect_filled(body, 4.0, col.gamma_multiply(0.35));
    ui.painter().rect_stroke(body, 4.0, egui::Stroke::new(1.5_f32, col));
    ui.painter().text(body.center(), egui::Align2::CENTER_CENTER, ellipsize(&caption, 5), egui::FontId::proportional(13.0), col);
}

fn scan(root: &Path, rel: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root.join(rel)) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let rel_path = rel.join(&name);
        match entry.file_type() {
            Ok(t) if t.is_dir() => scan(root, &rel_path, depth + 1, out),
            Ok(t) if t.is_file() => out.push(rel_path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asset_tab_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("textures")).unwrap();
        std::fs::create_dir_all(dir.join("audio/sfx")).unwrap();
        std::fs::write(dir.join("textures/brick.png"), b"x").unwrap();
        std::fs::write(dir.join("textures/grass.jpg"), b"x").unwrap();
        std::fs::write(dir.join("audio/sfx/hit.wav"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        dir
    }

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>, f: impl FnOnce(&mut egui::Ui)) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))),
            events,
            ..Default::default()
        };
        let mut f = Some(f);
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| (f.take().unwrap())(ui));
        });
    }

    #[test]
    fn empty_states_do_not_panic() {
        let ctx = egui::Context::default();
        // No root configured.
        let mut p = AssetsPanel::default();
        frame(&ctx, vec![], |ui| p.draw(ui));
        // Root that does not exist.
        let mut p = AssetsPanel::new(Some(PathBuf::from("/definitely/not/here")));
        assert!(p.files().is_empty());
        frame(&ctx, vec![], |ui| p.draw(ui));
        // Search with no match.
        let root = temp_root("empty");
        let mut p = AssetsPanel::new(Some(root.clone()));
        p.search = "zzzz".into();
        assert!(p.entries().is_empty());
        frame(&ctx, vec![], |ui| p.draw(ui));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_skips_hidden_and_lists_folders_then_files() {
        let root = temp_root("scan");
        let mut p = AssetsPanel::new(Some(root.clone()));
        assert_eq!(p.files().len(), 4, "{:?}", p.files());
        assert_eq!(p.entries(), vec![Entry::Dir("audio".into()), Entry::Dir("textures".into()), Entry::File("notes.txt".into())]);
        p.cwd = "textures".into();
        assert_eq!(p.entries(), vec![Entry::File("textures/brick.png".into()), Entry::File("textures/grass.jpg".into())]);
        p.cwd = "audio".into();
        assert_eq!(p.entries(), vec![Entry::Dir("audio/sfx".into())]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_filters_across_folders_case_insensitively() {
        let root = temp_root("search");
        let mut p = AssetsPanel::new(Some(root.clone()));
        p.search = "HIT".into();
        assert_eq!(p.entries(), vec![Entry::File("audio/sfx/hit.wav".into())]);
        p.search = "textures".into();
        assert_eq!(p.entries().len(), 2);
        let ctx = egui::Context::default();
        frame(&ctx, vec![], |ui| p.draw(ui));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn drawing_requests_thumbnails_only_for_images() {
        let root = temp_root("thumbs");
        let mut p = AssetsPanel::new(Some(root.clone()));
        p.cwd = "textures".into();
        let ctx = egui::Context::default();
        frame(&ctx, vec![], |ui| p.draw(ui));
        // No GL in tests, so nothing is ever loaded; drawing must still work.
        assert!(p.browser.thumbnail(&root.join("textures/brick.png")).is_none());
        assert!(!is_dragging_asset(&ctx));
        // A non-image folder requests nothing and still draws.
        p.cwd = PathBuf::new();
        p.search = "notes".into();
        frame(&ctx, vec![], |ui| p.draw(ui));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Lay out once, then click (and double click) the first tile.
    fn first_tile_pos(p: &mut AssetsPanel, ctx: &egui::Context) -> Pos2 {
        frame(ctx, vec![], |ui| p.draw(ui));
        frame(ctx, vec![], |ui| p.draw(ui));
        // Header rows (search, up) + separator, then the first tile.
        Pos2::new(8.0 + TILE / 2.0, 100.0 + TILE / 2.0)
    }

    fn click(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }
    }

    #[test]
    fn click_selects_double_click_opens_and_drag_sets_payload() {
        let root = temp_root("interact");
        let mut p = AssetsPanel::new(Some(root.clone()));
        p.cwd = "textures".into();
        let ctx = egui::Context::default();
        let pos = first_tile_pos(&mut p, &ctx);

        // Single click selects.
        frame(&ctx, vec![egui::Event::PointerMoved(pos)], |ui| p.draw(ui));
        frame(&ctx, vec![click(pos, true)], |ui| p.draw(ui));
        frame(&ctx, vec![click(pos, false)], |ui| p.draw(ui));
        assert_eq!(p.selected(), Some(root.join("textures/brick.png")));
        assert!(p.take_opened().is_empty());

        // Second click quickly after the first → double click → opened.
        frame(&ctx, vec![click(pos, true)], |ui| p.draw(ui));
        frame(&ctx, vec![click(pos, false)], |ui| p.draw(ui));
        assert_eq!(p.take_opened(), vec![root.join("textures/brick.png")]);

        // Press, move away, and the payload is set.
        let ctx = egui::Context::default();
        let pos = first_tile_pos(&mut p, &ctx);
        frame(&ctx, vec![egui::Event::PointerMoved(pos)], |ui| p.draw(ui));
        frame(&ctx, vec![click(pos, true)], |ui| p.draw(ui));
        frame(&ctx, vec![egui::Event::PointerMoved(pos + Vec2::new(60.0, 40.0))], |ui| p.draw(ui));
        frame(&ctx, vec![egui::Event::PointerMoved(pos + Vec2::new(120.0, 80.0))], |ui| p.draw(ui));
        let payload = egui::DragAndDrop::payload::<AssetDrag>(&ctx).expect("drag started");
        assert_eq!(payload.path, root.join("textures/brick.png"));
        let _ = std::fs::remove_dir_all(root);
    }
}
