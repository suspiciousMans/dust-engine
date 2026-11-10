//! Oxidized (`.ox`) scripting: scripts transpile to Rust, build into a cdylib,
//! and are hot-swapped via `libloading`. See `docs/SCRIPTING-OXIDIZED.md`.

pub mod api;
pub mod build;
pub mod runtime;

pub use api::{BehaviorCtx, EntityState, HeadlessApi, InputSnapshot, OxApi, OxCommand, OxServices};
pub use build::{ScriptDiagnostic, ScriptDiagnostics};
pub use runtime::{LoadedScript, OxBehavior, OxRuntime, ReloadReport, ScriptSlot};

#[cfg(test)]
mod tests;
