//! Persisted editor preferences + dock layout (RON files in a config dir).

use std::path::{Path, PathBuf};

use egui_dock::DockState;
use serde::{Deserialize, Serialize};

use super::state::KeymapPreset;
use crate::ui::dock_skeleton::{self, EditorTab, LayoutPreset};
use crate::ui::theme::EditorTheme;

impl Serialize for KeymapPreset {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Self::Unity => "Unity",
            Self::Blender => "Blender",
        })
    }
}
impl<'de> Deserialize<'de> for KeymapPreset {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "Unity" => Ok(Self::Unity),
            "Blender" => Ok(Self::Blender),
            o => Err(serde::de::Error::custom(format!("unknown keymap preset '{o}'"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPrefs {
    pub theme: EditorTheme,
    pub keymap: KeymapPreset,
    pub ui_scale: f32,
    pub layout_preset: LayoutPreset,
    pub show_grid: bool,
}

impl Default for EditorPrefs {
    fn default() -> Self {
        Self {
            theme: EditorTheme::builtin_default(),
            keymap: KeymapPreset::default(),
            ui_scale: 1.0,
            layout_preset: LayoutPreset::Default,
            show_grid: true,
        }
    }
}

impl EditorPrefs {
    pub fn to_ron(&self) -> Result<String, String> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).map_err(|e| e.to_string())
    }
    pub fn from_ron(s: &str) -> Result<Self, String> {
        let mut p: Self = ron::from_str(s).map_err(|e| e.to_string())?;
        p.ui_scale = p.ui_scale.clamp(0.5, 3.0);
        Ok(p)
    }
}

impl EditorPrefs {
    /// Back to the shipped defaults: theme (colours, font, size), keymap, UI
    /// scale, grid and layout preset. The dock layout itself is rebuilt by
    /// the caller (the Settings tab requests the default preset).
    pub fn reset_to_defaults(&mut self) {
        *self = Self::default();
    }
}

/// Saves [`EditorPrefs`] when they change (Settings tab edits), so settings
/// survive a crash or a game that never calls `save_layout`. Waits while a
/// pointer button is down so dragging a slider or colour picker writes one
/// file at the end, not one per frame.
#[derive(Debug, Default)]
pub struct PrefsAutosave {
    saved: Option<EditorPrefs>,
}

impl PrefsAutosave {
    /// Treat `loaded` (what is already on disk, or the defaults) as saved.
    pub fn new(loaded: &EditorPrefs) -> Self {
        Self { saved: Some(loaded.clone()) }
    }

    /// Call once per frame. `Some(result)` when a save was attempted; a failed
    /// save is reported once, not retried every frame.
    pub fn tick(&mut self, dir: &Path, prefs: &EditorPrefs, pointer_down: bool) -> Option<Result<(), String>> {
        if pointer_down || self.saved.as_ref() == Some(prefs) {
            return None;
        }
        self.saved = Some(prefs.clone());
        Some(save_prefs(dir, prefs))
    }
}

pub fn prefs_path(dir: &Path) -> PathBuf {
    dir.join("editor_prefs.ron")
}
pub fn layout_path(dir: &Path) -> PathBuf {
    dir.join("editor_layout.ron")
}

pub fn save_prefs(dir: &Path, p: &EditorPrefs) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(prefs_path(dir), p.to_ron()?).map_err(|e| e.to_string())
}
/// Missing or corrupt files fall back to defaults (the error is returned for the console).
pub fn load_prefs(dir: &Path) -> (EditorPrefs, Option<String>) {
    match std::fs::read_to_string(prefs_path(dir)) {
        Ok(t) => match EditorPrefs::from_ron(&t) {
            Ok(p) => (p, None),
            Err(e) => (EditorPrefs::default(), Some(format!("editor prefs ignored: {e}"))),
        },
        Err(_) => (EditorPrefs::default(), None),
    }
}

pub fn save_layout(dir: &Path, l: &DockState<EditorTab>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(layout_path(dir), dock_skeleton::layout_to_ron(l)?).map_err(|e| e.to_string())
}
pub fn load_layout(dir: &Path) -> Result<DockState<EditorTab>, String> {
    dock_skeleton::layout_from_ron(&std::fs::read_to_string(layout_path(dir)).map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dust_prefs_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn prefs_roundtrip_and_clamp() {
        let mut p = EditorPrefs::default();
        p.keymap = KeymapPreset::Blender;
        p.ui_scale = 1.5;
        p.layout_preset = LayoutPreset::Tall;
        p.theme.accent = [0.1, 0.9, 0.3];
        assert_eq!(EditorPrefs::from_ron(&p.to_ron().unwrap()).unwrap(), p);
        assert_eq!(EditorPrefs::from_ron("(ui_scale: 99.0)").unwrap().ui_scale, 3.0);
        assert!(EditorPrefs::from_ron("(keymap: \"Emacs\")").is_err());
    }

    #[test]
    fn reset_restores_every_default() {
        let mut p = EditorPrefs::default();
        p.keymap = KeymapPreset::Blender;
        p.ui_scale = 2.5;
        p.show_grid = false;
        p.layout_preset = LayoutPreset::Tall;
        p.theme.accent = [1.0, 0.0, 0.0];
        p.theme.font = "monospace".into();
        p.theme.font_size = 20.0;
        p.reset_to_defaults();
        assert_eq!(p, EditorPrefs::default());
    }

    #[test]
    fn autosave_writes_on_change_after_pointer_release_and_only_once() {
        let d = tmp("autosave");
        let mut p = EditorPrefs::default();
        let mut auto = PrefsAutosave::new(&p);
        assert!(auto.tick(&d, &p, false).is_none(), "unchanged prefs are not written");
        assert!(!prefs_path(&d).exists());

        p.ui_scale = 1.75;
        assert!(auto.tick(&d, &p, true).is_none(), "wait while dragging");
        assert!(!prefs_path(&d).exists());
        assert!(matches!(auto.tick(&d, &p, false), Some(Ok(()))));
        assert_eq!(load_prefs(&d).0.ui_scale, 1.75);
        assert!(auto.tick(&d, &p, false).is_none(), "saved once");

        p.theme.accent = [0.2, 0.3, 0.4];
        assert!(matches!(auto.tick(&d, &p, false), Some(Ok(()))));
        assert_eq!(load_prefs(&d).0, p);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn autosave_reports_a_failed_write_once() {
        // A file where the config directory should be: create_dir_all fails.
        let d = tmp("autosave_fail");
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        std::fs::write(&d, "not a dir").unwrap();
        let mut p = EditorPrefs::default();
        let mut auto = PrefsAutosave::new(&p);
        p.show_grid = false;
        assert!(matches!(auto.tick(&d, &p, false), Some(Err(_))));
        assert!(auto.tick(&d, &p, false).is_none(), "not retried every frame");
        let _ = std::fs::remove_file(&d);
    }

    #[test]
    fn files_roundtrip_and_corrupt_falls_back() {
        let d = tmp("files");
        let mut p = EditorPrefs::default();
        p.ui_scale = 2.0;
        save_prefs(&d, &p).unwrap();
        assert_eq!(load_prefs(&d), (p, None));
        std::fs::write(prefs_path(&d), "garbage(").unwrap();
        let (p2, err) = load_prefs(&d);
        assert_eq!(p2, EditorPrefs::default());
        assert!(err.is_some());
        let layout = dock_skeleton::build_layout(LayoutPreset::Tall, &[]);
        save_layout(&d, &layout).unwrap();
        let back = load_layout(&d).unwrap();
        assert_eq!(dock_skeleton::layout_to_ron(&back), dock_skeleton::layout_to_ron(&layout));
        let _ = std::fs::remove_dir_all(&d);
    }
}
