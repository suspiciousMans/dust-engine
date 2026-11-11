//! Bus/mixer model: named buses with gain, mute and solo, arranged as a tree.
//! Pure data and math (no device); `AudioEngine` owns one and multiplies a
//! track's own gain by its bus chain.
//!
//! * Built-in buses: `master` (root), and `music`, `sfx`, `ui`, each a child
//!   of `master`. They cannot be removed (`master` cannot be re-parented
//!   either). Add your own with [`Mixer::add_bus`].
//! * **Effective gain** of a track = `track gain * product of the gains of
//!   its bus and every ancestor`. A mute anywhere on the chain silences it.
//! * **Solo**: while any bus or track is soloed, a track is audible only if
//!   it is itself soloed, or its bus is soloed, or one of its bus's
//!   ancestors is soloed (soloing `music` keeps everything under `music`;
//!   soloing a child keeps only that child, the ancestors just pass it
//!   through). Mute always wins over solo.
//! * Settings persist as RON via [`MixerSettings`], read through the scene
//!   module's `ron_options()` (so `Some(..)` may be omitted by hand).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const MASTER_BUS: &str = "master";
pub const MUSIC_BUS: &str = "music";
pub const SFX_BUS: &str = "sfx";
pub const UI_BUS: &str = "ui";

const BUILT_IN: [&str; 4] = [MASTER_BUS, MUSIC_BUS, SFX_BUS, UI_BUS];

#[derive(Clone, Debug, PartialEq)]
pub struct Bus {
    pub name: String,
    /// 0..=1.
    pub gain: f32,
    pub muted: bool,
    pub solo: bool,
    /// `None` only for `master`.
    pub parent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MixerError {
    #[error("bus name is empty")]
    EmptyName,
    #[error("bus `{0}` already exists")]
    Exists(String),
    #[error("no bus named `{0}`")]
    Unknown(String),
    #[error("`{0}` is a built-in bus and cannot be changed this way")]
    BuiltIn(String),
    #[error("making `{child}` a child of `{parent}` would form a cycle")]
    Cycle { child: String, parent: String },
}

fn sane(g: f32) -> Option<f32> {
    g.is_finite().then(|| g.clamp(0.0, 1.0))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mixer {
    buses: Vec<Bus>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    /// `master`, `music`, `sfx`, `ui` at gain 1.
    pub fn new() -> Self {
        let bus = |name: &str, parent: Option<&str>| Bus {
            name: name.to_string(),
            gain: 1.0,
            muted: false,
            solo: false,
            parent: parent.map(str::to_string),
        };
        Self { buses: vec![bus(MASTER_BUS, None), bus(MUSIC_BUS, Some(MASTER_BUS)), bus(SFX_BUS, Some(MASTER_BUS)), bus(UI_BUS, Some(MASTER_BUS))] }
    }

    pub fn is_built_in(name: &str) -> bool {
        BUILT_IN.contains(&name)
    }

    /// All buses, parents always before their children.
    pub fn buses(&self) -> &[Bus] {
        &self.buses
    }

    pub fn bus(&self, name: &str) -> Option<&Bus> {
        self.buses.iter().find(|b| b.name == name)
    }

    pub fn has_bus(&self, name: &str) -> bool {
        self.bus(name).is_some()
    }

    fn bus_mut(&mut self, name: &str) -> Option<&mut Bus> {
        self.buses.iter_mut().find(|b| b.name == name)
    }

    /// Direct children of `name`.
    pub fn children(&self, name: &str) -> Vec<&str> {
        self.buses.iter().filter(|b| b.parent.as_deref() == Some(name)).map(|b| b.name.as_str()).collect()
    }

    /// `name`, its parent, ..., `master`. Empty for an unknown bus.
    pub fn chain(&self, name: &str) -> Vec<&Bus> {
        let mut out = Vec::new();
        let mut cur = self.bus(name);
        while let Some(b) = cur {
            if out.len() > self.buses.len() {
                break; // defensive: cannot happen, cycles are rejected
            }
            out.push(b);
            cur = b.parent.as_deref().and_then(|p| self.bus(p));
        }
        out
    }

    /// Adds a bus under `parent`.
    pub fn add_bus(&mut self, name: &str, parent: &str) -> Result<(), MixerError> {
        if name.trim().is_empty() {
            return Err(MixerError::EmptyName);
        }
        if self.has_bus(name) {
            return Err(MixerError::Exists(name.to_string()));
        }
        if !self.has_bus(parent) {
            return Err(MixerError::Unknown(parent.to_string()));
        }
        self.buses.push(Bus { name: name.to_string(), gain: 1.0, muted: false, solo: false, parent: Some(parent.to_string()) });
        Ok(())
    }

    /// Removes a user bus; its children move to its parent. Returns the
    /// parent's name (where tracks on the removed bus should go).
    pub fn remove_bus(&mut self, name: &str) -> Result<String, MixerError> {
        if Self::is_built_in(name) {
            return Err(MixerError::BuiltIn(name.to_string()));
        }
        let i = self.buses.iter().position(|b| b.name == name).ok_or_else(|| MixerError::Unknown(name.to_string()))?;
        let removed = self.buses.remove(i);
        let parent = removed.parent.unwrap_or_else(|| MASTER_BUS.to_string());
        for b in &mut self.buses {
            if b.parent.as_deref() == Some(name) {
                b.parent = Some(parent.clone());
            }
        }
        Ok(parent)
    }

    /// Moves `name` under `parent`. Rejects `master`, unknown buses and cycles.
    pub fn set_parent(&mut self, name: &str, parent: &str) -> Result<(), MixerError> {
        if name == MASTER_BUS {
            return Err(MixerError::BuiltIn(name.to_string()));
        }
        if !self.has_bus(name) {
            return Err(MixerError::Unknown(name.to_string()));
        }
        if !self.has_bus(parent) {
            return Err(MixerError::Unknown(parent.to_string()));
        }
        if self.chain(parent).iter().any(|b| b.name == name) {
            return Err(MixerError::Cycle { child: name.to_string(), parent: parent.to_string() });
        }
        self.bus_mut(name).expect("checked").parent = Some(parent.to_string());
        self.reorder();
        Ok(())
    }

    /// Keeps "parents before children" after a re-parent.
    fn reorder(&mut self) {
        let mut ordered: Vec<Bus> = Vec::with_capacity(self.buses.len());
        let mut rest = std::mem::take(&mut self.buses);
        while !rest.is_empty() {
            let before = rest.len();
            let mut keep = Vec::new();
            for b in rest {
                let ready = b.parent.as_deref().map_or(true, |p| ordered.iter().any(|o| o.name == p));
                if ready {
                    ordered.push(b);
                } else {
                    keep.push(b);
                }
            }
            rest = keep;
            if rest.len() == before {
                ordered.append(&mut rest); // unreachable for a valid tree
            }
        }
        self.buses = ordered;
    }

    // ---- per-bus controls --------------------------------------------------

    pub fn gain(&self, name: &str) -> Option<f32> {
        self.bus(name).map(|b| b.gain)
    }

    pub fn muted(&self, name: &str) -> Option<bool> {
        self.bus(name).map(|b| b.muted)
    }

    pub fn solo(&self, name: &str) -> Option<bool> {
        self.bus(name).map(|b| b.solo)
    }

    /// Sets a bus gain (clamped 0..=1; NaN ignored). False for an unknown bus.
    pub fn set_gain(&mut self, name: &str, gain: f32) -> bool {
        match self.bus_mut(name) {
            Some(b) => {
                if let Some(g) = sane(gain) {
                    b.gain = g;
                }
                true
            }
            None => false,
        }
    }

    pub fn set_muted(&mut self, name: &str, muted: bool) -> bool {
        self.bus_mut(name).map(|b| b.muted = muted).is_some()
    }

    pub fn set_solo(&mut self, name: &str, solo: bool) -> bool {
        self.bus_mut(name).map(|b| b.solo = solo).is_some()
    }

    pub fn any_bus_solo(&self) -> bool {
        self.buses.iter().any(|b| b.solo)
    }

    // ---- effective gain ----------------------------------------------------

    /// Product of gains along the chain; 0 if any bus on it is muted. Ignores solo.
    pub fn chain_gain(&self, name: &str) -> f32 {
        let chain = self.chain(name);
        if chain.is_empty() || chain.iter().any(|b| b.muted) {
            return 0.0;
        }
        chain.iter().map(|b| b.gain).product()
    }

    /// True if `name` or one of its ancestors is soloed.
    pub fn in_solo_scope(&self, name: &str) -> bool {
        self.chain(name).iter().any(|b| b.solo)
    }

    /// The multiplier a track on `bus` gets from the mixer: chain gain, mute
    /// and solo applied. `track_solo` is the track's own solo flag;
    /// `any_track_solo` says whether any track anywhere is soloed.
    pub fn effective_gain(&self, bus: &str, track_solo: bool, any_track_solo: bool) -> f32 {
        let g = self.chain_gain(bus);
        if g <= 0.0 {
            return 0.0;
        }
        let solo_active = any_track_solo || self.any_bus_solo();
        if solo_active && !track_solo && !self.in_solo_scope(bus) {
            return 0.0;
        }
        g
    }

    // ---- persistence -------------------------------------------------------

    pub fn to_settings(&self) -> MixerSettings {
        MixerSettings {
            format_version: MixerSettings::VERSION,
            buses: self
                .buses
                .iter()
                .map(|b| BusSettings { name: b.name.clone(), gain: b.gain, muted: b.muted, solo: b.solo, parent: b.parent.clone() })
                .collect(),
            routes: BTreeMap::new(),
        }
    }

    /// Applies saved bus settings: existing buses are updated, unknown ones
    /// created, built-ins that the file omits keep their current state.
    /// Returns human-readable warnings for anything skipped (a cycle, an
    /// unknown parent falls back to `master`).
    pub fn apply_settings(&mut self, s: &MixerSettings) -> Vec<String> {
        let mut warnings = Vec::new();
        // Pass 1: make sure every bus exists (new ones under master for now).
        for b in &s.buses {
            if b.name.trim().is_empty() {
                warnings.push("skipped a bus with an empty name".to_string());
                continue;
            }
            if !self.has_bus(&b.name) {
                let _ = self.add_bus(&b.name, MASTER_BUS);
            }
            if let Some(bus) = self.bus_mut(&b.name) {
                if let Some(g) = sane(b.gain) {
                    bus.gain = g;
                }
                bus.muted = b.muted;
                bus.solo = b.solo;
            }
        }
        // Pass 2: parents, now that every name resolves.
        for b in &s.buses {
            let Some(parent) = &b.parent else { continue };
            if b.name == MASTER_BUS || !self.has_bus(&b.name) {
                continue;
            }
            let target = if self.has_bus(parent) {
                parent.as_str()
            } else {
                warnings.push(format!("bus `{}`: unknown parent `{parent}`, using master", b.name));
                MASTER_BUS
            };
            if let Err(e) = self.set_parent(&b.name, target) {
                warnings.push(format!("bus `{}`: {e}; parent left unchanged", b.name));
            }
        }
        warnings
    }
}

/// One bus in a saved mixer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusSettings {
    pub name: String,
    #[serde(default = "one")]
    pub gain: f32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub parent: Option<String>,
}

fn one() -> f32 {
    1.0
}

fn one_u32() -> u32 {
    MixerSettings::VERSION
}

/// The persisted form of the mixer (RON): buses plus the track-name -> bus
/// routing table (applied whenever a track with that name is created).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixerSettings {
    #[serde(default = "one_u32")]
    pub format_version: u32,
    #[serde(default)]
    pub buses: Vec<BusSettings>,
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
}

impl Default for MixerSettings {
    fn default() -> Self {
        Mixer::new().to_settings()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MixerSettingsError {
    #[error("mixer settings: {0}")]
    Parse(#[from] ron::error::SpannedError),
    #[error("mixer settings: {0}")]
    Write(#[from] ron::Error),
    #[error("mixer settings file: {0}")]
    Io(#[from] std::io::Error),
    #[error("mixer settings are format version {found}, this build reads up to {supported}")]
    TooNew { found: u32, supported: u32 },
}

impl MixerSettings {
    pub const VERSION: u32 = 1;

    pub fn to_ron(&self) -> Result<String, MixerSettingsError> {
        Ok(ron::ser::to_string_pretty(self, crate::editor::scene::pretty())?)
    }

    /// Parses through the scene module's `ron_options()` (IMPLICIT_SOME).
    pub fn from_ron(text: &str) -> Result<Self, MixerSettingsError> {
        let s: MixerSettings = crate::editor::scene::ron_options().from_str(text)?;
        if s.format_version > Self::VERSION {
            return Err(MixerSettingsError::TooNew { found: s.format_version, supported: Self::VERSION });
        }
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> Result<(), MixerSettingsError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_ron()?)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, MixerSettingsError> {
        Self::from_ron(&std::fs::read_to_string(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    }

    #[test]
    fn defaults_and_hierarchy() {
        let m = Mixer::new();
        let names: Vec<&str> = m.buses().iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["master", "music", "sfx", "ui"]);
        assert_eq!(m.children("master"), ["music", "sfx", "ui"]);
        assert_eq!(m.chain("ui").iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["ui", "master"]);
        assert!(m.chain("nope").is_empty());
        assert_eq!(m.chain_gain("nope"), 0.0);
    }

    #[test]
    fn chain_gain_is_product_and_mute_zeroes() {
        let mut m = Mixer::new();
        m.add_bus("footsteps", "sfx").unwrap();
        m.set_gain("master", 0.5);
        m.set_gain("sfx", 0.5);
        m.set_gain("footsteps", 0.8);
        close(m.chain_gain("footsteps"), 0.2); // 0.8 * 0.5 * 0.5
        close(m.effective_gain("footsteps", false, false), 0.2);
        close(m.chain_gain("music"), 0.5);
        m.set_muted("sfx", true); // a muted ancestor silences the child
        assert_eq!(m.chain_gain("footsteps"), 0.0);
        close(m.chain_gain("music"), 0.5);
        m.set_muted("sfx", false);
        m.set_gain("master", f32::NAN); // ignored
        close(m.gain("master").unwrap(), 0.5);
        m.set_gain("master", 7.0);
        assert_eq!(m.gain("master"), Some(1.0));
        assert!(!m.set_gain("nope", 0.5));
    }

    #[test]
    fn solo_scope_and_mute_wins() {
        let mut m = Mixer::new();
        m.add_bus("footsteps", "sfx").unwrap();
        m.add_bus("voices", "sfx").unwrap();
        // Nothing soloed: everything audible.
        assert_eq!(m.effective_gain("music", false, false), 1.0);
        // Solo a leaf: only it (and track-soloed tracks) stay.
        m.set_solo("footsteps", true);
        assert_eq!(m.effective_gain("footsteps", false, false), 1.0);
        assert_eq!(m.effective_gain("voices", false, false), 0.0);
        assert_eq!(m.effective_gain("music", false, false), 0.0);
        assert_eq!(m.effective_gain("sfx", false, false), 0.0); // ancestor: signal passes through, its own tracks do not
        assert_eq!(m.effective_gain("voices", true, true), 1.0); // soloed track
        // Solo an interior bus: its whole subtree stays.
        m.set_solo("footsteps", false);
        m.set_solo("sfx", true);
        assert_eq!(m.effective_gain("footsteps", false, false), 1.0);
        assert_eq!(m.effective_gain("voices", false, false), 1.0);
        assert_eq!(m.effective_gain("music", false, false), 0.0);
        // Mute beats solo.
        m.set_muted("voices", true);
        assert_eq!(m.effective_gain("voices", false, false), 0.0);
        assert_eq!(m.effective_gain("voices", true, true), 0.0);
        // A soloed track alone silences non-soloed tracks everywhere.
        m.set_solo("sfx", false);
        m.set_muted("voices", false);
        assert_eq!(m.effective_gain("music", false, true), 0.0);
        assert_eq!(m.effective_gain("music", true, true), 1.0);
    }

    #[test]
    fn add_remove_reparent_and_cycles() {
        let mut m = Mixer::new();
        assert_eq!(m.add_bus("", "master"), Err(MixerError::EmptyName));
        assert_eq!(m.add_bus("sfx", "master"), Err(MixerError::Exists("sfx".into())));
        assert_eq!(m.add_bus("x", "nope"), Err(MixerError::Unknown("nope".into())));
        m.add_bus("a", "sfx").unwrap();
        m.add_bus("b", "a").unwrap();
        // Cycles and self-parenting are rejected; the tree is unchanged.
        assert!(matches!(m.set_parent("a", "b"), Err(MixerError::Cycle { .. })));
        assert!(matches!(m.set_parent("a", "a"), Err(MixerError::Cycle { .. })));
        assert!(matches!(m.set_parent("master", "a"), Err(MixerError::BuiltIn(_))));
        assert_eq!(m.bus("a").unwrap().parent.as_deref(), Some("sfx"));
        // Re-parent keeps parents-before-children ordering.
        m.set_parent("a", "music").unwrap();
        let order: Vec<&str> = m.buses().iter().map(|b| b.name.as_str()).collect();
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("music") < pos("a") && pos("a") < pos("b"));
        // Removing an interior bus re-homes its children; built-ins are protected.
        assert_eq!(m.remove_bus("a"), Ok("music".to_string()));
        assert_eq!(m.bus("b").unwrap().parent.as_deref(), Some("music"));
        assert!(matches!(m.remove_bus("sfx"), Err(MixerError::BuiltIn(_))));
        assert!(matches!(m.remove_bus("zzz"), Err(MixerError::Unknown(_))));
    }

    #[test]
    fn settings_roundtrip_through_ron() {
        let mut m = Mixer::new();
        m.add_bus("ambience", "music").unwrap();
        m.set_gain("ambience", 0.25);
        m.set_gain("master", 0.8);
        m.set_muted("ui", true);
        let mut s = m.to_settings();
        s.routes.insert("rain".into(), "ambience".into());
        let text = s.to_ron().unwrap();
        let back = MixerSettings::from_ron(&text).unwrap();
        assert_eq!(back, s);
        let mut fresh = Mixer::new();
        assert!(fresh.apply_settings(&back).is_empty());
        assert_eq!(fresh, m);
    }

    #[test]
    fn hand_written_ron_uses_implicit_some_and_defaults() {
        // `parent: "sfx"` without `Some(..)`, missing fields default.
        let text = r#"(
            buses: [
                (name: "master", gain: 0.5),
                (name: "footsteps", gain: 0.3, parent: "sfx"),
                (name: "music", muted: true),
            ],
        )"#;
        let s = MixerSettings::from_ron(text).unwrap();
        assert_eq!(s.buses[1].parent.as_deref(), Some("sfx"));
        let mut m = Mixer::new();
        assert!(m.apply_settings(&s).is_empty());
        close(m.gain("master").unwrap(), 0.5);
        close(m.gain("footsteps").unwrap(), 0.3);
        assert_eq!(m.bus("footsteps").unwrap().parent.as_deref(), Some("sfx"));
        assert_eq!(m.muted("music"), Some(true));
        assert_eq!(m.gain("sfx"), Some(1.0)); // omitted built-in untouched
        assert_eq!(MixerSettings::from_ron("()").unwrap().buses.len(), 0);
    }

    #[test]
    fn bad_settings_are_reported_not_applied_blindly() {
        let mut m = Mixer::new();
        let s = MixerSettings {
            format_version: 1,
            buses: vec![
                BusSettings { name: "a".into(), gain: 2.0, muted: false, solo: false, parent: Some("b".into()) },
                BusSettings { name: "b".into(), gain: f32::NAN, muted: false, solo: false, parent: Some("a".into()) },
                BusSettings { name: "c".into(), gain: 1.0, muted: false, solo: false, parent: Some("ghost".into()) },
                BusSettings { name: " ".into(), gain: 1.0, muted: false, solo: false, parent: None },
            ],
            routes: BTreeMap::new(),
        };
        let w = m.apply_settings(&s);
        assert!(w.iter().any(|x| x.contains("cycle")), "{w:?}");
        assert!(w.iter().any(|x| x.contains("ghost")), "{w:?}");
        assert!(w.iter().any(|x| x.contains("empty")), "{w:?}");
        assert_eq!(m.gain("a"), Some(1.0)); // clamped
        assert_eq!(m.gain("b"), Some(1.0)); // NaN ignored
        assert_eq!(m.bus("c").unwrap().parent.as_deref(), Some("master"));
        // Whatever happened, every chain still ends at master.
        for b in m.buses() {
            assert_eq!(m.chain(&b.name).last().unwrap().name, "master");
        }
        assert!(MixerSettings::from_ron("(format_version: 99)").is_err());
        assert!(MixerSettings::from_ron("not ron").is_err());
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("jame_mixer_{}", std::process::id()));
        let path = dir.join("nested").join("mixer.ron");
        let mut m = Mixer::new();
        m.set_gain("music", 0.4);
        m.to_settings().save(&path).unwrap();
        let loaded = MixerSettings::load(&path).unwrap();
        assert_eq!(loaded.buses.iter().find(|b| b.name == "music").unwrap().gain, 0.4);
        assert!(MixerSettings::load(&dir.join("missing.ron")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
