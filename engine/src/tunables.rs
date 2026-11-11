//! Named constants loaded from a RON file, retunable while the game runs.
//!
//! File format: a flat RON map of name -> value. Values are untagged, so
//! numbers and strings are written naturally:
//!
//! ```ron
//! {
//!     "music.near": 12.0,   // float
//!     "ghost.life": 4,      // integer
//!     "debug.god": true,    // bool
//!     "music.track": "a",   // string
//! }
//! ```
//!
//! Getters take a compiled-in default and never fail: a missing name or a
//! value of the wrong type returns the default and is recorded in
//! [`Tunables::misses`] (and logged once per name). An integer value is
//! accepted by `f32` getters (`12` reads as `12.0`); nothing else coerces.
//!
//! Live reload: call [`Tunables::reload_if_changed`] once a frame (one
//! `metadata` call: it compares the file's mtime and length), and/or feed
//! it the paths from `HotReloadWatcher::poll_events` via
//! [`Tunables::reload_if_touched`]. A file that fails to parse leaves the
//! previous values untouched and sets [`Tunables::last_error`]. Each
//! successful reload rebuilds [`Tunables::changes`] so the game can react.
//!
//! Shipped builds: [`Tunables::for_build`] ignores the file in release
//! builds unless the `JAME_TUNABLES` env var is set, so players cannot
//! retune a shipped game by accident.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::editor::scene::ron_options;

/// A single tunable value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TunableValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// Why a getter fell back to its default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissKind {
    /// The name is not in the file.
    Missing,
    /// The name exists but holds a different type.
    WrongType,
}

/// One entry of the change list produced by a reload. `old`/`new` are `None`
/// when the name was added/removed.
#[derive(Clone, Debug, PartialEq)]
pub struct TunableChange {
    pub name: String,
    pub old: Option<TunableValue>,
    pub new: Option<TunableValue>,
}

type Stamp = Option<(SystemTime, u64)>;

pub struct Tunables {
    path: Option<PathBuf>,
    values: BTreeMap<String, TunableValue>,
    stamp: Stamp,
    changes: Vec<TunableChange>,
    last_error: Option<String>,
    misses: Mutex<BTreeMap<String, MissKind>>,
}

impl Tunables {
    /// No file: every getter returns its default.
    pub fn defaults_only() -> Self {
        Self {
            path: None,
            values: BTreeMap::new(),
            stamp: None,
            changes: Vec::new(),
            last_error: None,
            misses: Mutex::new(BTreeMap::new()),
        }
    }

    /// Load `path`. Never fails: a missing file is treated as empty (it may
    /// appear later), a broken one is empty plus [`Tunables::last_error`].
    pub fn load(path: impl AsRef<Path>) -> Self {
        let mut t = Self::defaults_only();
        t.path = Some(path.as_ref().to_path_buf());
        t.reload_inner(true);
        t.changes.clear();
        t
    }

    /// [`Tunables::load`] in debug builds or when `JAME_TUNABLES` is set;
    /// otherwise [`Tunables::defaults_only`].
    pub fn for_build(path: impl AsRef<Path>) -> Self {
        if cfg!(debug_assertions) || std::env::var_os("JAME_TUNABLES").is_some() {
            Self::load(path)
        } else {
            Self::defaults_only()
        }
    }

    /// Parse RON text directly (no file, no reload).
    pub fn from_ron_str(text: &str) -> Result<Self, String> {
        let mut t = Self::defaults_only();
        t.values = parse(text)?;
        Ok(t)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get(&self, name: &str) -> Option<&TunableValue> {
        self.values.get(name)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn f32(&self, name: &str, default: f32) -> f32 {
        match self.values.get(name) {
            Some(TunableValue::Float(v)) => *v as f32,
            Some(TunableValue::Int(v)) => *v as f32,
            other => self.miss(name, other.is_some(), default),
        }
    }

    pub fn i64(&self, name: &str, default: i64) -> i64 {
        match self.values.get(name) {
            Some(TunableValue::Int(v)) => *v,
            other => self.miss(name, other.is_some(), default),
        }
    }

    pub fn bool(&self, name: &str, default: bool) -> bool {
        match self.values.get(name) {
            Some(TunableValue::Bool(v)) => *v,
            other => self.miss(name, other.is_some(), default),
        }
    }

    pub fn string(&self, name: &str, default: &str) -> String {
        match self.values.get(name) {
            Some(TunableValue::Str(v)) => v.clone(),
            other => self.miss(name, other.is_some(), default.to_string()),
        }
    }

    fn miss<T>(&self, name: &str, present: bool, default: T) -> T {
        let kind = if present { MissKind::WrongType } else { MissKind::Missing };
        if let Ok(mut m) = self.misses.lock() {
            if m.insert(name.to_string(), kind).is_none() {
                match kind {
                    MissKind::Missing => log::debug!("tunable '{name}' not set; using default"),
                    MissKind::WrongType => log::warn!("tunable '{name}' has the wrong type; using default"),
                }
            }
        }
        default
    }

    /// Names whose getter fell back to the default since the last
    /// [`Tunables::clear_misses`], sorted by name.
    pub fn misses(&self) -> Vec<(String, MissKind)> {
        self.misses.lock().map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect()).unwrap_or_default()
    }

    pub fn clear_misses(&self) {
        if let Ok(mut m) = self.misses.lock() {
            m.clear();
        }
    }

    /// Check the file's mtime/length and reload if it changed. Returns true
    /// if new values were applied. On a parse error the old values stay and
    /// [`Tunables::last_error`] is set. Cheap enough to call every frame.
    pub fn reload_if_changed(&mut self) -> bool {
        self.reload_inner(false)
    }

    /// Reload if `touched` (e.g. from `HotReloadWatcher::poll_events`)
    /// contains this file, regardless of mtime. Returns true if new values
    /// were applied.
    pub fn reload_if_touched(&mut self, touched: &[PathBuf]) -> bool {
        let Some(path) = self.path.as_ref() else { return false };
        let canon = std::fs::canonicalize(path).ok();
        let hit = touched.iter().any(|p| {
            p == path || (canon.is_some() && std::fs::canonicalize(p).ok() == canon)
        });
        hit && self.reload_inner(true)
    }

    fn reload_inner(&mut self, force: bool) -> bool {
        let Some(path) = self.path.clone() else { return false };
        let stamp: Stamp = std::fs::metadata(&path).and_then(|m| Ok((m.modified()?, m.len()))).ok();
        if stamp.is_none() {
            // Missing file (e.g. mid atomic-save): keep what we have.
            return false;
        }
        if !force && stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|text| parse(&text).map_err(|e| format!("{}: {e}", path.display())));
        match parsed {
            Ok(new_values) => {
                self.changes = diff(&self.values, &new_values);
                self.values = new_values;
                self.last_error = None;
                true
            }
            Err(e) => {
                log::warn!("tunables reload failed, keeping last good values: {e}");
                self.changes.clear();
                self.last_error = Some(e);
                false
            }
        }
    }

    /// Changes applied by the most recent successful reload (sorted by name).
    /// Replaced on every reload attempt.
    pub fn changes(&self) -> &[TunableChange] {
        &self.changes
    }

    /// Take the change list, leaving it empty.
    pub fn take_changes(&mut self) -> Vec<TunableChange> {
        std::mem::take(&mut self.changes)
    }

    /// Error from the most recent load/reload attempt, `None` if it
    /// succeeded.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

fn parse(text: &str) -> Result<BTreeMap<String, TunableValue>, String> {
    ron_options().from_str(text).map_err(|e| e.to_string())
}

fn diff(old: &BTreeMap<String, TunableValue>, new: &BTreeMap<String, TunableValue>) -> Vec<TunableChange> {
    let mut out = Vec::new();
    for (k, v) in new {
        if old.get(k) != Some(v) {
            out.push(TunableChange { name: k.clone(), old: old.get(k).cloned(), new: Some(v.clone()) });
        }
    }
    for (k, v) in old {
        if !new.contains_key(k) {
            out.push(TunableChange { name: k.clone(), old: Some(v.clone()), new: None });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "jame_tunables_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write and push mtime forward so coarse filesystem timestamps can't
    /// hide an edit.
    fn write_bump(p: &Path, text: &str, secs: u64) {
        std::fs::write(p, text).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)).unwrap();
    }

    const BASE: &str = r#"{ "music.near": 12.5, "ghost.life": 4, "god": true, "track": "a" }"#;

    #[test]
    fn typed_getters_and_defaults() {
        let t = Tunables::from_ron_str(BASE).unwrap();
        assert_eq!(t.f32("music.near", 1.0), 12.5);
        assert_eq!(t.i64("ghost.life", 0), 4);
        assert!(t.bool("god", false));
        assert_eq!(t.string("track", "z"), "a");
        assert_eq!(t.f32("ghost.life", 0.0), 4.0); // int reads as f32
        assert!(t.misses().is_empty());

        assert_eq!(t.f32("nope", 7.0), 7.0);
        assert_eq!(t.i64("nope2", -3), -3);
        assert_eq!(t.misses(), vec![("nope".into(), MissKind::Missing), ("nope2".into(), MissKind::Missing)]);
    }

    #[test]
    fn type_mismatch_falls_back_and_records() {
        let t = Tunables::from_ron_str(BASE).unwrap();
        assert_eq!(t.f32("track", 9.0), 9.0);
        assert_eq!(t.i64("music.near", 5), 5); // float is not an int
        assert!(!t.bool("ghost.life", false));
        assert_eq!(t.string("god", "d"), "d");
        let m = t.misses();
        assert_eq!(m.len(), 4);
        assert!(m.iter().all(|(_, k)| *k == MissKind::WrongType));
        t.clear_misses();
        assert!(t.misses().is_empty());
    }

    #[test]
    fn missing_file_is_defaults_then_appears() {
        let d = tmp_dir("appear");
        let p = d.join("tunables.ron");
        let mut t = Tunables::load(&p);
        assert!(t.last_error().is_none());
        assert_eq!(t.f32("a", 2.0), 2.0);
        assert!(!t.reload_if_changed());
        write_bump(&p, r#"{ "a": 3.0 }"#, 1);
        assert!(t.reload_if_changed());
        assert_eq!(t.f32("a", 2.0), 3.0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reload_picks_up_edits_and_lists_changes() {
        let d = tmp_dir("reload");
        let p = d.join("tunables.ron");
        write_bump(&p, BASE, 1);
        let mut t = Tunables::load(&p);
        assert_eq!(t.f32("music.near", 0.0), 12.5);
        assert!(t.changes().is_empty());
        assert!(!t.reload_if_changed()); // unchanged file

        write_bump(&p, r#"{ "music.near": 20.0, "ghost.life": 4, "god": true, "extra": 1 }"#, 2);
        assert!(t.reload_if_changed());
        assert_eq!(t.f32("music.near", 0.0), 20.0);
        assert!(t.last_error().is_none());
        assert_eq!(
            t.changes(),
            &[
                TunableChange { name: "extra".into(), old: None, new: Some(TunableValue::Int(1)) },
                TunableChange {
                    name: "music.near".into(),
                    old: Some(TunableValue::Float(12.5)),
                    new: Some(TunableValue::Float(20.0))
                },
                TunableChange { name: "track".into(), old: Some(TunableValue::Str("a".into())), new: None },
            ]
        );
        assert_eq!(t.take_changes().len(), 3);
        assert!(t.changes().is_empty());
        assert!(!t.reload_if_changed());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn broken_file_keeps_last_good_and_reports() {
        let d = tmp_dir("broken");
        let p = d.join("tunables.ron");
        write_bump(&p, BASE, 1);
        let mut t = Tunables::load(&p);

        write_bump(&p, r#"{ "music.near": 99.0, "ghost.li"#, 2); // truncated
        assert!(!t.reload_if_changed());
        assert!(t.last_error().is_some());
        assert_eq!(t.f32("music.near", 0.0), 12.5);
        assert_eq!(t.i64("ghost.life", 0), 4);
        assert!(t.changes().is_empty());
        // Not re-reported/re-parsed every frame.
        assert!(!t.reload_if_changed());

        write_bump(&p, "[1, 2, 3]", 3); // valid RON, wrong shape
        assert!(!t.reload_if_changed());
        assert!(t.last_error().is_some());
        assert_eq!(t.f32("music.near", 0.0), 12.5);

        write_bump(&p, r#"{ "music.near": 30.0 }"#, 4);
        assert!(t.reload_if_changed());
        assert!(t.last_error().is_none());
        assert_eq!(t.f32("music.near", 0.0), 30.0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn broken_on_first_load_is_empty_with_error() {
        let d = tmp_dir("firstbad");
        let p = d.join("tunables.ron");
        write_bump(&p, "{ nonsense", 1);
        let t = Tunables::load(&p);
        assert!(t.last_error().is_some());
        assert!(t.is_empty());
        assert_eq!(t.f32("x", 1.0), 1.0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn deleted_file_keeps_values() {
        let d = tmp_dir("deleted");
        let p = d.join("tunables.ron");
        write_bump(&p, BASE, 1);
        let mut t = Tunables::load(&p);
        std::fs::remove_file(&p).unwrap();
        assert!(!t.reload_if_changed());
        assert_eq!(t.f32("music.near", 0.0), 12.5);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reload_if_touched_matches_watcher_paths() {
        let d = tmp_dir("touched");
        let p = d.join("tunables.ron");
        write_bump(&p, r#"{ "a": 1.0 }"#, 1);
        let mut t = Tunables::load(&p);
        // Same mtime+len content change: mtime polling can't see it, a
        // watcher event forces it.
        std::fs::write(&p, r#"{ "a": 2.0 }"#).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001)).unwrap();
        assert!(!t.reload_if_touched(&[d.join("other.ron")]));
        assert_eq!(t.f32("a", 0.0), 1.0);
        assert!(t.reload_if_touched(&[d.join("other.ron"), p.clone()]));
        assert_eq!(t.f32("a", 0.0), 2.0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn defaults_only_never_reloads() {
        let mut t = Tunables::defaults_only();
        assert!(!t.reload_if_changed());
        assert_eq!(t.bool("x", true), true);
    }
}
