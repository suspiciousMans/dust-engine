//! Compile diagnostics in a shape an editor console can show directly.

/// One compiler message. `line`/`col` are 1-based, `0` when unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptDiagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub message: String,
}

impl std::fmt::Display for ScriptDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(f, "{}:{}:{}: {}", self.file, self.line, self.col, self.message)
        } else {
            write!(f, "{}: {}", self.file, self.message)
        }
    }
}

/// All diagnostics from one failed build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptDiagnostics {
    pub items: Vec<ScriptDiagnostic>,
}

impl ScriptDiagnostics {
    pub fn single(file: &str, line: u32, col: u32, message: impl Into<String>) -> Self {
        Self { items: vec![ScriptDiagnostic { file: file.to_string(), line, col, message: message.into() }] }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// One `file:line:col: message` string per diagnostic, for console output.
    pub fn to_strings(&self) -> Vec<String> {
        self.items.iter().map(|d| d.to_string()).collect()
    }
}

impl std::fmt::Display for ScriptDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, d) in self.items.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ScriptDiagnostics {}

/// Parses `oxidized` compiler stderr. Located errors look like
/// ` oxidized: type error: <msg> at line 2, col 5 — in path/to/file.ox`
/// (the ` — in` suffix appears only for imported files), followed by a
/// `-->`/caret snippet we drop. `default_file` is used when no file is named.
pub fn parse_oxidized_stderr(stderr: &str, default_file: &str) -> ScriptDiagnostics {
    let mut items = Vec::new();
    for raw in stderr.lines() {
        let t = raw.trim_start();
        let is_head = t.starts_with("oxidized:")
            || (!raw.starts_with(' ') && t.split(':').next().map_or(false, |p| p.ends_with("error") && p.len() < 24));
        if !is_head {
            continue;
        }
        let mut msg = t.strip_prefix("oxidized:").unwrap_or(t).trim().to_string();
        let mut file = default_file.to_string();
        if let Some(i) = msg.rfind(" — in ") {
            file = msg[i + " — in ".len()..].trim().to_string();
            msg.truncate(i);
        }
        let (mut line, mut col) = (0, 0);
        if let Some(i) = msg.rfind(" at line ") {
            let loc = &msg[i + " at line ".len()..];
            let mut parts = loc.split(", col ");
            line = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            col = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            if line > 0 {
                msg.truncate(i);
            }
        }
        items.push(ScriptDiagnostic { file, line, col, message: msg });
    }
    if items.is_empty() {
        let text = stderr.trim();
        items.push(ScriptDiagnostic {
            file: default_file.to_string(),
            line: 0,
            col: 0,
            message: if text.is_empty() { "oxidized failed with no output".to_string() } else { text.to_string() },
        });
    }
    ScriptDiagnostics { items }
}

/// Parses `rustc` stderr (`error[E0308]: msg` + ` --> file:line:col`). These
/// point into generated Rust, so they are labelled as such.
pub fn parse_rustc_stderr(stderr: &str, script_file: &str) -> ScriptDiagnostics {
    let mut items: Vec<ScriptDiagnostic> = Vec::new();
    let mut pending: Option<String> = None;
    for raw in stderr.lines() {
        if raw.starts_with("error") && raw.contains(':') && !raw.starts_with("error: aborting") {
            pending = Some(raw.split_once(':').map(|x| x.1).unwrap_or(raw).trim().to_string());
        } else if let (Some(msg), Some(loc)) = (pending.as_ref(), raw.trim_start().strip_prefix("--> ")) {
            let mut it = loc.rsplitn(3, ':');
            let col = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let line = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let _file = it.next().unwrap_or("");
            items.push(ScriptDiagnostic {
                file: script_file.to_string(),
                line: 0,
                col: 0,
                message: format!("rustc: {msg} (generated Rust line {line}, col {col})"),
            });
            pending = None;
        }
        if items.len() >= 8 {
            break;
        }
    }
    if let Some(msg) = pending {
        items.push(ScriptDiagnostic { file: script_file.to_string(), line: 0, col: 0, message: format!("rustc: {msg}") });
    }
    if items.is_empty() {
        items.push(ScriptDiagnostic {
            file: script_file.to_string(),
            line: 0,
            col: 0,
            message: format!("rustc failed: {}", stderr.trim()),
        });
    }
    ScriptDiagnostics { items }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_located_error_in_import() {
        let s = " oxidized: type error: type mismatch: `a` expected `Int` but found `String` at line 2, col 5 — in imp/bad.ox\n   --> 2:5\n    |\n  2 |     let a: Int = \"x\"\n    |     ^^^\n";
        let d = parse_oxidized_stderr(s, "dflt.ox");
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].file, "imp/bad.ox");
        assert_eq!((d.items[0].line, d.items[0].col), (2, 5));
        assert!(d.items[0].message.starts_with("type error: type mismatch"));
    }

    #[test]
    fn parses_multiple_errors_default_file() {
        let s = " oxidized: type error: call to `ping` has 2 argument(s), expected 1 at line 3, col 5\n   --> 3:5\n    |\n  3 |     ping(dt, 2)\n    |     ^^^^\ntype error: call to undefined function `nosuch` at line 4, col 5\n   --> 4:5\n";
        let d = parse_oxidized_stderr(s, "me.ox");
        assert_eq!(d.items.len(), 2);
        assert_eq!(d.items[1].line, 4);
        assert_eq!(d.items[1].file, "me.ox");
    }

    #[test]
    fn rustc_errors() {
        let s = "error[E0308]: mismatched types\n --> shim.rs:10:5\n  |\nerror: aborting due to 1 previous error\n";
        let d = parse_rustc_stderr(s, "a.ox");
        assert_eq!(d.items.len(), 1);
        assert!(d.items[0].message.contains("mismatched types"));
    }
}
