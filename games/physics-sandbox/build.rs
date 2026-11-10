use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let scripts = manifest.join("scripts");

    let mut generated = String::new();

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&scripts)
        .expect("scripts/ directory missing")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ox"))
        .collect();
    entries.sort();

    for entry in entries {
        let stem = entry.file_stem().unwrap().to_str().unwrap().to_string();
        let rs_path = out_dir.join(format!("{stem}.rs"));

        let exe = std::env::var("OXIDIZED").unwrap_or_else(|_| "oxidized".into());

        println!("cargo:rerun-if-changed={}", entry.display());
        let out = Command::new(&exe)
            .arg("build")
            .arg(&entry)
            .arg("-o")
            .arg(&rs_path)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `{exe}` on {}: {e}", entry.display()));

        if !out.status.success() {
            panic!(
                "oxidized failed on {}:\n{}",
                entry.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let code = std::fs::read_to_string(&rs_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", rs_path.display()));

        let (pre_main, main_part) = split_at_main(&code);
        let body = make_pub(pre_main);
        let main_body = make_pub(main_part);
        let main_body = rename_main(&main_body);

        generated.push_str(&format!(
            "pub mod {stem} {{
#[allow(unused_imports, dead_code, unused_variables, unused_mut, unused_parens)]
use crate::__oxidized_natives::*;
{body}
{main_body}
}}
"
        ));
    }

    let include = out_dir.join("ox_generated.rs");
    std::fs::write(&include, format!("pub mod ox_modules {{\n{generated}\n}}\n"))
        .unwrap_or_else(|e| panic!("writing ox_generated.rs: {e}"));

    println!("cargo:rerun-if-changed=scripts");
    println!("cargo:rerun-if-changed=assets");
}

fn split_at_main(code: &str) -> (&str, &str) {
    let mut offset = 0usize;
    for line in code.lines() {
        if line.starts_with("fn main(") {
            return (&code[..offset], &code[offset..]);
        }
        offset += line.len() + 1;
    }
    (code, "\nfn main() {}\n")
}

fn make_pub(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    for line in code.lines() {
        if let Some(rest) = line.strip_prefix("fn ") {
            if rest.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                out.push_str("pub fn ");
                out.push_str(rest);
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn rename_main(code: &str) -> String {
    code.replacen("fn main(", "fn ox_main(", 1)
}
