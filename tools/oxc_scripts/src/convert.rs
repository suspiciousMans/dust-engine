//! Best-effort `.pss` → `.ox` behavior-script converter.
//!
//! Handles the behavior subset (`ready/update/interact/trigger_*` hooks, the
//! ScriptApi calls, top-level `let` state). Top-level `let`s become engine-held
//! per-entity variables (`get_var`/`set_var`), because `.ox` has no globals.
//! Smoke-test driver scripts (`tick()` + press/release/finish) are rejected:
//! they use a different host and stay `.pss` under the `legacy-pss` feature.

pub struct Converted {
    pub ox: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(String),
    Str(String),
    Comment(String),
    Ws(String),
    Punct(String),
}

fn lex(src: &str) -> Vec<Tok> {
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            let s = i;
            while i < c.len() && c[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok::Ws(c[s..i].iter().collect()));
        } else if ch == '/' && c.get(i + 1) == Some(&'/') {
            let s = i;
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            out.push(Tok::Comment(c[s..i].iter().collect()));
        } else if ch == '"' {
            let s = i;
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(c[s..i.min(c.len())].iter().collect()));
        } else if ch.is_ascii_digit() {
            let s = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(c[s..i].iter().collect()));
        } else if ch.is_alphabetic() || ch == '_' {
            let s = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(c[s..i].iter().collect()));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            if ["==", "!=", "<=", ">=", "&&", "||"].contains(&two.as_str()) {
                out.push(Tok::Punct(two));
                i += 2;
            } else {
                out.push(Tok::Punct(ch.to_string()));
                i += 1;
            }
        }
    }
    out
}

const HOOK_MAP: &[(&str, &str, &str)] = &[
    ("ready", "on_ready", ""),
    ("update", "on_update", "dt: Float"),
    ("interact", "on_interact", ""),
    ("trigger_enter", "on_trigger_enter", "other: String"),
    ("trigger_exit", "on_trigger_exit", "other: String"),
];

pub fn convert(src: &str, label: &str) -> Result<Converted, String> {
    let toks = lex(src);
    if toks.iter().any(|t| matches!(t, Tok::Ident(i) if i == "tick" || i == "finish" || i == "press")) {
        return Err("smoke-test driver script (tick/press/finish) — not a behavior; keep as .pss (legacy-pss feature)".into());
    }
    let mut warnings = Vec::new();

    // Pass 1: top-level `let name = <expr>;` (brace depth 0) → globals.
    let mut globals: Vec<(String, String)> = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct(p) if p == "{" => depth += 1,
            Tok::Punct(p) if p == "}" => depth -= 1,
            Tok::Ident(k) if k == "let" && depth == 0 => {
                let mut j = i + 1;
                let mut name = None;
                let mut init = String::new();
                let mut seen_eq = false;
                while j < toks.len() {
                    match &toks[j] {
                        Tok::Punct(p) if p == ";" => break,
                        Tok::Ident(n) if name.is_none() => name = Some(n.clone()),
                        Tok::Punct(p) if p == "=" && !seen_eq => seen_eq = true,
                        t if seen_eq => init.push_str(&tok_text(t)),
                        _ => {}
                    }
                    j += 1;
                }
                if let Some(n) = name {
                    globals.push((n, init.trim().to_string()));
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Pass 2: emit.
    let mut out = format!("# Converted from {label} by `oxc_scripts convert`. Review before use.\n# Top-level `let` state now lives in engine-held per-entity vars (get_var/set_var).\n\n");
    let mut depth = 0i32;
    let mut i = 0;
    let mut locals: Vec<String> = Vec::new();
    let mut fn_depth = 0i32;
    let mut close_pending = false; // closing `)` for a rewritten assignment
    let mut stmt_start = true;
    let mut in_global_let = false;
    while i < toks.len() {
        let t = &toks[i];
        // Skip top-level `let ...;`
        if in_global_let {
            if matches!(t, Tok::Punct(p) if p == ";") {
                in_global_let = false;
                // swallow following blank line whitespace handled naturally
            }
            i += 1;
            continue;
        }
        match t {
            Tok::Ident(k) if k == "let" && depth == 0 => {
                in_global_let = true;
                i += 1;
                continue;
            }
            Tok::Ident(k) if k == "fn" && depth == 0 => {
                // fn <name> ( params )
                let mut j = i + 1;
                while matches!(toks[j], Tok::Ws(_)) {
                    j += 1;
                }
                let Tok::Ident(name) = &toks[j] else { return Err("malformed fn".into()) };
                let (new_name, params, is_hook) = match HOOK_MAP.iter().find(|(o, _, _)| o == name) {
                    Some((_, n, p)) => (n.to_string(), Some(p.to_string()), true),
                    None => (name.clone(), None, false),
                };
                // collect original params
                let mut k = j + 1;
                while !matches!(&toks[k], Tok::Punct(p) if p == "(") {
                    k += 1;
                }
                let mut orig = Vec::new();
                k += 1;
                while !matches!(&toks[k], Tok::Punct(p) if p == ")") {
                    if let Tok::Ident(p) = &toks[k] {
                        orig.push(p.clone());
                    }
                    k += 1;
                }
                locals = orig.clone();
                out.push_str("fn ");
                if is_hook {
                    let p = params.unwrap();
                    // keep the script's own param name, apply the hook's type
                    let ty = p.split(": ").nth(1).unwrap_or("Float");
                    let shown = match orig.first() {
                        Some(n) if !p.is_empty() => format!("{n}: {ty}"),
                        _ => p,
                    };
                    out.push_str(&format!("{new_name}({shown})"));
                } else {
                    out.push_str(&format!("{new_name}({})", orig.join(", ")));
                    warnings.push(format!("helper fn `{name}`: parameters are untyped (dynamic); add annotations if you want static checks"));
                }
                i = k + 1;
                fn_depth = depth;
                continue;
            }
            Tok::Punct(p) if p == "{" => {
                depth += 1;
                out.push('{');
            }
            Tok::Punct(p) if p == "}" => {
                depth -= 1;
                out.push('}');
                if depth == fn_depth {
                    locals.clear();
                }
            }
            Tok::Punct(p) if p == ";" => {
                if close_pending {
                    out.push(')');
                    close_pending = false;
                }
                stmt_start = true;
                i += 1;
                continue;
            }
            Tok::Ws(w) => {
                out.push_str(w);
            }
            Tok::Comment(c) => out.push_str(c),
            Tok::Str(s) => {
                out.push_str(s);
                stmt_start = false;
            }
            Tok::Num(n) => {
                if n.starts_with("6.28318") {
                    out.push_str("tau()");
                } else {
                    out.push_str(n);
                }
                stmt_start = false;
            }
            Tok::Ident(name) => {
                let next_non_ws = toks[i + 1..].iter().find(|t| !matches!(t, Tok::Ws(_)));
                let is_call = matches!(next_non_ws, Some(Tok::Punct(p)) if p == "(");
                if name == "let" && depth > 0 {
                    // record local
                    if let Some(Tok::Ident(n)) = toks[i + 1..].iter().find(|t| !matches!(t, Tok::Ws(_))) {
                        locals.push(n.clone());
                    }
                    out.push_str("let");
                } else if is_call {
                    // consume `()` for zero-arg getters
                    let mapped = match name.as_str() {
                        "get_x" | "get_y" | "get_z" => {
                            let axis = match name.as_str() { "get_x" => 0, "get_y" => 1, _ => 2 };
                            // skip to after ')'
                            let mut k = i + 1;
                            while !matches!(&toks[k], Tok::Punct(p) if p == ")") {
                                k += 1;
                            }
                            out.push_str(&format!("pos({axis})"));
                            i = k + 1;
                            stmt_start = false;
                            continue;
                        }
                        "set_position" => "set_pos",
                        "play_sfx" => "play_sound",
                        "screen_flash" => {
                            warnings.push("screen_flash has no .ox API yet; left as a call that will fail to typecheck — remove or add it to spec.rs".into());
                            "screen_flash"
                        }
                        "log" | "time" | "move_by" | "play_tone" | "hud_bar" | "toast" | "camera_shake" | "sin" | "cos" => name.as_str(),
                        other => {
                            if !other.is_empty() && !HOOK_MAP.iter().any(|h| h.0 == other) {
                                // user helper or unknown native — leave as is
                            }
                            other
                        }
                    };
                    out.push_str(mapped);
                } else if let Some((_, init)) = globals.iter().find(|(g, _)| g == name).filter(|_| !locals.contains(name)) {
                    // assignment?
                    let is_assign = stmt_start
                        && matches!(next_non_ws, Some(Tok::Punct(p)) if p == "=")
                        && !matches!(toks[i + 1..].iter().filter(|t| !matches!(t, Tok::Ws(_))).nth(1), Some(Tok::Punct(p)) if p == "=");
                    if is_assign {
                        // skip to after '='
                        let mut k = i + 1;
                        while !matches!(&toks[k], Tok::Punct(p) if p == "=") {
                            k += 1;
                        }
                        out.push_str(&format!("set_var(\"{name}\", "));
                        close_pending = true;
                        i = k + 1;
                        while matches!(toks.get(i), Some(Tok::Ws(_))) {
                            i += 1;
                        }
                        stmt_start = false;
                        continue;
                    }
                    out.push_str(&format!("get_var(\"{name}\", {})", if init.contains('.') { init.clone() } else { format!("{init}.0") }));
                } else {
                    out.push_str(name);
                }
                stmt_start = false;
            }
            Tok::Punct(p) => {
                out.push_str(p);
                if p == "{" || p == "}" {
                    stmt_start = true;
                } else {
                    stmt_start = false;
                }
            }
        }
        if matches!(t, Tok::Punct(p) if p == "{" || p == "}") {
            stmt_start = true;
        }
        i += 1;
    }
    if out.contains("+ \"") || out.contains("\" +") {
        warnings.push("string concatenation with `+` found; .ox panics on string+number — use interpolation: \"text {value}\"".into());
    }
    Ok(Converted { ox: out, warnings })
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Ident(s) | Tok::Num(s) | Tok::Str(s) | Tok::Comment(s) | Tok::Ws(s) | Tok::Punct(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOB: &str = include_str!("../../../games/sandbox/scripts/bob_demo.pss");

    #[test]
    fn converts_bob_demo() {
        let c = convert(BOB, "bob_demo.pss").unwrap();
        assert!(c.ox.contains("fn on_ready()"), "{}", c.ox);
        assert!(c.ox.contains("fn on_update(dt: Float)"));
        assert!(c.ox.contains("fn on_interact()"));
        assert!(c.ox.contains("set_var(\"phase\", get_var(\"phase\", 0.0) + dt)"), "{}", c.ox);
        assert!(c.ox.contains("tau()"));
        assert!(c.ox.contains("play_sound(\"sfx/demo_blip.wav\")"));
        assert!(!c.ox.contains(';') || c.ox.matches(';').count() == 0, "semicolons left: {}", c.ox);
    }

    #[test]
    fn rejects_smoke_scripts() {
        let s = include_str!("../../../games/sandbox/scripts/smoke/basic_playthrough.pss");
        assert!(convert(s, "x.pss").is_err());
    }
}
