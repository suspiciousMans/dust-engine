//! Animation state machine over skeletal clips, and the ECS glue to run it.
//!
//! * [`AnimatorController`] is the serializable *asset*: parameters, named
//!   states (each a single clip or a 1-D blend), and transitions with
//!   conditions, crossfade duration and optional exit time. Everything is
//!   referenced by name so the file stays hand-editable.
//! * [`SkeletalAnimator`] is the runtime *component*: it owns the playback
//!   state, current parameter values, and the resulting [`Pose`] and joint
//!   palette. [`SkeletalAnimator::update`] is a pure function of the state
//!   and `dt` — no clocks, no randomness, no hash-map iteration — so a
//!   fixed `dt` sequence always yields bit-identical output.
//! * [`skeletal_animation_system`] is the [`Schedule`](crate::schedule::Schedule)
//!   system that updates every animator and applies root motion.
//!
//! Semantics worth knowing:
//!
//! * Each state keeps a normalized `phase` (0..1 of its motion). Looping
//!   states wrap it; non-looping states clamp at 1. A transition's
//!   `exit_time` is compared against that phase (so for a looping state it
//!   is a point within the loop).
//! * Transitions are checked once per update, only when no crossfade is
//!   running (crossfades are not interruptible). Candidates are tried in
//!   declaration order; the first whose source matches, exit time has
//!   passed and conditions all hold wins. A transition with `from: None`
//!   ("any state") never targets the state already playing. Triggers named
//!   by the winning transition's conditions are consumed.
//! * During a crossfade both states keep advancing; the pose is a linear
//!   blend with weight `elapsed / duration`. A `duration` of 0 switches
//!   instantly.

use std::sync::Arc;

use glam::Mat4;
use serde::{Deserialize, Serialize};

use super::clip::{AnimationClip, RootMotionDelta};
use super::pose::Pose;
use super::skeleton::Skeleton;
use crate::ecs::Transform;
use crate::schedule::SystemCtx;

/// A skeleton and the clips authored for it.
#[derive(Clone, Debug)]
pub struct AnimationSet {
    pub skeleton: Skeleton,
    pub clips: Vec<AnimationClip>,
}

impl AnimationSet {
    pub fn clip_index(&self, name: &str) -> Option<usize> {
        self.clips.iter().position(|c| c.name == name)
    }
}

// --- controller asset -------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    Float(f32),
    Bool(bool),
    /// A one-shot flag: set it, and the first transition whose conditions
    /// include `Compare::Triggered` on it consumes it.
    Trigger(bool),
}

impl ParamValue {
    fn same_kind(&self, other: &ParamValue) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamDef {
    pub name: String,
    pub default: ParamValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlendPoint {
    pub threshold: f32,
    pub clip: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Motion {
    Clip { clip: String },
    /// Blends the two clips whose thresholds bracket the float parameter;
    /// clamps to the first/last clip outside the range. Clips are sampled at
    /// the same normalized phase.
    Blend1D { parameter: String, points: Vec<BlendPoint> },
}

fn one() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateDef {
    pub name: String,
    pub motion: Motion,
    /// Playback speed multiplier for this state.
    #[serde(default = "one")]
    pub speed: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Compare {
    /// Float parameter `>` value.
    Greater(f32),
    /// Float parameter `<` value.
    Less(f32),
    /// Bool parameter is true.
    IsTrue,
    /// Bool parameter is false.
    IsFalse,
    /// Trigger parameter is set.
    Triggered,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub parameter: String,
    pub compare: Compare,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionDef {
    /// Source state; `None` = from any state.
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    /// All must hold (an empty list always holds).
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Crossfade seconds (`0` = instant switch).
    #[serde(default)]
    pub duration: f32,
    /// The transition may only fire once the source state's normalized
    /// phase has reached this value (`1.0` = at the end of a clip).
    #[serde(default)]
    pub exit_time: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnimatorController {
    pub name: String,
    #[serde(default)]
    pub parameters: Vec<ParamDef>,
    pub states: Vec<StateDef>,
    #[serde(default)]
    pub transitions: Vec<TransitionDef>,
    /// State entered on creation; the first state when empty.
    #[serde(default)]
    pub default_state: String,
}

impl AnimatorController {
    /// One looping-or-not clip, no transitions.
    pub fn single_clip(clip: &str) -> Self {
        Self {
            name: clip.to_string(),
            parameters: vec![],
            states: vec![StateDef { name: clip.to_string(), motion: Motion::Clip { clip: clip.to_string() }, speed: 1.0 }],
            transitions: vec![],
            default_state: clip.to_string(),
        }
    }

    pub fn load_from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(ron::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        std::fs::write(path, ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())?)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AnimatorError {
    #[error("controller has no states")]
    NoStates,
    #[error("state '{state}' uses unknown clip '{clip}'")]
    UnknownClip { state: String, clip: String },
    #[error("unknown state '{0}'")]
    UnknownState(String),
    #[error("unknown parameter '{0}'")]
    UnknownParameter(String),
    #[error("parameter '{0}' is declared twice")]
    DuplicateParameter(String),
    #[error("parameter '{parameter}' has the wrong type for its use in {context}")]
    ParameterType { parameter: String, context: String },
    #[error("state '{0}' has a blend with no points")]
    EmptyBlend(String),
}

// --- compiled (index-based) form -------------------------------------------

#[derive(Clone, Debug)]
enum CMotion {
    Clip(usize),
    /// `points` sorted by threshold: `(threshold, clip index)`.
    Blend1D { param: usize, points: Vec<(f32, usize)> },
}

#[derive(Clone, Debug)]
struct CState {
    name: String,
    motion: CMotion,
    speed: f32,
}

#[derive(Clone, Debug)]
struct CTransition {
    from: Option<usize>,
    to: usize,
    conditions: Vec<(usize, Compare)>,
    duration: f32,
    exit_time: Option<f32>,
}

#[derive(Clone, Debug)]
struct Compiled {
    param_names: Vec<String>,
    param_defaults: Vec<ParamValue>,
    states: Vec<CState>,
    transitions: Vec<CTransition>,
    default_state: usize,
}

impl Compiled {
    fn build(set: &AnimationSet, c: &AnimatorController) -> Result<Self, AnimatorError> {
        if c.states.is_empty() {
            return Err(AnimatorError::NoStates);
        }
        let mut param_names: Vec<String> = Vec::new();
        for p in &c.parameters {
            if param_names.contains(&p.name) {
                return Err(AnimatorError::DuplicateParameter(p.name.clone()));
            }
            param_names.push(p.name.clone());
        }
        let param = |name: &str| {
            param_names.iter().position(|n| n == name).ok_or_else(|| AnimatorError::UnknownParameter(name.to_string()))
        };
        let clip = |state: &str, name: &str| {
            set.clip_index(name).ok_or_else(|| AnimatorError::UnknownClip { state: state.to_string(), clip: name.to_string() })
        };

        let mut states = Vec::new();
        for s in &c.states {
            let motion = match &s.motion {
                Motion::Clip { clip: name } => CMotion::Clip(clip(&s.name, name)?),
                Motion::Blend1D { parameter, points } => {
                    if points.is_empty() {
                        return Err(AnimatorError::EmptyBlend(s.name.clone()));
                    }
                    let pi = param(parameter)?;
                    if !matches!(c.parameters[pi].default, ParamValue::Float(_)) {
                        return Err(AnimatorError::ParameterType {
                            parameter: parameter.clone(),
                            context: format!("blend in state '{}'", s.name),
                        });
                    }
                    let mut pts = Vec::new();
                    for p in points {
                        pts.push((p.threshold, clip(&s.name, &p.clip)?));
                    }
                    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
                    CMotion::Blend1D { param: pi, points: pts }
                }
            };
            states.push(CState { name: s.name.clone(), motion, speed: s.speed });
        }
        let state = |name: &str| {
            states.iter().position(|s| s.name == name).ok_or_else(|| AnimatorError::UnknownState(name.to_string()))
        };

        let mut transitions = Vec::new();
        for t in &c.transitions {
            let mut conditions = Vec::new();
            for cond in &t.conditions {
                let pi = param(&cond.parameter)?;
                let ok = matches!(
                    (&c.parameters[pi].default, cond.compare),
                    (ParamValue::Float(_), Compare::Greater(_) | Compare::Less(_))
                        | (ParamValue::Bool(_), Compare::IsTrue | Compare::IsFalse)
                        | (ParamValue::Trigger(_), Compare::Triggered)
                );
                if !ok {
                    return Err(AnimatorError::ParameterType {
                        parameter: cond.parameter.clone(),
                        context: format!("a condition of the transition to '{}'", t.to),
                    });
                }
                conditions.push((pi, cond.compare));
            }
            transitions.push(CTransition {
                from: t.from.as_deref().map(&state).transpose()?,
                to: state(&t.to)?,
                conditions,
                duration: t.duration.max(0.0),
                exit_time: t.exit_time,
            });
        }
        let default_state = if c.default_state.is_empty() { 0 } else { state(&c.default_state)? };
        Ok(Self {
            param_names,
            param_defaults: c.parameters.iter().map(|p| p.default).collect(),
            states,
            transitions,
            default_state,
        })
    }
}

// --- runtime -----------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Playback {
    state: usize,
    /// Normalized position within the state's motion.
    phase: f32,
}

#[derive(Clone, Copy, Debug)]
struct ActiveTransition {
    to: Playback,
    elapsed: f32,
    duration: f32,
}

/// ECS component: a skeleton + clips + controller playing on one entity.
#[derive(Clone)]
pub struct SkeletalAnimator {
    set: Arc<AnimationSet>,
    controller: Arc<AnimatorController>,
    compiled: Compiled,
    params: Vec<ParamValue>,
    current: Playback,
    transition: Option<ActiveTransition>,
    pose: Pose,
    palette: Vec<Mat4>,
    root_delta: RootMotionDelta,
    /// Global speed multiplier on top of each state's own `speed`
    /// (negative values are treated as 0).
    pub speed: f32,
    /// When false, `update` does nothing (pose and palette freeze).
    pub playing: bool,
    /// Remove each clip's configured root motion from the pose and report it
    /// via [`SkeletalAnimator::root_motion_delta`]. When false clips play
    /// raw (the mesh moves with the root joint) and the delta is zero.
    pub extract_root_motion: bool,
    /// Whether [`skeletal_animation_system`] applies the extracted delta to
    /// the entity's `Transform`.
    pub apply_root_motion: bool,
}

impl SkeletalAnimator {
    pub fn new(set: Arc<AnimationSet>, controller: Arc<AnimatorController>) -> Result<Self, AnimatorError> {
        let compiled = Compiled::build(&set, &controller)?;
        let mut a = Self {
            params: compiled.param_defaults.clone(),
            current: Playback { state: compiled.default_state, phase: 0.0 },
            transition: None,
            pose: Pose::default(),
            palette: Vec::new(),
            root_delta: RootMotionDelta::ZERO,
            speed: 1.0,
            playing: true,
            extract_root_motion: true,
            apply_root_motion: true,
            set,
            controller,
            compiled,
        };
        a.rebuild_output();
        Ok(a)
    }

    pub fn set(&self) -> &Arc<AnimationSet> {
        &self.set
    }

    pub fn controller(&self) -> &Arc<AnimatorController> {
        &self.controller
    }

    // -- parameters --

    fn param_slot(&self, name: &str) -> Option<usize> {
        self.compiled.param_names.iter().position(|n| n == name)
    }

    /// Returns false if `name` is not a float parameter.
    pub fn set_float(&mut self, name: &str, value: f32) -> bool {
        self.set_param(name, ParamValue::Float(value))
    }

    /// Returns false if `name` is not a bool parameter.
    pub fn set_bool(&mut self, name: &str, value: bool) -> bool {
        self.set_param(name, ParamValue::Bool(value))
    }

    /// Raises a trigger; false if `name` is not a trigger parameter.
    pub fn set_trigger(&mut self, name: &str) -> bool {
        self.set_param(name, ParamValue::Trigger(true))
    }

    pub fn reset_trigger(&mut self, name: &str) -> bool {
        self.set_param(name, ParamValue::Trigger(false))
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> bool {
        match self.param_slot(name) {
            Some(i) if self.params[i].same_kind(&v) => {
                self.params[i] = v;
                true
            }
            _ => false,
        }
    }

    pub fn float(&self, name: &str) -> Option<f32> {
        match self.params.get(self.param_slot(name)?) {
            Some(ParamValue::Float(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.params.get(self.param_slot(name)?) {
            Some(ParamValue::Bool(v)) => Some(*v),
            _ => None,
        }
    }

    /// Whether the trigger is currently raised.
    pub fn trigger_pending(&self, name: &str) -> Option<bool> {
        match self.params.get(self.param_slot(name)?) {
            Some(ParamValue::Trigger(v)) => Some(*v),
            _ => None,
        }
    }

    // -- state queries / control --

    /// Name of the state currently playing (the *source* during a crossfade).
    pub fn current_state(&self) -> &str {
        &self.compiled.states[self.current.state].name
    }

    /// Name of the state being crossfaded to, if a transition is running.
    pub fn next_state(&self) -> Option<&str> {
        self.transition.as_ref().map(|t| self.compiled.states[t.to.state].name.as_str())
    }

    pub fn is_transitioning(&self) -> bool {
        self.transition.is_some()
    }

    /// Crossfade progress `0..=1`, or `None` outside a transition.
    pub fn transition_weight(&self) -> Option<f32> {
        self.transition.map(|t| fade_weight(t.elapsed, t.duration))
    }

    /// Normalized phase of the current state's motion.
    pub fn current_phase(&self) -> f32 {
        self.current.phase
    }

    /// Jumps straight to `state` at phase 0, cancelling any crossfade.
    pub fn play(&mut self, state: &str) -> Result<(), AnimatorError> {
        let s = self.state_index(state)?;
        self.current = Playback { state: s, phase: 0.0 };
        self.transition = None;
        self.rebuild_output();
        Ok(())
    }

    /// Starts a crossfade to `state` regardless of the transition table
    /// (replacing any crossfade already running).
    pub fn crossfade_to(&mut self, state: &str, duration: f32) -> Result<(), AnimatorError> {
        let s = self.state_index(state)?;
        if let Some(t) = self.transition.take() {
            self.current = t.to; // snap to the in-flight target to avoid a pop-back
        }
        if duration <= 0.0 {
            self.current = Playback { state: s, phase: 0.0 };
        } else {
            self.transition = Some(ActiveTransition { to: Playback { state: s, phase: 0.0 }, elapsed: 0.0, duration });
        }
        self.rebuild_output();
        Ok(())
    }

    fn state_index(&self, name: &str) -> Result<usize, AnimatorError> {
        self.compiled.states.iter().position(|s| s.name == name).ok_or_else(|| AnimatorError::UnknownState(name.to_string()))
    }

    // -- outputs --

    /// Current local pose.
    pub fn pose(&self) -> &Pose {
        &self.pose
    }

    /// Skinning palette (`world * inverse_bind`) for the current pose.
    pub fn palette(&self) -> &[Mat4] {
        &self.palette
    }

    /// Root motion extracted during the last `update` (parent space of the
    /// root joint). Zero unless a played clip configures it.
    pub fn root_motion_delta(&self) -> RootMotionDelta {
        self.root_delta
    }

    // -- update --

    /// Advances the animator by `dt` seconds (times `speed`): moves playback,
    /// advances any crossfade, evaluates transitions, then rebuilds pose,
    /// palette and root-motion delta.
    pub fn update(&mut self, dt: f32) {
        if !self.playing {
            return;
        }
        let dt = if dt.is_finite() { dt.max(0.0) * self.speed.max(0.0) } else { 0.0 };
        let delta;

        let (cur_delta, cur) = self.advance(self.current, dt);
        self.current = cur;
        if let Some(mut tr) = self.transition {
            let (to_delta, to) = self.advance(tr.to, dt);
            tr.to = to;
            tr.elapsed += dt;
            let w = fade_weight(tr.elapsed, tr.duration);
            delta = cur_delta.blend(&to_delta, w);
            if tr.elapsed >= tr.duration {
                self.current = tr.to;
                self.transition = None;
            } else {
                self.transition = Some(tr);
            }
        } else {
            delta = cur_delta;
            if let Some(i) = self.find_transition() {
                self.fire_transition(i);
            }
        }
        self.root_delta = delta;
        self.rebuild_output();
    }

    fn check(&self, param: usize, cmp: Compare) -> bool {
        match (self.params[param], cmp) {
            (ParamValue::Float(v), Compare::Greater(x)) => v > x,
            (ParamValue::Float(v), Compare::Less(x)) => v < x,
            (ParamValue::Bool(v), Compare::IsTrue) => v,
            (ParamValue::Bool(v), Compare::IsFalse) => !v,
            (ParamValue::Trigger(v), Compare::Triggered) => v,
            _ => false,
        }
    }

    fn find_transition(&self) -> Option<usize> {
        let cur = self.current.state;
        self.compiled.transitions.iter().position(|t| {
            match t.from {
                Some(f) if f != cur => return false,
                None if t.to == cur => return false,
                _ => {}
            }
            if t.exit_time.is_some_and(|x| self.current.phase < x) {
                return false;
            }
            t.conditions.iter().all(|&(p, c)| self.check(p, c))
        })
    }

    fn fire_transition(&mut self, index: usize) {
        let t = self.compiled.transitions[index].clone();
        for (p, cmp) in &t.conditions {
            if matches!(cmp, Compare::Triggered) {
                self.params[*p] = ParamValue::Trigger(false);
            }
        }
        let to = Playback { state: t.to, phase: 0.0 };
        if t.duration <= 0.0 {
            self.current = to;
        } else {
            self.transition = Some(ActiveTransition { to, elapsed: 0.0, duration: t.duration });
        }
    }

    /// Moves one playback forward `dt`, returning the root motion covered and
    /// the new playback.
    fn advance(&self, mut pb: Playback, dt: f32) -> (RootMotionDelta, Playback) {
        let state = &self.compiled.states[pb.state];
        let (dur, looping) = self.motion_timing(&state.motion);
        let new_phase = if dur > 0.0 { pb.phase + dt * state.speed.max(0.0) / dur } else { 1.0 };
        let delta = if self.extract_root_motion {
            self.motion_root_delta(&state.motion, pb.phase, new_phase)
        } else {
            RootMotionDelta::ZERO
        };
        pb.phase = if looping && dur > 0.0 { new_phase.rem_euclid(1.0) } else { new_phase.min(1.0) };
        (delta, pb)
    }

    /// `(duration, looping)` of a motion at the current parameter values.
    fn motion_timing(&self, m: &CMotion) -> (f32, bool) {
        match m {
            CMotion::Clip(i) => (self.set.clips[*i].duration, self.set.clips[*i].looping),
            CMotion::Blend1D { param, points } => {
                let (a, b, w) = blend_pair(points, self.param_float(*param));
                let (ca, cb) = (&self.set.clips[a], &self.set.clips[b]);
                (ca.duration + (cb.duration - ca.duration) * w, points.iter().all(|p| self.set.clips[p.1].looping))
            }
        }
    }

    fn param_float(&self, p: usize) -> f32 {
        match self.params[p] {
            ParamValue::Float(v) => v,
            _ => 0.0,
        }
    }

    fn motion_root_delta(&self, m: &CMotion, p0: f32, p1: f32) -> RootMotionDelta {
        let skel = &self.set.skeleton;
        let clip_delta = |i: usize| {
            let c = &self.set.clips[i];
            c.root_motion_delta(skel, p0 * c.duration, p1 * c.duration)
        };
        match m {
            CMotion::Clip(i) => clip_delta(*i),
            CMotion::Blend1D { param, points } => {
                let (a, b, w) = blend_pair(points, self.param_float(*param));
                if a == b {
                    clip_delta(a)
                } else {
                    clip_delta(a).blend(&clip_delta(b), w)
                }
            }
        }
    }

    fn motion_pose(&self, m: &CMotion, phase: f32) -> Pose {
        let skel = &self.set.skeleton;
        let sample = |i: usize| {
            let c = &self.set.clips[i];
            if self.extract_root_motion {
                c.sample_without_root_motion(skel, phase * c.duration)
            } else {
                c.sample(skel, phase * c.duration)
            }
        };
        match m {
            CMotion::Clip(i) => sample(*i),
            CMotion::Blend1D { param, points } => {
                let (a, b, w) = blend_pair(points, self.param_float(*param));
                if a == b {
                    sample(a)
                } else {
                    sample(a).blend(&sample(b), w)
                }
            }
        }
    }

    fn rebuild_output(&mut self) {
        let cur = self.motion_pose(&self.compiled.states[self.current.state].motion, self.current.phase);
        self.pose = match self.transition {
            Some(t) => {
                let to = self.motion_pose(&self.compiled.states[t.to.state].motion, t.to.phase);
                cur.blend(&to, fade_weight(t.elapsed, t.duration))
            }
            None => cur,
        };
        let skel = &self.set.skeleton;
        skel.joint_palette_into(&self.pose, &mut self.palette);
    }
}

fn fade_weight(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        1.0
    } else {
        (elapsed / duration).clamp(0.0, 1.0)
    }
}

/// For a sorted blend list and parameter value: the two clips to mix and the
/// weight of the second (`a == b`, `w == 0` outside the range).
fn blend_pair(points: &[(f32, usize)], x: f32) -> (usize, usize, f32) {
    let first = points[0];
    let last = points[points.len() - 1];
    if x <= first.0 || points.len() == 1 {
        return (first.1, first.1, 0.0);
    }
    if x >= last.0 {
        return (last.1, last.1, 0.0);
    }
    let hi = points.partition_point(|p| p.0 <= x);
    let (lo_p, hi_p) = (points[hi - 1], points[hi]);
    let span = hi_p.0 - lo_p.0;
    let w = if span > 0.0 { (x - lo_p.0) / span } else { 0.0 };
    (lo_p.1, hi_p.1, w)
}

/// Name [`skeletal_animation_system`] should be registered under.
pub const SKELETAL_ANIMATION_SYSTEM: &str = "skeletal_animation";

/// Schedule system: updates every [`SkeletalAnimator`] by `ctx.dt` and, for
/// those with `apply_root_motion`, moves the entity's [`Transform`] by the
/// extracted root motion (translation rotated and scaled into world space,
/// rotation composed on the right).
///
/// ```ignore
/// schedule.add_system(SKELETAL_ANIMATION_SYSTEM, skeletal_animation_system);
/// ```
pub fn skeletal_animation_system(ctx: &mut SystemCtx<'_>) {
    let dt = ctx.dt;
    for (_entity, (anim, transform)) in ctx.world.query_mut::<(&mut SkeletalAnimator, Option<&mut Transform>)>() {
        anim.update(dt);
        if !anim.apply_root_motion {
            continue;
        }
        if let Some(t) = transform {
            let d = anim.root_motion_delta();
            t.position += t.rotation * (d.translation * t.scale);
            t.rotation = (t.rotation * d.rotation).normalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::World;
    use crate::resources::Resources;
    use crate::schedule::Schedule;
    use crate::skeletal::clip::{Interpolation, JointChannel, RootMotion, Track};
    use crate::skeletal::pose::JointTransform;
    use crate::skeletal::skeleton::{Joint, Skeleton};
    use glam::{Quat, Vec3};

    fn skel() -> Skeleton {
        Skeleton::new(vec![
            Joint { name: "root".into(), parent: None, bind: JointTransform::IDENTITY },
            Joint { name: "tip".into(), parent: Some(0), bind: JointTransform::from_translation(Vec3::Y) },
        ])
        .unwrap()
    }

    /// Clip that holds joint 1's translation.x at a constant `x`, so the pose
    /// of a state is one number and blends are exact lerps.
    fn constant_clip(name: &str, x: f32, duration: f32, looping: bool) -> AnimationClip {
        let mut c = AnimationClip::new(name, duration, looping);
        let mut ch = JointChannel::new(1);
        ch.translation = Some(
            Track::new(
                vec![0.0, duration],
                vec![Vec3::new(x, 1.0, 0.0), Vec3::new(x, 1.0, 0.0)],
                Interpolation::Linear,
            )
            .unwrap(),
        );
        c.channels.push(ch);
        c
    }

    fn set() -> Arc<AnimationSet> {
        Arc::new(AnimationSet {
            skeleton: skel(),
            clips: vec![
                constant_clip("idle", 1.0, 1.0, true),
                constant_clip("walk", 5.0, 1.0, true),
                constant_clip("jump", 9.0, 1.0, false),
            ],
        })
    }

    fn state(name: &str, clip: &str) -> StateDef {
        StateDef { name: name.into(), motion: Motion::Clip { clip: clip.into() }, speed: 1.0 }
    }

    fn controller() -> AnimatorController {
        AnimatorController {
            name: "test".into(),
            parameters: vec![
                ParamDef { name: "speed".into(), default: ParamValue::Float(0.0) },
                ParamDef { name: "jump".into(), default: ParamValue::Trigger(false) },
                ParamDef { name: "grounded".into(), default: ParamValue::Bool(true) },
            ],
            states: vec![state("idle", "idle"), state("walk", "walk"), state("jump", "jump")],
            transitions: vec![
                TransitionDef {
                    from: None,
                    to: "jump".into(),
                    conditions: vec![Condition { parameter: "jump".into(), compare: Compare::Triggered }],
                    duration: 0.25,
                    exit_time: None,
                },
                TransitionDef {
                    from: Some("idle".into()),
                    to: "walk".into(),
                    conditions: vec![Condition { parameter: "speed".into(), compare: Compare::Greater(0.1) }],
                    duration: 0.5,
                    exit_time: None,
                },
                TransitionDef {
                    from: Some("walk".into()),
                    to: "idle".into(),
                    conditions: vec![Condition { parameter: "speed".into(), compare: Compare::Less(0.1) }],
                    duration: 0.5,
                    exit_time: None,
                },
                TransitionDef {
                    from: Some("jump".into()),
                    to: "idle".into(),
                    conditions: vec![],
                    duration: 0.5,
                    exit_time: Some(1.0),
                },
            ],
            default_state: "idle".into(),
        }
    }

    fn animator() -> SkeletalAnimator {
        SkeletalAnimator::new(set(), Arc::new(controller())).unwrap()
    }

    fn x(a: &SkeletalAnimator) -> f32 {
        a.pose().joints[1].translation.x
    }

    #[test]
    fn starts_in_default_state_with_valid_palette() {
        let a = animator();
        assert_eq!(a.current_state(), "idle");
        assert_eq!(x(&a), 1.0);
        assert_eq!(a.palette().len(), 2);
        // idle pose == bind in Y, x offset 1 => palette[1] translates by (1,0,0)
        assert!((a.palette()[1].transform_point3(Vec3::new(0.0, 1.0, 0.0)) - Vec3::new(1.0, 1.0, 0.0)).length() < 1e-5);
    }

    #[test]
    fn condition_driven_transition_crossfades_over_fixed_dt() {
        let mut a = animator();
        a.update(0.125);
        a.update(0.125);
        assert_eq!(a.current_state(), "idle");
        assert!(!a.is_transitioning());

        a.set_float("speed", 1.0);
        a.update(0.125); // fires; fade starts at weight 0
        assert!(a.is_transitioning());
        assert_eq!(a.next_state(), Some("walk"));
        assert_eq!(a.transition_weight(), Some(0.0));
        assert_eq!(x(&a), 1.0);

        // crossfade 0.5s at dt 0.125: weights 0.25, 0.5, 0.75, then done.
        // idle=1, walk=5 => 2, 3, 4, then 5.
        a.update(0.125);
        assert_eq!(a.transition_weight(), Some(0.25));
        assert_eq!(x(&a), 2.0);
        a.update(0.125);
        assert_eq!(x(&a), 3.0);
        a.update(0.125);
        assert_eq!(x(&a), 4.0);
        a.update(0.125);
        assert!(!a.is_transitioning());
        assert_eq!(a.current_state(), "walk");
        assert_eq!(x(&a), 5.0);

        // and back
        a.set_float("speed", 0.0);
        a.update(0.125);
        assert_eq!(a.next_state(), Some("idle"));
    }

    #[test]
    fn same_inputs_give_bit_identical_output() {
        let run = || {
            let mut a = animator();
            let mut trace = Vec::new();
            for step in 0..64 {
                if step == 5 {
                    a.set_float("speed", 0.8);
                }
                if step == 20 {
                    a.set_trigger("jump");
                }
                if step == 40 {
                    a.set_float("speed", 0.0);
                }
                a.update(1.0 / 60.0);
                trace.push((
                    a.current_state().to_string(),
                    a.next_state().map(str::to_string),
                    a.pose().clone(),
                    a.palette().to_vec(),
                ));
            }
            trace
        };
        let (t1, t2) = (run(), run());
        assert_eq!(t1, t2);
        // sanity: the run actually went through several states
        let names: std::collections::BTreeSet<_> = t1.iter().map(|t| t.0.clone()).collect();
        assert!(names.len() >= 3, "{names:?}");
    }

    #[test]
    fn trigger_is_consumed_and_exit_time_gates_transition() {
        let mut a = animator();
        a.set_trigger("jump");
        assert_eq!(a.trigger_pending("jump"), Some(true));
        a.update(0.25); // any-state -> jump fires (0.25s fade)
        assert_eq!(a.next_state(), Some("jump"));
        assert_eq!(a.trigger_pending("jump"), Some(false));
        a.update(0.25); // fade done: now in jump at phase 0.25
        assert_eq!(a.current_state(), "jump");
        assert_eq!(a.current_phase(), 0.25);
        assert_eq!(x(&a), 9.0);
        // jump -> idle needs phase >= 1.0 (no conditions)
        a.update(0.25);
        a.update(0.25);
        assert_eq!(a.current_state(), "jump");
        assert!(!a.is_transitioning());
        a.update(0.25); // phase hits 1.0 -> fade to idle starts
        assert_eq!(a.current_phase(), 1.0);
        assert_eq!(a.next_state(), Some("idle"));
        a.update(0.25);
        assert_eq!(x(&a), 5.0); // halfway between jump 9 and idle 1
        a.update(0.25);
        assert_eq!(a.current_state(), "idle");
        assert_eq!(x(&a), 1.0);
    }

    #[test]
    fn trigger_stays_pending_until_a_transition_uses_it_and_any_state_skips_self() {
        let mut a = animator();
        a.set_trigger("jump");
        a.update(0.0); // dt 0 still evaluates transitions
        assert_eq!(a.next_state(), Some("jump"));
        a.update(0.5);
        assert_eq!(a.current_state(), "jump");
        a.set_trigger("jump"); // any-state->jump must not retarget the state already playing
        a.update(0.1);
        assert!(!a.is_transitioning());
        assert_eq!(a.trigger_pending("jump"), Some(true));
    }

    #[test]
    fn instant_transition_and_bool_conditions() {
        let mut c = controller();
        c.transitions = vec![TransitionDef {
            from: Some("idle".into()),
            to: "walk".into(),
            conditions: vec![Condition { parameter: "grounded".into(), compare: Compare::IsFalse }],
            duration: 0.0,
            exit_time: None,
        }];
        let mut a = SkeletalAnimator::new(set(), Arc::new(c)).unwrap();
        a.update(0.1);
        assert_eq!(a.current_state(), "idle");
        a.set_bool("grounded", false);
        a.update(0.1);
        assert_eq!(a.current_state(), "walk");
        assert!(!a.is_transitioning());
        assert_eq!(x(&a), 5.0);
        assert_eq!(a.current_phase(), 0.0);
    }

    #[test]
    fn parameter_api_type_checks() {
        let mut a = animator();
        assert!(a.set_float("speed", 2.0));
        assert_eq!(a.float("speed"), Some(2.0));
        assert!(!a.set_bool("speed", true));
        assert!(!a.set_float("nope", 1.0));
        assert!(!a.set_trigger("grounded"));
        assert_eq!(a.bool("grounded"), Some(true));
        assert_eq!(a.float("grounded"), None);
        assert!(a.reset_trigger("jump"));
    }

    #[test]
    fn compile_errors() {
        let s = set();
        let mut c = controller();
        c.states[0].motion = Motion::Clip { clip: "missing".into() };
        assert!(matches!(SkeletalAnimator::new(s.clone(), Arc::new(c)).err(), Some(AnimatorError::UnknownClip { .. })));
        let mut c = controller();
        c.transitions[0].to = "nowhere".into();
        assert_eq!(
            SkeletalAnimator::new(s.clone(), Arc::new(c)).err(),
            Some(AnimatorError::UnknownState("nowhere".into()))
        );
        let mut c = controller();
        c.transitions[1].conditions[0].parameter = "grounded".into(); // Greater on a bool
        assert!(matches!(SkeletalAnimator::new(s.clone(), Arc::new(c)).err(), Some(AnimatorError::ParameterType { .. })));
        let mut c = controller();
        c.transitions[1].conditions[0].parameter = "ghost".into();
        assert_eq!(SkeletalAnimator::new(s.clone(), Arc::new(c)).err(), Some(AnimatorError::UnknownParameter("ghost".into())));
        let mut c = controller();
        c.states.clear();
        assert_eq!(SkeletalAnimator::new(s.clone(), Arc::new(c)).err(), Some(AnimatorError::NoStates));
        let mut c = controller();
        c.parameters.push(c.parameters[0].clone());
        assert!(matches!(SkeletalAnimator::new(s, Arc::new(c)).err(), Some(AnimatorError::DuplicateParameter(_))));
    }

    #[test]
    fn blend1d_state_mixes_by_parameter() {
        let mut c = controller();
        c.transitions.clear();
        c.states = vec![StateDef {
            name: "locomotion".into(),
            motion: Motion::Blend1D {
                parameter: "speed".into(),
                // deliberately unsorted: compile sorts them
                points: vec![
                    BlendPoint { threshold: 2.0, clip: "walk".into() },
                    BlendPoint { threshold: 0.0, clip: "idle".into() },
                ],
            },
            speed: 1.0,
        }];
        c.default_state = String::new();
        let mut a = SkeletalAnimator::new(set(), Arc::new(c)).unwrap();
        assert_eq!(x(&a), 1.0); // speed 0 -> idle
        a.set_float("speed", 0.5); // 25% of the way idle(1) -> walk(5)
        a.update(0.1);
        assert!((x(&a) - 2.0).abs() < 1e-5);
        a.set_float("speed", 1.0);
        a.update(0.1);
        assert!((x(&a) - 3.0).abs() < 1e-5);
        a.set_float("speed", 99.0); // clamps to the last clip
        a.update(0.1);
        assert_eq!(x(&a), 5.0);
        a.set_float("speed", -4.0);
        a.update(0.1);
        assert_eq!(x(&a), 1.0);
    }

    #[test]
    fn phase_wraps_for_looping_and_clamps_otherwise() {
        let mut a = animator();
        a.update(0.75);
        a.update(0.75);
        assert!((a.current_phase() - 0.5).abs() < 1e-6);
        a.play("jump").unwrap();
        a.update(5.0);
        assert_eq!(a.current_phase(), 1.0);
    }

    #[test]
    fn global_speed_and_pause() {
        let mut a = animator();
        a.speed = 2.0;
        a.update(0.25);
        assert_eq!(a.current_phase(), 0.5);
        a.playing = false;
        a.update(0.25);
        assert_eq!(a.current_phase(), 0.5);
        a.playing = true;
        a.speed = -1.0;
        a.update(0.25);
        assert_eq!(a.current_phase(), 0.5);
    }

    #[test]
    fn manual_crossfade_and_play() {
        let mut a = animator();
        a.crossfade_to("walk", 1.0).unwrap();
        a.update(0.5);
        assert_eq!(x(&a), 3.0);
        a.play("jump").unwrap();
        assert!(!a.is_transitioning());
        assert_eq!(x(&a), 9.0);
        assert_eq!(a.play("zzz").err(), Some(AnimatorError::UnknownState("zzz".into())));
    }

    // -- root motion through the animator + system --

    fn root_motion_set() -> Arc<AnimationSet> {
        let mut run = AnimationClip::new("run", 1.0, true);
        let mut ch = JointChannel::new(0);
        ch.translation = Some(
            Track::new(vec![0.0, 1.0], vec![Vec3::ZERO, Vec3::new(0.0, 0.0, 3.0)], Interpolation::Linear).unwrap(),
        );
        run.channels.push(ch);
        run.root_motion = Some(RootMotion::horizontal(0));
        Arc::new(AnimationSet { skeleton: skel(), clips: vec![run] })
    }

    #[test]
    fn system_moves_transform_by_root_motion_and_strips_pose() {
        let mut world = World::new();
        let mut a = SkeletalAnimator::new(root_motion_set(), Arc::new(AnimatorController::single_clip("run"))).unwrap();
        a.speed = 1.0;
        let e = world.spawn((
            a,
            Transform { position: Vec3::new(1.0, 0.0, 0.0), rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2), scale: Vec3::ONE },
        ));
        let mut res = Resources::new();
        let mut sched = Schedule::new();
        sched.add_system(SKELETAL_ANIMATION_SYSTEM, skeletal_animation_system);
        for _ in 0..4 {
            sched.run(&mut world, &mut res, 0.25).unwrap();
        }
        let t = world.get::<&Transform>(e).unwrap();
        // 1 s of run = +3 along local Z; entity faces +90deg about Y so local +Z is world +X.
        assert!((t.position - Vec3::new(4.0, 0.0, 0.0)).length() < 1e-4, "{:?}", t.position);
        let a = world.get::<&SkeletalAnimator>(e).unwrap();
        // pose has the root pinned (no sliding)
        assert_eq!(a.pose().joints[0].translation, Vec3::ZERO);
    }

    #[test]
    fn root_motion_off_leaves_transform_and_plays_raw() {
        let mut a = SkeletalAnimator::new(root_motion_set(), Arc::new(AnimatorController::single_clip("run"))).unwrap();
        a.extract_root_motion = false;
        a.update(0.5);
        assert_eq!(a.root_motion_delta().translation, Vec3::ZERO);
        assert!((a.pose().joints[0].translation.z - 1.5).abs() < 1e-5);

        let mut b = SkeletalAnimator::new(root_motion_set(), Arc::new(AnimatorController::single_clip("run"))).unwrap();
        b.apply_root_motion = false;
        let mut world = World::new();
        let e = world.spawn((b, Transform::default()));
        let mut res = Resources::new();
        let mut sched = Schedule::new();
        sched.add_system(SKELETAL_ANIMATION_SYSTEM, skeletal_animation_system);
        sched.run(&mut world, &mut res, 0.5).unwrap();
        assert_eq!(world.get::<&Transform>(e).unwrap().position, Vec3::ZERO);
    }

    #[test]
    fn root_motion_blends_during_crossfade_and_survives_loop_wrap() {
        let mut a = SkeletalAnimator::new(root_motion_set(), Arc::new(AnimatorController::single_clip("run"))).unwrap();
        a.update(0.75);
        a.update(0.5); // crosses the loop boundary: 0.25 left + 0.25 of next => 0.5 s * 3 u/s
        assert!((a.root_motion_delta().translation.z - 1.5).abs() < 1e-4, "{:?}", a.root_motion_delta());
    }

    #[test]
    fn controller_ron_round_trip() {
        let c = controller();
        let text = ron::ser::to_string_pretty(&c, ron::ser::PrettyConfig::default()).unwrap();
        let back: AnimatorController = ron::from_str(&text).unwrap();
        assert_eq!(c, back);
        // defaults let a minimal file omit optional fields
        let min: AnimatorController = ron::from_str(
            r#"(name: "m", states: [(name: "a", motion: Clip(clip: "idle"))])"#,
        )
        .unwrap();
        assert_eq!(min.states[0].speed, 1.0);
        assert!(SkeletalAnimator::new(set(), Arc::new(min)).is_ok());
    }
}
