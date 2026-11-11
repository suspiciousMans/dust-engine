//! Confirmation prompts of the File menu: unsaved changes, autosave
//! recovery, legacy-level overwrite and revert. Pure data plus one egui
//! function; the shell decides what each answer does.

use std::path::{Path, PathBuf};

/// What the user asked for while a prompt was in the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingAction {
    NewScene,
    OpenScene(PathBuf),
    /// The host asked to close the editor ([`super::shell::EditorShell::request_quit`]).
    Quit,
}

impl PendingAction {
    fn verb(&self) -> &'static str {
        match self {
            Self::NewScene => "creating a new scene",
            Self::OpenScene(_) => "opening another scene",
            Self::Quit => "quitting",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// Unsaved changes stand in the way of `PendingAction`.
    Unsaved(PendingAction),
    /// An autosave newer than the scene at this path exists.
    Recover(PathBuf),
    /// Saving would write a legacy level file in the new scene format.
    LevelSave { path: PathBuf, message: String, then: Option<PendingAction> },
    /// Revert discards unsaved changes.
    Revert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Save / Recover / Save as Scene / Revert.
    Primary,
    /// Don't Save / Open saved file.
    Secondary,
    Cancel,
}

impl Confirm {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Unsaved(_) => "Unsaved changes",
            Self::Recover(_) => "Recover autosave?",
            Self::LevelSave { .. } => "Convert level to scene format?",
            Self::Revert => "Revert scene?",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Unsaved(a) => format!("The scene has unsaved changes. Save them before {}?", a.verb()),
            Self::Recover(p) => format!(
                "{} has an autosave that is newer than the saved file (the editor probably closed unexpectedly). Recover the autosaved version?",
                file_name(p)
            ),
            Self::LevelSave { message, .. } => message.clone(),
            Self::Revert => "Discard all unsaved changes and reload the scene from disk?".into(),
        }
    }

    pub fn labels(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Unsaved(_) => ("Save", Some("Don't Save")),
            Self::Recover(_) => ("Recover", Some("Open saved file")),
            Self::LevelSave { .. } => ("Save as Scene", None),
            Self::Revert => ("Revert", None),
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Draws the prompt; returns the user's answer once they click.
pub fn draw_confirm(ctx: &egui::Context, c: &Confirm) -> Option<Choice> {
    let mut out = None;
    let (primary, secondary) = c.labels();
    egui::Window::new(c.title()).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(ctx, |ui| {
        ui.set_max_width(380.0);
        ui.label(c.message());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(primary).clicked() {
                out = Some(Choice::Primary);
            }
            if let Some(s) = secondary {
                if ui.button(s).clicked() {
                    out = Some(Choice::Secondary);
                }
            }
            if ui.button("Cancel").clicked() {
                out = Some(Choice::Cancel);
            }
        });
    });
    out
}
