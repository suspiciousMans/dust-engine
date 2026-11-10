//! Drives `oxidized` and `rustc` to turn one `.ox` file into a loadable cdylib.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::codegen::{find_hooks, gen_shim};
use super::diag::{parse_oxidized_stderr, parse_rustc_stderr, ScriptDiagnostics};
use super::spec;

/// Where the compilers are. `oxidized` is a separate project, so it is looked
/// up rather than linked: `$OXIDIZED_BIN`, then `PATH`, then well-known spots.
#[derive(Clone, Debug)]
pub struct Toolchain {
    pub oxidized: PathBuf,
    pub rustc: PathBuf,
}

impl Toolchain {
    pub fn discover() -> Result<Self, String> {
        let oxidized = find_exe("OXIDIZED_BIN", "oxidized", &["~/.local/bin/oxidized", "~/oxidized/target/release/oxidized"])
            .ok_or("`oxidized` compiler not found (set OXIDIZED_BIN, or put it on PATH)")?;
        let rustc = find_exe("RUSTC", "rustc", &["~/.cargo/bin/rustc"]).ok_or("`rustc` not found on PATH")?;
        Ok(Self { oxidized, rustc })
    }
}

fn find_exe(env: &str, name: &str, fallbacks: &[&str]) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env).map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let c = dir.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    fallbacks.iter().map(|f| PathBuf::from(f.replacen('~', &home, 1))).find(|p| p.is_file())
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// rustc `-C opt-level` for the script cdylib. 1 is ~3 s; 0 is ~2 s.
    pub opt_level: u8,
    /// Appended to the output file name so a reload never reuses a dlopen'ed path.
    pub generation: u64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { opt_level: 1, generation: 0 }
    }
}

#[derive(Clone, Debug)]
pub struct BuiltScript {
    pub name: String,
    pub lib_path: PathBuf,
    pub rs_path: PathBuf,
    pub hooks: Vec<String>,
}

pub fn lib_file_name(name: &str, generation: u64) -> String {
    let (pre, ext) = if cfg!(windows) {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    format!("{pre}oxs_{name}.{generation}.{ext}")
}

/// Compiles `src` (a `.ox` file) into `out_dir/<name>/…`. Never panics on bad
/// scripts: every failure comes back as [`ScriptDiagnostics`].
pub fn build_script(tc: &Toolchain, src: &Path, out_dir: &Path, opts: &BuildOptions) -> Result<BuiltScript, ScriptDiagnostics> {
    let file_label = src.display().to_string();
    let io = |what: &str, e: std::io::Error| ScriptDiagnostics::single(&file_label, 0, 0, format!("{what}: {e}"));
    let name = src.file_stem().and_then(|s| s.to_str()).unwrap_or("script").replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    let src = std::fs::canonicalize(src).map_err(|e| io("cannot read script", e))?;
    let source = std::fs::read_to_string(&src).map_err(|e| io("cannot read script", e))?;
    let hooks = find_hooks(&source, &file_label)?;

    let work = std::fs::canonicalize({
        std::fs::create_dir_all(out_dir.join(&name)).map_err(|e| io("cannot create build dir", e))?;
        out_dir.join(&name)
    })
    .map_err(|e| io("build dir", e))?;

    let api_path = work.join("engine_api.ox");
    write_if_changed(&api_path, &spec::api_ox()).map_err(|e| io("write engine_api.ox", e))?;
    let entry = work.join("entry.ox");
    let entry_src = format!("import {:?}\nimport {:?}\n", api_path.display().to_string(), src.display().to_string());
    write_if_changed(&entry, &entry_src).map_err(|e| io("write entry.ox", e))?;

    let rs_path = work.join(format!("{name}.rs"));
    let out = Command::new(&tc.oxidized)
        .arg("build")
        .arg(&entry)
        .arg("-o")
        .arg(&rs_path)
        .current_dir(&work)
        .output()
        .map_err(|e| io("cannot run oxidized", e))?;
    if !out.status.success() {
        let text = format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
        // Errors in the entry stub itself can't happen; anything unlabelled is the script.
        return Err(parse_oxidized_stderr(&text, &file_label));
    }

    // The transpiler emits private fns; the shim lives outside `mod ox`, so export the hooks.
    let mut rs = std::fs::read_to_string(&rs_path).map_err(|e| io("read transpiled rust", e))?;
    for h in &hooks {
        rs = rs.replace(&format!("\nfn {}(", h.name), &format!("\npub fn {}(", h.name));
    }
    let pub_rs_path = work.join(format!("{name}.pub.rs"));
    std::fs::write(&pub_rs_path, rs).map_err(|e| io("write transpiled rust", e))?;
    let shim_path = work.join(format!("{name}.shim.rs"));
    let hook_refs: Vec<&_> = hooks.iter().copied().collect();
    std::fs::write(&shim_path, gen_shim(&pub_rs_path.display().to_string(), &hook_refs)).map_err(|e| io("write shim", e))?;

    let lib_path = work.join(lib_file_name(&name, opts.generation));
    let out = Command::new(&tc.rustc)
        .args(["--edition", "2021", "--crate-type", "cdylib", "--crate-name"])
        .arg(format!("oxs_{name}"))
        .args(["-C", &format!("opt-level={}", opts.opt_level), "-C", "debuginfo=0", "-C", "panic=unwind", "--cap-lints", "allow", "-o"])
        .arg(&lib_path)
        .arg(&shim_path)
        .current_dir(&work)
        .output()
        .map_err(|e| io("cannot run rustc", e))?;
    if !out.status.success() {
        return Err(parse_rustc_stderr(&String::from_utf8_lossy(&out.stderr), &file_label));
    }
    Ok(BuiltScript { name, lib_path, rs_path, hooks: hooks.iter().map(|h| h.name.to_string()).collect() })
}

fn write_if_changed(path: &Path, text: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).map(|c| c == text).unwrap_or(false) {
        return Ok(());
    }
    std::fs::write(path, text)
}
