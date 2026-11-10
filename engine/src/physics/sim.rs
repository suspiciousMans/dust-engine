//! The simulation core: integration, broadphase, solver, sleeping, trigger
//! events and the fixed-timestep driver ([`PhysicsWorld`]).
//!
//! Determinism: bodies are processed in ascending entity order, pair lists are
//! sorted, no hash-map iteration order is ever observed, and there is no
//! threading — the same inputs give bit-identical results.

use std::collections::{BTreeMap, BTreeSet};

use glam::{Quat, Vec3};
use hecs::Entity;

use super::layers::{CollisionLayers, PhysicsMaterial};
use super::narrow::{collide, shape_bounds, Pose};
use super::{Collider, ColliderShape, PhysicsParams, RigidBody};
use crate::ecs::Transform;

/// Impact speeds below this never bounce (stops resting jitter).
const RESTITUTION_THRESHOLD: f32 = 1.0;
/// Broadphase bounds inflation so small solver pushes don't leave the pair list.
const BROADPHASE_MARGIN: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SleepConfig {
    pub enabled: bool,
    /// Speed (m/s) below which a body counts as resting.
    pub linear_threshold: f32,
    /// Seconds of continuous rest before sleeping.
    pub time_to_sleep: f32,
}

impl Default for SleepConfig {
    fn default() -> Self {
        Self { enabled: true, linear_threshold: 0.08, time_to_sleep: 0.5 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerEventKind {
    Enter,
    Stay,
    Exit,
}

/// A trigger overlap transition. `entity` is the *dynamic* body (the one whose
/// `on_trigger_enter/exit(other_name)` behavior hook should fire, exactly like
/// the pairs returned by the legacy [`super::step`]); `other` is what it overlaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicsEvent {
    pub kind: TriggerEventKind,
    pub entity: Entity,
    pub other: Entity,
}

#[derive(Default)]
pub(crate) struct SimState {
    pub sleep: SleepConfig,
    timers: BTreeMap<Entity, f32>,
    asleep: BTreeMap<Entity, Vec3>,
}

#[derive(Default)]
pub(crate) struct StepOutput {
    /// `(dynamic entity, other entity)` pairs, sorted.
    pub trigger_overlaps: Vec<(Entity, Entity)>,
    /// Solid contacts this step: `(a, b, contact)` with normal from b toward a.
    pub contacts: Vec<(Entity, Entity, super::Contact)>,
}

struct Body {
    entity: Entity,
    shape: ColliderShape,
    rot: Quat,
    pos: Vec3,
    vel: Vec3,
    grounded: bool,
    dynamic: bool,
    trigger: bool,
    layers: CollisionLayers,
    material: Option<PhysicsMaterial>,
    lo: Vec3,
    hi: Vec3,
}

pub(crate) fn run_step(
    world: &mut hecs::World,
    dt: f32,
    params: &PhysicsParams,
    iterations: usize,
    mut state: Option<&mut SimState>,
) -> StepOutput {
    let sleep_cfg = state.as_ref().map(|s| s.sleep).filter(|c| c.enabled);

    // 1. Integrate awake dynamic bodies.
    let mut dyn_ids: Vec<Entity> = world.query::<(&Transform, &RigidBody)>().iter().map(|(e, _)| e).collect();
    dyn_ids.sort();
    for &e in &dyn_ids {
        let Ok(mut q) = world.query_one::<(&mut Transform, &mut RigidBody)>(e) else { continue };
        let Some((t, b)) = q.get() else { continue };
        if let Some(st) = state.as_deref_mut() {
            if let Some(p) = st.asleep.get(&e).copied() {
                // Woken externally by a teleport or a velocity kick.
                if (t.position - p).length_squared() > 1e-8 || b.velocity.length() > 2.0 * st.sleep.linear_threshold {
                    st.asleep.remove(&e);
                    st.timers.remove(&e);
                } else {
                    continue;
                }
            }
        }
        b.velocity.y -= params.gravity * dt;
        let damping = (1.0 - params.linear_damping * dt).max(0.0);
        b.velocity *= damping;
        t.position += b.velocity * dt;
        b.grounded = false;
    }

    // 2. Snapshot bodies (sorted by entity id).
    let mut bodies: Vec<Body> = world
        .query::<(&Transform, &Collider, Option<&RigidBody>, Option<&CollisionLayers>, Option<&PhysicsMaterial>)>()
        .iter()
        .map(|(entity, (t, c, rb, l, m))| {
            let (lo, hi) = shape_bounds(c.shape, Pose::from(t));
            Body {
                entity,
                shape: c.shape,
                rot: t.rotation,
                pos: t.position,
                vel: rb.map_or(Vec3::ZERO, |r| r.velocity),
                grounded: rb.map_or(false, |r| r.grounded),
                dynamic: rb.is_some(),
                trigger: c.is_trigger,
                layers: l.copied().unwrap_or_default(),
                material: m.copied(),
                lo: lo - Vec3::splat(BROADPHASE_MARGIN),
                hi: hi + Vec3::splat(BROADPHASE_MARGIN),
            }
        })
        .collect();
    bodies.sort_by_key(|b| b.entity);

    // 3. Broadphase: sweep-and-prune on x, then y/z bounds test.
    let mut order: Vec<usize> = (0..bodies.len()).collect();
    order.sort_by(|&a, &b| bodies[a].lo.x.total_cmp(&bodies[b].lo.x).then(a.cmp(&b)));
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (oi, &i) in order.iter().enumerate() {
        for &j in &order[oi + 1..] {
            if bodies[j].lo.x > bodies[i].hi.x {
                break;
            }
            let (a, b) = (&bodies[i], &bodies[j]);
            if !a.dynamic && !b.dynamic {
                continue;
            }
            if a.lo.y > b.hi.y || b.lo.y > a.hi.y || a.lo.z > b.hi.z || b.lo.z > a.hi.z {
                continue;
            }
            if !a.layers.collides_with(&b.layers) {
                continue;
            }
            pairs.push((i.min(j), i.max(j)));
        }
    }
    pairs.sort_unstable();

    let mut out = StepOutput::default();

    // 4. Trigger overlaps (once, on post-integration positions).
    for &(i, j) in &pairs {
        let (a, b) = (&bodies[i], &bodies[j]);
        if !(a.trigger || b.trigger) {
            continue;
        }
        if collide(a.shape, Pose::new(a.pos, a.rot), b.shape, Pose::new(b.pos, b.rot)).is_none() {
            continue;
        }
        if a.dynamic {
            out.trigger_overlaps.push((a.entity, b.entity));
        }
        if b.dynamic {
            out.trigger_overlaps.push((b.entity, a.entity));
        }
    }
    out.trigger_overlaps.sort();

    // 5. Solid contacts.
    for iter in 0..iterations.max(1) {
        for &(i, j) in &pairs {
            let (lo_i, hi_i) = bodies.split_at_mut(j);
            let a = &mut lo_i[i];
            let b = &mut hi_i[0];
            if a.trigger || b.trigger {
                continue;
            }
            let asleep_a = a.dynamic && state.as_ref().map_or(false, |s| s.asleep.contains_key(&a.entity));
            let asleep_b = b.dynamic && state.as_ref().map_or(false, |s| s.asleep.contains_key(&b.entity));
            // A sleeping body only matters if something awake touches it.
            let a_awake = a.dynamic && !asleep_a;
            let b_awake = b.dynamic && !asleep_b;
            if !a_awake && !b_awake {
                continue;
            }
            let Some(c) = collide(a.shape, Pose::new(a.pos, a.rot), b.shape, Pose::new(b.pos, b.rot)) else {
                continue;
            };
            if iter == 0 {
                out.contacts.push((a.entity, b.entity, c));
            }
            if let Some(st) = state.as_deref_mut() {
                for (body, asleep) in [(&*a, asleep_a), (&*b, asleep_b)] {
                    if asleep {
                        st.asleep.remove(&body.entity);
                        st.timers.remove(&body.entity);
                    }
                }
            }
            let mat = PhysicsMaterial::combine(a.material, b.material);
            let n = c.normal;
            match (a.dynamic, b.dynamic) {
                (true, false) => {
                    a.pos += n * c.depth;
                    contact_velocity(&mut a.vel, n, mat);
                    a.grounded |= n.y > 0.5;
                }
                (false, true) => {
                    b.pos -= n * c.depth;
                    contact_velocity(&mut b.vel, -n, mat);
                    b.grounded |= -n.y > 0.5;
                }
                _ => {
                    // Equal-mass pair: split the correction, exchange impulse.
                    a.pos += n * (c.depth * 0.5);
                    b.pos -= n * (c.depth * 0.5);
                    let rel = (a.vel - b.vel).dot(n);
                    if rel < 0.0 {
                        let e = if -rel > RESTITUTION_THRESHOLD { mat.restitution } else { 0.0 };
                        let j_n = -(1.0 + e) * rel * 0.5;
                        let vt = (a.vel - b.vel) - n * rel;
                        let vt_len = vt.length();
                        let jt = if vt_len > 1e-6 { (mat.friction * j_n).min(vt_len * 0.5) } else { 0.0 };
                        let tdir = if vt_len > 1e-6 { vt / vt_len } else { Vec3::ZERO };
                        a.vel += n * j_n - tdir * jt;
                        b.vel -= n * j_n - tdir * jt;
                    }
                    a.grounded |= n.y > 0.5;
                    b.grounded |= -n.y > 0.5;
                }
            }
        }
    }

    // 6. Write back dynamic bodies, update sleep state.
    for b in bodies.iter().filter(|b| b.dynamic) {
        let asleep = state.as_ref().map_or(false, |s| s.asleep.contains_key(&b.entity));
        if asleep {
            continue;
        }
        if let Ok(mut q) = world.query_one::<(&mut Transform, &mut RigidBody)>(b.entity) {
            if let Some((t, rb)) = q.get() {
                t.position = b.pos;
                rb.velocity = b.vel;
                rb.grounded = b.grounded;
            }
        }
    }
    if let (Some(cfg), Some(st)) = (sleep_cfg, state.as_deref_mut()) {
        for b in bodies.iter().filter(|b| b.dynamic && !b.trigger) {
            if st.asleep.contains_key(&b.entity) {
                continue;
            }
            if b.vel.length() < cfg.linear_threshold && b.grounded {
                let t = st.timers.entry(b.entity).or_insert(0.0);
                *t += dt;
                if *t >= cfg.time_to_sleep {
                    st.asleep.insert(b.entity, b.pos);
                    if let Ok(mut rb) = world.get::<&mut RigidBody>(b.entity) {
                        rb.velocity = Vec3::ZERO;
                    }
                }
            } else {
                st.timers.remove(&b.entity);
            }
        }
        // Drop bookkeeping for despawned entities.
        st.asleep.retain(|e, _| world.contains(*e));
        st.timers.retain(|e, _| world.contains(*e));
    }
    out
}

/// Velocity response against an immovable surface with normal `n` (pointing at the body).
fn contact_velocity(v: &mut Vec3, n: Vec3, mat: PhysicsMaterial) {
    let vn = v.dot(n);
    if vn >= 0.0 {
        return;
    }
    let vt = *v - n * vn;
    let vt_len = vt.length();
    let vt = if vt_len > 1e-6 {
        let reduce = (mat.friction * -vn).min(vt_len);
        vt * (1.0 - reduce / vt_len)
    } else {
        vt
    };
    let bounce = if -vn > RESTITUTION_THRESHOLD { mat.restitution } else { 0.0 };
    *v = vt - n * (vn * bounce);
}

/// Stateful physics driver: fixed timestep with accumulator, interpolation
/// alpha, sleeping and trigger enter/stay/exit events.
pub struct PhysicsWorld {
    pub params: PhysicsParams,
    pub fixed_dt: f32,
    /// Cap on substeps per `update` (spiral-of-death guard).
    pub max_substeps: u32,
    pub solver_iterations: usize,
    accumulator: f32,
    tick: u64,
    state: SimState,
    active: BTreeSet<(Entity, Entity)>,
    prev: BTreeMap<Entity, Vec3>,
    last_contacts: Vec<(Entity, Entity, super::Contact)>,
}

impl PhysicsWorld {
    pub fn new(params: PhysicsParams) -> Self {
        Self {
            params,
            fixed_dt: 1.0 / 60.0,
            max_substeps: 8,
            solver_iterations: 4,
            accumulator: 0.0,
            tick: 0,
            state: SimState::default(),
            active: BTreeSet::new(),
            prev: BTreeMap::new(),
            last_contacts: Vec::new(),
        }
    }

    pub fn sleep_config(&self) -> SleepConfig {
        self.state.sleep
    }
    pub fn set_sleep_config(&mut self, c: SleepConfig) {
        self.state.sleep = c;
    }
    pub fn tick(&self) -> u64 {
        self.tick
    }
    /// Fraction (0..1) of a fixed step the render frame is past the last simulated step.
    pub fn alpha(&self) -> f32 {
        (self.accumulator / self.fixed_dt).clamp(0.0, 1.0)
    }
    pub fn is_sleeping(&self, e: Entity) -> bool {
        self.state.asleep.contains_key(&e)
    }
    pub fn sleeping_count(&self) -> usize {
        self.state.asleep.len()
    }
    /// Solid contacts found in the most recent substep (for debug drawing).
    pub fn last_contacts(&self) -> &[(Entity, Entity, super::Contact)] {
        &self.last_contacts
    }

    /// Render position blended between the previous and current fixed step.
    pub fn interpolated_position(&self, world: &hecs::World, e: Entity) -> Option<Vec3> {
        let cur = world.get::<&Transform>(e).ok()?.position;
        Some(match self.prev.get(&e) {
            Some(p) => p.lerp(cur, self.alpha()),
            None => cur,
        })
    }

    /// Accumulates `frame_dt` and runs as many fixed steps as fit.
    pub fn update(&mut self, world: &mut hecs::World, frame_dt: f32) -> Vec<PhysicsEvent> {
        self.accumulator += frame_dt.max(0.0);
        let mut events = Vec::new();
        let mut n = 0;
        while self.accumulator >= self.fixed_dt && n < self.max_substeps {
            events.extend(self.step_fixed(world));
            self.accumulator -= self.fixed_dt;
            n += 1;
        }
        if n == self.max_substeps {
            self.accumulator = self.accumulator.min(self.fixed_dt); // drop the backlog
        }
        events
    }

    /// Runs exactly one fixed step, ignoring the accumulator.
    pub fn step_fixed(&mut self, world: &mut hecs::World) -> Vec<PhysicsEvent> {
        self.prev.clear();
        for (e, (t, _)) in world.query::<(&Transform, &RigidBody)>().iter() {
            self.prev.insert(e, t.position);
        }
        let out = run_step(world, self.fixed_dt, &self.params, self.solver_iterations, Some(&mut self.state));
        self.tick += 1;
        self.last_contacts = out.contacts;

        let now: BTreeSet<(Entity, Entity)> = out.trigger_overlaps.into_iter().collect();
        let mut events = Vec::new();
        for &(entity, other) in &now {
            let kind = if self.active.contains(&(entity, other)) { TriggerEventKind::Stay } else { TriggerEventKind::Enter };
            events.push(PhysicsEvent { kind, entity, other });
        }
        for &(entity, other) in self.active.difference(&now) {
            events.push(PhysicsEvent { kind: TriggerEventKind::Exit, entity, other });
        }
        self.active = now;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::*;

    fn ground(w: &mut hecs::World) -> Entity {
        w.spawn((Transform::from_position(Vec3::new(0.0, -0.5, 0.0)), Collider::aabb(Vec3::new(50.0, 0.5, 50.0))))
    }
    fn ball(w: &mut hecs::World, p: Vec3) -> Entity {
        w.spawn((Transform::from_position(p), Collider::sphere(0.5), RigidBody::default()))
    }

    #[test]
    fn legacy_step_lands_and_reports_grounded() {
        let mut w = hecs::World::new();
        ground(&mut w);
        let b = ball(&mut w, Vec3::new(0.0, 2.0, 0.0));
        let p = PhysicsParams::default();
        for _ in 0..240 {
            step(&mut w, 1.0 / 60.0, &p);
        }
        let y = w.get::<&Transform>(b).unwrap().position.y;
        assert!((y - 0.5).abs() < 0.05, "y={y}");
        assert!(w.get::<&RigidBody>(b).unwrap().grounded);
    }

    #[test]
    fn legacy_step_reports_trigger_pairs() {
        let mut w = hecs::World::new();
        let t = w.spawn((Transform::default(), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::ONE })));
        let b = w.spawn((Transform::default(), Collider::sphere(0.5), RigidBody::default()));
        let o = step(&mut w, 0.0, &PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        assert_eq!(o, vec![(b, t)]);
    }

    #[test]
    fn trigger_enter_stay_exit() {
        let mut w = hecs::World::new();
        let t = w.spawn((Transform::default(), Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::ONE })));
        let b = w.spawn((
            Transform::from_position(Vec3::new(0.0, 5.0, 0.0)),
            Collider::sphere(0.25),
            RigidBody::default(),
        ));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        assert!(pw.step_fixed(&mut w).is_empty());
        w.get::<&mut Transform>(b).unwrap().position = Vec3::ZERO;
        let e = pw.step_fixed(&mut w);
        assert_eq!(e, vec![PhysicsEvent { kind: TriggerEventKind::Enter, entity: b, other: t }]);
        assert_eq!(pw.step_fixed(&mut w)[0].kind, TriggerEventKind::Stay);
        w.get::<&mut Transform>(b).unwrap().position = Vec3::new(0.0, 9.0, 0.0);
        assert_eq!(pw.step_fixed(&mut w)[0].kind, TriggerEventKind::Exit);
        assert!(pw.step_fixed(&mut w).is_empty());
    }

    #[test]
    fn layers_filter_collisions() {
        let mut w = hecs::World::new();
        w.spawn((
            Transform::from_position(Vec3::new(0.0, -0.5, 0.0)),
            Collider::aabb(Vec3::new(50.0, 0.5, 50.0)),
            CollisionLayers::new(LayerMask::bit(1), LayerMask::ALL),
        ));
        let ghost = w.spawn((
            Transform::from_position(Vec3::new(0.0, 2.0, 0.0)),
            Collider::sphere(0.5),
            RigidBody::default(),
            CollisionLayers::new(LayerMask::bit(2), LayerMask::bit(3)),
        ));
        let p = PhysicsParams::default();
        for _ in 0..120 {
            step(&mut w, 1.0 / 60.0, &p);
        }
        assert!(w.get::<&Transform>(ghost).unwrap().position.y < -2.0, "should fall through");
    }

    #[test]
    fn restitution_bounces_and_friction_slows() {
        let mut w = hecs::World::new();
        w.spawn((
            Transform::from_position(Vec3::new(0.0, -0.5, 0.0)),
            Collider::aabb(Vec3::new(50.0, 0.5, 50.0)),
            PhysicsMaterial { friction: 1.0, restitution: 0.8 },
        ));
        let b = w.spawn((
            Transform::from_position(Vec3::new(0.0, 0.52, 0.0)),
            Collider::sphere(0.5),
            RigidBody { velocity: Vec3::new(4.0, -6.0, 0.0), grounded: false },
            PhysicsMaterial { friction: 1.0, restitution: 0.8 },
        ));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        pw.step_fixed(&mut w);
        let v = w.get::<&RigidBody>(b).unwrap().velocity;
        assert!(v.y > 4.0, "bounced: {v:?}");
        assert!(v.x < 4.0, "friction slowed x: {v:?}");
    }

    #[test]
    fn stack_settles_and_sleeps() {
        let mut w = hecs::World::new();
        ground(&mut w);
        let a = w.spawn((Transform::from_position(Vec3::new(0.0, 0.5, 0.0)), Collider::aabb(Vec3::splat(0.5)), RigidBody::default()));
        let b = w.spawn((Transform::from_position(Vec3::new(0.0, 1.6, 0.0)), Collider::aabb(Vec3::splat(0.5)), RigidBody::default()));
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        for _ in 0..300 {
            pw.step_fixed(&mut w);
        }
        assert!(pw.is_sleeping(a) && pw.is_sleeping(b), "sleeping a={} b={}", pw.is_sleeping(a), pw.is_sleeping(b));
        let (ya, yb) = (w.get::<&Transform>(a).unwrap().position.y, w.get::<&Transform>(b).unwrap().position.y);
        assert!((ya - 0.5).abs() < 0.05 && (yb - 1.5).abs() < 0.08, "ya={ya} yb={yb}");
        // A falling ball wakes the stack.
        ball(&mut w, Vec3::new(0.0, 2.6, 0.0));
        for _ in 0..40 {
            pw.step_fixed(&mut w);
        }
        assert!(!pw.is_sleeping(b));
    }

    #[test]
    fn fixed_timestep_accumulator_and_alpha() {
        let mut w = hecs::World::new();
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        pw.update(&mut w, 0.010);
        assert_eq!(pw.tick(), 0);
        pw.update(&mut w, 0.010); // 20ms > 16.67ms
        assert_eq!(pw.tick(), 1);
        assert!((pw.alpha() - (0.020 - 1.0 / 60.0) * 60.0).abs() < 1e-4);
        pw.update(&mut w, 10.0); // capped
        assert_eq!(pw.tick(), 1 + pw.max_substeps as u64);
    }

    #[test]
    fn interpolation_between_steps() {
        let mut w = hecs::World::new();
        let b = w.spawn((Transform::default(), Collider::sphere(0.5), RigidBody { velocity: Vec3::new(6.0, 0.0, 0.0), grounded: false }));
        let mut pw = PhysicsWorld::new(PhysicsParams { gravity: 0.0, linear_damping: 0.0 });
        pw.update(&mut w, 1.0 / 60.0 + 1.0 / 120.0); // one step + half
        let cur = w.get::<&Transform>(b).unwrap().position.x;
        let i = pw.interpolated_position(&w, b).unwrap().x;
        assert!((cur - 0.1).abs() < 1e-4);
        assert!((i - 0.05).abs() < 1e-3, "i={i}");
    }

    fn run_scene() -> u64 {
        let mut w = hecs::World::new();
        ground(&mut w);
        w.spawn((Transform::from_position(Vec3::new(3.0, 0.5, 0.0)), Collider::aabb(Vec3::splat(0.5))));
        let mut s = 12345u32;
        let mut rnd = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / (1u32 << 24) as f32
        };
        for i in 0..40 {
            let p = Vec3::new(rnd() * 6.0 - 3.0, 1.0 + i as f32 * 0.7, rnd() * 6.0 - 3.0);
            let shape = match i % 4 {
                0 => ColliderShape::Sphere { radius: 0.4 },
                1 => ColliderShape::Capsule { radius: 0.3, half_height: 0.4 },
                2 => ColliderShape::Box { half_extents: Vec3::splat(0.4) },
                _ => ColliderShape::Aabb { half_extents: Vec3::splat(0.35) },
            };
            w.spawn((
                Transform::from_position(p),
                Collider::new(shape),
                RigidBody { velocity: Vec3::new(rnd() - 0.5, 0.0, rnd() - 0.5), grounded: false },
                PhysicsMaterial { friction: 0.5, restitution: 0.3 },
            ));
        }
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        for _ in 0..1000 {
            pw.step_fixed(&mut w);
        }
        let mut items: Vec<_> = w.query::<(&Transform, &RigidBody)>().iter().map(|(e, (t, _))| (e, t.position)).collect();
        items.sort_by_key(|(e, _)| *e);
        let mut h = 1469598103934665603u64;
        for (_, p) in items {
            for c in p.to_array() {
                h = (h ^ c.to_bits() as u64).wrapping_mul(1099511628211);
            }
        }
        h
    }

    #[test]
    fn deterministic_1000_steps() {
        assert_eq!(run_scene(), run_scene());
    }

    #[test]
    fn five_hundred_body_benchmark() {
        let mut w = hecs::World::new();
        ground(&mut w);
        for i in 0..500 {
            let (x, y, z) = (i % 10, i / 100, (i / 10) % 10);
            let shape = if i % 2 == 0 { ColliderShape::Sphere { radius: 0.4 } } else { ColliderShape::Box { half_extents: Vec3::splat(0.35) } };
            w.spawn((
                Transform::from_position(Vec3::new(x as f32 * 1.0 - 5.0, 1.0 + y as f32 * 1.0, z as f32 * 1.0 - 5.0)),
                Collider::new(shape),
                RigidBody::default(),
            ));
        }
        let mut pw = PhysicsWorld::new(PhysicsParams::default());
        let start = std::time::Instant::now();
        for _ in 0..120 {
            pw.step_fixed(&mut w);
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / 120.0;
        eprintln!("BENCH 500 bodies: {per:.3} ms/step (sleeping {})", pw.sleeping_count());
        assert!(per < 50.0);
    }
}
