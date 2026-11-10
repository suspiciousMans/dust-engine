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
        let d = std::env::temp_dir().join(format!("jame_prefs_{name}_{}", std::process::id()));
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
