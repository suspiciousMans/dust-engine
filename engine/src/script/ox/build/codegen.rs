//! Hook discovery and cdylib shim generation.

use super::diag::ScriptDiagnostics;
use super::spec;

/// A lifecycle hook a script may define, and how the shim calls it.
#[derive(Debug)]
pub struct HookDef {
    /// Script-side function name.
    pub name: &'static str,
    /// Required parameter types (`Float` / `String`), in order.
    pub params: &'static [&'static str],
}

/// Hooks mirror `behavior::Behavior`: `on_ready`, `on_update`, `on_interact`,
/// `on_trigger_enter/exit`. `on_start` is accepted as an alias of `on_ready`.
pub const HOOKS: &[HookDef] = &[
    HookDef { name: "on_ready", params: &[] },
    HookDef { name: "on_start", params: &[] },
    HookDef { name: "on_update", params: &["Float"] },
    HookDef { name: "on_interact", params: &[] },
    HookDef { name: "on_trigger_enter", params: &["String"] },
    HookDef { name: "on_trigger_exit", params: &["String"] },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundFn {
    pub name: String,
    /// `(param, annotation)`
    pub params: Vec<(String, Option<String>)>,
    pub line: u32,
}

/// Finds top-level (column-0) `fn name(params)` declarations, ignoring comments.
pub fn scan_fns(src: &str) -> Vec<FoundFn> {
    let mut out = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("");
        let line = line.split("//").next().unwrap_or("");
        let Some(rest) = line.strip_prefix("fn ") else { continue };
        let rest = rest.trim_start();
        if rest.starts_with("native ") {
            continue;
        }
        let Some(open) = rest.find('(') else { continue };
        let name = rest[..open].trim().to_string();
        let Some(close) = rest[open..].find(')') else { continue };
        let inner = &rest[open + 1..open + close];
        let params = inner
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once(':') {
                Some((n, t)) => (n.trim().to_string(), Some(t.trim().to_string())),
                None => (p.to_string(), None),
            })
            .collect();
        out.push(FoundFn { name, params, line: i as u32 + 1 });
    }
    out
}

fn norm_ty(t: &str) -> &str {
    match t {
        "f64" | "f32" | "Float" => "Float",
        "String" | "string" => "String",
        o => o,
    }
}

/// Which hooks the script defines, validating their signatures.
pub fn find_hooks(src: &str, file: &str) -> Result<Vec<&'static HookDef>, ScriptDiagnostics> {
    let fns = scan_fns(src);
    let mut hooks = Vec::new();
    let mut diags = ScriptDiagnostics::default();
    for def in HOOKS {
        let Some(f) = fns.iter().find(|f| f.name == def.name) else { continue };
        let ok = f.params.len() == def.params.len()
            && f.params.iter().zip(def.params).all(|((_, t), want)| t.as_deref().map(norm_ty) == Some(*want));
        if ok {
            hooks.push(def);
        } else {
            let want: Vec<String> = def.params.iter().enumerate().map(|(i, t)| format!("{}: {t}", ["dt", "other"][i.min(1)])).collect();
            diags.items.push(super::diag::ScriptDiagnostic {
                file: file.to_string(),
                line: f.line,
                col: 1,
                message: format!("hook `{}` must be declared `fn {}({})`", def.name, def.name, want.join(", ")),
            });
        }
    }
    if diags.is_empty() {
        Ok(hooks)
    } else {
        Err(diags)
    }
}

/// Generates the cdylib root for one script. `rs_file` is the absolute path of
/// the transpiled Rust. Exports `ox_<hook>` for each defined hook.
pub fn gen_shim(rs_file: &str, hooks: &[&HookDef]) -> String {
    let mut s = String::new();
    s.push_str("#![allow(warnings)]\n");
    s.push_str(&format!("const OX_API_HASH: u64 = {};\n", spec::api_hash()));
    s.push_str(spec::SHIM_PRELUDE);
    s.push_str(&spec::shim_natives_rs());
    s.push_str(&format!("mod ox {{ include!({rs_file:?}); }}\n"));
    for h in hooks {
        match (h.name, h.params) {
            (name, []) => s.push_str(&format!(
                "#[no_mangle] pub extern \"C\" fn ox_{name}() -> i32 {{ guarded(|| {{ ox::{name}(); }}) }}\n"
            )),
            (name, ["Float"]) => s.push_str(&format!(
                "#[no_mangle] pub extern \"C\" fn ox_{name}(a: f64) -> i32 {{ guarded(move || {{ ox::{name}(a); }}) }}\n"
            )),
            (name, ["String"]) => s.push_str(&format!(
                "#[no_mangle] pub extern \"C\" fn ox_{name}(p: *const u8, n: usize) -> i32 {{ let a = String::from_utf8_lossy(unsafe {{ std::slice::from_raw_parts(p, n) }}).into_owned(); guarded(move || {{ ox::{name}(a); }}) }}\n"
            )),
            _ => unreachable!(),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_hooks_and_validates() {
        let src = "fn native x();\nfn on_update(dt: Float) { }\n# fn on_ready() {}\nfn on_trigger_enter(other: String) {}\n";
        let h = find_hooks(src, "a.ox").unwrap();
        let names: Vec<_> = h.iter().map(|h| h.name).collect();
        assert_eq!(names, ["on_update", "on_trigger_enter"]);
    }

    #[test]
    fn rejects_untyped_hook() {
        let e = find_hooks("fn on_update(dt) {}\n", "a.ox").unwrap_err();
        assert_eq!(e.items[0].line, 1);
        assert!(e.items[0].message.contains("on_update(dt: Float)"));
    }
}
