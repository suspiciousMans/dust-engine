//! Console tab model: a capped log buffer with levels and a filter.
//! egui-free so it can be fed from anywhere (script diagnostics, `log`
//! bridges, editor errors) and unit-tested.

use std::collections::VecDeque;

use crate::script::ox::{ReloadReport, ScriptDiagnostics};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: LogLevel,
    pub text: String,
}

pub struct Console {
    entries: VecDeque<ConsoleEntry>,
    pub capacity: usize,
    pub filter: String,
    pub min_level: LogLevel,
    pub auto_scroll: bool,
}

impl Default for Console {
    fn default() -> Self {
        Self { entries: VecDeque::new(), capacity: 2000, filter: String::new(), min_level: LogLevel::Info, auto_scroll: true }
    }
}

impl Console {
    pub fn push(&mut self, level: LogLevel, text: impl Into<String>) {
        if self.entries.len() >= self.capacity.max(1) {
            self.entries.pop_front();
        }
        self.entries.push_back(ConsoleEntry { level, text: text.into() });
    }
    pub fn info(&mut self, t: impl Into<String>) {
        self.push(LogLevel::Info, t)
    }
    pub fn warn(&mut self, t: impl Into<String>) {
        self.push(LogLevel::Warn, t)
    }
    pub fn error(&mut self, t: impl Into<String>) {
        self.push(LogLevel::Error, t)
    }

    /// One Error entry per compiler diagnostic (`file:line:col: message`).
    pub fn push_diagnostics(&mut self, d: &ScriptDiagnostics) {
        for s in d.to_strings() {
            self.error(s);
        }
    }

    /// Reports the outcome of a hot-reload (`OxRuntime::poll`).
    pub fn push_reload(&mut self, r: &ReloadReport) {
        match &r.result {
            Ok(()) => self.info(format!("script '{}' reloaded", r.script)),
            Err(d) => {
                self.error(format!("script '{}' failed to build:", r.script));
                self.push_diagnostics(d);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn count(&self, level: LogLevel) -> usize {
        self.entries.iter().filter(|e| e.level == level).count()
    }

    /// Entries passing the level threshold and the case-insensitive filter.
    pub fn visible(&self) -> impl Iterator<Item = &ConsoleEntry> {
        let needle = self.filter.to_lowercase();
        let min = self.min_level;
        self.entries.iter().filter(move |e| e.level >= min && (needle.is_empty() || e.text.to_lowercase().contains(&needle)))
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Clear").clicked() {
                self.clear();
            }
            egui::ComboBox::from_id_salt("console_level")
                .selected_text(format!("{:?}+", self.min_level))
                .show_ui(ui, |ui| {
                    for l in [LogLevel::Info, LogLevel::Warn, LogLevel::Error] {
                        ui.selectable_value(&mut self.min_level, l, format!("{l:?}"));
                    }
                });
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter").desired_width(140.0));
            ui.checkbox(&mut self.auto_scroll, "Auto-scroll");
            ui.weak(format!("{} err  {} warn", self.count(LogLevel::Error), self.count(LogLevel::Warn)));
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).stick_to_bottom(self.auto_scroll).show(ui, |ui| {
            for e in self.visible() {
                let color = match e.level {
                    LogLevel::Info => ui.visuals().text_color(),
                    LogLevel::Warn => egui::Color32::from_rgb(230, 190, 70),
                    LogLevel::Error => egui::Color32::from_rgb(235, 90, 80),
                };
                ui.label(egui::RichText::new(&e.text).monospace().color(color));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_filter_and_levels() {
        let mut c = Console { capacity: 3, ..Default::default() };
        c.info("a");
        c.warn("b Warp");
        c.error("c");
        c.info("d");
        assert_eq!(c.len(), 3);
        assert_eq!(c.visible().next().unwrap().text, "b Warp");
        c.min_level = LogLevel::Warn;
        assert_eq!(c.visible().count(), 2);
        c.filter = "WARP".into();
        assert_eq!(c.visible().count(), 1);
    }

    #[test]
    fn diagnostics_become_error_lines() {
        let mut c = Console::default();
        c.push_diagnostics(&ScriptDiagnostics::single("a.ox", 3, 7, "boom"));
        let e: Vec<_> = c.visible().collect();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].level, LogLevel::Error);
        assert_eq!(e[0].text, "a.ox:3:7: boom");
        c.push_reload(&ReloadReport { script: "x".into(), result: Ok(()) });
        assert_eq!(c.count(LogLevel::Info), 1);
    }
}
