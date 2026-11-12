//! Oxidized (`.ox`) scripting: scripts transpile to Rust, build into a cdylib,
//! and are hot-swapped via `libloading`. See `docs/SCRIPTING-OXIDIZED.md`.

pub mod api;
pub mod build;
pub mod host;
pub mod runtime;
pub mod smoke;

pub use api::{BehaviorCtx, EntityState, HeadlessApi, InputSnapshot, OxApi, OxCommand, OxServices};
pub use build::{ScriptDiagnostic, ScriptDiagnostics};
pub use host::OxScripts;
pub use smoke::SmokeDriver;
pub use runtime::{LoadedScript, OxBehavior, OxRuntime, ReloadReport, ScriptSlot};

#[cfg(test)]
mod tests;
