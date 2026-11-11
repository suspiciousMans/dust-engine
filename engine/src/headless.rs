//! Headless test harness: run game and editor logic frame by frame with no
//! window, no GL context and no SDL video/event subsystem.
//!
//! [`Headless`] owns a [`World`], a [`Resources`] map and a [`Schedule`], and
//! steps them with a fixed `dt`, so a test can say "run 100 frames" and get
//! the same answer every time. It adds:
//!
//! * a [`FrameClock`] resource (frame number, `dt`, elapsed seconds),
//! * a [`HeadlessInput`] resource fed from a *script* (`frame -> key/button
//!   events`), readable from any system via `ctx.res::<HeadlessInput>()`,
//! * [`with_default_systems`](Headless::with_default_systems): `"physics"`
//!   ([`physics::step`] with the [`PhysicsParams`] resource) in
//!   [`Stage::PostUpdate`] and `"effects"` ([`effects::tick`]: lifetimes,
//!   fades, timed texture overrides, despawn) in [`Stage::Last`], so game
//!   logic in `Update` runs before both,
//! * query helpers ([`count`](Headless::count),
//!   [`entities_with`](Headless::entities_with), [`get`](Headless::get), ...),
//! * [`EditorHeadless`]: [`EditorState`] + [`EditorShell`] + a `Headless`, to
//!   test Play / Stop / undo without egui rendering.
//!
//! ## Frame semantics
//!
//! `clock.frame` is the index of the frame *about to run* (0 for the first
//! `step`). Scripted input for frame `f` is applied at the start of the step
//! where `clock.frame == f`, so systems running in that step see it
//! (`just_pressed` is true for exactly that step). After the step the clock
//! advances: `frame += 1`, `elapsed += dt`.
//!
//! ```
//! use engine::ecs::Transform;
//! use engine::effects::Lifetime;
//! use engine::headless::{Headless, HeadlessInput};
//!
//! let mut h = Headless::with_default_systems();
//! h.set_dt(0.25); // powers of two keep the f32 sums exact
//! let spark = h.spawn((Transform::default(), Lifetime::new(1.0)));
//!
//! // Scripted input: press "jump" on frame 2, release on frame 5.
//! h.press(2, "jump");
//! h.release(5, "jump");
//! h.add_system("count_held", |ctx| {
//!     if ctx.res::<HeadlessInput>().is_down("jump") {
//!         *ctx.res_mut::<u32>() += 1;
//!     }
//! });
//! h.resources.insert(0u32);
//!
//! // The Lifetime(1.0) entity is gone after exactly 4 frames at dt = 0.25.
//! assert_eq!(h.run_until(100, |h| !h.contains(spark)), Some(4));
//! h.step_n(6, 0.25);
//! assert_eq!(h.clock().frame, 10);
//! assert_eq!(*h.resources.require::<u32>(), 3); // held on frames 2, 3, 4
//! ```
//!
//! Nothing here creates a GL or SDL context. Components that hold GPU handles
//! can be faked with [`fakes`].

use std::collections::BTreeMap;

use glam::{Quat, Vec3};
use hecs::{Component, DynamicBundle, Entity, Query, World};

use crate::ecs::Transform;
use crate::editor::command::{Command, SpawnSpec};
use crate::editor::shell::{EditorShell, ShellEvent};
use crate::editor::state::{EditorMode, EditorState};
use crate::editor::Name;
use crate::effects;
use crate::physics::{self, Collider, PhysicsParams, RigidBody};
use crate::resources::Resources;
use crate::schedule::{Schedule, ScheduleError, Stage, SystemConfig, SystemCtx};

/// Default fixed timestep: 60 frames per second.
pub const DEFAULT_DT: f32 = 1.0 / 60.0;

// ------------------------------------------------------------------- clock

/// Frame counter and clock, stored as a resource by [`Headless`].
///
/// During a step `frame` is the index of the running frame and `elapsed` is
/// the simulated time at its start; `dt` is that frame's timestep.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameClock {
    pub frame: u64,
    pub dt: f32,
    /// Sum of all previous steps' `dt`, in `f64` so long runs don't drift.
    pub elapsed: f64,
}

/// Overlapping (dynamic, other) pairs reported by the `"physics"` system on
/// the most recent frame. Stored as a resource by
/// [`Headless::with_default_systems`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TriggerOverlaps(pub Vec<(Entity, Entity)>);

// ------------------------------------------------------------------- input

/// One scripted input event. Keys, mouse buttons and controller buttons are
/// all just names (`"Space"`, `"Mouse1"`, `"A"`): the harness does not
/// interpret them, so no sdl2 types are involved.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Press(String),
    Release(String),
    /// Sets an analog axis (`"LeftX"`, ...) to a value that persists until
    /// changed.
    Axis(String, f32),
}

/// Plain input state with the same edge semantics as [`crate::input::Input`]
/// (held / just pressed / just released), keyed by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeadlessInput {
    down: std::collections::BTreeSet<String>,
    pressed: std::collections::BTreeSet<String>,
    released: std::collections::BTreeSet<String>,
    axes: BTreeMap<String, f32>,
}

impl HeadlessInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears the edge-triggered state (just pressed / just released).
    pub fn begin_frame(&mut self) {
        self.pressed.clear();
        self.released.clear();
    }

    pub fn apply(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Press(k) => {
                // Like a real key repeat, pressing a held key is not a new edge.
                if self.down.insert(k.clone()) {
                    self.pressed.insert(k.clone());
                }
            }
            InputEvent::Release(k) => {
                if self.down.remove(k) {
                    self.released.insert(k.clone());
                }
            }
            InputEvent::Axis(a, v) => {
                self.axes.insert(a.clone(), *v);
            }
        }
    }

    pub fn is_down(&self, key: &str) -> bool {
        self.down.contains(key)
    }

    pub fn just_pressed(&self, key: &str) -> bool {
        self.pressed.contains(key)
    }

    pub fn just_released(&self, key: &str) -> bool {
        self.released.contains(key)
    }

    /// Axis value, `0.0` if never set.
    pub fn axis(&self, name: &str) -> f32 {
        self.axes.get(name).copied().unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------- harness

/// A world + resources + schedule that steps without a window. See the
/// [module docs](self).
pub struct Headless {
    pub world: World,
    pub resources: Resources,
    pub schedule: Schedule,
    dt: f32,
    script: BTreeMap<u64, Vec<InputEvent>>,
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

impl Headless {
    /// Empty harness with [`FrameClock`] and [`HeadlessInput`] resources and
    /// a 60 Hz fixed timestep. No systems.
    pub fn new() -> Self {
        let mut resources = Resources::new();
        resources.insert(FrameClock { frame: 0, dt: DEFAULT_DT, elapsed: 0.0 });
        resources.insert(HeadlessInput::new());
        Self { world: World::new(), resources, schedule: Schedule::new(), dt: DEFAULT_DT, script: BTreeMap::new() }
    }

    /// [`new`](Self::new) plus the engine's built-in systems:
    ///
    /// * `"physics"` — [`physics::step`] using the [`PhysicsParams`] resource
    ///   (inserted with defaults if absent; edit it to change gravity); the
    ///   trigger overlaps land in the [`TriggerOverlaps`] resource. Stage
    ///   `PostUpdate`, so velocities set in `Update` take effect that frame.
    /// * `"effects"` — [`effects::tick`] (lifetimes, fades, timed texture
    ///   overrides, despawn of expired entities). Stage `Last`, after physics.
    pub fn with_default_systems() -> Self {
        let mut h = Self::new();
        h.add_default_systems();
        h
    }

    /// Adds the systems described in [`with_default_systems`](Self::with_default_systems).
    /// Panics if they are already registered.
    pub fn add_default_systems(&mut self) {
        self.resources.get_or_insert_with(PhysicsParams::default);
        self.resources.get_or_insert_with(TriggerOverlaps::default);
        self.schedule
            .add_system("physics", |ctx: &mut SystemCtx<'_>| {
                let params = *ctx.res::<PhysicsParams>();
                let overlaps = physics::step(ctx.world, ctx.dt, &params);
                ctx.res_mut::<TriggerOverlaps>().0 = overlaps;
            })
            .stage(Stage::PostUpdate);
        self.schedule
            .add_system("effects", |ctx: &mut SystemCtx<'_>| {
                effects::tick(ctx.world, ctx.dt);
            })
            .stage(Stage::Last)
            .after("physics");
    }

    /// Registers a named system (see [`Schedule::add_system`]).
    #[track_caller]
    pub fn add_system(&mut self, name: &str, system: impl FnMut(&mut SystemCtx<'_>) + 'static) -> SystemConfig<'_> {
        self.schedule.add_system(name, system)
    }

    // ---- time

    /// Timestep used by [`tick`](Self::tick), [`step_frames`](Self::step_frames)
    /// and [`run_until`](Self::run_until).
    pub fn dt(&self) -> f32 {
        self.dt
    }

    pub fn set_dt(&mut self, dt: f32) {
        self.dt = dt;
    }

    /// Builder form of [`set_dt`](Self::set_dt).
    pub fn with_dt(mut self, dt: f32) -> Self {
        self.dt = dt;
        self
    }

    /// Current clock: `frame` is the number of completed steps.
    pub fn clock(&self) -> FrameClock {
        *self.resources.require::<FrameClock>()
    }

    pub fn frame(&self) -> u64 {
        self.clock().frame
    }

    // ---- input script

    /// Queues `event` for the step that runs as frame `frame`. Events for a
    /// frame already passed are never applied.
    pub fn script(&mut self, frame: u64, event: InputEvent) {
        self.script.entry(frame).or_default().push(event);
    }

    pub fn press(&mut self, frame: u64, key: &str) {
        self.script(frame, InputEvent::Press(key.to_string()));
    }

    pub fn release(&mut self, frame: u64, key: &str) {
        self.script(frame, InputEvent::Release(key.to_string()));
    }

    /// Press on `frame`, release on `frame + 1` (held for exactly one frame).
    pub fn tap(&mut self, frame: u64, key: &str) {
        self.press(frame, key);
        self.release(frame + 1, key);
    }

    pub fn set_axis(&mut self, frame: u64, axis: &str, value: f32) {
        self.script(frame, InputEvent::Axis(axis.to_string(), value));
    }

    /// Input as of the last step.
    pub fn input(&self) -> &HeadlessInput {
        self.resources.require::<HeadlessInput>()
    }

    // ---- stepping

    /// Runs one frame with timestep `dt`: clears input edges, applies the
    /// scripted events for this frame, runs the schedule, advances the clock.
    /// Returns how many systems ran.
    pub fn try_step(&mut self, dt: f32) -> Result<usize, ScheduleError> {
        let frame = self.clock().frame;
        {
            let input = self.resources.require_mut::<HeadlessInput>();
            input.begin_frame();
            if let Some(events) = self.script.remove(&frame) {
                for e in &events {
                    input.apply(e);
                }
            }
        }
        self.resources.require_mut::<FrameClock>().dt = dt;
        let ran = self.schedule.run(&mut self.world, &mut self.resources, dt)?;
        let clock = self.resources.require_mut::<FrameClock>();
        clock.frame += 1;
        clock.elapsed += f64::from(dt);
        Ok(ran)
    }

    /// [`try_step`](Self::try_step), panicking on a schedule error (cycle,
    /// unknown name) — in a test that is the failure you want.
    #[track_caller]
    pub fn step(&mut self, dt: f32) {
        if let Err(e) = self.try_step(dt) {
            panic!("headless step failed: {e}");
        }
    }

    /// `n` steps of the same fixed `dt`.
    #[track_caller]
    pub fn step_n(&mut self, n: usize, dt: f32) {
        for _ in 0..n {
            self.step(dt);
        }
    }

    /// One step at the harness's own [`dt`](Self::dt).
    #[track_caller]
    pub fn tick(&mut self) {
        self.step(self.dt);
    }

    /// `n` steps at the harness's own [`dt`](Self::dt).
    #[track_caller]
    pub fn step_frames(&mut self, n: usize) {
        self.step_n(n, self.dt);
    }

    /// Steps (at [`dt`](Self::dt)) until `done(&self)` is true, checking
    /// before the first step and after each one, for at most `max_frames`
    /// steps. Returns how many steps it took (`Some(0)` if already true), or
    /// `None` if the limit was reached first.
    #[track_caller]
    pub fn run_until(&mut self, max_frames: usize, mut done: impl FnMut(&Headless) -> bool) -> Option<usize> {
        for frames in 0..=max_frames {
            if done(self) {
                return Some(frames);
            }
            if frames < max_frames {
                self.tick();
            }
        }
        None
    }

    // ---- entities and queries

    pub fn spawn(&mut self, bundle: impl DynamicBundle) -> Entity {
        self.world.spawn(bundle)
    }

    /// Spawns a named entity at `position` with `extra` components.
    pub fn spawn_at(&mut self, name: &str, position: Vec3, extra: impl DynamicBundle) -> Entity {
        let e = self.world.spawn((Name(name.to_string()), Transform::from_position(position)));
        self.world.insert(e, extra).expect("entity was just spawned");
        e
    }

    /// Spawns a simulated body: `Transform` at `position`, a `RigidBody` with
    /// `velocity`, and `Collider::sphere(radius)`.
    pub fn spawn_body(&mut self, name: &str, position: Vec3, velocity: Vec3, radius: f32) -> Entity {
        self.spawn_at(name, position, (RigidBody { velocity, grounded: false }, Collider::sphere(radius)))
    }

    pub fn despawn(&mut self, e: Entity) -> bool {
        self.world.despawn(e).is_ok()
    }

    pub fn contains(&self, e: Entity) -> bool {
        self.world.contains(e)
    }

    /// Number of entities matching query `Q`, e.g. `h.count::<&Lifetime>()`.
    pub fn count<Q: Query>(&self) -> usize {
        self.world.query::<Q>().iter().count()
    }

    /// Entities that have component `C`, sorted by entity id (stable order
    /// regardless of archetype layout).
    pub fn entities_with<C: Component>(&self) -> Vec<Entity> {
        let mut v: Vec<Entity> = self.world.query::<&C>().iter().map(|(e, _)| e).collect();
        v.sort_by_key(|e| e.to_bits());
        v
    }

    /// Clone of `e`'s component `C`, if it has one.
    pub fn get<C: Component + Clone>(&self, e: Entity) -> Option<C> {
        self.world.get::<&C>(e).ok().map(|c| (*c).clone())
    }

    /// First entity whose editor [`Name`] equals `name` (lowest id).
    pub fn find_by_name(&self, name: &str) -> Option<Entity> {
        self.world
            .query::<&Name>()
            .iter()
            .filter(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .min_by_key(|e| e.to_bits())
    }

    pub fn position(&self, e: Entity) -> Option<Vec3> {
        self.world.get::<&Transform>(e).ok().map(|t| t.position)
    }

    pub fn velocity(&self, e: Entity) -> Option<Vec3> {
        self.world.get::<&RigidBody>(e).ok().map(|b| b.velocity)
    }

    /// Transform + RigidBody of every entity that has a `Transform`, sorted
    /// by entity id. Equal states mean the scene is restored exactly.
    pub fn scene_state(&self) -> Vec<BodyState> {
        let mut out: Vec<BodyState> = self
            .world
            .query::<&Transform>()
            .iter()
            .map(|(e, t)| BodyState {
                entity: e,
                name: self.world.get::<&Name>(e).ok().map(|n| n.0.clone()),
                position: bits3(t.position),
                rotation: bits4(t.rotation),
                scale: bits3(t.scale),
                body: self.world.get::<&RigidBody>(e).ok().map(|b| (bits3(b.velocity), b.grounded)),
            })
            .collect();
        out.sort_by_key(|s| s.entity.to_bits());
        out
    }
}

// ------------------------------------------------------------------- fakes

/// GPU-handle stand-ins for tests that never create a GL context.
///
/// Neither `GpuTexture` nor `GpuMesh` has a `Drop` impl, so building one
/// around a made-up handle never touches GL — but **drawing or deleting it
/// would**. Use them only with headless logic (effects, snapshots, ...).
pub mod fakes {
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use crate::ecs::MeshRenderer;
    use crate::mesh::GpuMesh;
    use crate::texture::GpuTexture;

    /// A 1x1 texture whose handle is `id` (non-zero, so tests can tell
    /// textures apart with [`texture_id`]).
    pub fn texture(id: u32) -> Arc<GpuTexture> {
        Arc::new(GpuTexture { handle: glow::NativeTexture(NonZeroU32::new(id).expect("fake texture id must be non-zero")), width: 1, height: 1 })
    }

    /// The fake id of a texture slot, `None` for no texture.
    pub fn texture_id(t: &Option<Arc<GpuTexture>>) -> Option<u32> {
        t.as_ref().map(|t| t.handle.0.get())
    }

    /// A `GpuMesh` of made-up, non-zero handles.
    pub fn mesh() -> Arc<GpuMesh> {
        // `GpuMesh` is exactly four 4-byte, non-zero-valid fields (three
        // `NonZeroU32` GL names and an `i32` count), so any field order of
        // four non-zero u32s is a valid value. The assert pins the layout
        // assumption at compile time.
        const _: () = assert!(std::mem::size_of::<GpuMesh>() == 16 && std::mem::align_of::<GpuMesh>() == 4);
        // SAFETY: see above; no Drop impl, so no GL call on drop.
        Arc::new(unsafe { std::mem::transmute::<[u32; 4], GpuMesh>([1, 2, 3, 6]) })
    }

    /// A `MeshRenderer` with a fake mesh and `texture_id` as its texture.
    pub fn mesh_renderer(texture_id: Option<u32>) -> MeshRenderer {
        MeshRenderer { mesh: mesh(), texture: texture_id.map(texture) }
    }
}

// ------------------------------------------------------------------ editor

/// Position, rotation, scale and (when present) `RigidBody` of one entity,
/// bit-comparable so "restored exactly" can be `assert_eq!`.
#[derive(Clone, Debug, PartialEq)]
pub struct BodyState {
    pub entity: Entity,
    pub name: Option<String>,
    pub position: [u32; 3],
    pub rotation: [u32; 4],
    pub scale: [u32; 3],
    /// `(velocity bits, grounded)` if the entity has a `RigidBody`.
    pub body: Option<([u32; 3], bool)>,
}

fn bits3(v: Vec3) -> [u32; 3] {
    [v.x.to_bits(), v.y.to_bits(), v.z.to_bits()]
}

fn bits4(q: Quat) -> [u32; 4] {
    [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()]
}

/// Drives [`EditorState`] + [`EditorShell`] over a [`Headless`] world without
/// any egui rendering: spawn, Play, step, Pause, Stop, undo/redo.
///
/// The simulation only advances while the editor is in [`EditorMode::Play`];
/// in Edit and Pause [`step`](Self::step) is a no-op (so frames, physics and
/// effects all stand still, as in the real editor).
///
/// ```
/// use engine::headless::EditorHeadless;
/// use glam::Vec3;
///
/// let mut ed = EditorHeadless::new();
/// let ball = ed.sim.spawn_body("ball", Vec3::new(0.0, 5.0, 0.0), Vec3::ZERO, 0.5);
/// let before = ed.scene_state();
/// ed.play();
/// ed.step_n(30, 1.0 / 60.0);
/// assert!(ed.sim.position(ball).unwrap().y < 5.0); // gravity moved it
/// ed.stop();
/// assert_eq!(ed.scene_state(), before); // position and velocity restored
/// ```
pub struct EditorHeadless {
    pub sim: Headless,
    pub editor: EditorState,
    pub shell: EditorShell,
}

impl Default for EditorHeadless {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorHeadless {
    /// Default systems (physics + effects), default editor registry (which
    /// includes `Transform`, `RigidBody` and `Collider`).
    pub fn new() -> Self {
        Self::with_sim(Headless::with_default_systems())
    }

    pub fn with_sim(sim: Headless) -> Self {
        Self { sim, editor: EditorState::new(), shell: EditorShell::new(Vec::new()) }
    }

    pub fn mode(&self) -> EditorMode {
        self.editor.mode
    }

    pub fn world(&self) -> &World {
        &self.sim.world
    }

    // ---- editing (undoable)

    /// Spawns through the undo stack (like the hierarchy "+" button); the
    /// new entity becomes the selection.
    pub fn spawn_via_editor(&mut self, spec: SpawnSpec) -> Option<Entity> {
        self.editor.spawn(&mut self.sim.world, spec)
    }

    /// Runs any [`Command`] through the undo stack.
    pub fn exec(&mut self, cmd: impl Command + 'static) -> Result<(), String> {
        self.editor.exec(&mut self.sim.world, cmd)
    }

    pub fn undo(&mut self) -> bool {
        self.editor.undo(&mut self.sim.world)
    }

    pub fn redo(&mut self) -> bool {
        self.editor.redo(&mut self.sim.world)
    }

    // ---- play control

    /// Edit -> Play: captures the scene snapshot (no-op outside Edit).
    pub fn play(&mut self) {
        self.shell.play(&mut self.editor, &self.sim.world);
    }

    pub fn toggle_pause(&mut self) {
        self.shell.toggle_pause(&mut self.editor);
    }

    /// Play/Pause -> Edit: restores the snapshot (no-op in Edit).
    pub fn stop(&mut self) {
        self.shell.stop(&mut self.editor, &mut self.sim.world);
    }

    pub fn take_events(&mut self) -> Vec<ShellEvent> {
        self.shell.take_events()
    }

    // ---- stepping (Play only)

    /// Steps the simulation once if the editor is in Play. Returns whether
    /// it stepped.
    #[track_caller]
    pub fn step(&mut self, dt: f32) -> bool {
        if self.editor.mode != EditorMode::Play {
            return false;
        }
        self.sim.step(dt);
        true
    }

    /// `n` steps of `dt`; returns how many actually ran.
    #[track_caller]
    pub fn step_n(&mut self, n: usize, dt: f32) -> usize {
        (0..n).filter(|_| self.step(dt)).count()
    }

    // ---- observation

    /// See [`Headless::scene_state`].
    pub fn scene_state(&self) -> Vec<BodyState> {
        self.sim.scene_state()
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::editor::command::SetFieldCommand;
    use crate::editor::inspector::Value;
    use crate::effects::{apply_texture_override, Fade, Lifetime, TextureOverride};

    const DT: f32 = 0.125; // exact in f32

    // ---- (1) TextureOverride with a timer restores the original texture

    #[test]
    fn texture_override_timer_restores_original_over_frames() {
        let mut h = Headless::with_default_systems().with_dt(DT);
        let e = h.spawn((Transform::default(), fakes::mesh_renderer(Some(1))));
        let tex = |h: &Headless| fakes::texture_id(&h.world.get::<&crate::ecs::MeshRenderer>(e).unwrap().texture);
        assert_eq!(tex(&h), Some(1));

        // 1.0 s timer at dt = 0.125 -> restored by the 8th step.
        assert!(apply_texture_override(&mut h.world, e, Some(fakes::texture(2)), Some(1.0)));
        assert_eq!(tex(&h), Some(2));
        assert!(h.world.get::<&TextureOverride>(e).is_ok());

        h.step_frames(7);
        assert_eq!(tex(&h), Some(2), "still overridden after 7 frames");
        assert_eq!(h.frame(), 7);

        let took = h.run_until(100, |h| h.world.get::<&TextureOverride>(e).is_err());
        assert_eq!(took, Some(1), "restored on the 8th frame overall");
        assert_eq!(h.frame(), 8);
        assert_eq!(tex(&h), Some(1), "original texture is back");
        assert!(h.contains(e), "restoring does not despawn");

        // Stays restored.
        h.step_frames(20);
        assert_eq!(tex(&h), Some(1));
    }

    #[test]
    fn texture_override_without_timer_stays_until_restored() {
        let mut h = Headless::with_default_systems().with_dt(DT);
        let e = h.spawn((fakes::mesh_renderer(None),));
        apply_texture_override(&mut h.world, e, Some(fakes::texture(9)), None);
        h.step_frames(50);
        assert!(h.world.get::<&TextureOverride>(e).is_ok());
        assert!(effects::restore_texture(&mut h.world, e));
        assert_eq!(fakes::texture_id(&h.world.get::<&crate::ecs::MeshRenderer>(e).unwrap().texture), None);
    }

    // ---- (2) Lifetime entities despawn at the right frame

    #[test]
    fn lifetime_entities_despawn_on_their_frame() {
        let mut h = Headless::with_default_systems().with_dt(DT);
        // max = n * dt  ->  despawned by the n-th step (age == max).
        let spawned: Vec<(usize, Entity)> =
            [1usize, 3, 4, 10].iter().map(|&n| (n, h.spawn((Transform::default(), Lifetime::new(n as f32 * DT))))).collect();
        let forever = h.spawn((Transform::default(),));
        let fading = h.spawn((Transform::default(), Lifetime::new(4.0 * DT), Fade::shrink()));

        for frame in 1..=12usize {
            h.tick();
            for &(n, e) in &spawned {
                assert_eq!(h.contains(e), frame < n, "lifetime {n} at frame {frame}");
            }
            assert_eq!(h.contains(fading), frame < 4);
            assert!(h.contains(forever));
        }
        assert_eq!(h.count::<&Lifetime>(), 0);
        assert_eq!(h.entities_with::<Transform>(), vec![forever]);
    }

    #[test]
    fn run_until_reports_frames_and_gives_up() {
        let mut h = Headless::with_default_systems().with_dt(DT);
        let e = h.spawn((Lifetime::new(5.0 * DT),));
        assert_eq!(h.run_until(0, |h| !h.contains(e)), None, "limit 0 never steps");
        assert_eq!(h.frame(), 0);
        assert_eq!(h.run_until(4, |h| !h.contains(e)), None, "gone on frame 5, limit 4");
        assert_eq!(h.frame(), 4);
        assert_eq!(h.run_until(4, |h| !h.contains(e)), Some(1));
        assert_eq!(h.run_until(4, |h| !h.contains(e)), Some(0), "already true");
    }

    // ---- (3) editor Play/Stop restore after physics moved bodies

    #[test]
    fn editor_play_stop_restores_after_physics_moved_bodies() {
        let mut ed = EditorHeadless::new();
        // RigidBody must be snapshot-able for this test to mean anything.
        let probe = ed.sim.spawn_body("probe", Vec3::ZERO, Vec3::ZERO, 0.5);
        assert!(ed.editor.registry.present(&ed.sim.world, probe).iter().any(|c| c.name == "RigidBody"));
        ed.sim.despawn(probe);

        let ball = ed.sim.spawn_body("ball", Vec3::new(0.0, 10.0, 0.0), Vec3::new(1.0, 0.0, 0.0), 0.5);
        let shot = ed.sim.spawn_body("shot", Vec3::new(5.0, 2.0, -1.0), Vec3::new(0.0, 3.0, 0.0), 0.25);
        let floor = ed.sim.spawn_at("floor", Vec3::new(0.0, -1.0, 0.0), (Collider::aabb(Vec3::new(50.0, 0.5, 50.0)),));
        let before = ed.scene_state();
        assert_eq!(before.len(), 3);

        assert_eq!(ed.step_n(5, DEFAULT_DT), 0, "no simulation in Edit mode");
        assert_eq!(ed.scene_state(), before);

        ed.play();
        assert_eq!(ed.mode(), EditorMode::Play);
        assert_eq!(ed.step_n(120, DEFAULT_DT), 120);

        let moved = ed.scene_state();
        assert_ne!(moved, before);
        assert!(ed.sim.position(ball).unwrap().y < 10.0, "gravity moved the ball");
        assert_ne!(ed.sim.velocity(ball).unwrap(), Vec3::new(1.0, 0.0, 0.0), "velocity changed");
        assert_ne!(ed.sim.velocity(shot).unwrap(), Vec3::new(0.0, 3.0, 0.0));
        assert_eq!(ed.sim.frame(), 120);

        // Pause freezes the simulation.
        ed.toggle_pause();
        assert_eq!(ed.step_n(10, DEFAULT_DT), 0);
        assert_eq!(ed.scene_state(), moved);
        ed.toggle_pause();

        // Entities spawned and despawned during play are also undone.
        let junk = ed.sim.spawn_at("junk", Vec3::ONE, ());
        ed.sim.despawn(floor);
        ed.step_n(3, DEFAULT_DT);

        ed.stop();
        assert_eq!(ed.mode(), EditorMode::Edit);
        assert_eq!(ed.scene_state(), before, "positions and RigidBody velocity restored exactly");
        assert!(!ed.sim.contains(junk));
        assert!(ed.sim.contains(floor));
        assert_eq!(ed.sim.velocity(ball), Some(Vec3::new(1.0, 0.0, 0.0)));
        assert_eq!(
            ed.take_events(),
            vec![ShellEvent::PlayStarted, ShellEvent::Paused, ShellEvent::Resumed, ShellEvent::Stopped { restored: true }]
        );

        // A second Play/Stop cycle restores again.
        ed.play();
        ed.step_n(60, DEFAULT_DT);
        ed.stop();
        assert_eq!(ed.scene_state(), before);
    }

    #[test]
    fn editor_undo_redo_work_around_play() {
        let mut ed = EditorHeadless::new();
        let a = ed.spawn_via_editor(SpawnSpec::empty("a", None)).unwrap();
        ed.exec(SetFieldCommand::new(vec![a], "Transform", "position", Value::Vec3(Vec3::new(1.0, 2.0, 3.0)))).unwrap();
        assert_eq!(ed.sim.position(a), Some(Vec3::new(1.0, 2.0, 3.0)));

        ed.play();
        ed.step_n(3, DEFAULT_DT);
        ed.stop();
        assert_eq!(ed.sim.position(a), Some(Vec3::new(1.0, 2.0, 3.0)));

        assert!(ed.undo());
        assert_eq!(ed.sim.position(a), Some(Vec3::ZERO));
        assert!(ed.undo());
        assert!(!ed.sim.contains(a));
        assert!(ed.redo());
        assert!(ed.redo());
        assert_eq!(ed.sim.position(a), Some(Vec3::new(1.0, 2.0, 3.0)));
    }

    // ---- (4) scheduled systems run in order across 100 frames, deterministically

    #[derive(Default, Debug, PartialEq)]
    struct Trace {
        order: Vec<(u64, &'static str)>,
        acc: u64,
        held_frames: u32,
        presses: Vec<u64>,
    }

    fn build() -> Headless {
        let mut h = Headless::with_default_systems().with_dt(DT);
        h.resources.insert(Trace::default());
        let ball = h.spawn_body("ball", Vec3::new(0.0, 100.0, 0.0), Vec3::new(2.0, 0.0, 0.0), 0.5);
        let _ = ball;

        fn mark(name: &'static str) -> impl FnMut(&mut SystemCtx<'_>) + 'static {
            move |ctx| {
                let frame = ctx.res::<FrameClock>().frame;
                ctx.res_mut::<Trace>().order.push((frame, name));
            }
        }
        // Registered out of order on purpose; stages/constraints decide.
        h.add_system("post", mark("post")).stage(Stage::PostUpdate).after("physics");
        h.add_system("late", mark("late")).stage(Stage::Last).after("effects");
        h.add_system("update_b", |ctx: &mut SystemCtx<'_>| {
            // Order-sensitive: (acc + 3) * 2 only equals the expected value if
            // update_a ran first.
            let frame = ctx.res::<FrameClock>().frame;
            let t = ctx.res_mut::<Trace>();
            t.order.push((frame, "update_b"));
            t.acc = (t.acc + 3) * 2 % 1_000_003;
        })
        .after("update_a");
        h.add_system("update_a", |ctx: &mut SystemCtx<'_>| {
            let frame = ctx.res::<FrameClock>().frame;
            let input_down = ctx.res::<HeadlessInput>().is_down("fire");
            let pressed = ctx.res::<HeadlessInput>().just_pressed("fire");
            let t = ctx.res_mut::<Trace>();
            t.order.push((frame, "update_a"));
            t.acc += frame + 1;
            if input_down {
                t.held_frames += 1;
            }
            if pressed {
                t.presses.push(frame);
            }
        });
        h.add_system("first", mark("first")).stage(Stage::First);
        h.press(10, "fire");
        h.release(20, "fire");
        h.tap(50, "fire");
        h
    }

    fn run_100() -> (Trace, Vec<BodyState>, FrameClock) {
        let mut h = build();
        h.step_frames(100);
        let clock = h.clock();
        let bodies = h.scene_state();
        (h.resources.remove::<Trace>().unwrap(), bodies, clock)
    }

    #[test]
    fn scheduled_systems_run_in_order_and_deterministically_over_100_frames() {
        let (trace, bodies, clock) = run_100();

        // Order within every frame: first, update_a, update_b, physics/effects
        // (unlogged), post, late.
        assert_eq!(trace.order.len(), 100 * 5);
        for (f, chunk) in trace.order.chunks(5).enumerate() {
            let names: Vec<_> = chunk.iter().map(|&(_, n)| n).collect();
            assert_eq!(names, ["first", "update_a", "update_b", "post", "late"], "frame {f}");
            assert!(chunk.iter().all(|&(frame, _)| frame == f as u64));
        }

        // Closed-form check of the order-sensitive accumulator.
        let mut acc = 0u64;
        for frame in 0..100u64 {
            acc += frame + 1;
            acc = (acc + 3) * 2 % 1_000_003;
        }
        assert_eq!(trace.acc, acc);

        // Scripted input: held on 10..20 and on frame 50 only.
        assert_eq!(trace.held_frames, 10 + 1);
        assert_eq!(trace.presses, vec![10, 50]);

        assert_eq!(clock.frame, 100);
        assert_eq!(clock.elapsed, 100.0 * f64::from(DT));

        // Same again: identical trace, accumulators, and bit-identical physics.
        let (trace2, bodies2, clock2) = run_100();
        assert_eq!(trace, trace2);
        assert_eq!(bodies, bodies2);
        assert_eq!(clock, clock2);
        let ball = &bodies[0];
        assert!(f32::from_bits(ball.position[1]) < 100.0, "physics ran in the default stage");
    }

    // ---- input and harness details

    #[test]
    fn input_edges_follow_the_script() {
        let mut h = Headless::new();
        h.press(1, "Space");
        h.press(1, "Space"); // duplicate press is not a second edge
        h.release(3, "Space");
        h.set_axis(2, "LeftX", 0.5);
        h.tap(5, "Enter");

        let mut seen = Vec::new();
        for _ in 0..8 {
            h.tick();
            let i = h.input();
            seen.push((i.is_down("Space"), i.just_pressed("Space"), i.just_released("Space"), i.is_down("Enter")));
        }
        assert_eq!(seen[0], (false, false, false, false));
        assert_eq!(seen[1], (true, true, false, false));
        assert_eq!(seen[2], (true, false, false, false));
        assert_eq!(seen[3], (false, false, true, false));
        assert_eq!(seen[5], (false, false, false, true));
        assert_eq!(seen[6], (false, false, false, false));
        assert_eq!(h.input().axis("LeftX"), 0.5);
        assert_eq!(h.input().axis("RightX"), 0.0);
    }

    #[test]
    fn clock_tracks_frames_and_elapsed() {
        let mut h = Headless::new();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = Rc::clone(&seen);
        h.add_system("watch", move |ctx| {
            let c = *ctx.res::<FrameClock>();
            s.borrow_mut().push((c.frame, c.elapsed, c.dt, ctx.dt));
        });
        h.step(0.5);
        h.step_n(2, 0.25);
        assert_eq!(*seen.borrow(), vec![(0, 0.0, 0.5, 0.5), (1, 0.5, 0.25, 0.25), (2, 0.75, 0.25, 0.25)]);
        assert_eq!(h.clock(), FrameClock { frame: 3, dt: 0.25, elapsed: 1.0 });
    }

    #[test]
    fn schedule_errors_surface_without_panicking_via_try_step() {
        let mut h = Headless::new();
        h.add_system("a", |_| {}).after("missing");
        assert!(matches!(h.try_step(DT), Err(ScheduleError::UnknownSystem { .. })));
        assert_eq!(h.frame(), 0, "failed step does not advance the clock");
    }

    #[test]
    fn query_helpers() {
        let mut h = Headless::new();
        let a = h.spawn_at("a", Vec3::X, ());
        let b = h.spawn_body("b", Vec3::Y, Vec3::Z, 1.0);
        assert_eq!(h.count::<&Transform>(), 2);
        assert_eq!(h.entities_with::<RigidBody>(), vec![b]);
        assert_eq!(h.find_by_name("a"), Some(a));
        assert_eq!(h.find_by_name("nope"), None);
        assert_eq!(h.get::<RigidBody>(b).unwrap().velocity, Vec3::Z);
        assert!(h.get::<RigidBody>(a).is_none());
        assert_eq!(h.position(a), Some(Vec3::X));
        assert!(h.despawn(a));
        assert!(!h.contains(a));
    }
}
