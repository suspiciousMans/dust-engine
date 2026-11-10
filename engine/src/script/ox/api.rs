//! The stable script-facing surface (`OxApi`) and the engine services it
//! needs that the legacy `behavior::ScriptApi` trait doesn't have (rotation,
//! scale, input, spawn/despawn, tunables, per-entity script variables).
//!
//! Scripts only ever see the natives listed in `build::spec::SPEC`; this file
//! is the Rust-side meaning of each one.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use crate::behavior::ScriptApi;

/// Everything a script can do to its entity and the world. One implementation
/// is built per hook dispatch. All vectors are `[x, y, z]`.
pub trait OxApi {
    fn log(&mut self, msg: &str);
    /// Seconds since game start.
    fn time(&self) -> f64;
    fn position(&self) -> [f32; 3];
    fn set_position(&mut self, p: [f32; 3]);
    /// Euler rotation in radians.
    fn rotation(&self) -> [f32; 3];
    fn set_rotation(&mut self, r: [f32; 3]);
    fn scale(&self) -> [f32; 3];
    fn set_scale(&mut self, s: [f32; 3]);
    fn key_down(&self, key: &str) -> bool;
    /// True only on the frame the key went down.
    fn key_pressed(&self, key: &str) -> bool;
    /// Named analog axis in `-1.0..=1.0` (e.g. "move_x").
    fn axis(&self, name: &str) -> f32;
    /// Queues a spawn of the named prefab/class; returns an id valid for `despawn`.
    fn spawn(&mut self, name: &str, p: [f32; 3]) -> i64;
    fn despawn(&mut self, id: i64);
    fn play_sound(&mut self, path: &str);
    fn play_tone(&mut self, freq_hz: f32, secs: f32);
    /// Designer-editable value; `default` when the instance has no override.
    fn tunable(&self, name: &str, default: f64) -> f64;
    /// Per-entity script variable (scripts are stateless functions; state lives here).
    fn var(&self, name: &str, default: f64) -> f64;
    fn set_var(&mut self, name: &str, value: f64);
    fn hud_bar(&mut self, name: &str, fraction: f32);
    fn toast(&mut self, msg: &str, secs: f32);
    fn camera_shake(&mut self, intensity: f32, secs: f32);
}

/// Input as the app snapshots it once per frame.
#[derive(Clone, Debug, Default)]
pub struct InputSnapshot {
    pub down: HashSet<String>,
    pub pressed: HashSet<String>,
    pub axes: HashMap<String, f32>,
}

/// Side effects scripts request that need `World` access the behavior layer
/// doesn't have. The app drains these after dispatching hooks.
#[derive(Clone, Debug, PartialEq)]
pub enum OxCommand {
    Spawn { id: i64, name: String, position: [f32; 3], requested_by: u64 },
    Despawn { id: i64 },
    SetRotation { entity: u64, rotation: [f32; 3] },
    SetScale { entity: u64, scale: [f32; 3] },
}

/// Shared between the app and every `OxBehavior`.
#[derive(Default)]
pub struct OxServices {
    input: Mutex<std::sync::Arc<InputSnapshot>>,
    commands: Mutex<Vec<OxCommand>>,
    next_id: AtomicI64,
}

impl OxServices {
    pub fn set_input(&self, snapshot: InputSnapshot) {
        *self.input.lock().unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(snapshot);
    }
    pub fn drain_commands(&self) -> Vec<OxCommand> {
        std::mem::take(&mut *self.commands.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub(crate) fn push(&self, c: OxCommand) {
        self.commands.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    }
    pub(crate) fn new_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
    pub(crate) fn input(&self) -> std::sync::Arc<InputSnapshot> {
        self.input.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Per-entity data a script keeps between hook calls.
#[derive(Clone, Debug)]
pub struct EntityState {
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
    pub vars: HashMap<String, f64>,
    pub tunables: HashMap<String, f64>,
}

impl Default for EntityState {
    fn default() -> Self {
        Self { rotation: [0.0; 3], scale: [1.0; 3], vars: HashMap::new(), tunables: HashMap::new() }
    }
}

/// `OxApi` over the engine's existing per-entity `behavior::ScriptApi`.
pub struct BehaviorCtx<'a> {
    pub api: &'a mut dyn ScriptApi,
    pub state: &'a mut EntityState,
    pub services: &'a OxServices,
    pub input: &'a InputSnapshot,
    pub entity: u64,
}

impl OxApi for BehaviorCtx<'_> {
    fn log(&mut self, msg: &str) {
        self.api.log(msg);
    }
    fn time(&self) -> f64 {
        self.api.elapsed() as f64
    }
    fn position(&self) -> [f32; 3] {
        let (x, y, z) = self.api.position();
        [x, y, z]
    }
    fn set_position(&mut self, p: [f32; 3]) {
        self.api.set_position(p[0], p[1], p[2]);
    }
    fn rotation(&self) -> [f32; 3] {
        self.state.rotation
    }
    fn set_rotation(&mut self, r: [f32; 3]) {
        self.state.rotation = r;
        self.services.push(OxCommand::SetRotation { entity: self.entity, rotation: r });
    }
    fn scale(&self) -> [f32; 3] {
        self.state.scale
    }
    fn set_scale(&mut self, s: [f32; 3]) {
        self.state.scale = s;
        self.services.push(OxCommand::SetScale { entity: self.entity, scale: s });
    }
    fn key_down(&self, key: &str) -> bool {
        self.input.down.contains(key)
    }
    fn key_pressed(&self, key: &str) -> bool {
        self.input.pressed.contains(key)
    }
    fn axis(&self, name: &str) -> f32 {
        self.input.axes.get(name).copied().unwrap_or(0.0)
    }
    fn spawn(&mut self, name: &str, p: [f32; 3]) -> i64 {
        let id = self.services.new_id();
        self.services.push(OxCommand::Spawn { id, name: name.to_string(), position: p, requested_by: self.entity });
        id
    }
    fn despawn(&mut self, id: i64) {
        self.services.push(OxCommand::Despawn { id });
    }
    fn play_sound(&mut self, path: &str) {
        self.api.play_sfx(path);
    }
    fn play_tone(&mut self, freq_hz: f32, secs: f32) {
        self.api.play_tone(freq_hz, secs);
    }
    fn tunable(&self, name: &str, default: f64) -> f64 {
        self.state.tunables.get(name).copied().unwrap_or(default)
    }
    fn var(&self, name: &str, default: f64) -> f64 {
        self.state.vars.get(name).copied().unwrap_or(default)
    }
    fn set_var(&mut self, name: &str, value: f64) {
        self.state.vars.insert(name.to_string(), value);
    }
    fn hud_bar(&mut self, name: &str, fraction: f32) {
        self.api.set_hud_bar(name, fraction);
    }
    fn toast(&mut self, msg: &str, secs: f32) {
        self.api.show_toast(msg, secs);
    }
    fn camera_shake(&mut self, intensity: f32, secs: f32) {
        self.api.camera_shake(intensity, secs);
    }
}

/// A recording `ScriptApi` with no GL/audio/ECS — for headless tests and
/// editor dry-runs.
#[derive(Default, Debug)]
pub struct HeadlessApi {
    pub position: (f32, f32, f32),
    pub elapsed: f32,
    pub logs: Vec<String>,
    pub tones: Vec<(f32, f32)>,
    pub sfx: Vec<String>,
    pub hud_bars: Vec<(String, f32)>,
    pub toasts: Vec<String>,
    pub shakes: Vec<(f32, f32)>,
}

impl ScriptApi for HeadlessApi {
    fn log(&mut self, message: &str) {
        self.logs.push(message.to_string());
    }
    fn position(&self) -> (f32, f32, f32) {
        self.position
    }
    fn set_position(&mut self, x: f32, y: f32, z: f32) {
        self.position = (x, y, z);
    }
    fn move_by(&mut self, dx: f32, dy: f32, dz: f32) {
        self.position = (self.position.0 + dx, self.position.1 + dy, self.position.2 + dz);
    }
    fn play_tone(&mut self, f: f32, d: f32) {
        self.tones.push((f, d));
    }
    fn play_sfx(&mut self, path: &str) {
        self.sfx.push(path.to_string());
    }
    fn elapsed(&self) -> f32 {
        self.elapsed
    }
    fn set_hud_bar(&mut self, name: &str, fraction: f32) {
        self.hud_bars.push((name.to_string(), fraction));
    }
    fn show_toast(&mut self, message: &str, _seconds: f32) {
        self.toasts.push(message.to_string());
    }
    fn screen_flash(&mut self, _r: f32, _g: f32, _b: f32, _s: f32, _a: f32, _b2: f32, _c: f32) {}
    fn camera_shake(&mut self, i: f32, d: f32) {
        self.shakes.push((i, d));
    }
}
