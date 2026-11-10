//! Engine-agnostic editor core: scene hierarchy, undo/redo commands,
//! selection, reflection-lite inspector and egui panels.
//!
//! Nothing here touches SDL or OpenGL, so everything except the egui draw
//! functions in [`ui`] is unit-testable headless (and even `ui` is tested
//! against a bare `egui::Context`).

pub mod command;
pub mod hierarchy;
pub mod inspector;
pub mod selection;
pub mod state;

pub use hierarchy::{Children, Locked, Name, Parent, Primitive, SpawnOrder, Visible};
pub use inspector::{FieldDesc, FieldKind, Registry, Value};
pub use command::{Command, Ctx, UndoStack};
pub use selection::Selection;
pub use state::{EditorAction, EditorMode, EditorState, KeymapPreset, Tool};
