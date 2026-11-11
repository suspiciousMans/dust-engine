//! Undoable prefab operations (create / apply / revert / unpack).
//!
//! `prefab.rs` implements these as plain functions over the world and the
//! library. [`PrefabOpCommand`] wraps them so they go through the editor's
//! undo stack: it snapshots the affected entities (and the prefab file the
//! operation writes) before and after, and undo/redo put those exact states
//! back. Entity handles stay valid across undo/redo, so commands recorded
//! earlier or later in the history keep pointing at the right entities.
//!
//! Scope of a snapshot: every instance the operation can touch, plus the
//! parent of each instance root (its `Children` list changes when a wrapper
//! root is inserted). Components the registry doesn't know are left alone,
//! like everywhere else in the editor's undo.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use hecs::{BuiltEntityClone, Entity, World};

use super::command::{Command, Ctx};
use super::hierarchy::{self, Children, Locked, Name, Parent, SpawnOrder, Visible};
use super::inspector::Registry;
use super::prefab::{self, PrefabInstance, PrefabMember, SharedLibrary};
use super::scene::{EntityId, Scene, UnknownComponents};

#[derive(Clone, Debug, PartialEq)]
pub enum PrefabOp {
    /// Turn `root`'s subtree into the prefab file `source` and make it an instance.
    Create { root: Entity, source: String },
    /// Write the instance back into its prefab ("Apply All").
    Apply { root: Entity },
    /// Drop all overrides but the root placement ("Revert All").
    RevertAll { root: Entity },
    RevertField { root: Entity, target: EntityId, comp: String, field: String },
    Unpack { root: Entity, completely: bool },
}

impl PrefabOp {
    pub fn root(&self) -> Entity {
        match self {
            Self::Create { root, .. }
            | Self::Apply { root }
            | Self::RevertAll { root }
            | Self::RevertField { root, .. }
            | Self::Unpack { root, .. } => *root,
        }
    }

    fn label(&self, world: &World) -> String {
        let name = hierarchy::display_name(world, self.root());
        match self {
            Self::Create { .. } => format!("Create Prefab {name}"),
            Self::Apply { .. } => format!("Apply Prefab {name}"),
            Self::RevertAll { .. } => format!("Revert Prefab {name}"),
            Self::RevertField { comp, field, .. } => format!("Revert {comp}.{field}"),
            Self::Unpack { .. } => format!("Unpack {name}"),
        }
    }

    /// Prefab file this operation writes.
    fn file_source(&self, world: &World) -> Option<String> {
        match self {
            Self::Create { source, .. } => Some(source.clone()),
            Self::Apply { root } => prefab::instance_of(world, *root).map(|i| i.source),
            _ => None,
        }
    }

    /// Instance roots whose subtrees the operation can modify.
    fn touched_roots(&self, world: &World) -> Vec<Entity> {
        match self {
            Self::Apply { root } => {
                let Some(src) = prefab::instance_of(world, *root).map(|i| i.source) else { return vec![*root] };
                let mut v = vec![*root];
                v.extend(
                    prefab::instance_roots(world)
                        .into_iter()
                        .filter(|r| r != root && prefab::instance_of(world, *r).is_some_and(|i| i.source == src)),
                );
                v
            }
            other => vec![other.root()],
        }
    }
}

/// Which entity an operation produced (`Create`: the instance root, which is
/// a new wrapper when the selection was already an instance).
#[derive(Clone, Default)]
pub struct OpHandle(Rc<Cell<Option<Entity>>>);

impl OpHandle {
    pub fn get(&self) -> Option<Entity> {
        self.0.get()
    }
}

// ----------------------------------------------------------------- snapshots

struct EntitySnap {
    entity: Entity,
    built: BuiltEntityClone,
    registered: Vec<&'static str>,
}

struct Snap(Vec<EntitySnap>);

fn capture(world: &World, reg: &Registry, set: &[Entity]) -> Snap {
    Snap(
        set.iter()
            .filter_map(|&e| {
                let built = reg.snapshot(world, e)?;
                let registered = reg.present(world, e).iter().map(|c| c.name).collect();
                Some(EntitySnap { entity: e, built, registered })
            })
            .collect(),
    )
}

/// Components the prefab operations add, change or remove outside the
/// registry's field-level components.
fn strip_structural(world: &mut World, e: Entity) {
    let _ = world.remove_one::<Name>(e);
    let _ = world.remove_one::<Parent>(e);
    let _ = world.remove_one::<Children>(e);
    let _ = world.remove_one::<Visible>(e);
    let _ = world.remove_one::<Locked>(e);
    let _ = world.remove_one::<SpawnOrder>(e);
    let _ = world.remove_one::<EntityId>(e);
    let _ = world.remove_one::<UnknownComponents>(e);
    let _ = world.remove_one::<PrefabInstance>(e);
    let _ = world.remove_one::<PrefabMember>(e);
}

/// Puts `snap` back. Entities in `universe` that the snapshot doesn't have
/// are despawned; missing ones are respawned under their old handle.
fn restore(world: &mut World, reg: &Registry, snap: &Snap, universe: &HashSet<Entity>) {
    let keep: HashSet<Entity> = snap.0.iter().map(|s| s.entity).collect();
    for e in universe {
        if !keep.contains(e) && world.contains(*e) {
            let _ = world.despawn(*e);
        }
    }
    for s in &snap.0 {
        if world.contains(s.entity) {
            let added: Vec<&'static str> = reg
                .present(world, s.entity)
                .iter()
                .map(|c| c.name)
                .filter(|n| !s.registered.contains(n))
                .collect();
            for n in added {
                reg.remove_component(world, s.entity, n);
            }
            strip_structural(world, s.entity);
            let _ = world.insert(s.entity, &s.built);
        } else {
            world.spawn_at(s.entity, &s.built);
        }
    }
}

/// `subtree(parent-or-self)` of every live root, de-duplicated.
fn scope(world: &World, roots: &[Entity]) -> Vec<Entity> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for &r in roots.iter().filter(|r| world.contains(**r)) {
        let top = hierarchy::parent_of(world, r).unwrap_or(r);
        for e in hierarchy::subtree(world, top) {
            if seen.insert(e) {
                out.push(e);
            }
        }
    }
    out
}

struct FileChange {
    source: String,
    before: Option<Scene>,
    after: Option<Scene>,
}

fn write_file(lib: &SharedLibrary, source: &str, content: Option<&Scene>) {
    let mut lib = lib.borrow_mut();
    match content {
        Some(s) => {
            let _ = lib.save(source, s);
        }
        None => {
            let _ = std::fs::remove_file(lib.path_of(source));
            lib.invalidate(source);
        }
    }
}

struct Done {
    before: Snap,
    after: Snap,
    universe: HashSet<Entity>,
    file: Option<FileChange>,
}

// ------------------------------------------------------------------ command

pub struct PrefabOpCommand {
    lib: SharedLibrary,
    op: PrefabOp,
    label: String,
    result: OpHandle,
    done: Option<Done>,
}

impl PrefabOpCommand {
    pub fn new(lib: SharedLibrary, op: PrefabOp, world: &World) -> (Self, OpHandle) {
        let result = OpHandle::default();
        let label = op.label(world);
        (Self { lib, op, label, result: result.clone(), done: None }, result)
    }

    fn run(&self, ctx: &mut Ctx) -> Result<Option<Entity>, String> {
        let mut lib = self.lib.borrow_mut();
        let (world, reg) = (&mut *ctx.world, ctx.registry);
        match &self.op {
            PrefabOp::Create { root, source } => {
                if lib.path_of(source).exists() {
                    return Err(format!("prefab '{source}' already exists"));
                }
                prefab::create_from_selection(world, reg, &mut lib, *root, source).map(Some).map_err(|e| e.to_string())
            }
            PrefabOp::Apply { root } => {
                prefab::apply_to_prefab(world, reg, &mut lib, *root).map(|_| Some(*root)).map_err(|e| e.to_string())
            }
            PrefabOp::RevertAll { root } => {
                prefab::revert_instance(world, reg, &mut lib, *root).map(|_| Some(*root)).map_err(|e| e.to_string())
            }
            PrefabOp::RevertField { root, target, comp, field } => {
                match prefab::revert_field(world, reg, &mut lib, *root, *target, comp, field) {
                    Ok(true) => Ok(Some(*root)),
                    Ok(false) => Err(format!("{comp}.{field} is not overridden")),
                    Err(e) => Err(e.to_string()),
                }
            }
            PrefabOp::Unpack { root, completely } => {
                if prefab::instance_of(world, *root).is_none() {
                    return Err("not a prefab instance".into());
                }
                if *completely {
                    prefab::unpack_completely(world, *root);
                } else {
                    prefab::unpack(world, *root);
                }
                Ok(Some(*root))
            }
        }
    }
}

impl Command for PrefabOpCommand {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        if let Some(d) = &self.done {
            // redo: exact states, same handles
            restore(ctx.world, ctx.registry, &d.after, &d.universe);
            if let Some(f) = &d.file {
                write_file(&self.lib, &f.source, f.after.as_ref());
            }
            return Ok(());
        }
        let roots = self.op.touched_roots(ctx.world);
        let before_set = scope(ctx.world, &roots);
        let before = capture(ctx.world, ctx.registry, &before_set);
        let source = self.op.file_source(ctx.world);
        let file_before = source.as_ref().and_then(|s| self.lib.borrow_mut().get(s).ok());

        let produced = self.run(ctx);

        // Everything that may have changed: the old scope that still exists
        // plus the (re-evaluated) scope of the roots, which now includes any
        // wrapper / spawned members.
        let mut after_set: Vec<Entity> = before_set.iter().copied().filter(|e| ctx.world.contains(*e)).collect();
        let mut seen: HashSet<Entity> = after_set.iter().copied().collect();
        for e in scope(ctx.world, &roots) {
            if seen.insert(e) {
                after_set.push(e);
            }
        }
        let mut universe: HashSet<Entity> = before_set.iter().copied().collect();
        universe.extend(after_set.iter().copied());

        match produced {
            Err(e) => {
                restore(ctx.world, ctx.registry, &before, &universe);
                Err(e)
            }
            Ok(res) => {
                let after = capture(ctx.world, ctx.registry, &after_set);
                let file = source.map(|s| {
                    let after = self.lib.borrow_mut().get(&s).ok();
                    FileChange { source: s, before: file_before, after }
                });
                self.result.0.set(res);
                self.done = Some(Done { before, after, universe, file });
                Ok(())
            }
        }
    }

    fn undo(&mut self, ctx: &mut Ctx) {
        let Some(d) = &self.done else { return };
        restore(ctx.world, ctx.registry, &d.before, &d.universe);
        if let Some(f) = &d.file {
            write_file(&self.lib, &f.source, f.before.as_ref());
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
