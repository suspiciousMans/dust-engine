//! Oxidized (`.ox`) scripting: scripts transpile to Rust, build into a cdylib,
//! and are hot-swapped via `libloading`. See `docs/SCRIPTING-OXIDIZED.md`.

pub mod api;
pub mod build;
#[cfg(not(target_os = "emscripten"))]
pub mod host;
#[cfg(not(target_os = "emscripten"))]
pub mod runtime;
#[cfg(not(target_os = "emscripten"))]
pub mod smoke;

pub use api::{BehaviorCtx, EntityState, HeadlessApi, InputSnapshot, OxApi, OxCommand, OxServices};
pub use build::{ScriptDiagnostic, ScriptDiagnostics};
#[cfg(not(target_os = "emscripten"))]
pub use host::OxScripts;
#[cfg(not(target_os = "emscripten"))]
pub use smoke::SmokeDriver;
#[cfg(not(target_os = "emscripten"))]
pub use runtime::{LoadedScript, OxBehavior, OxRuntime, ScriptSlot};

#[cfg(test)]
mod tests;

/// Result of one (re)build, as surfaced to the editor console.
#[derive(Clone, Debug)]
pub struct ReloadReport {
    pub script: String,
    pub result: Result<(), ScriptDiagnostics>,
}

