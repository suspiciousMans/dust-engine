//! Balance/telemetry CSV hook. Replaces per-game ad-hoc CSV loggers.
//!
//! ```ignore
//! let mut t = Telemetry::from_env("DREAMSCAPE_BALANCE"); // disabled when unset
//! t.record("kill", &[("t", 12.5), ("hp", 3.0)]);
//! ```
//!
//! The output is a single CSV file. The first time an event name is seen (or
//! when its field list changes) a header row `event,<field>,...` is written,
//! then data rows `<name>,<value>,...`. Output is buffered; call [`flush`]
//! or drop the value to push it out. A disabled hook costs one branch per
//! `record`. IO errors never panic: the first one is logged and the hook
//! disables itself.
//!
//! [`flush`]: Telemetry::flush

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct Telemetry {
    out: Option<BufWriter<std::fs::File>>,
    /// Last header (field names) written for each event name.
    headers: HashMap<String, Vec<String>>,
}

impl Telemetry {
    /// Open `path` for appending (created if missing). On failure the error
    /// is logged once and the returned hook is disabled.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => Self { out: Some(BufWriter::new(f)), headers: HashMap::new() },
            Err(e) => {
                log::warn!("telemetry: cannot open {}: {e}; disabled", path.display());
                Self::disabled()
            }
        }
    }

    /// A hook that does nothing.
    pub fn disabled() -> Self {
        Self { out: None, headers: HashMap::new() }
    }

    /// Enabled only if env var `var` is set and non-empty; its value is the
    /// output path.
    pub fn from_env(var: &str) -> Self {
        match std::env::var_os(var) {
            Some(p) if !p.is_empty() => Self::new(p),
            _ => Self::disabled(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.out.is_some()
    }

    /// Append one row for `event`. No-op when disabled.
    pub fn record(&mut self, event: &str, fields: &[(&str, f64)]) {
        if self.out.is_none() {
            return;
        }
        let mut text = String::new();
        let header_matches = self
            .headers
            .get(event)
            .is_some_and(|h| h.len() == fields.len() && h.iter().zip(fields).all(|(a, (b, _))| a == b));
        if !header_matches {
            text.push_str("event");
            for (name, _) in fields {
                text.push(',');
                push_csv(&mut text, name);
            }
            text.push('\n');
            self.headers.insert(event.to_string(), fields.iter().map(|(n, _)| n.to_string()).collect());
        }
        push_csv(&mut text, event);
        for (_, v) in fields {
            text.push(',');
            text.push_str(&v.to_string());
        }
        text.push('\n');
        let res = self.out.as_mut().map(|w| w.write_all(text.as_bytes()));
        if let Some(Err(e)) = res {
            self.fail(e);
        }
    }

    /// Push buffered rows to disk. No-op when disabled.
    pub fn flush(&mut self) {
        if let Some(Err(e)) = self.out.as_mut().map(|w| w.flush()) {
            self.fail(e);
        }
    }

    fn fail(&mut self, e: std::io::Error) {
        log::warn!("telemetry: write failed: {e}; disabled");
        self.out = None;
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(w) = self.out.as_mut() {
            if let Err(e) = w.flush() {
                log::warn!("telemetry: flush on drop failed: {e}");
            }
        }
    }
}

fn push_csv(out: &mut String, s: &str) {
    if s.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "jame_telemetry_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn header_per_event_name_and_rows() {
        let d = tmp_dir("rows");
        let p = d.join("t.csv");
        {
            let mut t = Telemetry::new(&p);
            assert!(t.is_enabled());
            t.record("kill", &[("t", 1.5), ("hp", 3.0)]);
            t.record("kill", &[("t", 2.0), ("hp", 2.0)]);
            t.record("pickup", &[("id", 7.0)]);
            t.record("kill", &[("t", 3.0), ("hp", 1.0)]);
        } // flushed on drop
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s, "event,t,hp\nkill,1.5,3\nkill,2,2\nevent,id\npickup,7\nkill,3,1\n");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn changed_field_list_reemits_header() {
        let d = tmp_dir("hdr");
        let p = d.join("t.csv");
        let mut t = Telemetry::new(&p);
        t.record("e", &[("a", 1.0)]);
        t.record("e", &[("a", 2.0), ("b", 3.0)]);
        t.flush();
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s, "event,a\ne,1\nevent,a,b\ne,2,3\n");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn explicit_flush_before_drop_and_quoting() {
        let d = tmp_dir("flush");
        let p = d.join("t.csv");
        let mut t = Telemetry::new(&p);
        t.record("a,b", &[("x\"y", 0.0)]);
        t.flush();
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s, "event,\"x\"\"y\"\n\"a,b\",0\n");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn disabled_is_noop() {
        let mut t = Telemetry::disabled();
        assert!(!t.is_enabled());
        t.record("x", &[("a", 1.0)]);
        t.flush();
        let e = Telemetry::from_env("JAME_TELEMETRY_TEST_SURELY_UNSET_VAR");
        assert!(!e.is_enabled());
    }

    #[test]
    fn unwritable_path_does_not_panic() {
        let d = tmp_dir("bad");
        // Parent directory does not exist.
        let mut t = Telemetry::new(d.join("missing_dir").join("t.csv"));
        assert!(!t.is_enabled());
        t.record("x", &[("a", 1.0)]);
        t.flush();
        // A directory is not a writable file.
        let mut t = Telemetry::new(&d);
        assert!(!t.is_enabled());
        t.record("x", &[("a", 1.0)]);
        let _ = std::fs::remove_dir_all(d);
    }
}
