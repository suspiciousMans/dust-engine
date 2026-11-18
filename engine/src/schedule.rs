//! A registry of named systems with explicit ordering and run conditions.
//!
//! A *system* is a plain `fn` or closure taking a [`SystemCtx`] (the ECS
//! `World`, the typed [`Resources`] map and the frame `dt`). A [`Schedule`]
//! decides **when** each one runs:
//!
//! * [`Stage`]: coarse, hard ordering (`First < PreUpdate < Update <
//!   PostUpdate < Last`). Default is `Update`.
//! * `before(name)` / `after(name)`: fine ordering by system name, within or
//!   across stages. A constraint that contradicts stage order is a cycle.
//! * Ties (no constraint either way) run in stage order, then registration
//!   order, so the order is fully deterministic.
//! * [`run_if`](SystemConfig::run_if): a predicate over `(&World, &Resources)`
//!   evaluated every run; when false the system is skipped but still
//!   participates in ordering.
//! * [`set_enabled`](Schedule::set_enabled): turn a system off/on by name.
//!
//! The order is computed lazily and cached; cycles, unknown names in
//! `before`/`after`, and duplicate names are reported as [`ScheduleError`]
//! from [`Schedule::build`] / [`Schedule::run`] — never a panic.
//!
//! The schedule does not own the world, and it does not touch the `Game`
//! trait or the app loop: a game keeps its `World` and `Resources` and calls
//! `schedule.run(..)` from its own `update`, migrating one feature at a time.
//!
//! ```
//! use dust_engine::ecs::World;
//! use dust_engine::resources::Resources;
//! use dust_engine::schedule::{Schedule, Stage, SystemCtx};
//!
//! struct Ticks(u32);
//! #[derive(Clone, Copy, PartialEq)]
//! enum Mode { Edit, Play }
//!
//! fn count(ctx: &mut SystemCtx) { ctx.res_mut::<Ticks>().0 += 1; }
//!
//! let mut schedule = Schedule::new();
//! schedule
//!     .add_system("count", count)
//!     .stage(Stage::Update)
//!     .run_if_resource::<Mode>(|m| *m == Mode::Play);
//!
//! let (mut world, mut res) = (World::new(), Resources::new());
//! res.insert(Ticks(0));
//! res.insert(Mode::Edit);
//! schedule.run(&mut world, &mut res, 1.0 / 60.0).unwrap();
//! assert_eq!(res.require::<Ticks>().0, 0); // skipped in Edit mode
//! res.insert(Mode::Play);
//! schedule.run(&mut world, &mut res, 1.0 / 60.0).unwrap();
//! assert_eq!(res.require::<Ticks>().0, 1);
//! ```

use std::collections::BTreeSet;

use crate::ecs::World;
use crate::resources::Resources;

/// Everything a system can touch for one run.
pub struct SystemCtx<'a> {
    pub world: &'a mut World,
    pub resources: &'a mut Resources,
    /// Seconds since the previous `Schedule::run`.
    pub dt: f32,
}

impl SystemCtx<'_> {
    /// Shorthand for `self.resources.require::<T>()` (panics if missing).
    #[track_caller]
    pub fn res<T: 'static>(&self) -> &T {
        self.resources.require::<T>()
    }

    /// Shorthand for `self.resources.require_mut::<T>()` (panics if missing).
    #[track_caller]
    pub fn res_mut<T: 'static>(&mut self) -> &mut T {
        self.resources.require_mut::<T>()
    }
}

/// Coarse execution phase. Every system in an earlier stage runs before every
/// system in a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Stage {
    First,
    PreUpdate,
    #[default]
    Update,
    PostUpdate,
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("system `{0}` is already registered")]
    DuplicateSystem(String),
    #[error("system `{system}` has a before/after constraint on unknown system `{referenced}`")]
    UnknownSystem { system: String, referenced: String },
    /// Names in execution-constraint order; the last wraps back to the first.
    #[error("ordering cycle (stage order and before/after constraints): {} -> {}", .0.join(" -> "), .0[0])]
    Cycle(Vec<String>),
}

type SystemFn = Box<dyn FnMut(&mut SystemCtx<'_>)>;
type Condition = Box<dyn FnMut(&World, &Resources) -> bool>;

struct Entry {
    name: String,
    stage: Stage,
    before: Vec<String>,
    after: Vec<String>,
    condition: Option<Condition>,
    enabled: bool,
    run: SystemFn,
}

#[derive(Default)]
pub struct Schedule {
    systems: Vec<Entry>,
    /// Indices into `systems` in execution order; `None` when stale.
    order: Option<Vec<usize>>,
}

/// Chainable configuration for a just-added system.
pub struct SystemConfig<'a> {
    schedule: &'a mut Schedule,
    index: usize,
}

impl SystemConfig<'_> {
    fn entry(&mut self) -> &mut Entry {
        self.schedule.order = None;
        &mut self.schedule.systems[self.index]
    }

    pub fn stage(mut self, stage: Stage) -> Self {
        self.entry().stage = stage;
        self
    }

    /// This system runs before the system called `other`.
    pub fn before(mut self, other: &str) -> Self {
        self.entry().before.push(other.to_string());
        self
    }

    /// This system runs after the system called `other`.
    pub fn after(mut self, other: &str) -> Self {
        self.entry().after.push(other.to_string());
        self
    }

    /// Only run while `cond(world, resources)` is true. Replaces any earlier
    /// condition.
    pub fn run_if(mut self, cond: impl FnMut(&World, &Resources) -> bool + 'static) -> Self {
        self.entry().condition = Some(Box::new(cond));
        self
    }

    /// Only run while resource `T` exists and `pred(&T)` is true (for example
    /// "only in Play mode"). A missing resource counts as false.
    pub fn run_if_resource<T: 'static>(self, mut pred: impl FnMut(&T) -> bool + 'static) -> Self {
        self.run_if(move |_, res| res.get::<T>().is_some_and(&mut pred))
    }

    /// Starts disabled (see [`Schedule::set_enabled`]).
    pub fn disabled(mut self) -> Self {
        self.entry().enabled = false;
        self
    }
}

impl Schedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a system. Fails only if the name is already taken.
    pub fn try_add_system(
        &mut self,
        name: &str,
        system: impl FnMut(&mut SystemCtx<'_>) + 'static,
    ) -> Result<SystemConfig<'_>, ScheduleError> {
        if self.index_of(name).is_some() {
            return Err(ScheduleError::DuplicateSystem(name.to_string()));
        }
        self.systems.push(Entry {
            name: name.to_string(),
            stage: Stage::default(),
            before: Vec::new(),
            after: Vec::new(),
            condition: None,
            enabled: true,
            run: Box::new(system),
        });
        self.order = None;
        let index = self.systems.len() - 1;
        Ok(SystemConfig { schedule: self, index })
    }

    /// Like [`try_add_system`](Self::try_add_system) but panics on a duplicate
    /// name (a programming error at registration time).
    #[track_caller]
    pub fn add_system(
        &mut self,
        name: &str,
        system: impl FnMut(&mut SystemCtx<'_>) + 'static,
    ) -> SystemConfig<'_> {
        match self.try_add_system(name, system) {
            Ok(cfg) => cfg,
            Err(e) => panic!("{e}"),
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.systems.iter().position(|s| s.name == name)
    }

    pub fn len(&self) -> usize {
        self.systems.len()
    }

    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    /// Enables or disables a system. Returns false if no such system exists.
    /// Disabled systems are skipped but keep their place in the ordering.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.index_of(name) {
            Some(i) => {
                self.systems[i].enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// `None` if no such system exists.
    pub fn is_enabled(&self, name: &str) -> Option<bool> {
        self.index_of(name).map(|i| self.systems[i].enabled)
    }

    /// Removes a system by name; returns whether it existed. Other systems'
    /// `before`/`after` references to it become `UnknownSystem` errors.
    pub fn remove_system(&mut self, name: &str) -> bool {
        match self.index_of(name) {
            Some(i) => {
                self.systems.remove(i);
                self.order = None;
                true
            }
            None => false,
        }
    }

    /// Computes (and caches) the execution order, returning system names.
    pub fn build(&mut self) -> Result<Vec<&str>, ScheduleError> {
        self.ensure_order()?;
        let order = self.order.as_ref().expect("order was just built");
        Ok(order.iter().map(|&i| self.systems[i].name.as_str()).collect())
    }

    fn ensure_order(&mut self) -> Result<(), ScheduleError> {
        if self.order.is_none() {
            self.order = Some(self.compute_order()?);
        }
        Ok(())
    }

    fn compute_order(&self) -> Result<Vec<usize>, ScheduleError> {
        let n = self.systems.len();
        let mut succs: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
        let mut edge = |from: usize, to: usize| {
            succs[from].insert(to);
            preds[to].insert(from);
        };

        for (i, a) in self.systems.iter().enumerate() {
            for (j, b) in self.systems.iter().enumerate() {
                if a.stage < b.stage {
                    edge(i, j);
                }
            }
            for other in &a.before {
                edge(i, self.resolve(a, other)?);
            }
            for other in &a.after {
                edge(self.resolve(a, other)?, i);
            }
        }

        // Kahn's algorithm; among ready systems always take the lowest
        // (stage, registration index), which makes ties deterministic.
        let mut indegree: Vec<usize> = preds.iter().map(BTreeSet::len).collect();
        let mut ready: BTreeSet<(Stage, usize)> = (0..n)
            .filter(|&i| indegree[i] == 0)
            .map(|i| (self.systems[i].stage, i))
            .collect();
        let mut order = Vec::with_capacity(n);
        while let Some((_, i)) = ready.pop_first() {
            order.push(i);
            for &j in &succs[i] {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    ready.insert((self.systems[j].stage, j));
                }
            }
        }

        if order.len() == n {
            return Ok(order);
        }

        // Every unscheduled node has an unscheduled predecessor, so walking
        // predecessors from the lowest one must revisit a node: that's a cycle.
        let stuck = |i: usize| indegree[i] > 0;
        let start = (0..n).find(|&i| stuck(i)).expect("order shorter than n implies a stuck node");
        let mut path = vec![start];
        loop {
            let cur = *path.last().expect("path is never empty");
            let prev = *preds[cur].iter().find(|&&p| stuck(p)).expect("stuck node has a stuck predecessor");
            if let Some(pos) = path.iter().position(|&x| x == prev) {
                let mut cycle: Vec<usize> = path[pos..].to_vec();
                cycle.reverse(); // predecessors were walked; present in run order
                let lowest = (0..cycle.len()).min_by_key(|&k| cycle[k]).expect("cycle is non-empty");
                cycle.rotate_left(lowest); // start at the earliest-registered member
                let names = cycle.iter().map(|&i| self.systems[i].name.clone()).collect();
                return Err(ScheduleError::Cycle(names));
            }
            path.push(prev);
        }
    }

    fn resolve(&self, from: &Entry, other: &str) -> Result<usize, ScheduleError> {
        self.index_of(other).ok_or_else(|| ScheduleError::UnknownSystem {
            system: from.name.clone(),
            referenced: other.to_string(),
        })
    }

    /// Runs every enabled system whose condition (if any) holds, in order.
    /// Returns how many systems actually ran. Errors (before running anything)
    /// if the ordering can't be built.
    pub fn run(&mut self, world: &mut World, resources: &mut Resources, dt: f32) -> Result<usize, ScheduleError> {
        self.ensure_order()?;
        let order = self.order.as_ref().expect("order was just built");
        let mut ran = 0;
        for &i in order {
            let entry = &mut self.systems[i];
            if !entry.enabled {
                continue;
            }
            if let Some(cond) = entry.condition.as_mut() {
                if !cond(world, resources) {
                    continue;
                }
            }
            (entry.run)(&mut SystemCtx { world: &mut *world, resources: &mut *resources, dt });
            ran += 1;
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<&'static str>>>;

    fn logger(log: &Log, tag: &'static str) -> impl FnMut(&mut SystemCtx<'_>) + 'static {
        let log = Rc::clone(log);
        move |_| log.borrow_mut().push(tag)
    }

    fn run(s: &mut Schedule) -> Result<usize, ScheduleError> {
        s.run(&mut World::new(), &mut Resources::new(), 0.016)
    }

    fn new_log() -> Log {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn stages_order_systems_regardless_of_registration() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("last", logger(&log, "last")).stage(Stage::Last);
        s.add_system("update", logger(&log, "update"));
        s.add_system("first", logger(&log, "first")).stage(Stage::First);
        s.add_system("post", logger(&log, "post")).stage(Stage::PostUpdate);
        s.add_system("pre", logger(&log, "pre")).stage(Stage::PreUpdate);
        assert_eq!(run(&mut s), Ok(5));
        assert_eq!(*log.borrow(), ["first", "pre", "update", "post", "last"]);
    }

    #[test]
    fn before_and_after_constraints_are_respected() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("c", logger(&log, "c")).after("b");
        s.add_system("a", logger(&log, "a")).before("b");
        s.add_system("b", logger(&log, "b"));
        s.add_system("d", logger(&log, "d")).before("a"); // forward reference
        run(&mut s).unwrap();
        assert_eq!(*log.borrow(), ["d", "a", "b", "c"]);
    }

    #[test]
    fn constraints_apply_within_stage_and_stage_wins_across() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("x", logger(&log, "x")).stage(Stage::PostUpdate);
        s.add_system("y", logger(&log, "y")).stage(Stage::PostUpdate).before("x");
        s.add_system("z", logger(&log, "z")); // Update: before both despite order
        run(&mut s).unwrap();
        assert_eq!(*log.borrow(), ["z", "y", "x"]);
    }

    #[test]
    fn ties_run_in_registration_order_and_are_deterministic() {
        for _ in 0..20 {
            let log = new_log();
            let mut s = Schedule::new();
            let tags = ["t0", "t1", "t2", "t3", "t4", "t5"];
            for t in tags {
                s.add_system(t, logger(&log, t));
            }
            run(&mut s).unwrap();
            assert_eq!(*log.borrow(), tags);
        }
    }

    #[test]
    fn unconstrained_ties_do_not_jump_ahead_of_constrained_ones() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("a", logger(&log, "a")).after("c");
        s.add_system("b", logger(&log, "b"));
        s.add_system("c", logger(&log, "c"));
        run(&mut s).unwrap();
        // b and c are ready first (registration order), then a.
        assert_eq!(*log.borrow(), ["b", "c", "a"]);
    }

    #[test]
    fn cycle_is_an_error_naming_the_systems_and_nothing_runs() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("ok", logger(&log, "ok"));
        s.add_system("a", logger(&log, "a")).after("c");
        s.add_system("b", logger(&log, "b")).after("a");
        s.add_system("c", logger(&log, "c")).after("b");
        let err = run(&mut s).unwrap_err();
        match &err {
            ScheduleError::Cycle(names) => {
                let mut sorted = names.clone();
                sorted.sort();
                assert_eq!(sorted, ["a", "b", "c"]);
                // listed in run order starting from the earliest-registered member
                assert_eq!(names, &["a", "b", "c"]);
            }
            other => panic!("expected cycle, got {other:?}"),
        }
        assert!(err.to_string().contains("a -> b -> c -> a"));
        assert!(log.borrow().is_empty());
        assert!(s.build().is_err());
    }

    #[test]
    fn self_constraint_and_stage_contradiction_are_cycles() {
        let mut s = Schedule::new();
        s.add_system("me", |_| {}).before("me");
        assert_eq!(s.build().unwrap_err(), ScheduleError::Cycle(vec!["me".into()]));

        let mut s = Schedule::new();
        s.add_system("late", |_| {}).stage(Stage::PostUpdate).before("early");
        s.add_system("early", |_| {}).stage(Stage::PreUpdate);
        assert!(matches!(s.build(), Err(ScheduleError::Cycle(_))));
    }

    #[test]
    fn fixing_a_cycle_by_removing_a_system_recovers() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("a", logger(&log, "a")).after("b");
        s.add_system("b", logger(&log, "b")).after("a");
        assert!(run(&mut s).is_err());
        s.remove_system("b");
        // `a` still names `b`: now an unknown-system error, not a cycle.
        assert_eq!(
            run(&mut s).unwrap_err(),
            ScheduleError::UnknownSystem { system: "a".into(), referenced: "b".into() }
        );
    }

    #[test]
    fn unknown_reference_and_duplicate_name_are_errors() {
        let mut s = Schedule::new();
        s.add_system("a", |_| {}).before("ghost");
        assert_eq!(
            s.build().unwrap_err(),
            ScheduleError::UnknownSystem { system: "a".into(), referenced: "ghost".into() }
        );
        assert!(matches!(s.try_add_system("a", |_| {}), Err(ScheduleError::DuplicateSystem(n)) if n == "a"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    #[should_panic(expected = "system `a` is already registered")]
    fn add_system_panics_on_duplicate() {
        let mut s = Schedule::new();
        s.add_system("a", |_| {});
        s.add_system("a", |_| {});
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Mode {
        Edit,
        Play,
    }

    #[test]
    fn run_conditions_skip_systems() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("always", logger(&log, "always"));
        s.add_system("play_only", logger(&log, "play")).run_if_resource::<Mode>(|m| *m == Mode::Play);
        s.add_system("custom", logger(&log, "custom")).run_if(|_, res| res.contains::<u8>());

        let mut world = World::new();
        let mut res = Resources::new();

        // No Mode resource at all: conditional system is skipped.
        assert_eq!(s.run(&mut world, &mut res, 0.0), Ok(1));
        res.insert(Mode::Edit);
        assert_eq!(s.run(&mut world, &mut res, 0.0), Ok(1));
        res.insert(Mode::Play);
        assert_eq!(s.run(&mut world, &mut res, 0.0), Ok(2));
        res.insert(1u8);
        assert_eq!(s.run(&mut world, &mut res, 0.0), Ok(3));
        assert_eq!(
            *log.borrow(),
            ["always", "always", "always", "play", "always", "play", "custom"]
        );
    }

    #[test]
    fn condition_can_read_world() {
        let ran = Rc::new(RefCell::new(0));
        let r = Rc::clone(&ran);
        let mut s = Schedule::new();
        s.add_system("needs_entities", move |_| *r.borrow_mut() += 1).run_if(|w, _| w.len() > 0);
        let mut world = World::new();
        let mut res = Resources::new();
        s.run(&mut world, &mut res, 0.0).unwrap();
        world.spawn((1u32,));
        s.run(&mut world, &mut res, 0.0).unwrap();
        assert_eq!(*ran.borrow(), 1);
    }

    #[test]
    fn disabling_skips_but_keeps_ordering_and_can_reenable() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("a", logger(&log, "a"));
        s.add_system("b", logger(&log, "b")).after("a");
        s.add_system("c", logger(&log, "c")).after("b").disabled();
        assert_eq!(s.is_enabled("c"), Some(false));
        assert_eq!(run(&mut s), Ok(2));

        assert!(s.set_enabled("a", false));
        assert!(!s.set_enabled("nope", false));
        assert_eq!(s.is_enabled("nope"), None);
        assert_eq!(run(&mut s), Ok(1));

        s.set_enabled("a", true);
        s.set_enabled("c", true);
        assert_eq!(run(&mut s), Ok(3));
        assert_eq!(*log.borrow(), ["a", "b", "b", "a", "b", "c"]);
    }

    #[test]
    fn systems_mutate_world_and_resources_through_context() {
        #[derive(Debug, PartialEq)]
        struct Pos(f32);
        struct Elapsed(f32);

        fn integrate(ctx: &mut SystemCtx) {
            let dt = ctx.dt;
            for (_, p) in ctx.world.query_mut::<&mut Pos>() {
                p.0 += 10.0 * dt;
            }
            ctx.res_mut::<Elapsed>().0 += dt;
        }

        let mut world = World::new();
        let e = world.spawn((Pos(0.0),));
        let mut res = Resources::new();
        res.insert(Elapsed(0.0));
        let mut s = Schedule::new();
        s.add_system("integrate", integrate);
        s.run(&mut world, &mut res, 0.5).unwrap();
        s.run(&mut world, &mut res, 0.5).unwrap();
        assert_eq!(world.get::<&Pos>(e).unwrap().0, 10.0);
        assert_eq!(res.require::<Elapsed>().0, 1.0);
    }

    #[test]
    fn later_system_sees_earlier_systems_writes() {
        struct Value(i32);
        let mut s = Schedule::new();
        s.add_system("double", |c| c.res_mut::<Value>().0 *= 2).after("add_one");
        s.add_system("add_one", |c| c.res_mut::<Value>().0 += 1);
        let mut res = Resources::new();
        res.insert(Value(4));
        s.run(&mut World::new(), &mut res, 0.0).unwrap();
        assert_eq!(res.require::<Value>().0, 10);
    }

    #[test]
    fn system_can_add_entities_during_run() {
        struct Spawned;
        let mut s = Schedule::new();
        s.add_system("spawner", |c| {
            c.world.spawn((Spawned,));
            c.world.spawn((Spawned,));
        });
        s.add_system("counter", |c| {
            let n = c.world.query::<&Spawned>().iter().count() as u32;
            c.resources.insert(n);
        })
        .after("spawner");

        let mut world = World::new();
        let mut res = Resources::new();
        s.run(&mut world, &mut res, 0.0).unwrap();
        assert_eq!(res.get::<u32>(), Some(&2));
        s.run(&mut world, &mut res, 0.0).unwrap();
        assert_eq!(res.get::<u32>(), Some(&4));
        assert_eq!(world.len(), 4);
    }

    #[test]
    fn order_cache_is_invalidated_by_new_registrations() {
        let log = new_log();
        let mut s = Schedule::new();
        s.add_system("a", logger(&log, "a"));
        run(&mut s).unwrap();
        s.add_system("b", logger(&log, "b")).before("a");
        run(&mut s).unwrap();
        assert_eq!(*log.borrow(), ["a", "b", "a"]);
        assert_eq!(s.build().unwrap(), ["b", "a"]);
    }

    #[test]
    fn empty_schedule_runs_nothing() {
        let mut s = Schedule::new();
        assert!(s.is_empty());
        assert_eq!(run(&mut s), Ok(0));
    }
}
