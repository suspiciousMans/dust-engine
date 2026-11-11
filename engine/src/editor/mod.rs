//! Engine-agnostic editor core: scene hierarchy, undo/redo commands,
//! selection, reflection-lite inspector and egui panels.
//!
//! Nothing here touches SDL or OpenGL, so everything except the egui draw
//! functions in [`ui`] is unit-testable headless (and even `ui` is tested
//! against a bare `egui::Context`).

pub mod bounds;
pub mod cam_matrices;
pub mod camera;
pub mod command;
pub mod console;
pub mod gizmo;
pub mod interaction;
pub mod modal;
pub mod picking;
pub mod prefs;
pub mod scene_io;
pub mod shell;
pub mod snapping;
pub mod world_xform;
pub mod hierarchy;
pub mod inspector;
pub mod selection;
pub mod state;
pub mod ui;
pub mod viewport_target;

pub use hierarchy::{Children, Locked, Name, Parent, Primitive, SpawnOrder, Visible};
pub use inspector::{FieldDesc, FieldKind, Registry, Value};
pub use command::{Command, Ctx, UndoStack};
pub use selection::Selection;
pub use state::{EditorAction, EditorMode, EditorState, KeymapPreset, Tool};
pub use ui::{draw_hierarchy, draw_inspector, handle_shortcuts};
