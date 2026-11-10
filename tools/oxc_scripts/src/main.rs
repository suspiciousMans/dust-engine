//! oxc_scripts — compile a directory of `.ox` game scripts to loadable
//! libraries, print the generated API declarations, or convert `.pss`.
//!
//!   oxc_scripts build <scripts_dir> [--out DIR] [--opt N]
//!   oxc_scripts api [--out FILE]
//!   oxc_scripts convert <file.pss>... [--out-dir DIR]

#[path = "../../../engine/src/script/ox/build/mod.rs"]
#[allow(dead_code)]
mod build;
mod convert;

use build::{build_script, BuildOptions, Toolchain};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("build") => cmd_build(&args[1..]),
        Some("api") => cmd_api(&args[1..]),
        Some("convert") => cmd_convert(&args[1..]),
        _ => {
            eprintln!("usage:\n  oxc_scripts build <scripts_dir> [--out DIR] [--opt N]\n  oxc_scripts api [--out FILE]\n  oxc_scripts convert <file.pss>... [--out-dir DIR]");
            2
        }
    };
    std::process::exit(code);
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn positional(args: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if a.starts_with("--") {
            skip = true;
        } else {
            v.push(a.clone());
        }
    }
    v
}

fn cmd_build(args: &[String]) -> i32 {
    let Some(dir) = positional(args).into_iter().next() else {
        eprintln!("build: missing <scripts_dir>");
        return 2;
    };
    let out = PathBuf::from(flag(args, "--out").unwrap_or_else(|| format!("{dir}/.ox-build")));
    let opt_level = flag(args, "--opt").and_then(|s| s.parse().ok()).unwrap_or(1);
    let tc = match Toolchain::discover() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.extension().map_or(false, |e| e == "ox")).collect(),
        Err(e) => {
            eprintln!("error: cannot read {dir}: {e}");
            return 2;
        }
    };
    files.sort();
    let mut failed = 0;
    for f in &files {
        let t = std::time::Instant::now();
        match build_script(&tc, f, &out, &BuildOptions { opt_level, generation: 0 }) {
            Ok(b) => println!("ok    {} -> {} [hooks: {}] ({:.1}s)", b.name, b.lib_path.display(), b.hooks.join(", "), t.elapsed().as_secs_f32()),
            Err(d) => {
                failed += 1;
                println!("FAIL  {}", f.display());
                for line in d.to_strings() {
                    println!("  {line}");
                }
            }
        }
    }
    println!("{} script(s), {} failed", files.len(), failed);
    if failed > 0 { 1 } else { 0 }
}

fn cmd_api(args: &[String]) -> i32 {
    let text = build::spec::api_ox();
    match flag(args, "--out") {
        Some(p) => match std::fs::write(&p, text) {
            Ok(()) => {
                println!("wrote {p}");
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        None => {
            print!("{text}");
            0
        }
    }
}

fn cmd_convert(args: &[String]) -> i32 {
    let files = positional(args);
    if files.is_empty() {
        eprintln!("convert: no input files");
        return 2;
    }
    let out_dir = flag(args, "--out-dir");
    let mut status = 0;
    for f in files {
        let src = match std::fs::read_to_string(&f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{f}: {e}");
                status = 1;
                continue;
            }
        };
        let name = PathBuf::from(&f).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        match convert::convert(&src, &format!("{name}.pss")) {
            Ok(res) => {
                let target = match &out_dir {
                    Some(d) => PathBuf::from(d).join(format!("{name}.ox")),
                    None => PathBuf::from(&f).with_extension("ox"),
                };
                if let Some(p) = target.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                if let Err(e) = std::fs::write(&target, res.ox) {
                    eprintln!("{}: {e}", target.display());
                    status = 1;
                    continue;
                }
                println!("converted {f} -> {}", target.display());
                for w in res.warnings {
                    println!("  warning: {w}");
                }
            }
            Err(e) => {
                println!("skipped   {f}: {e}");
                status = 1;
            }
        }
    }
    status
}
