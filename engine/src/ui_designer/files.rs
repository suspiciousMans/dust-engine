//! Loading, saving and hot-reloading UI documents (`ui/*.ron`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::model::UiDoc;

pub fn load_doc(path: &Path) -> Result<UiDoc, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    UiDoc::from_ron(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Write via a temp file + rename so a game hot-reloading the same file
/// never sees a half-written document.
pub fn save_doc(doc: &UiDoc, path: &Path) -> Result<(), String> {
    let text = doc.to_ron().map_err(|e| format!("serialize: {e}"))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
    }
    let tmp = path.with_extension("ron.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Watches one document file by modification time. Games call [`Self::poll`]
/// each frame (it is a single `stat`); the designer uses the same type.
pub struct UiDocWatcher {
    path: PathBuf,
    last_seen: Option<SystemTime>,
}

impl UiDocWatcher {
    /// Load `path` and start watching it.
    pub fn open(path: impl Into<PathBuf>) -> Result<(UiDocWatcher, UiDoc), String> {
        let path = path.into();
        let doc = load_doc(&path)?;
        let last_seen = mtime(&path);
        Ok((UiDocWatcher { path, last_seen }, doc))
    }

    /// Watch a path without loading (e.g. right after saving to it).
    pub fn watch(path: impl Into<PathBuf>) -> UiDocWatcher {
        let path = path.into();
        let last_seen = mtime(&path);
        UiDocWatcher { path, last_seen }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True if the file changed on disk since it was last loaded/saved here.
    pub fn changed(&self) -> bool {
        let now = mtime(&self.path);
        now.is_some() && now != self.last_seen
    }

    /// Remember the current mtime as "seen" (call after saving the file ourselves).
    pub fn mark_seen(&mut self) {
        self.last_seen = mtime(&self.path);
    }

    /// If the file changed, reload it. A file that fails to parse (e.g.
    /// caught mid-write) yields `Some(Err)` once; the next real change is
    /// tried again.
    pub fn poll(&mut self) -> Option<Result<UiDoc, String>> {
        if !self.changed() {
            return None;
        }
        self.mark_seen();
        Some(load_doc(&self.path))
    }
}

#[cfg(test)]
pub(crate) fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ui_designer_test_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::model::*;
    use std::time::Duration;

    fn sample(name: &str) -> UiDoc {
        UiDoc::new(name, UiNode::new(NodeKind::Label { text: name.into() }))
    }

    /// Bump mtime explicitly: filesystems may have coarse timestamps.
    fn touch_later(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn save_then_load_roundtrip_creates_dirs() {
        let path = temp_path("a").with_file_name("ui").join("x.ron");
        let doc = sample("x");
        save_doc(&doc, &path).unwrap();
        assert_eq!(load_doc(&path).unwrap(), doc);
        assert!(!path.with_extension("ron.tmp").exists());
    }

    #[test]
    fn load_errors_name_the_file() {
        let path = temp_path("bad.ron");
        std::fs::write(&path, "(root: oops").unwrap();
        assert!(load_doc(&path).unwrap_err().contains("bad.ron"));
        assert!(load_doc(&temp_path("missing.ron")).unwrap_err().contains("missing.ron"));
    }

    #[test]
    fn watcher_detects_external_change_once() {
        let path = temp_path("w.ron");
        save_doc(&sample("one"), &path).unwrap();
        let (mut w, doc) = UiDocWatcher::open(&path).unwrap();
        assert_eq!(doc.name, "one");
        assert!(w.poll().is_none(), "unchanged → nothing");

        save_doc(&sample("two"), &path).unwrap();
        touch_later(&path, 5);
        let reloaded = w.poll().expect("changed").unwrap();
        assert_eq!(reloaded.name, "two");
        assert!(w.poll().is_none(), "reported only once");
    }

    #[test]
    fn own_save_is_not_reported_after_mark_seen() {
        let path = temp_path("own.ron");
        save_doc(&sample("one"), &path).unwrap();
        let (mut w, _) = UiDocWatcher::open(&path).unwrap();
        save_doc(&sample("two"), &path).unwrap();
        touch_later(&path, 7);
        w.mark_seen();
        assert!(w.poll().is_none());
    }

    #[test]
    fn broken_file_reports_error_then_recovers() {
        let path = temp_path("b.ron");
        save_doc(&sample("ok"), &path).unwrap();
        let (mut w, _) = UiDocWatcher::open(&path).unwrap();
        std::fs::write(&path, "(((").unwrap();
        touch_later(&path, 3);
        assert!(w.poll().unwrap().is_err());
        assert!(w.poll().is_none());
        save_doc(&sample("fixed"), &path).unwrap();
        touch_later(&path, 9);
        assert_eq!(w.poll().unwrap().unwrap().name, "fixed");
    }

    #[test]
    fn deleted_file_is_not_a_change() {
        let path = temp_path("gone.ron");
        save_doc(&sample("one"), &path).unwrap();
        let (mut w, _) = UiDocWatcher::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(w.poll().is_none());
    }
}
