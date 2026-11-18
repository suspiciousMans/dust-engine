//! An egui mixer: one strip per bus (fader, mute, solo) and one row per
//! track (bus picker, gain, mute, solo, and a level meter when the backend
//! can report levels).
//!
//! Calling it from an editor host (`ShellHost::tab_ui` hands you the tab's
//! `&mut egui::Ui`; the host owns the engine and a [`MixerPanelState`]):
//!
//! ```ignore
//! use dust_engine::audio::{draw_mixer_panel, AudioContext, MixerPanelState};
//! use dust_engine::editor::shell::ShellHost;
//! use dust_engine::ui::EditorTab;
//!
//! struct Host { audio: Option<AudioContext>, mixer_ui: MixerPanelState }
//!
//! impl ShellHost for Host {
//!     fn tab_ui(&mut self, tab: &EditorTab, _world: &mut World, ui: &mut egui::Ui) -> bool {
//!         match (tab, self.audio.as_mut()) {
//!             (EditorTab::Host(name), Some(audio)) if name == "Mixer" => {
//!                 for edit in draw_mixer_panel(ui, audio, &mut self.mixer_ui) {
//!                     // optional: persist, e.g. audio.save_mixer_settings(path)
//!                     let _ = edit;
//!                 }
//!                 true
//!             }
//!             _ => false,
//!         }
//!     }
//! }
//! ```
//!
//! The panel edits the engine directly (the same `set_bus_gain` /
//! `set_track_gain` / ... calls a game would make) and returns what changed
//! this frame as [`MixerEdit`]s. Edits can also be queued from outside with
//! [`MixerPanelState::queue`] (hotkeys, scripted tests); they are applied
//! before drawing, so the faders show them in the same frame.
//!
//! Levels: `track_level` reports a peak only on backends that can measure it
//! (native `rodio`); elsewhere the meter column is simply not drawn and the
//! panel shows gains only. Buses have no meter of their own.

use std::collections::HashMap;

use super::backend::AudioBackend;
use super::engine::AudioEngine;
use super::mixer::MASTER_BUS;

/// One change made through the panel (or queued into it).
#[derive(Clone, Debug, PartialEq)]
pub enum MixerEdit {
    BusGain { bus: String, gain: f32 },
    BusMute { bus: String, muted: bool },
    BusSolo { bus: String, solo: bool },
    TrackGain { track: String, gain: f32 },
    TrackMute { track: String, muted: bool },
    TrackSolo { track: String, solo: bool },
    TrackBus { track: String, bus: String },
}

/// UI-side state the host keeps between frames.
#[derive(Debug, Default)]
pub struct MixerPanelState {
    /// Edits to apply at the start of the next draw (e.g. from a hotkey, or a
    /// test simulating a fader move).
    pub queue: Vec<MixerEdit>,
    /// Smoothed meter value per track (peak, decaying), for tracks whose
    /// backend reports a level.
    meters: HashMap<String, f32>,
    /// Set by the last draw: whether any track reported a level.
    pub meters_available: bool,
}

impl MixerPanelState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The smoothed meter value last drawn for `track`, if it has one.
    pub fn meter(&self, track: &str) -> Option<f32> {
        self.meters.get(track).copied()
    }
}

/// Applies one edit to the engine. False if the target does not exist.
pub fn apply_mixer_edit<B: AudioBackend>(audio: &mut AudioEngine<B>, edit: &MixerEdit) -> bool {
    match edit {
        MixerEdit::BusGain { bus, gain } => audio.set_bus_gain(bus, *gain),
        MixerEdit::BusMute { bus, muted } => audio.set_bus_muted(bus, *muted),
        MixerEdit::BusSolo { bus, solo } => audio.set_bus_solo(bus, *solo),
        MixerEdit::TrackGain { track, gain } => audio.set_track_gain(track, *gain),
        MixerEdit::TrackMute { track, muted } => audio.set_track_muted(track, *muted),
        MixerEdit::TrackSolo { track, solo } => audio.set_track_solo(track, *solo),
        MixerEdit::TrackBus { track, bus } => audio.assign_track_bus(track, bus),
    }
}

/// Buses in tree order (parent before children, depth for indentation).
fn bus_tree<B: AudioBackend>(audio: &AudioEngine<B>) -> Vec<(String, usize)> {
    fn walk(m: &super::mixer::Mixer, name: &str, depth: usize, out: &mut Vec<(String, usize)>) {
        if depth > 32 || out.iter().any(|(n, _)| n == name) {
            return;
        }
        out.push((name.to_string(), depth));
        for child in m.children(name) {
            walk(m, child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(audio.mixer(), MASTER_BUS, 0, &mut out);
    out
}

/// Applies the edits widgets produced since the last flush.
fn flush<B: AudioBackend>(audio: &mut AudioEngine<B>, edits: &[MixerEdit], applied: &mut usize) {
    for e in &edits[*applied..] {
        apply_mixer_edit(audio, e);
    }
    *applied = edits.len();
}

const METER_W: f32 = 56.0;

fn meter_bar(ui: &mut egui::Ui, value: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(METER_W, 10.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let v = value.clamp(0.0, 1.0);
    if v > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * v);
        let color = if v > 0.9 {
            egui::Color32::from_rgb(220, 70, 60)
        } else if v > 0.7 {
            egui::Color32::from_rgb(220, 190, 60)
        } else {
            egui::Color32::from_rgb(70, 190, 90)
        };
        p.rect_filled(fill, 2.0, color);
    }
}

/// Draws the mixer into `ui` and returns the edits made this frame (queued
/// ones included). See the [module docs](self) for hosting it in a
/// `ShellHost::tab_ui`.
pub fn draw_mixer_panel<B: AudioBackend>(ui: &mut egui::Ui, audio: &mut AudioEngine<B>, state: &mut MixerPanelState) -> Vec<MixerEdit> {
    let mut edits: Vec<MixerEdit> = Vec::new();
    for e in std::mem::take(&mut state.queue) {
        if apply_mixer_edit(audio, &e) {
            edits.push(e);
        }
    }

    let mut applied = edits.len();

    // ---- buses -----------------------------------------------------------
    ui.heading("Buses");
    let buses = bus_tree(audio);
    egui::ScrollArea::horizontal().id_salt("mixer_buses").show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for (name, depth) in &buses {
                let (mut gain, mut muted, mut solo) = {
                    let m = audio.mixer();
                    (m.gain(name).unwrap_or(1.0), m.muted(name).unwrap_or(false), m.solo(name).unwrap_or(false))
                };
                ui.push_id(("bus", name), |ui| {
                    ui.vertical(|ui| {
                        ui.set_min_width(54.0);
                        ui.label(egui::RichText::new(name).strong());
                        if *depth > 0 {
                            ui.weak(format!("{}↳", "·".repeat(*depth - 1)));
                        }
                        if ui.add(egui::Slider::new(&mut gain, 0.0..=1.0).vertical().show_value(true)).changed() {
                            edits.push(MixerEdit::BusGain { bus: name.clone(), gain });
                        }
                        ui.horizontal(|ui| {
                            if ui.toggle_value(&mut muted, "M").on_hover_text("Mute").changed() {
                                edits.push(MixerEdit::BusMute { bus: name.clone(), muted });
                            }
                            if ui.toggle_value(&mut solo, "S").on_hover_text("Solo").changed() {
                                edits.push(MixerEdit::BusSolo { bus: name.clone(), solo });
                            }
                        });
                        ui.weak(format!("→ {:.2}", audio.bus_effective_gain(name)));
                    });
                });
                ui.separator();
            }
        });
    });
    flush(audio, &edits, &mut applied);

    // ---- tracks ----------------------------------------------------------
    ui.separator();
    ui.heading("Tracks");
    let names = audio.track_names();
    if names.is_empty() {
        ui.weak("no tracks");
    }
    let bus_names = audio.bus_names();
    let mut any_level = false;
    let mut live: Vec<String> = Vec::new();
    egui::Grid::new("mixer_tracks").num_columns(6).spacing([8.0, 4.0]).striped(true).show(ui, |ui| {
        for name in &names {
            live.push(name.clone());
            let mut gain = audio.track_gain(name).unwrap_or(1.0);
            let mut muted = audio.track_muted(name).unwrap_or(false);
            let mut solo = audio.track_solo(name).unwrap_or(false);
            let bus = audio.track_bus(name).unwrap_or(MASTER_BUS).to_string();

            ui.label(name);
            let mut chosen = bus.clone();
            egui::ComboBox::from_id_salt(("mixer_route", name)).selected_text(&bus).width(80.0).show_ui(ui, |ui| {
                for b in &bus_names {
                    ui.selectable_value(&mut chosen, b.clone(), b);
                }
            });
            if chosen != bus {
                edits.push(MixerEdit::TrackBus { track: name.clone(), bus: chosen });
            }
            if ui.add(egui::Slider::new(&mut gain, 0.0..=1.0).show_value(true)).changed() {
                edits.push(MixerEdit::TrackGain { track: name.clone(), gain });
            }
            ui.horizontal(|ui| {
                if ui.toggle_value(&mut muted, "M").on_hover_text("Mute").changed() {
                    edits.push(MixerEdit::TrackMute { track: name.clone(), muted });
                }
                if ui.toggle_value(&mut solo, "S").on_hover_text("Solo").changed() {
                    edits.push(MixerEdit::TrackSolo { track: name.clone(), solo });
                }
            });
            ui.weak(format!("→ {:.2}", audio.track_effective_gain(name).unwrap_or(0.0)));
            match audio.track_level(name) {
                Some(level) => {
                    any_level = true;
                    let shown = state.meters.entry(name.clone()).or_insert(0.0);
                    // Instant attack, gentle release.
                    *shown = if level >= *shown { level } else { *shown * 0.9 + level * 0.1 };
                    meter_bar(ui, *shown);
                }
                None => {
                    ui.label("");
                }
            }
            ui.end_row();
        }
    });
    flush(audio, &edits, &mut applied);
    state.meters.retain(|k, _| live.contains(k));
    state.meters_available = any_level;
    if !names.is_empty() && !any_level {
        ui.weak("Level meters unavailable on this audio backend (gains only).");
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::NullBackend;
    use crate::audio::mixer::{SFX_BUS, UI_BUS};

    type Engine = AudioEngine<NullBackend>;

    fn engine_with_tracks() -> Engine {
        let mut a = Engine::with_manual_time(NullBackend::new());
        a.add_track("drums", (vec![0.1; 1000], 1000));
        a.add_track("bass", (vec![0.1; 1000], 1000));
        a.assign_track_bus("drums", SFX_BUS);
        a
    }

    fn frame(ctx: &egui::Context, audio: &mut Engine, state: &mut MixerPanelState) -> Vec<MixerEdit> {
        // egui may run the closure again for a layout (sizing) pass; edits
        // are applied to the engine on the first run, so collect across runs.
        let mut out = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| out.extend(draw_mixer_panel(ui, audio, state)));
        });
        out
    }

    #[test]
    fn draws_on_a_bare_context_without_tracks_or_with_them() {
        let ctx = egui::Context::default();
        let mut state = MixerPanelState::new();
        let mut empty = Engine::with_manual_time(NullBackend::new());
        assert!(frame(&ctx, &mut empty, &mut state).is_empty());
        let mut a = engine_with_tracks();
        for _ in 0..3 {
            assert!(frame(&ctx, &mut a, &mut state).is_empty(), "drawing alone changes nothing");
        }
        assert!(!state.meters_available, "NullBackend reports no levels by default");
    }

    #[test]
    fn simulated_fader_changes_through_state_reach_the_engine() {
        let ctx = egui::Context::default();
        let mut a = engine_with_tracks();
        let mut state = MixerPanelState::new();
        state.queue.push(MixerEdit::BusGain { bus: SFX_BUS.into(), gain: 0.5 });
        state.queue.push(MixerEdit::TrackGain { track: "drums".into(), gain: 0.5 });
        state.queue.push(MixerEdit::BusMute { bus: UI_BUS.into(), muted: true });
        state.queue.push(MixerEdit::TrackSolo { track: "bass".into(), solo: true });
        state.queue.push(MixerEdit::TrackGain { track: "nope".into(), gain: 0.1 }); // unknown: dropped
        let edits = frame(&ctx, &mut a, &mut state);
        assert_eq!(edits.len(), 4);
        assert!(state.queue.is_empty());
        assert_eq!(a.bus_gain(SFX_BUS), Some(0.5));
        assert_eq!(a.track_gain("drums"), Some(0.5));
        assert_eq!(a.mixer().muted(UI_BUS), Some(true));
        assert_eq!(a.track_solo("bass"), Some(true));
        // drums: track 0.5 x sfx bus 0.5, but bass is soloed so drums is silenced
        assert_eq!(a.track_effective_gain("drums"), Some(0.0));
        assert!(a.track_effective_gain("bass").unwrap() > 0.0);

        state.queue.push(MixerEdit::TrackSolo { track: "bass".into(), solo: false });
        frame(&ctx, &mut a, &mut state);
        assert_eq!(a.track_effective_gain("drums"), Some(0.25));
        let id = a.track_spatial_params("drums"); // plain track: not spatial
        assert!(id.is_none());
    }

    #[test]
    fn routing_edit_moves_a_track_to_another_bus() {
        let ctx = egui::Context::default();
        let mut a = engine_with_tracks();
        let mut state = MixerPanelState::new();
        state.queue.push(MixerEdit::TrackBus { track: "bass".into(), bus: SFX_BUS.into() });
        state.queue.push(MixerEdit::TrackBus { track: "bass".into(), bus: "no-such-bus".into() });
        assert_eq!(frame(&ctx, &mut a, &mut state).len(), 1);
        assert_eq!(a.track_bus("bass"), Some(SFX_BUS));
    }

    #[test]
    fn meters_appear_only_when_the_backend_reports_levels() {
        let ctx = egui::Context::default();
        let mut a = engine_with_tracks();
        let mut state = MixerPanelState::new();
        frame(&ctx, &mut a, &mut state);
        assert_eq!(state.meter("drums"), None);

        let id = a.bank.get("drums").unwrap().id;
        a.backend_mut().levels.insert(id, 0.8);
        frame(&ctx, &mut a, &mut state);
        assert!(state.meters_available);
        assert_eq!(state.meter("drums"), Some(0.8));
        assert_eq!(state.meter("bass"), None);

        a.backend_mut().levels.insert(id, 0.0);
        frame(&ctx, &mut a, &mut state);
        let decayed = state.meter("drums").unwrap();
        assert!(decayed < 0.8 && decayed > 0.0, "release is gradual: {decayed}");

        a.stop_track("drums");
        frame(&ctx, &mut a, &mut state);
        assert_eq!(state.meter("drums"), None, "meter state is dropped with the track");
    }

    #[test]
    fn a_user_bus_hierarchy_is_listed_and_editable() {
        let ctx = egui::Context::default();
        let mut a = engine_with_tracks();
        a.add_bus("footsteps", SFX_BUS).unwrap();
        let tree = bus_tree(&a);
        assert_eq!(tree[0], (MASTER_BUS.to_string(), 0));
        assert!(tree.contains(&("footsteps".to_string(), 2)));
        let mut state = MixerPanelState::new();
        state.queue.push(MixerEdit::BusGain { bus: "footsteps".into(), gain: 0.2 });
        frame(&ctx, &mut a, &mut state);
        assert_eq!(a.bus_gain("footsteps"), Some(0.2));
    }
}
