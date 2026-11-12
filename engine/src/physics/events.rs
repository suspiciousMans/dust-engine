//! Per-step trigger events as a resource, plus a ready-made physics system for
//! the [`Schedule`](crate::schedule::Schedule) / [`Resources`] pipeline.
//!
//! ```ignore
//! // Stateless-step mode (what `Headless::with_default_systems` uses):
//! schedule.add_system("physics", physics::physics_system).stage(Stage::PostUpdate);
//! // Stateful mode (fixed timestep, sleeping, joints, events natively):
//! resources.insert(PhysicsWorld::new(PhysicsParams::default()));
//! // Then in any later system:
//! for ev in ctx.res::<TriggerEvents>().entered() { /* ev.entity entered ev.other */ }
//! ```

use std::collections::BTreeSet;

use hecs::{Entity, World};

use super::sim::{PhysicsEvent, PhysicsWorld, TriggerEventKind};
use super::{PhysicsParams};
use crate::resources::Resources;
use crate::schedule::SystemCtx;

/// Trigger Enter/Stay/Exit events produced by the most recent physics system run.
///
/// **Semantics**: an event is `(kind, entity, other)` where `entity` is the
/// *dynamic* body and `other` is the collider (usually the trigger volume) it
/// overlaps — or, if a dynamic body is itself a trigger, what it overlaps.
/// A pair produces `Enter` on the first step it overlaps, `Stay` on every
/// following step, and `Exit` on the first step it no longer does (also when
/// either entity was despawned or lost its collider; the ids in an `Exit` may
/// then be dead).
///
/// **Ordering** is stable and deterministic: within one physics step all
/// `Enter`/`Stay` events come first sorted by `(entity, other)` ascending,
/// then all `Exit` events sorted the same way. When several fixed substeps run
/// in one frame ([`PhysicsWorld::update`]) their lists are concatenated in
/// substep order. The list is **replaced** every run (empty when nothing
/// changed or no substep ran), never accumulated.
///
/// Works in two modes. With a [`PhysicsWorld`] resource the events come from it
/// directly. Without one, the legacy [`super::step`] runs and the events are
/// derived by diffing this frame's overlaps against the previous frame's.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TriggerEvents {
    events: Vec<PhysicsEvent>,
    active: BTreeSet<(Entity, Entity)>,
}

impl TriggerEvents {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> &[PhysicsEvent] {
        &self.events
    }
    pub fn iter(&self) -> std::slice::Iter<'_, PhysicsEvent> {
        self.events.iter()
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    pub fn entered(&self) -> impl Iterator<Item = &PhysicsEvent> {
        self.of(TriggerEventKind::Enter)
    }
    pub fn stayed(&self) -> impl Iterator<Item = &PhysicsEvent> {
        self.of(TriggerEventKind::Stay)
    }
    pub fn exited(&self) -> impl Iterator<Item = &PhysicsEvent> {
        self.of(TriggerEventKind::Exit)
    }
    fn of(&self, kind: TriggerEventKind) -> impl Iterator<Item = &PhysicsEvent> {
        self.events.iter().filter(move |e| e.kind == kind)
    }
    /// Events where `e` is either side of the pair.
    pub fn involving(&self, e: Entity) -> impl Iterator<Item = &PhysicsEvent> {
        self.events.iter().filter(move |ev| ev.entity == e || ev.other == e)
    }
    /// Pairs currently overlapping (after the latest run), sorted.
    pub fn active_pairs(&self) -> impl Iterator<Item = &(Entity, Entity)> {
        self.active.iter()
    }
    pub fn clear(&mut self) {
        self.events.clear();
        self.active.clear();
    }

    /// Replaces the list with events computed elsewhere (a [`PhysicsWorld`] run)
    /// and keeps the active-pair set consistent with them.
    pub fn set(&mut self, events: Vec<PhysicsEvent>) {
        for ev in &events {
            match ev.kind {
                TriggerEventKind::Enter | TriggerEventKind::Stay => {
                    self.active.insert((ev.entity, ev.other));
                }
                TriggerEventKind::Exit => {
                    self.active.remove(&(ev.entity, ev.other));
                }
            }
        }
        self.events = events;
    }

    /// Derives Enter/Stay/Exit by diffing `overlaps` (pairs currently touching,
    /// any order) against the previous call's overlaps.
    pub fn update_from_overlaps(&mut self, overlaps: &[(Entity, Entity)]) {
        let now: BTreeSet<(Entity, Entity)> = overlaps.iter().copied().collect();
        let mut events = Vec::with_capacity(now.len());
        for &(entity, other) in &now {
            let kind = if self.active.contains(&(entity, other)) { TriggerEventKind::Stay } else { TriggerEventKind::Enter };
            events.push(PhysicsEvent { kind, entity, other });
        }
        for &(entity, other) in self.active.difference(&now) {
            events.push(PhysicsEvent { kind: TriggerEventKind::Exit, entity, other });
        }
        self.active = now;
        self.events = events;
    }
}

/// One physics frame driven by resources; the body of [`physics_system`].
///
/// * [`PhysicsParams`] resource (default inserted if missing) is authoritative:
///   it is copied into a present [`PhysicsWorld`] every frame, so editing it
///   changes gravity either way.
/// * With a [`PhysicsWorld`] resource: `update(world, dt)` (fixed timestep,
///   accumulator, sleeping, joints, native events). Without: stateless
///   [`super::step`] (one fixed pass, no sleeping, joints solved with the
///   default iteration count).
/// * Fills the [`TriggerEvents`] resource (inserted if missing) and returns the
///   `(dynamic, other)` pairs overlapping after the frame, sorted.
pub fn physics_step(world: &mut World, resources: &mut Resources, dt: f32) -> Vec<(Entity, Entity)> {
    let params = *resources.get_or_insert_with(PhysicsParams::default);
    let mut pw = resources.remove::<PhysicsWorld>();
    let overlaps = match pw.as_mut() {
        Some(pw) => {
            pw.params = params;
            let events = pw.update(world, dt);
            let pairs: BTreeSet<(Entity, Entity)> = events
                .iter()
                .filter(|e| e.kind != TriggerEventKind::Exit)
                .map(|e| (e.entity, e.other))
                .collect();
            resources.get_or_insert_with(TriggerEvents::default).set(events);
            pairs.into_iter().collect()
        }
        None => {
            let overlaps = super::step(world, dt, &params);
            resources.get_or_insert_with(TriggerEvents::default).update_from_overlaps(&overlaps);
            overlaps
        }
    };
    if let Some(pw) = pw {
        resources.insert(pw);
    }
    overlaps
}

/// Schedule system: `schedule.add_system("physics", physics::physics_system)`.
/// See [`physics_step`] for what it reads and writes.
pub fn physics_system(ctx: &mut SystemCtx<'_>) {
    physics_step(ctx.world, ctx.resources, ctx.dt);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::Transform;
    use crate::physics::{Collider, ColliderShape, RigidBody};
    use crate::schedule::{Schedule, Stage};
    use glam::Vec3;

    fn zero_g() -> PhysicsParams {
        PhysicsParams { gravity: 0.0, linear_damping: 0.0 }
    }

    fn kinds(t: &TriggerEvents) -> Vec<(TriggerEventKind, Entity, Entity)> {
        t.iter().map(|e| (e.kind, e.entity, e.other)).collect()
    }

    /// A ball flies through a trigger box along +X at 6 m/s (0.1 m per 1/60 s).
    fn fly_through(use_world: bool) -> Vec<Vec<(TriggerEventKind, Entity, Entity)>> {
        let mut world = World::new();
        let mut res = Resources::new();
        res.insert(zero_g());
        if use_world {
            res.insert(PhysicsWorld::new(zero_g()));
        }
        let trig = world.spawn((Transform::default(), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::splat(1.0) })));
        let ball = world.spawn((
            Transform::from_position(Vec3::new(-3.0, 0.0, 0.0)),
            Collider::sphere(0.25),
            RigidBody { velocity: Vec3::new(6.0, 0.0, 0.0), grounded: false },
        ));
        let mut sched = Schedule::new();
        sched.add_system("physics", physics_system).stage(Stage::PostUpdate);
        let mut per_frame = Vec::new();
        for _ in 0..80 {
            sched.run(&mut world, &mut res, 1.0 / 60.0).unwrap();
            let k = kinds(res.get::<TriggerEvents>().unwrap());
            for (_, e, o) in &k {
                assert_eq!((*e, *o), (ball, trig));
            }
            per_frame.push(k);
        }
        per_frame
    }

    #[test]
    fn enter_stay_exit_through_the_schedule_in_both_modes() {
        for use_world in [false, true] {
            let frames = fly_through(use_world);
            let flat: Vec<TriggerEventKind> = frames.iter().flatten().map(|k| k.0).collect();
            let enters = flat.iter().filter(|k| **k == TriggerEventKind::Enter).count();
            let exits = flat.iter().filter(|k| **k == TriggerEventKind::Exit).count();
            assert_eq!((enters, exits), (1, 1), "world={use_world}");
            let first = flat.iter().position(|k| *k == TriggerEventKind::Enter).unwrap();
            let last = flat.iter().position(|k| *k == TriggerEventKind::Exit).unwrap();
            assert!(first < last);
            assert!(flat[first + 1..last].iter().all(|k| *k == TriggerEventKind::Stay) && last - first > 10, "world={use_world}");
            // Frames with no change have an empty (replaced, not accumulated) list.
            assert!(frames[0].is_empty() && frames[79].is_empty());
        }
    }

    #[test]
    fn both_modes_agree_on_event_frames() {
        assert_eq!(fly_through(false), fly_through(true));
    }

    #[test]
    fn ordering_is_sorted_and_exits_come_last() {
        let mut world = World::new();
        let mut res = Resources::new();
        res.insert(zero_g());
        res.insert(PhysicsWorld::new(zero_g()));
        let trig = world.spawn((Transform::default(), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::splat(5.0) })));
        let mut balls = Vec::new();
        for i in 0..4 {
            balls.push(world.spawn((
                Transform::from_position(Vec3::new(i as f32 * 0.5, 0.0, 0.0)),
                Collider::sphere(0.2),
                RigidBody::default(),
            )));
        }
        physics_step(&mut world, &mut res, 1.0 / 60.0);
        let t = res.get::<TriggerEvents>().unwrap();
        assert_eq!(t.len(), 4);
        assert!(t.entered().count() == 4);
        let pairs: Vec<_> = t.iter().map(|e| (e.entity, e.other)).collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted);
        // Move two out: next frame has 2 Stay then 2 Exit, each group sorted.
        world.get::<&mut Transform>(balls[0]).unwrap().position = Vec3::new(50.0, 0.0, 0.0);
        world.get::<&mut Transform>(balls[2]).unwrap().position = Vec3::new(50.0, 0.0, 0.0);
        physics_step(&mut world, &mut res, 1.0 / 60.0);
        let t = res.get::<TriggerEvents>().unwrap();
        let k: Vec<_> = t.iter().map(|e| e.kind).collect();
        assert_eq!(k, vec![TriggerEventKind::Stay, TriggerEventKind::Stay, TriggerEventKind::Exit, TriggerEventKind::Exit]);
        let ex: Vec<_> = t.exited().map(|e| e.entity).collect();
        let mut exs = ex.clone();
        exs.sort();
        assert_eq!(ex, exs);
        assert_eq!(t.involving(balls[0]).count(), 1);
        assert_eq!(t.involving(trig).count(), 4);
        assert_eq!(t.active_pairs().count(), 2);
    }

    #[test]
    fn despawned_body_produces_exit() {
        let mut world = World::new();
        let mut res = Resources::new();
        res.insert(zero_g());
        let trig = world.spawn((Transform::default(), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::ONE })));
        let b = world.spawn((Transform::default(), Collider::sphere(0.2), RigidBody::default()));
        physics_step(&mut world, &mut res, 1.0 / 60.0);
        assert_eq!(res.get::<TriggerEvents>().unwrap().entered().count(), 1);
        world.despawn(b).unwrap();
        physics_step(&mut world, &mut res, 1.0 / 60.0);
        let ev = res.get::<TriggerEvents>().unwrap().events().to_vec();
        assert_eq!(ev, vec![PhysicsEvent { kind: TriggerEventKind::Exit, entity: b, other: trig }]);
    }
}
