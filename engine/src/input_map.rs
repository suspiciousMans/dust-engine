//! Action maps: name *actions* once, bind them to devices in data, rebind at runtime.
//!
//! ```ron
//! (version: 1,
//!  contexts: [(name: "gameplay"), (name: "menu", enabled: false, exclusive: true)],
//!  actions: [
//!    (name: "jump", context: "gameplay", bindings: [Key("Space"), PadButton("A")]),
//!    (name: "move", context: "gameplay", bindings: [
//!        Composite(up: Key("W"), down: Key("S"), left: Key("A"), right: Key("D")),
//!        Stick(x: "LeftX", y: "LeftY")]),
//!  ])
//! ```
//! A fully commented example lives in `docs/examples/input_map.ron`.
//!
//! # Pieces
//! * [`ActionMap`] — the asset. Parse with [`ActionMap::from_ron`] / [`ActionMap::load`]
//!   (uses the scene module's `ron_options()`, so `context: "menu"` needs no `Some`).
//! * [`Binding`] — `Key`, `Mouse`, `PadButton`, `PadAxis{axis,sign,deadzone}`,
//!   `Pair{negative,positive}` (1D), `Composite{up,down,left,right}` (WASD-style vec2)
//!   and `Stick{x,y,deadzone,invert_y}` (radial deadzone vec2).
//! * [`ActionState`] — resolved **once per frame**: `state.resolve(&map, &input)` where
//!   `input` is any [`InputSource`] (`engine::input::Input` and
//!   `headless::HeadlessInput` implement it). Query `pressed / just_pressed /
//!   just_released / axis / vec2` by action name; unknown actions read as idle.
//! * Contexts (layers): actions carry an optional `context`. Toggle with
//!   [`ActionState::set_context_enabled`] (initial values come from the map). An enabled
//!   `exclusive` context mutes all contexts declared before it; actions with no context
//!   are always live. Muted actions read idle and emit `just_released` if they were held.
//! * Rebinding: [`ActionMap::rebind`] with a [`ConflictPolicy`] (`Reject`, `Swap`, `Allow`)
//!   and [`ActionMap::conflicts_for`] / [`ActionMap::all_conflicts`]; [`RebindState::poll`]
//!   listens for the next key/button/axis (Escape cancels) on any [`InputSource`].
//! * Persistence: user rebinds are a **separate** RON of per-action binding lists
//!   ([`Overrides`]): [`ActionMap::save_overrides`] writes only actions that differ from
//!   the shipped default; [`ActionMap::load_layered`] loads default then layers overrides
//!   (unknown actions are reported as warnings, a missing file is fine).
//! * UI: [`draw_rebind_panel`] (in-game settings or editor window via
//!   [`show_rebind_window`]), with per-action and global "Reset to default".
//!
//! # Conventions
//! * vec2: +x right, **+y up** (`Stick` flips SDL's down-positive Y unless `invert_y: false`).
//! * `PadAxis.sign`: `Both` = signed value; `Positive`/`Negative` = only that half, reported
//!   as `0..1` (so `LeftX` + `Negative` is a "move left" magnitude). Deadzone is rescaled so
//!   the output ramps from 0 at the edge of the deadzone to 1.
//! * An axis used as a button counts as pressed above [`PRESS_THRESHOLD`].
//! * `axis(action)` is the strongest binding value (signed); for vec2 bindings it is the
//!   vector length. `vec2(action)` sums vec2 bindings and clamps the length to 1.
//! * Headless names ([`HeadlessInput`]): keys as-is (`"W"`), mouse `"MouseLeft"`,
//!   pad buttons `"PadA"`, axes as-is (`"LeftX"`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};
use glam::Vec2;
use sdl2::controller::{Axis, Button};
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use serde::{Deserialize, Serialize};

use crate::editor::scene::{pretty, ron_options};
use crate::headless::HeadlessInput;
use crate::input::Input;

/// Axis magnitude at which an axis bound to a "button" action counts as pressed.
pub const PRESS_THRESHOLD: f32 = 0.5;
/// Axis magnitude at which [`RebindState`] accepts an axis as the captured input.
pub const CAPTURE_THRESHOLD: f32 = 0.6;
/// Current on-disk version of both the map and the overrides file.
pub const FORMAT_VERSION: u32 = 1;

fn d_true() -> bool {
    true
}
fn d_deadzone() -> f32 {
    0.2
}

// ------------------------------------------------------------------ device names

thread_local! {
    static KEY_CACHE: RefCell<HashMap<String, Option<Keycode>>> = RefCell::new(HashMap::new());
}

/// SDL key by name (cached: the SDL lookup is a linear scan).
pub fn keycode(name: &str) -> Option<Keycode> {
    KEY_CACHE.with(|c| {
        if let Some(k) = c.borrow().get(name) {
            return *k;
        }
        let k = Keycode::from_name(name);
        c.borrow_mut().insert(name.to_string(), k);
        k
    })
}

pub fn mouse_button(name: &str) -> Option<MouseButton> {
    Some(match name {
        "Left" => MouseButton::Left,
        "Middle" => MouseButton::Middle,
        "Right" => MouseButton::Right,
        "X1" => MouseButton::X1,
        "X2" => MouseButton::X2,
        _ => return None,
    })
}

pub fn mouse_button_name(b: MouseButton) -> Option<&'static str> {
    Some(match b {
        MouseButton::Left => "Left",
        MouseButton::Middle => "Middle",
        MouseButton::Right => "Right",
        MouseButton::X1 => "X1",
        MouseButton::X2 => "X2",
        MouseButton::Unknown => return None,
    })
}

const PAD_BUTTONS: &[(&str, Button)] = &[
    ("A", Button::A),
    ("B", Button::B),
    ("X", Button::X),
    ("Y", Button::Y),
    ("Back", Button::Back),
    ("Guide", Button::Guide),
    ("Start", Button::Start),
    ("LeftStick", Button::LeftStick),
    ("RightStick", Button::RightStick),
    ("LeftShoulder", Button::LeftShoulder),
    ("RightShoulder", Button::RightShoulder),
    ("DPadUp", Button::DPadUp),
    ("DPadDown", Button::DPadDown),
    ("DPadLeft", Button::DPadLeft),
    ("DPadRight", Button::DPadRight),
    ("Misc1", Button::Misc1),
    ("Paddle1", Button::Paddle1),
    ("Paddle2", Button::Paddle2),
    ("Paddle3", Button::Paddle3),
    ("Paddle4", Button::Paddle4),
    ("Touchpad", Button::Touchpad),
];

pub fn pad_button(name: &str) -> Option<Button> {
    PAD_BUTTONS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

pub fn pad_button_name(b: Button) -> &'static str {
    PAD_BUTTONS.iter().find(|(_, x)| *x == b).map(|(n, _)| *n).unwrap_or("?")
}

const PAD_AXES: &[(&str, Axis)] = &[
    ("LeftX", Axis::LeftX),
    ("LeftY", Axis::LeftY),
    ("RightX", Axis::RightX),
    ("RightY", Axis::RightY),
    ("TriggerLeft", Axis::TriggerLeft),
    ("TriggerRight", Axis::TriggerRight),
];

pub fn pad_axis(name: &str) -> Option<Axis> {
    PAD_AXES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
}

// ------------------------------------------------------------------ InputSource

/// What [`ActionState`] needs from a device layer. Names are the strings used in the RON
/// (`"W"`, `"Left"`, `"A"`, `"LeftX"`). Implemented for [`Input`] and [`HeadlessInput`];
/// tests use a fake.
pub trait InputSource {
    fn key_down(&self, name: &str) -> bool;
    fn key_just_pressed(&self, name: &str) -> bool;
    fn mouse_down(&self, name: &str) -> bool;
    fn mouse_just_pressed(&self, name: &str) -> bool;
    fn pad_down(&self, name: &str) -> bool;
    fn pad_just_pressed(&self, name: &str) -> bool;
    /// Raw axis value in `-1..=1` (sticks: SDL convention, down = +).
    fn axis(&self, name: &str) -> f32;
    /// Everything that was pressed this frame / axes pushed far from rest, for rebinding
    /// capture. Default: nothing (the source cannot be listened to).
    fn capture_candidates(&self) -> Vec<Source> {
        Vec::new()
    }
}

impl InputSource for Input {
    fn key_down(&self, name: &str) -> bool {
        keycode(name).is_some_and(|k| self.is_key_down(k))
    }
    fn key_just_pressed(&self, name: &str) -> bool {
        keycode(name).is_some_and(|k| self.just_pressed(k))
    }
    fn mouse_down(&self, name: &str) -> bool {
        mouse_button(name).is_some_and(|b| self.is_button_down(b))
    }
    fn mouse_just_pressed(&self, name: &str) -> bool {
        mouse_button(name).is_some_and(|b| self.button_just_pressed(b))
    }
    fn pad_down(&self, name: &str) -> bool {
        pad_button(name).is_some_and(|b| self.controller_button_down(b))
    }
    fn pad_just_pressed(&self, name: &str) -> bool {
        pad_button(name).is_some_and(|b| self.controller_just_pressed(b))
    }
    fn axis(&self, name: &str) -> f32 {
        pad_axis(name).map_or(0.0, |a| self.pad_axis(a))
    }
    fn capture_candidates(&self) -> Vec<Source> {
        let mut out = Vec::new();
        out.extend(self.keys_just_pressed().map(|k| Source::Key(k.name())));
        out.extend(
            self.buttons_just_pressed()
                .filter_map(mouse_button_name)
                .map(|n| Source::Mouse(n.to_string())),
        );
        out.extend(
            self.controller_buttons_just_pressed()
                .map(|b| Source::PadButton(pad_button_name(b).to_string())),
        );
        for (name, axis) in PAD_AXES {
            let v = self.pad_axis(*axis);
            if v.abs() >= CAPTURE_THRESHOLD {
                out.push(Source::axis_for(name, v));
            }
        }
        out
    }
}

/// Name-keyed headless input: keys by name, mouse as `"Mouse<Name>"`, pad buttons as
/// `"Pad<Name>"`, axes by name (`InputEvent::Axis("LeftX", 1.0)`).
impl InputSource for HeadlessInput {
    fn key_down(&self, name: &str) -> bool {
        self.is_down(name)
    }
    fn key_just_pressed(&self, name: &str) -> bool {
        self.just_pressed(name)
    }
    fn mouse_down(&self, name: &str) -> bool {
        self.is_down(&format!("Mouse{name}"))
    }
    fn mouse_just_pressed(&self, name: &str) -> bool {
        self.just_pressed(&format!("Mouse{name}"))
    }
    fn pad_down(&self, name: &str) -> bool {
        self.is_down(&format!("Pad{name}"))
    }
    fn pad_just_pressed(&self, name: &str) -> bool {
        self.just_pressed(&format!("Pad{name}"))
    }
    fn axis(&self, name: &str) -> f32 {
        HeadlessInput::axis(self, name)
    }
}

// ------------------------------------------------------------------ bindings

/// Which half of an axis counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sign {
    /// Signed value, `-1..=1`.
    #[default]
    Both,
    /// Only `> 0`, reported `0..=1`.
    Positive,
    /// Only `< 0`, reported as `0..=1` (magnitude).
    Negative,
}

/// A single physical input; also the unit of conflict detection and rebinding capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Key(String),
    Mouse(String),
    PadButton(String),
    PadAxis {
        axis: String,
        #[serde(default)]
        sign: Sign,
        #[serde(default = "d_deadzone")]
        deadzone: f32,
    },
}

impl Source {
    /// Axis source for a captured value (sign follows the push direction; triggers are
    /// always positive).
    pub fn axis_for(axis: &str, value: f32) -> Source {
        let sign = if axis.starts_with("Trigger") || value >= 0.0 { Sign::Positive } else { Sign::Negative };
        Source::PadAxis { axis: axis.to_string(), sign, deadzone: d_deadzone() }
    }

    /// Human readable, e.g. `Key W`, `Pad A`, `LeftX −`.
    pub fn label(&self) -> String {
        match self {
            Source::Key(k) => format!("Key {k}"),
            Source::Mouse(m) => format!("Mouse {m}"),
            Source::PadButton(b) => format!("Pad {b}"),
            Source::PadAxis { axis, sign, .. } => match sign {
                Sign::Both => axis.clone(),
                Sign::Positive => format!("{axis} +"),
                Sign::Negative => format!("{axis} -"),
            },
        }
    }

    /// Do the two sources fire from the same physical input?
    pub fn overlaps(&self, other: &Source) -> bool {
        match (self, other) {
            (Source::Key(a), Source::Key(b)) => a == b,
            (Source::Mouse(a), Source::Mouse(b)) => a == b,
            (Source::PadButton(a), Source::PadButton(b)) => a == b,
            (Source::PadAxis { axis: a, sign: sa, .. }, Source::PadAxis { axis: b, sign: sb, .. }) => {
                a == b && (*sa == Sign::Both || *sb == Sign::Both || sa == sb)
            }
            _ => false,
        }
    }

    fn is_axis(&self) -> bool {
        matches!(self, Source::PadAxis { .. })
    }

    /// Processed value: buttons `0/1`, axes after sign and deadzone.
    fn value(&self, src: &dyn InputSource) -> f32 {
        match self {
            Source::Key(k) => src.key_down(k) as u8 as f32,
            Source::Mouse(m) => src.mouse_down(m) as u8 as f32,
            Source::PadButton(b) => src.pad_down(b) as u8 as f32,
            Source::PadAxis { axis, sign, deadzone } => {
                let raw = src.axis(axis);
                let v = match sign {
                    Sign::Both => raw,
                    Sign::Positive => raw.max(0.0),
                    Sign::Negative => (-raw).max(0.0),
                };
                apply_deadzone(v, *deadzone)
            }
        }
    }

    fn just_pressed(&self, src: &dyn InputSource) -> bool {
        match self {
            Source::Key(k) => src.key_just_pressed(k),
            Source::Mouse(m) => src.mouse_just_pressed(m),
            Source::PadButton(b) => src.pad_just_pressed(b),
            Source::PadAxis { .. } => false, // derived from the action's own edge
        }
    }
}

/// One way to drive an action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Binding {
    Key(String),
    Mouse(String),
    PadButton(String),
    PadAxis {
        axis: String,
        #[serde(default)]
        sign: Sign,
        #[serde(default = "d_deadzone")]
        deadzone: f32,
    },
    /// 1D axis from two inputs: `positive − negative`.
    Pair { negative: Source, positive: Source },
    /// 2D vector from four inputs (WASD, d-pad, ...). Diagonals are normalised.
    Composite { up: Source, down: Source, left: Source, right: Source },
    /// 2D vector from two axes with a radial deadzone.
    Stick {
        x: String,
        y: String,
        #[serde(default = "d_deadzone")]
        deadzone: f32,
        /// SDL reports down as `+y`; flip so up is `+y` (default).
        #[serde(default = "d_true")]
        invert_y: bool,
    },
}

/// A rebindable slot inside a composite binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Part {
    Up,
    Down,
    Left,
    Right,
    Negative,
    Positive,
    StickX,
    StickY,
}

impl Part {
    pub fn label(self) -> &'static str {
        match self {
            Part::Up => "Up",
            Part::Down => "Down",
            Part::Left => "Left",
            Part::Right => "Right",
            Part::Negative => "−",
            Part::Positive => "+",
            Part::StickX => "X axis",
            Part::StickY => "Y axis",
        }
    }
}

impl From<Source> for Binding {
    fn from(s: Source) -> Binding {
        match s {
            Source::Key(k) => Binding::Key(k),
            Source::Mouse(m) => Binding::Mouse(m),
            Source::PadButton(b) => Binding::PadButton(b),
            Source::PadAxis { axis, sign, deadzone } => Binding::PadAxis { axis, sign, deadzone },
        }
    }
}

impl Binding {
    /// The single source of a simple binding.
    pub fn as_source(&self) -> Option<Source> {
        Some(match self.clone() {
            Binding::Key(k) => Source::Key(k),
            Binding::Mouse(m) => Source::Mouse(m),
            Binding::PadButton(b) => Source::PadButton(b),
            Binding::PadAxis { axis, sign, deadzone } => Source::PadAxis { axis, sign, deadzone },
            _ => return None,
        })
    }

    /// Parts of this binding that can be rebound individually.
    pub fn parts(&self) -> Vec<Part> {
        match self {
            Binding::Pair { .. } => vec![Part::Negative, Part::Positive],
            Binding::Composite { .. } => vec![Part::Up, Part::Down, Part::Left, Part::Right],
            Binding::Stick { .. } => vec![Part::StickX, Part::StickY],
            _ => Vec::new(),
        }
    }

    /// Source in `part` (or the whole simple binding when `part` is `None`).
    pub fn source_at(&self, part: Option<Part>) -> Option<Source> {
        let axis_both = |a: &String| Source::PadAxis { axis: a.clone(), sign: Sign::Both, deadzone: d_deadzone() };
        match (part, self) {
            (None, b) => b.as_source(),
            (Some(Part::Negative), Binding::Pair { negative, .. }) => Some(negative.clone()),
            (Some(Part::Positive), Binding::Pair { positive, .. }) => Some(positive.clone()),
            (Some(Part::Up), Binding::Composite { up, .. }) => Some(up.clone()),
            (Some(Part::Down), Binding::Composite { down, .. }) => Some(down.clone()),
            (Some(Part::Left), Binding::Composite { left, .. }) => Some(left.clone()),
            (Some(Part::Right), Binding::Composite { right, .. }) => Some(right.clone()),
            (Some(Part::StickX), Binding::Stick { x, .. }) => Some(axis_both(x)),
            (Some(Part::StickY), Binding::Stick { y, .. }) => Some(axis_both(y)),
            _ => None,
        }
    }

    /// Replace the source at `part`. Fails if the part doesn't exist or the source kind
    /// doesn't fit (stick parts need an axis).
    pub fn set_source_at(&mut self, part: Option<Part>, s: Source) -> Result<(), RebindError> {
        match (part, &mut *self) {
            (None, b) => {
                // Keep the old deadzone/sign when an axis replaces an axis.
                let new = match (&*b, s) {
                    (Binding::PadAxis { deadzone, .. }, Source::PadAxis { axis, sign, .. }) => {
                        Binding::PadAxis { axis, sign, deadzone: *deadzone }
                    }
                    (_, s) => Binding::from(s),
                };
                *b = new;
                Ok(())
            }
            (Some(Part::Negative), Binding::Pair { negative, .. }) => {
                *negative = s;
                Ok(())
            }
            (Some(Part::Positive), Binding::Pair { positive, .. }) => {
                *positive = s;
                Ok(())
            }
            (Some(Part::Up), Binding::Composite { up, .. }) => {
                *up = s;
                Ok(())
            }
            (Some(Part::Down), Binding::Composite { down, .. }) => {
                *down = s;
                Ok(())
            }
            (Some(Part::Left), Binding::Composite { left, .. }) => {
                *left = s;
                Ok(())
            }
            (Some(Part::Right), Binding::Composite { right, .. }) => {
                *right = s;
                Ok(())
            }
            (Some(Part::StickX), Binding::Stick { x, .. }) => match s {
                Source::PadAxis { axis, .. } => {
                    *x = axis;
                    Ok(())
                }
                _ => Err(RebindError::NeedsAxis),
            },
            (Some(Part::StickY), Binding::Stick { y, .. }) => match s {
                Source::PadAxis { axis, .. } => {
                    *y = axis;
                    Ok(())
                }
                _ => Err(RebindError::NeedsAxis),
            },
            _ => Err(RebindError::BadTarget),
        }
    }

    /// Every physical input this binding listens to (for conflict checks).
    pub fn sources(&self) -> Vec<(Option<Part>, Source)> {
        match self {
            Binding::Pair { .. } | Binding::Composite { .. } | Binding::Stick { .. } => self
                .parts()
                .into_iter()
                .filter_map(|p| self.source_at(Some(p)).map(|s| (Some(p), s)))
                .collect(),
            _ => self.as_source().map(|s| (None, s)).into_iter().collect(),
        }
    }

    /// Short description for UIs.
    pub fn label(&self) -> String {
        match self {
            Binding::Pair { negative, positive } => format!("{} / {}", negative.label(), positive.label()),
            Binding::Composite { up, down, left, right } => {
                format!("{} {} {} {}", up.label(), left.label(), down.label(), right.label())
            }
            Binding::Stick { x, y, .. } => format!("Stick {x}/{y}"),
            other => other.as_source().map(|s| s.label()).unwrap_or_default(),
        }
    }

    fn eval(&self, src: &dyn InputSource) -> Eval {
        match self {
            Binding::Pair { negative, positive } => {
                let v = positive.value(src).max(0.0) - negative.value(src).max(0.0);
                Eval {
                    down: v.abs() >= PRESS_THRESHOLD,
                    pressed: negative.just_pressed(src) || positive.just_pressed(src),
                    value: v,
                    vec: Vec2::ZERO,
                }
            }
            Binding::Composite { up, down, left, right } => {
                let v = Vec2::new(
                    right.value(src).max(0.0) - left.value(src).max(0.0),
                    up.value(src).max(0.0) - down.value(src).max(0.0),
                );
                let v = clamp_len(v);
                Eval {
                    down: v.length() >= PRESS_THRESHOLD,
                    pressed: [up, down, left, right].iter().any(|s| s.just_pressed(src)),
                    value: v.length(),
                    vec: v,
                }
            }
            Binding::Stick { x, y, deadzone, invert_y } => {
                let mut v = Vec2::new(src.axis(x), src.axis(y));
                if *invert_y {
                    v.y = -v.y;
                }
                let len = v.length();
                let v = if len <= *deadzone || len == 0.0 {
                    Vec2::ZERO
                } else {
                    v / len * ((len - deadzone) / (1.0 - deadzone).max(1e-3)).min(1.0)
                };
                Eval { down: v.length() >= PRESS_THRESHOLD, pressed: false, value: v.length(), vec: v }
            }
            simple => {
                let s = simple.as_source().expect("simple binding");
                let value = s.value(src);
                Eval {
                    down: if s.is_axis() { value.abs() >= PRESS_THRESHOLD } else { value > 0.0 },
                    pressed: s.just_pressed(src),
                    value,
                    vec: Vec2::ZERO,
                }
            }
        }
    }
}

struct Eval {
    down: bool,
    pressed: bool,
    value: f32,
    vec: Vec2,
}

fn apply_deadzone(v: f32, dz: f32) -> f32 {
    let a = v.abs();
    if a <= dz {
        0.0
    } else {
        v.signum() * ((a - dz) / (1.0 - dz).max(1e-3)).min(1.0)
    }
}

fn clamp_len(v: Vec2) -> Vec2 {
    if v.length() > 1.0 {
        v.normalize()
    } else {
        v
    }
}

// ------------------------------------------------------------------ the asset

/// A named layer of actions that can be switched on and off together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextDef {
    pub name: String,
    /// Initial state in a new [`ActionState`].
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// While enabled, mutes every context declared before this one.
    #[serde(default)]
    pub exclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionDef {
    pub name: String,
    /// Display name in the rebinding UI (defaults to `name`).
    #[serde(default)]
    pub label: Option<String>,
    /// `None` = global, always active.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// `false` hides the action from the rebinding panel and ignores overrides.
    #[serde(default = "d_true")]
    pub rebindable: bool,
}

impl ActionDef {
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionMap {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub contexts: Vec<ContextDef>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
}

impl Default for ActionMap {
    fn default() -> Self {
        ActionMap { version: FORMAT_VERSION, contexts: Vec::new(), actions: Vec::new() }
    }
}

/// User rebinds: only the actions whose bindings differ from the shipped map.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub actions: Vec<ActionOverride>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionOverride {
    pub name: String,
    pub bindings: Vec<Binding>,
}

/// What to do when a new source is already used by another action in an overlapping
/// context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse and report the conflicts.
    #[default]
    Reject,
    /// Give the other action the input this slot had (single conflict only).
    Swap,
    /// Bind anyway; both actions fire. See [`ActionMap::all_conflicts`].
    Allow,
}

impl ConflictPolicy {
    pub const ALL: [ConflictPolicy; 3] = [ConflictPolicy::Reject, ConflictPolicy::Swap, ConflictPolicy::Allow];
    pub fn label(self) -> &'static str {
        match self {
            ConflictPolicy::Reject => "Reject",
            ConflictPolicy::Swap => "Swap",
            ConflictPolicy::Allow => "Allow shared",
        }
    }
}

/// Where a rebind lands: `slot` indexes the action's bindings (`== len` appends a new
/// simple binding); `part` selects a member of a composite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindingTarget {
    pub slot: usize,
    pub part: Option<Part>,
}

/// Another action already using a source.
#[derive(Clone, Debug, PartialEq)]
pub struct RebindConflict {
    pub action: String,
    pub target: BindingTarget,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RebindOutcome {
    Bound,
    /// The other action received this slot's previous input.
    Swapped { other: String },
    /// Bound while sharing the input with these actions ([`ConflictPolicy::Allow`]).
    Shared { with: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RebindError {
    UnknownAction,
    NotRebindable,
    BadTarget,
    /// A stick axis needs an axis input.
    NeedsAxis,
    Conflict(Vec<RebindConflict>),
}

impl std::fmt::Display for RebindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RebindError::UnknownAction => write!(f, "unknown action"),
            RebindError::NotRebindable => write!(f, "action is not rebindable"),
            RebindError::BadTarget => write!(f, "no such binding slot"),
            RebindError::NeedsAxis => write!(f, "this slot needs an analog axis"),
            RebindError::Conflict(c) => {
                let names: Vec<_> = c.iter().map(|c| c.action.as_str()).collect();
                write!(f, "{} is already bound to {}", c[0].source.label(), names.join(", "))
            }
        }
    }
}

impl std::error::Error for RebindError {}

impl ActionMap {
    // ---- parse / write

    pub fn from_ron(text: &str) -> Result<ActionMap> {
        ron_options().from_str(text).map_err(|e| anyhow!("action map: {e}"))
    }

    pub fn to_ron(&self) -> Result<String> {
        ron::ser::to_string_pretty(self, pretty()).map_err(|e| anyhow!("action map: {e}"))
    }

    pub fn load(path: &Path) -> Result<ActionMap> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_ron(&text).with_context(|| path.display().to_string())
    }

    /// Problems worth showing after a load: duplicate action names, unknown contexts,
    /// names no device recognises, bad deadzones, and shared inputs.
    pub fn validate(&self) -> Vec<String> {
        let mut w = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for a in &self.actions {
            if !seen.insert(a.name.as_str()) {
                w.push(format!("duplicate action '{}'", a.name));
            }
            if let Some(c) = &a.context {
                if !self.contexts.iter().any(|d| &d.name == c) {
                    w.push(format!("action '{}' uses undeclared context '{c}'", a.name));
                }
            }
            for b in &a.bindings {
                if let Binding::Stick { deadzone, .. } = b {
                    if !(0.0..1.0).contains(deadzone) {
                        w.push(format!("action '{}': stick deadzone {deadzone} outside 0..1", a.name));
                    }
                }
                for (_, s) in b.sources() {
                    match &s {
                        Source::Key(k) if keycode(k).is_none() => w.push(format!("action '{}': unknown key '{k}'", a.name)),
                        Source::Mouse(m) if mouse_button(m).is_none() => w.push(format!("action '{}': unknown mouse button '{m}'", a.name)),
                        Source::PadButton(p) if pad_button(p).is_none() => w.push(format!("action '{}': unknown pad button '{p}'", a.name)),
                        Source::PadAxis { axis, deadzone, .. } => {
                            if pad_axis(axis).is_none() {
                                w.push(format!("action '{}': unknown axis '{axis}'", a.name));
                            }
                            if !(0.0..1.0).contains(deadzone) {
                                w.push(format!("action '{}': deadzone {deadzone} outside 0..1", a.name));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for c in self.all_conflicts() {
            w.push(format!("'{}' and '{}' both use {}", c.0, c.1, c.2.label()));
        }
        w
    }

    // ---- lookup

    pub fn action(&self, name: &str) -> Option<&ActionDef> {
        self.actions.iter().find(|a| a.name == name)
    }

    pub fn action_mut(&mut self, name: &str) -> Option<&mut ActionDef> {
        self.actions.iter_mut().find(|a| a.name == name)
    }

    fn contexts_overlap(a: &ActionDef, b: &ActionDef) -> bool {
        match (&a.context, &b.context) {
            (Some(x), Some(y)) => x == y,
            _ => true, // a global action is live alongside everything
        }
    }

    // ---- conflicts

    /// Actions (other than `except`) that already use `source` in a context that is live
    /// at the same time as `context`.
    pub fn conflicts_for(&self, except: &str, source: &Source) -> Vec<RebindConflict> {
        let Some(me) = self.action(except) else { return Vec::new() };
        let mut out = Vec::new();
        for a in self.actions.iter().filter(|a| a.name != except && Self::contexts_overlap(me, a)) {
            for (slot, b) in a.bindings.iter().enumerate() {
                for (part, s) in b.sources() {
                    if s.overlaps(source) {
                        out.push(RebindConflict { action: a.name.clone(), target: BindingTarget { slot, part }, source: s });
                    }
                }
            }
        }
        out
    }

    /// Every pair of actions sharing a physical input: `(a, b, source)`.
    pub fn all_conflicts(&self) -> Vec<(String, String, Source)> {
        let mut out = Vec::new();
        for (i, a) in self.actions.iter().enumerate() {
            for b in &self.actions[i + 1..] {
                if !Self::contexts_overlap(a, b) {
                    continue;
                }
                for x in a.bindings.iter().flat_map(|b| b.sources()) {
                    for y in b.bindings.iter().flat_map(|b| b.sources()) {
                        if x.1.overlaps(&y.1) {
                            out.push((a.name.clone(), b.name.clone(), x.1.clone()));
                        }
                    }
                }
            }
        }
        out
    }

    // ---- rebinding

    /// Bind `source` at `target` of `action`, resolving conflicts per `policy`.
    /// On error the map is untouched.
    pub fn rebind(
        &mut self,
        action: &str,
        target: BindingTarget,
        source: Source,
        policy: ConflictPolicy,
    ) -> Result<RebindOutcome, RebindError> {
        let def = self.action(action).ok_or(RebindError::UnknownAction)?;
        if !def.rebindable {
            return Err(RebindError::NotRebindable);
        }
        if target.slot > def.bindings.len() || (target.slot == def.bindings.len() && target.part.is_some()) {
            return Err(RebindError::BadTarget);
        }
        let old = def.bindings.get(target.slot).and_then(|b| b.source_at(target.part));
        // Rebinding to the very same input is a no-op, not a conflict with itself.
        let conflicts: Vec<_> = self.conflicts_for(action, &source);
        let mut outcome = RebindOutcome::Bound;
        let mut swap: Option<(RebindConflict, Source)> = None;
        if !conflicts.is_empty() {
            match policy {
                ConflictPolicy::Reject => return Err(RebindError::Conflict(conflicts)),
                ConflictPolicy::Allow => {
                    let mut with: Vec<String> = conflicts.iter().map(|c| c.action.clone()).collect();
                    with.dedup();
                    outcome = RebindOutcome::Shared { with };
                }
                ConflictPolicy::Swap => {
                    let (Some(old), [c]) = (old.clone(), conflicts.as_slice()) else {
                        return Err(RebindError::Conflict(conflicts));
                    };
                    outcome = RebindOutcome::Swapped { other: c.action.clone() };
                    swap = Some((c.clone(), old));
                }
            }
        }
        // Validate the write on a scratch copy first so failures leave the map untouched.
        let mut scratch = self.clone();
        {
            let def = scratch.action_mut(action).unwrap();
            if target.slot == def.bindings.len() {
                def.bindings.push(Binding::from(source));
            } else {
                def.bindings[target.slot].set_source_at(target.part, source)?;
            }
        }
        if let Some((c, old)) = swap {
            let other = scratch.action_mut(&c.action).unwrap();
            other.bindings[c.target.slot].set_source_at(c.target.part, old)?;
        }
        *self = scratch;
        Ok(outcome)
    }

    pub fn remove_binding(&mut self, action: &str, slot: usize) -> bool {
        match self.action_mut(action) {
            Some(a) if a.rebindable && slot < a.bindings.len() => {
                a.bindings.remove(slot);
                true
            }
            _ => false,
        }
    }

    /// Restore one action's bindings from `defaults`. Returns whether anything changed.
    pub fn reset_action(&mut self, name: &str, defaults: &ActionMap) -> bool {
        let Some(d) = defaults.action(name) else { return false };
        match self.action_mut(name) {
            Some(a) if a.bindings != d.bindings => {
                a.bindings = d.bindings.clone();
                true
            }
            _ => false,
        }
    }

    pub fn reset_all(&mut self, defaults: &ActionMap) -> bool {
        let names: Vec<String> = self.actions.iter().map(|a| a.name.clone()).collect();
        names.iter().fold(false, |ch, n| self.reset_action(n, defaults) | ch)
    }

    pub fn is_default(&self, name: &str, defaults: &ActionMap) -> bool {
        match (self.action(name), defaults.action(name)) {
            (Some(a), Some(d)) => a.bindings == d.bindings,
            _ => true,
        }
    }

    // ---- overrides layer

    /// Actions whose bindings differ from `defaults`.
    pub fn diff(&self, defaults: &ActionMap) -> Overrides {
        Overrides {
            version: FORMAT_VERSION,
            actions: self
                .actions
                .iter()
                .filter(|a| a.rebindable && defaults.action(&a.name).is_some_and(|d| d.bindings != a.bindings))
                .map(|a| ActionOverride { name: a.name.clone(), bindings: a.bindings.clone() })
                .collect(),
        }
    }

    /// Layer `o` on top; returns warnings for entries that name no (rebindable) action.
    pub fn apply_overrides(&mut self, o: &Overrides) -> Vec<String> {
        let mut warnings = Vec::new();
        for ov in &o.actions {
            match self.action_mut(&ov.name) {
                Some(a) if a.rebindable => a.bindings = ov.bindings.clone(),
                Some(_) => warnings.push(format!("override for non-rebindable action '{}' ignored", ov.name)),
                None => warnings.push(format!("override for unknown action '{}' ignored", ov.name)),
            }
        }
        warnings
    }

    pub fn overrides_to_ron(o: &Overrides) -> Result<String> {
        ron::ser::to_string_pretty(o, pretty()).map_err(|e| anyhow!("overrides: {e}"))
    }

    pub fn overrides_from_ron(text: &str) -> Result<Overrides> {
        ron_options().from_str(text).map_err(|e| anyhow!("overrides: {e}"))
    }

    /// Write only the differences from `defaults` to `path` (atomic replace). With no
    /// differences the file is removed, so "reset all" leaves nothing behind.
    pub fn save_overrides(&self, defaults: &ActionMap, path: &Path) -> Result<()> {
        let o = self.diff(defaults);
        if o.actions.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
            };
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = path.with_extension("ron.tmp");
        std::fs::write(&tmp, Self::overrides_to_ron(&o)?).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    }

    /// Load the shipped `default_path`, then layer `overrides_path` if it exists. Returns
    /// the merged map and warnings (a corrupt overrides file is a warning, not an error:
    /// the defaults still work).
    pub fn load_layered(default_path: &Path, overrides_path: &Path) -> Result<(ActionMap, Vec<String>)> {
        let mut map = Self::load(default_path)?;
        let mut warnings = map.validate();
        match std::fs::read_to_string(overrides_path) {
            Ok(text) => match Self::overrides_from_ron(&text) {
                Ok(o) => warnings.extend(map.apply_overrides(&o)),
                Err(e) => warnings.push(format!("{}: {e} (using defaults)", overrides_path.display())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warnings.push(format!("{}: {e} (using defaults)", overrides_path.display())),
        }
        Ok((map, warnings))
    }
}

// ------------------------------------------------------------------ resolved state

/// One action's value for the current frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ActionValue {
    pub down: bool,
    pub just_pressed: bool,
    pub just_released: bool,
    pub value: f32,
    pub vec: Vec2,
}

/// Per-frame resolution of an [`ActionMap`]. Call [`resolve`](Self::resolve) once per
/// frame after input events were pumped; query by action name.
#[derive(Clone, Debug, Default)]
pub struct ActionState {
    values: HashMap<String, ActionValue>,
    context_enabled: HashMap<String, bool>,
}

impl ActionState {
    pub fn new(map: &ActionMap) -> Self {
        ActionState {
            values: HashMap::new(),
            context_enabled: map.contexts.iter().map(|c| (c.name.clone(), c.enabled)).collect(),
        }
    }

    pub fn set_context_enabled(&mut self, name: &str, enabled: bool) {
        self.context_enabled.insert(name.to_string(), enabled);
    }

    /// Undeclared contexts count as enabled until set otherwise.
    pub fn context_enabled(&self, name: &str) -> bool {
        self.context_enabled.get(name).copied().unwrap_or(true)
    }

    /// Whether actions of `context` are live right now (enabled and not muted by an
    /// exclusive context declared after it).
    pub fn context_active(&self, map: &ActionMap, context: &str) -> bool {
        if !self.context_enabled(context) {
            return false;
        }
        let idx = map.contexts.iter().position(|c| c.name == context);
        let top_exclusive = map
            .contexts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.exclusive && self.context_enabled(&c.name))
            .map(|(i, _)| i)
            .max();
        match (idx, top_exclusive) {
            (Some(i), Some(e)) => i >= e,
            _ => true,
        }
    }

    pub fn resolve(&mut self, map: &ActionMap, src: &dyn InputSource) {
        let mut seen = std::collections::HashSet::new();
        for def in &map.actions {
            seen.insert(def.name.as_str());
            let active = def.context.as_deref().map_or(true, |c| self.context_active(map, c));
            let prev = self.values.get(&def.name).map_or(false, |v| v.down);
            let mut next = ActionValue::default();
            if active {
                let mut any_pressed = false;
                let mut sum = Vec2::ZERO;
                for b in &def.bindings {
                    let e = b.eval(src);
                    next.down |= e.down;
                    any_pressed |= e.pressed;
                    if e.value.abs() > next.value.abs() {
                        next.value = e.value;
                    }
                    sum += e.vec;
                }
                next.vec = clamp_len(sum);
                next.just_pressed = any_pressed || (next.down && !prev);
            }
            next.just_released = prev && !next.down;
            self.values.insert(def.name.clone(), next);
        }
        self.values.retain(|k, _| seen.contains(k.as_str()));
    }

    pub fn get(&self, action: &str) -> ActionValue {
        self.values.get(action).copied().unwrap_or_default()
    }
    pub fn pressed(&self, action: &str) -> bool {
        self.get(action).down
    }
    pub fn just_pressed(&self, action: &str) -> bool {
        self.get(action).just_pressed
    }
    pub fn just_released(&self, action: &str) -> bool {
        self.get(action).just_released
    }
    /// Strongest binding value, signed (vec2 bindings contribute their length).
    pub fn axis(&self, action: &str) -> f32 {
        self.get(action).value
    }
    /// Summed vec2 bindings, length clamped to 1, `+y` up.
    pub fn vec2(&self, action: &str) -> Vec2 {
        self.get(action).vec
    }
}

// ------------------------------------------------------------------ rebinding capture

/// Result of one [`RebindState::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum RebindEvent {
    Cancelled,
    Done(Result<RebindOutcome, RebindError>),
}

/// UI/session state of the rebinding flow: what is being listened for and how conflicts
/// are handled.
#[derive(Clone, Debug, Default)]
pub struct RebindState {
    /// `(action, target)` currently waiting for an input.
    pub listening: Option<(String, BindingTarget)>,
    pub policy: ConflictPolicy,
    /// Last status line for the UI (conflict explanation, "bound X", ...).
    pub message: Option<String>,
    /// Conflicts from the last rejected attempt.
    pub last_conflicts: Vec<RebindConflict>,
    axis_armed: bool,
    grace: bool,
}

impl RebindState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&mut self, action: &str, target: BindingTarget) {
        self.listening = Some((action.to_string(), target));
        self.message = None;
        self.last_conflicts.clear();
        self.axis_armed = false;
        self.grace = true;
    }

    pub fn cancel(&mut self) {
        self.listening = None;
        self.last_conflicts.clear();
    }

    /// Feed one captured source (e.g. from egui events). Escape cancels. A rejected
    /// conflict keeps listening so the user can try another input.
    pub fn offer(&mut self, map: &mut ActionMap, source: Source) -> Option<RebindEvent> {
        let (action, target) = self.listening.clone()?;
        if source == Source::Key("Escape".into()) {
            self.cancel();
            self.message = Some("Cancelled".into());
            return Some(RebindEvent::Cancelled);
        }
        let result = map.rebind(&action, target, source.clone(), self.policy);
        match &result {
            Ok(outcome) => {
                self.listening = None;
                self.last_conflicts.clear();
                self.message = Some(match outcome {
                    RebindOutcome::Bound => format!("Bound {} to {}", source.label(), action),
                    RebindOutcome::Swapped { other } => format!("Bound {}; swapped with {other}", source.label()),
                    RebindOutcome::Shared { with } => format!("Bound {} (also used by {})", source.label(), with.join(", ")),
                });
            }
            Err(RebindError::Conflict(c)) => {
                self.last_conflicts = c.clone();
                self.message = Some(format!("{} — pick another input or change the conflict policy", RebindError::Conflict(c.clone())));
            }
            Err(e) => {
                self.listening = None;
                self.message = Some(e.to_string());
            }
        }
        Some(RebindEvent::Done(result))
    }

    /// Listen on a device source: takes the first button/key pressed this frame, or the
    /// first axis pushed past [`CAPTURE_THRESHOLD`] after all axes were seen at rest.
    pub fn poll(&mut self, map: &mut ActionMap, src: &dyn InputSource) -> Option<RebindEvent> {
        self.listening.as_ref()?;
        let candidates = src.capture_candidates();
        if !candidates.iter().any(Source::is_axis) {
            self.axis_armed = true;
        }
        let armed = self.axis_armed;
        let pick = candidates
            .iter()
            .find(|s| *s == &Source::Key("Escape".into()))
            .or_else(|| candidates.iter().find(|s| !s.is_axis()))
            .or_else(|| candidates.iter().find(|s| s.is_axis() && armed))
            .cloned()?;
        self.offer(map, pick)
    }
}

// ------------------------------------------------------------------ egui panel

fn egui_key_name(key: egui::Key) -> Option<String> {
    use egui::Key as K;
    let dbg = format!("{key:?}");
    let name = match key {
        K::ArrowUp => "Up".to_string(),
        K::ArrowDown => "Down".to_string(),
        K::ArrowLeft => "Left".to_string(),
        K::ArrowRight => "Right".to_string(),
        K::Enter => "Return".to_string(),
        K::Escape => "Escape".to_string(),
        _ => match dbg.strip_prefix("Num") {
            Some(d) if d.len() == 1 => d.to_string(),
            _ => dbg,
        },
    };
    keycode(&name).map(|_| name)
}

/// Inputs pressed this frame according to egui (keys and pointer buttons).
fn egui_capture(ui: &egui::Ui) -> Option<Source> {
    ui.input(|i| {
        for ev in &i.events {
            match ev {
                egui::Event::Key { key, pressed: true, repeat: false, .. } => {
                    if let Some(n) = egui_key_name(*key) {
                        return Some(Source::Key(n));
                    }
                }
                egui::Event::PointerButton { button, pressed: true, .. } => {
                    let n = match button {
                        egui::PointerButton::Primary => "Left",
                        egui::PointerButton::Secondary => "Right",
                        egui::PointerButton::Middle => "Middle",
                        egui::PointerButton::Extra1 => "X1",
                        egui::PointerButton::Extra2 => "X2",
                    };
                    return Some(Source::Mouse(n.to_string()));
                }
                _ => {}
            }
        }
        None
    })
}

/// Rebinding UI: one collapsing group per context, a row per action with a button per
/// binding (click, then press the new input; Escape cancels), `+` to add, `x` to remove
/// and *Reset* per action plus *Reset all*. `defaults` is the shipped map. `input` is the
/// live device source for gamepad/axis capture; keyboard and mouse are also read from
/// egui events, so `None` works for an editor window. Returns whether `map` changed
/// (the caller decides when to [`ActionMap::save_overrides`]).
pub fn draw_rebind_panel(
    ui: &mut egui::Ui,
    map: &mut ActionMap,
    state: &mut RebindState,
    defaults: &ActionMap,
    input: Option<&dyn InputSource>,
) -> bool {
    let before = map.clone();

    // 1. Capture first so a press this frame is applied before drawing.
    if state.listening.is_some() {
        if state.grace {
            state.grace = false; // ignore the click that started listening
        } else {
            let ev = input
                .and_then(|src| state.poll(map, src))
                .or_else(|| egui_capture(ui).and_then(|s| state.offer(map, s)));
            let _ = ev;
        }
        ui.ctx().request_repaint();
    }

    // 2. Header.
    ui.horizontal(|ui| {
        ui.label("On conflict:");
        egui::ComboBox::from_id_salt("rebind_policy")
            .selected_text(state.policy.label())
            .show_ui(ui, |ui| {
                for p in ConflictPolicy::ALL {
                    ui.selectable_value(&mut state.policy, p, p.label());
                }
            });
        if ui.button("Reset all to default").clicked() {
            map.reset_all(defaults);
            state.cancel();
            state.message = Some("All bindings reset".into());
        }
    });
    if let Some(m) = &state.message {
        ui.colored_label(
            if state.last_conflicts.is_empty() { ui.visuals().text_color() } else { ui.visuals().warn_fg_color },
            m,
        );
    }
    ui.separator();

    // 3. Groups: contexts in declaration order, then global / undeclared.
    let mut groups: Vec<(String, Option<String>)> =
        map.contexts.iter().map(|c| (c.name.clone(), Some(c.name.clone()))).collect();
    for a in &map.actions {
        if let Some(c) = &a.context {
            if !groups.iter().any(|(_, g)| g.as_ref() == Some(c)) {
                groups.push((c.clone(), Some(c.clone())));
            }
        }
    }
    groups.push(("Global".into(), None));

    let mut start: Option<(String, BindingTarget)> = None;
    let mut remove: Option<(String, usize)> = None;
    let mut reset: Option<String> = None;

    for (title, ctx) in groups {
        let names: Vec<String> = map
            .actions
            .iter()
            .filter(|a| a.context == ctx && a.rebindable)
            .map(|a| a.name.clone())
            .collect();
        if names.is_empty() {
            continue;
        }
        egui::CollapsingHeader::new(title.clone())
            .id_salt(("rebind_group", &title))
            .default_open(true)
            .show(ui, |ui| {
                for name in &names {
                    let Some(def) = map.action(name) else { continue };
                    ui.push_id(name, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(def.display_name()).strong());
                            for (slot, b) in def.bindings.iter().enumerate() {
                                ui.separator();
                                let parts = b.parts();
                                if parts.is_empty() {
                                    let tgt = BindingTarget { slot, part: None };
                                    binding_button(ui, state, name, tgt, &b.label(), &mut start);
                                } else {
                                    for p in parts {
                                        let tgt = BindingTarget { slot, part: Some(p) };
                                        let text = format!(
                                            "{}: {}",
                                            p.label(),
                                            b.source_at(Some(p)).map(|s| s.label()).unwrap_or_default()
                                        );
                                        binding_button(ui, state, name, tgt, &text, &mut start);
                                    }
                                }
                                if ui.small_button("x").on_hover_text("Remove this binding").clicked() {
                                    remove = Some((name.clone(), slot));
                                }
                            }
                            ui.separator();
                            let new_tgt = BindingTarget { slot: def.bindings.len(), part: None };
                            binding_button(ui, state, name, new_tgt, "+", &mut start);
                            let is_default = map.is_default(name, defaults);
                            if ui.add_enabled(!is_default, egui::Button::new("Reset")).on_hover_text("Reset to default").clicked() {
                                reset = Some(name.clone());
                            }
                        });
                    });
                }
            });
    }

    if let Some((a, slot)) = remove {
        map.remove_binding(&a, slot);
    }
    if let Some(a) = reset {
        map.reset_action(&a, defaults);
        if state.listening.as_ref().is_some_and(|(l, _)| *l == a) {
            state.cancel();
        }
    }
    if let Some((a, t)) = start {
        state.start(&a, t);
    }
    if state.listening.is_some() {
        ui.horizontal(|ui| {
            ui.label("Press the new input… (Escape to cancel)");
            if ui.button("Cancel").clicked() {
                state.cancel();
            }
        });
    }

    *map != before
}

fn binding_button(
    ui: &mut egui::Ui,
    state: &RebindState,
    action: &str,
    target: BindingTarget,
    text: &str,
    start: &mut Option<(String, BindingTarget)>,
) {
    let listening = state.listening.as_ref().is_some_and(|(a, t)| a == action && *t == target);
    let shown = if listening { "…".to_string() } else { text.to_string() };
    let conflicted = state.last_conflicts.iter().any(|c| c.action == action && c.target == target);
    let mut rt = egui::RichText::new(shown);
    if conflicted {
        rt = rt.color(ui.visuals().warn_fg_color);
    }
    if ui.selectable_label(listening, rt).clicked() && !listening {
        *start = Some((action.to_string(), target));
    }
}

/// [`draw_rebind_panel`] in a floating window (editor use).
pub fn show_rebind_window(
    ctx: &egui::Context,
    open: &mut bool,
    map: &mut ActionMap,
    state: &mut RebindState,
    defaults: &ActionMap,
    input: Option<&dyn InputSource>,
) -> bool {
    let mut changed = false;
    egui::Window::new("Input bindings").open(open).default_width(520.0).show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            changed = draw_rebind_panel(ui, map, state, defaults, input);
        });
    });
    changed
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    const EXAMPLE: &str = include_str!("../../docs/examples/input_map.ron");

    #[derive(Default)]
    struct Fake {
        keys: HashSet<String>,
        keys_pressed: HashSet<String>,
        pad: HashSet<String>,
        mouse: HashSet<String>,
        axes: HashMap<String, f32>,
        capture: Vec<Source>,
    }

    impl Fake {
        fn frame(&mut self) {
            self.keys_pressed.clear();
            self.capture.clear();
        }
        fn press(&mut self, k: &str) {
            if self.keys.insert(k.into()) {
                self.keys_pressed.insert(k.into());
            }
        }
        fn release(&mut self, k: &str) {
            self.keys.remove(k);
        }
        fn set_axis(&mut self, a: &str, v: f32) {
            self.axes.insert(a.into(), v);
        }
    }

    impl InputSource for Fake {
        fn key_down(&self, n: &str) -> bool {
            self.keys.contains(n)
        }
        fn key_just_pressed(&self, n: &str) -> bool {
            self.keys_pressed.contains(n)
        }
        fn mouse_down(&self, n: &str) -> bool {
            self.mouse.contains(n)
        }
        fn mouse_just_pressed(&self, _: &str) -> bool {
            false
        }
        fn pad_down(&self, n: &str) -> bool {
            self.pad.contains(n)
        }
        fn pad_just_pressed(&self, _: &str) -> bool {
            false
        }
        fn axis(&self, n: &str) -> f32 {
            self.axes.get(n).copied().unwrap_or(0.0)
        }
        fn capture_candidates(&self) -> Vec<Source> {
            self.capture.clone()
        }
    }

    fn example() -> ActionMap {
        ActionMap::from_ron(EXAMPLE).expect("example parses")
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("input-map-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn example_parses_and_has_no_warnings() {
        let m = example();
        assert_eq!(m.version, 1);
        assert_eq!(m.actions.len(), 8);
        assert_eq!(m.action("jump").unwrap().context.as_deref(), Some("gameplay"));
        assert!(m.action("pause").unwrap().context.is_none());
        assert!(!m.action("pause").unwrap().rebindable);
        assert!(matches!(m.action("move").unwrap().bindings[2], Binding::Stick { invert_y: true, .. }));
        assert_eq!(m.validate(), Vec::<String>::new());
        // RON roundtrip keeps it identical.
        assert_eq!(ActionMap::from_ron(&m.to_ron().unwrap()).unwrap(), m);
    }

    #[test]
    fn hand_written_implicit_some_and_defaults() {
        let m = ActionMap::from_ron(r#"(actions: [(name: "a", label: "Aaa", context: "x", bindings: [Key("A")])])"#).unwrap();
        assert_eq!(m.actions[0].label.as_deref(), Some("Aaa"));
        assert_eq!(m.actions[0].context.as_deref(), Some("x"));
        assert!(m.actions[0].rebindable);
        assert!(ActionMap::from_ron("(actions: [(name: )])").is_err());
    }

    #[test]
    fn pressed_just_pressed_just_released_edges() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        st.resolve(&m, &f);
        assert!(!st.pressed("jump"));

        f.frame();
        f.press("Space");
        st.resolve(&m, &f);
        assert!(st.pressed("jump") && st.just_pressed("jump") && !st.just_released("jump"));

        f.frame(); // held
        st.resolve(&m, &f);
        assert!(st.pressed("jump") && !st.just_pressed("jump"));

        f.frame();
        f.release("Space");
        st.resolve(&m, &f);
        assert!(!st.pressed("jump") && st.just_released("jump"));

        f.frame();
        st.resolve(&m, &f);
        assert!(!st.just_released("jump"));
        assert!(!st.pressed("no_such_action"));
    }

    #[test]
    fn press_and_release_inside_one_frame_still_reports_just_pressed() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.keys_pressed.insert("Space".into()); // tapped between resolves, not held now
        st.resolve(&m, &f);
        assert!(st.just_pressed("jump"));
    }

    #[test]
    fn deadzone_sign_and_axis_as_button() {
        let m = ActionMap::from_ron(
            r#"(actions: [
                (name: "right", bindings: [PadAxis(axis: "LeftX", sign: Positive, deadzone: 0.2)]),
                (name: "left",  bindings: [PadAxis(axis: "LeftX", sign: Negative, deadzone: 0.2)]),
                (name: "x",     bindings: [PadAxis(axis: "LeftX", deadzone: 0.5)]),
            ])"#,
        )
        .unwrap();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.set_axis("LeftX", 0.1); // inside deadzone
        st.resolve(&m, &f);
        assert_eq!(st.axis("right"), 0.0);
        assert!(!st.pressed("right"));

        f.set_axis("LeftX", 0.6); // (0.6-0.2)/0.8 = 0.5
        st.resolve(&m, &f);
        assert!((st.axis("right") - 0.5).abs() < 1e-5);
        assert_eq!(st.axis("left"), 0.0);
        assert!(st.pressed("right") && st.just_pressed("right"));
        assert!((st.axis("x") - 0.2).abs() < 1e-5); // (0.6-0.5)/0.5

        f.set_axis("LeftX", -1.0);
        st.resolve(&m, &f);
        assert_eq!(st.axis("right"), 0.0);
        assert!((st.axis("left") - 1.0).abs() < 1e-5);
        assert!(st.just_released("right") && st.just_pressed("left"));
        assert!((st.axis("x") + 1.0).abs() < 1e-5);
    }

    #[test]
    fn composite_vec2_normalises_diagonals_and_cancels() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.press("W");
        st.resolve(&m, &f);
        assert_eq!(st.vec2("move"), Vec2::new(0.0, 1.0));

        f.press("D");
        st.resolve(&m, &f);
        let v = st.vec2("move");
        assert!((v.length() - 1.0).abs() < 1e-5);
        assert!(v.x > 0.0 && v.y > 0.0 && (v.x - v.y).abs() < 1e-5);

        f.press("S"); // up + down cancel
        st.resolve(&m, &f);
        assert_eq!(st.vec2("move"), Vec2::new(1.0, 0.0));

        // Two bindings active at once still clamp to unit length.
        f.press("Left"); // arrow left: (-1,0) + wasd (1,0)
        st.resolve(&m, &f);
        assert!(st.vec2("move").length() <= 1.0 + 1e-6);
    }

    #[test]
    fn stick_radial_deadzone_and_inverted_y() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.set_axis("LeftX", 0.2);
        f.set_axis("LeftY", 0.1);
        st.resolve(&m, &f);
        assert_eq!(st.vec2("move"), Vec2::ZERO); // |(0.2,0.1)| = 0.22 < 0.25
        f.set_axis("LeftX", 0.0);
        f.set_axis("LeftY", -1.0); // SDL: stick pushed up
        st.resolve(&m, &f);
        assert!((st.vec2("move") - Vec2::new(0.0, 1.0)).length() < 1e-5);
        f.set_axis("LeftX", 1.0);
        f.set_axis("LeftY", 1.0); // down-right, corner > 1 clamps
        st.resolve(&m, &f);
        let v = st.vec2("move");
        assert!(v.x > 0.0 && v.y < 0.0 && v.length() <= 1.0 + 1e-5);
    }

    #[test]
    fn pair_axis_from_keys() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.press("E");
        st.resolve(&m, &f);
        assert_eq!(st.axis("throttle"), 1.0);
        f.press("Q");
        st.resolve(&m, &f);
        assert_eq!(st.axis("throttle"), 0.0);
        f.release("E");
        st.resolve(&m, &f);
        assert_eq!(st.axis("throttle"), -1.0);
    }

    #[test]
    fn context_layers_enable_disable_and_exclusive() {
        let m = example();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.press("Space");
        f.press("Return");
        f.press("Escape");
        st.resolve(&m, &f);
        // menu starts disabled
        assert!(st.pressed("jump"));
        assert!(!st.pressed("ui_accept"));
        assert!(st.pressed("pause"), "global action is always live");

        // Open the menu: exclusive, mutes gameplay, global stays.
        st.set_context_enabled("menu", true);
        st.resolve(&m, &f);
        assert!(st.pressed("ui_accept") && st.just_pressed("ui_accept"));
        assert!(!st.pressed("jump") && st.just_released("jump"));
        assert!(st.pressed("pause"));

        // Disabling gameplay explicitly while the menu is closed.
        st.set_context_enabled("menu", false);
        st.set_context_enabled("gameplay", false);
        st.resolve(&m, &f);
        assert!(!st.pressed("jump") && !st.pressed("ui_accept"));
        assert_eq!(st.vec2("move"), Vec2::ZERO);
    }

    #[test]
    fn conflict_detection_across_contexts_and_globals() {
        let m = example();
        // Return only used by the menu; gameplay may reuse it.
        assert!(m.conflicts_for("jump", &Source::Key("Return".into())).is_empty());
        // Space is jump (gameplay); another gameplay action conflicts.
        let c = m.conflicts_for("fire", &Source::Key("Space".into()));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].action, "jump");
        assert_eq!(c[0].target, BindingTarget { slot: 0, part: None });
        // Escape is global: conflicts with everything.
        assert_eq!(m.conflicts_for("ui_accept", &Source::Key("Escape".into()))[0].action, "pause");
        // Composite members count.
        let c = m.conflicts_for("jump", &Source::Key("W".into()));
        assert_eq!(c[0].action, "move");
        assert_eq!(c[0].target.part, Some(Part::Up));
        // Axis halves: RightX+ clashes with look_x (Both); LeftX stick as well.
        assert_eq!(m.conflicts_for("jump", &Source::axis_for("RightX", 1.0)).len(), 1);
        assert_eq!(m.conflicts_for("jump", &Source::axis_for("LeftX", -1.0))[0].action, "move");
        assert!(m.all_conflicts().is_empty());
    }

    #[test]
    fn sign_overlap_rules() {
        let pos = Source::axis_for("LeftX", 1.0);
        let neg = Source::axis_for("LeftX", -1.0);
        assert!(!pos.overlaps(&neg));
        assert!(pos.overlaps(&pos.clone()));
        let both = Source::PadAxis { axis: "LeftX".into(), sign: Sign::Both, deadzone: 0.2 };
        assert!(both.overlaps(&neg) && both.overlaps(&pos));
        assert!(!pos.overlaps(&Source::axis_for("LeftY", 1.0)));
        // Triggers never go negative.
        assert_eq!(Source::axis_for("TriggerLeft", -0.9), Source::axis_for("TriggerLeft", 0.9));
    }

    #[test]
    fn rebind_reject_swap_allow() {
        let defaults = example();
        let jump = |m: &ActionMap| m.action("jump").unwrap().bindings.clone();
        let t0 = BindingTarget { slot: 0, part: None };

        // Reject: map untouched.
        let mut m = defaults.clone();
        let err = m.rebind("fire", BindingTarget { slot: 0, part: None }, Source::Key("Space".into()), ConflictPolicy::Reject).unwrap_err();
        assert!(matches!(&err, RebindError::Conflict(c) if c[0].action == "jump"));
        assert_eq!(m, defaults);
        assert!(err.to_string().contains("jump"));

        // Free input: plain rebind.
        let out = m.rebind("jump", t0, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        assert_eq!(out, RebindOutcome::Bound);
        assert_eq!(jump(&m)[0], Binding::Key("J".into()));

        // Swap: fire's Left mouse <-> jump's J.
        let out = m.rebind("fire", t0, Source::Key("J".into()), ConflictPolicy::Swap).unwrap();
        assert_eq!(out, RebindOutcome::Swapped { other: "jump".into() });
        assert_eq!(jump(&m)[0], Binding::Mouse("Left".into()));
        assert_eq!(m.action("fire").unwrap().bindings[0], Binding::Key("J".into()));
        assert!(m.all_conflicts().is_empty());

        // Allow: shared, reported by all_conflicts.
        let out = m.rebind("fire", t0, Source::Key("W".into()), ConflictPolicy::Allow).unwrap();
        assert_eq!(out, RebindOutcome::Shared { with: vec!["move".into()] });
        assert_eq!(m.all_conflicts().len(), 1);
        assert!(m.validate().iter().any(|w| w.contains("fire") && w.contains("move")));

        // Append a new binding with the + slot.
        let n = m.action("jump").unwrap().bindings.len();
        m.rebind("jump", BindingTarget { slot: n, part: None }, Source::Key("K".into()), ConflictPolicy::Reject).unwrap();
        assert_eq!(m.action("jump").unwrap().bindings.len(), n + 1);

        // Errors.
        assert_eq!(m.rebind("nope", t0, Source::Key("K".into()), ConflictPolicy::Reject), Err(RebindError::UnknownAction));
        assert_eq!(m.rebind("pause", t0, Source::Key("P".into()), ConflictPolicy::Reject), Err(RebindError::NotRebindable));
        assert_eq!(
            m.rebind("jump", BindingTarget { slot: 9, part: None }, Source::Key("P".into()), ConflictPolicy::Reject),
            Err(RebindError::BadTarget)
        );
        // Stick axis slot needs an axis.
        let stick = BindingTarget { slot: 2, part: Some(Part::StickX) };
        assert_eq!(m.rebind("move", stick, Source::Key("P".into()), ConflictPolicy::Reject), Err(RebindError::NeedsAxis));
        m.rebind("move", stick, Source::axis_for("RightX", 1.0), ConflictPolicy::Allow).unwrap();
        assert!(matches!(&m.action("move").unwrap().bindings[2], Binding::Stick { x, .. } if x == "RightX"));
    }

    #[test]
    fn rebind_composite_part_and_swap_back_into_part() {
        let mut m = example();
        // Rebind "up" of WASD to Key T (free).
        let up = BindingTarget { slot: 0, part: Some(Part::Up) };
        m.rebind("move", up, Source::Key("T".into()), ConflictPolicy::Reject).unwrap();
        let mut st = ActionState::new(&m);
        let mut f = Fake::default();
        f.press("T");
        st.resolve(&m, &f);
        assert_eq!(st.vec2("move"), Vec2::new(0.0, 1.0));
        // jump takes S (the "down" part): swap gives move.down the Space key.
        m.rebind("jump", BindingTarget { slot: 0, part: None }, Source::Key("S".into()), ConflictPolicy::Swap).unwrap();
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::Key("S".into()));
        assert!(matches!(&m.action("move").unwrap().bindings[0], Binding::Composite { down: Source::Key(k), .. } if k == "Space"));
    }

    #[test]
    fn rebind_state_poll_listens_for_next_press_and_cancel() {
        let mut m = example();
        let mut state = RebindState::new();
        let mut f = Fake::default();
        let t0 = BindingTarget { slot: 0, part: None };
        assert_eq!(state.poll(&mut m, &f), None, "not listening");

        state.start("jump", t0);
        assert_eq!(state.poll(&mut m, &f), None, "nothing pressed yet");
        f.capture = vec![Source::Key("X".into())];
        let ev = state.poll(&mut m, &f).unwrap();
        assert_eq!(ev, RebindEvent::Done(Ok(RebindOutcome::Bound)));
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::Key("X".into()));
        assert!(state.listening.is_none());

        // Conflict keeps listening and reports; a free key then succeeds.
        state.start("fire", t0);
        f.capture = vec![Source::Key("X".into())];
        let ev = state.poll(&mut m, &f).unwrap();
        assert!(matches!(ev, RebindEvent::Done(Err(RebindError::Conflict(_)))));
        assert!(state.listening.is_some());
        assert_eq!(state.last_conflicts[0].action, "jump");
        assert!(state.message.as_deref().unwrap().contains("jump"));
        f.capture = vec![Source::Key("Y".into())];
        assert!(matches!(state.poll(&mut m, &f), Some(RebindEvent::Done(Ok(_)))));

        // Escape cancels (and wins over other candidates).
        state.start("jump", t0);
        f.capture = vec![Source::Key("Q".into()), Source::Key("Escape".into())];
        assert_eq!(state.poll(&mut m, &f), Some(RebindEvent::Cancelled));
        assert!(state.listening.is_none());
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::Key("X".into()));
    }

    #[test]
    fn axis_capture_needs_rest_first() {
        let mut m = example();
        let mut state = RebindState::new();
        let f_stuck = Fake { capture: vec![Source::axis_for("RightY", 1.0)], ..Default::default() };
        state.start("jump", BindingTarget { slot: 0, part: None });
        // Axis already deflected when listening starts: ignored until it returns to rest.
        assert_eq!(state.poll(&mut m, &f_stuck), None);
        let rest = Fake::default();
        assert_eq!(state.poll(&mut m, &rest), None);
        assert!(matches!(state.poll(&mut m, &f_stuck), Some(RebindEvent::Done(Ok(_)))));
        assert!(matches!(&m.action("jump").unwrap().bindings[0], Binding::PadAxis { axis, sign: Sign::Positive, .. } if axis == "RightY"));
        // Buttons win over axes in the same frame.
        state.start("jump", BindingTarget { slot: 0, part: None });
        state.poll(&mut m, &rest);
        let both = Fake { capture: vec![Source::axis_for("RightY", 1.0), Source::PadButton("B".into())], ..Default::default() };
        state.poll(&mut m, &both);
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::PadButton("B".into()));
    }

    #[test]
    fn reset_action_and_all() {
        let defaults = example();
        let mut m = defaults.clone();
        let t0 = BindingTarget { slot: 0, part: None };
        m.rebind("jump", t0, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        m.rebind("fire", t0, Source::Key("K".into()), ConflictPolicy::Reject).unwrap();
        assert!(!m.is_default("jump", &defaults));
        assert!(m.reset_action("jump", &defaults));
        assert!(m.is_default("jump", &defaults) && !m.is_default("fire", &defaults));
        assert!(!m.reset_action("jump", &defaults), "already default");
        assert!(m.remove_binding("fire", 0));
        assert!(m.reset_all(&defaults));
        assert_eq!(m, defaults);
    }

    #[test]
    fn override_layering_roundtrip_through_temp_dir() {
        let dir = tmp("layer");
        let default_path = dir.join("input_map.ron");
        let overrides_path = dir.join("user").join("input_overrides.ron");
        std::fs::write(&default_path, EXAMPLE).unwrap();
        let defaults = ActionMap::load(&default_path).unwrap();

        // No overrides file yet: defaults, no warnings.
        let (m, w) = ActionMap::load_layered(&default_path, &overrides_path).unwrap();
        assert_eq!(m, defaults);
        assert!(w.is_empty(), "{w:?}");

        // Rebind two actions, save: only the diff lands on disk.
        let mut user = defaults.clone();
        let t0 = BindingTarget { slot: 0, part: None };
        user.rebind("jump", t0, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        user.rebind("move", BindingTarget { slot: 0, part: Some(Part::Up) }, Source::Key("T".into()), ConflictPolicy::Reject).unwrap();
        user.save_overrides(&defaults, &overrides_path).unwrap();
        let text = std::fs::read_to_string(&overrides_path).unwrap();
        assert!(text.contains("jump") && text.contains("move") && !text.contains("fire"), "{text}");
        assert_eq!(ActionMap::overrides_from_ron(&text).unwrap().actions.len(), 2);

        // Load layered: user's map reconstructed; the default file is untouched.
        let (loaded, w) = ActionMap::load_layered(&default_path, &overrides_path).unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(loaded, user);
        assert_eq!(std::fs::read_to_string(&default_path).unwrap(), EXAMPLE);

        // A default that changed in a new release still reaches un-overridden actions.
        let newer = EXAMPLE.replace(r#"Mouse("Left")"#, r#"Mouse("Right")"#);
        std::fs::write(&default_path, newer).unwrap();
        let (loaded, _) = ActionMap::load_layered(&default_path, &overrides_path).unwrap();
        assert_eq!(loaded.action("fire").unwrap().bindings[0], Binding::Mouse("Right".into()));
        assert_eq!(loaded.action("jump").unwrap().bindings[0], Binding::Key("J".into()));

        // Unknown action in overrides -> warning, rest still applied; corrupt -> defaults.
        std::fs::write(
            &overrides_path,
            r#"(actions: [(name: "gone", bindings: [Key("G")]), (name: "jump", bindings: [Key("H")]), (name: "pause", bindings: [Key("P")])])"#,
        )
        .unwrap();
        let (loaded, w) = ActionMap::load_layered(&default_path, &overrides_path).unwrap();
        assert_eq!(loaded.action("jump").unwrap().bindings, vec![Binding::Key("H".into())]);
        assert!(w.iter().any(|s| s.contains("gone")) && w.iter().any(|s| s.contains("pause")), "{w:?}");
        assert_eq!(loaded.action("pause").unwrap().bindings[0], Binding::Key("Escape".into()));
        std::fs::write(&overrides_path, "(this is not ron").unwrap();
        let (loaded, w) = ActionMap::load_layered(&default_path, &overrides_path).unwrap();
        assert_eq!(loaded.action("jump").unwrap().bindings[0], Binding::Key("Space".into()));
        assert_eq!(w.len(), 1);

        // Reset all then save removes the file.
        let mut m = loaded;
        let defaults = ActionMap::load(&default_path).unwrap();
        m.rebind("jump", t0, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        m.save_overrides(&defaults, &overrides_path).unwrap();
        assert!(overrides_path.exists());
        m.reset_all(&defaults);
        m.save_overrides(&defaults, &overrides_path).unwrap();
        assert!(!overrides_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_input_is_an_input_source() {
        use crate::headless::InputEvent as E;
        let m = example();
        let mut st = ActionState::new(&m);
        let mut h = HeadlessInput::new();
        h.apply(&E::Press("Space".into()));
        h.apply(&E::Press("PadA".into()));
        h.apply(&E::Press("MouseLeft".into()));
        h.apply(&E::Axis("LeftY".into(), -1.0));
        st.resolve(&m, &h);
        assert!(st.just_pressed("jump") && st.pressed("fire"));
        assert_eq!(st.vec2("move"), Vec2::new(0.0, 1.0));
        h.begin_frame();
        h.apply(&E::Release("Space".into()));
        st.resolve(&m, &h);
        assert!(st.pressed("jump"), "pad A still held");
        h.apply(&E::Release("PadA".into()));
        st.resolve(&m, &h);
        assert!(st.just_released("jump"));
    }

    #[test]
    fn device_names_roundtrip() {
        for (n, b) in PAD_BUTTONS {
            assert_eq!(pad_button(n), Some(*b));
            assert_eq!(pad_button_name(*b), *n);
        }
        for n in ["W", "Space", "Left Shift", "Return", "Up", "F5", "Escape"] {
            assert!(keycode(n).is_some(), "{n}");
            assert_eq!(keycode(n).unwrap().name(), n);
        }
        assert!(keycode("NotAKey").is_none());
        assert_eq!(mouse_button_name(mouse_button("X2").unwrap()), Some("X2"));
        assert!(pad_axis("TriggerLeft").is_some() && pad_axis("Nope").is_none());
        assert_eq!(egui_key_name(egui::Key::ArrowUp).as_deref(), Some("Up"));
        assert_eq!(egui_key_name(egui::Key::A).as_deref(), Some("A"));
        assert_eq!(egui_key_name(egui::Key::Num5).as_deref(), Some("5"));
        assert_eq!(egui_key_name(egui::Key::Enter).as_deref(), Some("Return"));
        assert_eq!(egui_key_name(egui::Key::F5).as_deref(), Some("F5"));
    }

    #[test]
    fn validate_reports_bad_names_and_contexts() {
        let m = ActionMap::from_ron(
            r#"(actions: [
                (name: "a", context: "ghost", bindings: [Key("Nope"), Mouse("Z"), PadButton("Q"), PadAxis(axis: "Foo", deadzone: 2.0)]),
                (name: "a"),
            ])"#,
        )
        .unwrap();
        let w = m.validate().join("\n");
        for needle in ["duplicate action 'a'", "undeclared context 'ghost'", "unknown key 'Nope'", "unknown mouse button 'Z'", "unknown pad button 'Q'", "unknown axis 'Foo'", "deadzone 2"] {
            assert!(w.contains(needle), "missing {needle:?} in\n{w}");
        }
    }

    // ---- egui panel (no GL / SDL)

    fn run_frame_with(ctx: &egui::Context, input: egui::RawInput, mut f: impl FnMut(&mut egui::Ui)) {
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui));
        });
    }

    #[test]
    fn panel_draws_on_bare_context_and_window_variant() {
        let defaults = example();
        let mut m = defaults.clone();
        let mut st = RebindState::new();
        let ctx = egui::Context::default();
        run_frame_with(&ctx, egui::RawInput::default(), |ui| {
            assert!(!draw_rebind_panel(ui, &mut m, &mut st, &defaults, None));
        });
        assert_eq!(m, defaults);

        // Listening state, with a conflict message and a rebound action (Reset enabled).
        m.rebind("jump", BindingTarget { slot: 0, part: None }, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        st.start("fire", BindingTarget { slot: 0, part: None });
        st.last_conflicts = vec![RebindConflict {
            action: "jump".into(),
            target: BindingTarget { slot: 0, part: None },
            source: Source::Key("J".into()),
        }];
        st.message = Some("conflict".into());
        run_frame_with(&ctx, egui::RawInput::default(), |ui| {
            draw_rebind_panel(ui, &mut m, &mut st, &defaults, None);
        });

        let mut open = true;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            show_rebind_window(ctx, &mut open, &mut m, &mut st, &defaults, Some(&Fake::default()));
        });
        // Empty map is fine too.
        let mut empty = ActionMap::default();
        run_frame_with(&ctx, egui::RawInput::default(), |ui| {
            draw_rebind_panel(ui, &mut empty, &mut RebindState::new(), &ActionMap::default(), None);
        });
    }

    #[test]
    fn panel_captures_egui_key_event_after_grace_frame() {
        let defaults = example();
        let mut m = defaults.clone();
        let mut st = RebindState::new();
        let ctx = egui::Context::default();
        st.start("jump", BindingTarget { slot: 0, part: None });
        let key_event = || egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::J,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        // First frame is the grace frame (the click that started listening): a key arriving
        // in it is ignored. (egui then flags the same still-held key as `repeat`, so the
        // capture frame uses a fresh context.)
        run_frame_with(&ctx, key_event(), |ui| {
            assert!(!draw_rebind_panel(ui, &mut m, &mut st, &defaults, None));
        });
        assert!(st.listening.is_some());
        let ctx = egui::Context::default();
        run_frame_with(&ctx, key_event(), |ui| {
            assert!(draw_rebind_panel(ui, &mut m, &mut st, &defaults, None));
        });
        assert!(st.listening.is_none());
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::Key("J".into()));

        // Escape via egui cancels.
        st.start("jump", BindingTarget { slot: 0, part: None });
        let esc = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        run_frame_with(&ctx, esc.clone(), |ui| {
            draw_rebind_panel(ui, &mut m, &mut st, &defaults, None);
        });
        run_frame_with(&egui::Context::default(), esc, |ui| {
            draw_rebind_panel(ui, &mut m, &mut st, &defaults, None);
        });
        assert!(st.listening.is_none());
        assert_eq!(m.action("jump").unwrap().bindings[0], Binding::Key("J".into()));
    }

    #[test]
    fn overrides_follow_map_order() {
        let defaults = example();
        let mut m = defaults.clone();
        let t0 = BindingTarget { slot: 0, part: None };
        m.rebind("fire", t0, Source::Key("K".into()), ConflictPolicy::Reject).unwrap();
        m.rebind("jump", t0, Source::Key("J".into()), ConflictPolicy::Reject).unwrap();
        let names: Vec<_> = m.diff(&defaults).actions.into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["jump", "fire"]);
    }
}
