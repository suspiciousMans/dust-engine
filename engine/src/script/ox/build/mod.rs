//! Compile-side of Oxidized scripting: pure `std`, no engine dependencies, so
//! both the engine (hot reload) and `tools/oxc_scripts` (build step) include it.
//!
//! Pipeline per script: `foo.ox` → (`oxidized build`) → `foo.rs` → generated
//! shim (`include!`s foo.rs, exports C hooks) → `rustc --crate-type cdylib`.
pub mod codegen;
pub mod compile;
pub mod diag;
pub mod spec;

#[allow(unused_imports)]
pub use compile::{build_script, BuildOptions, BuiltScript, Toolchain};
#[allow(unused_imports)]
pub use diag::{ScriptDiagnostic, ScriptDiagnostics};
