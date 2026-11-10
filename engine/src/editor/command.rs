//! Command-pattern undo/redo.
//!
//! Every user-visible edit is a [`Command`]. `execute` must be repeatable as
//! "redo": it re-applies the same change, re-creating deleted entities with
//! their original [`Entity`] handles so later commands in the history still
//! point at the right things.
//!
//! Limitation: only components known to the [`Registry`] (registered or
//! cloneable) survive delete-undo and duplicate. Unregistered components are
//! lost; register them (see `ecs::register_engine_components`).

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;

use glam::Vec3;
use hecs::{BuiltEntityClone, Entity, World};

use super::hierarchy::{
    self, children, delete_subtree, display_name, duplicate_subtree, parent_of, sibling_index,
    spawn_object, subtree, Children, Locked, Name, PrimitiveShape, SpawnOrder, Visible,
};
use super::inspector::{ComponentSnapshot, Registry, Value};

/// What commands operate on.
pub struct Ctx<'a> {
    pub world: &'a mut World,
    pub registry: &'a Registry,
}

pub trait Command: Any {
    /// Shown in "Undo <label>" menus.
    fn label(&self) -> String;
    /// Apply (first run and every redo). `Err` means nothing was changed and
    /// the command is not recorded.
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String>;
    fn undo(&mut self, ctx: &mut Ctx);
    /// Called on the *previous* command with the command that was just
    /// executed. Return true to absorb `next` (the stack then drops it), so a
    /// slider drag becomes one undo step.
    fn merge_with(&mut self, _next: &dyn Command) -> bool {
        false
    }
    fn as_any(&self) -> &dyn Any;
}

// ------------------------------------------------------------- UndoStack

pub struct UndoStack {
    undo: Vec<Box<dyn Command>>,
    redo: Vec<Box<dyn Command>>,
    /// `undo.len()` at the last save; `None` if that state is unreachable.
    saved: Option<usize>,
    /// When set, the next command will not merge into the previous one.
    sealed: bool,
    txn: Option<(String, Vec<Box<dyn Command>>)>,
    limit: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), saved: Some(0), sealed: true, txn: None, limit: 500 }
    }
}

impl UndoStack {
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() && self.txn.is_none()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty() && self.txn.is_none()
    }
    pub fn undo_label(&self) -> Option<String> {
        self.undo.last().map(|c| c.label())
    }
    pub fn redo_label(&self) -> Option<String> {
        self.redo.last().map(|c| c.label())
    }
    /// Labels oldest -> newest, for a history panel.
    pub fn history(&self) -> Vec<String> {
        self.undo.iter().map(|c| c.label()).collect()
    }
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(1);
        self.trim();
    }

    /// Ends the current merge session: the next command starts a new step.
    /// Call when a drag/typing interaction finishes (mouse released, focus lost).
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.saved != Some(self.undo.len())
    }
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.undo.len());
        self.sealed = true;
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.txn = None;
        self.saved = Some(0);
        self.sealed = true;
    }

    fn trim(&mut self) {
        while self.undo.len() > self.limit {
            self.undo.remove(0);
            self.saved = self.saved.and_then(|s| s.checked_sub(1));
        }
    }

    /// Runs `cmd`. On `Err` nothing changed and nothing is recorded.
    pub fn execute(&mut self, mut cmd: Box<dyn Command>, ctx: &mut Ctx) -> Result<(), String> {
        cmd.execute(ctx)?;
        if let Some((_, pending)) = &mut self.txn {
            pending.push(cmd);
            return Ok(());
        }
        // Redo branch is gone after a new edit; the saved state may be in it.
        let had_redo = !self.redo.is_empty();
        if had_redo {
            self.redo.clear();
            if self.saved.map(|s| s > self.undo.len()).unwrap_or(false) {
                self.saved = None;
            }
        }
        if !self.sealed && !had_redo {
            if let Some(last) = self.undo.last_mut() {
                if last.merge_with(cmd.as_ref()) {
                    if self.saved == Some(self.undo.len()) {
                        self.saved = None;
                    }
                    return Ok(());
                }
            }
        }
        self.undo.push(cmd);
        self.sealed = false;
        self.trim();
        Ok(())
    }

    pub fn undo(&mut self, ctx: &mut Ctx) -> bool {
        if self.txn.is_some() {
            return false;
        }
        match self.undo.pop() {
            Some(mut c) => {
                c.undo(ctx);
                self.redo.push(c);
                self.sealed = true;
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, ctx: &mut Ctx) -> bool {
        if self.txn.is_some() {
            return false;
        }
        match self.redo.pop() {
            Some(mut c) => {
                self.sealed = true;
                if c.execute(ctx).is_ok() {
                    self.undo.push(c);
                    true
                } else {
                    // History no longer applies cleanly; drop the rest.
                    self.redo.clear();
                    false
                }
            }
            None => false,
        }
    }

    // --- transactions: several commands = one undo step

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Starts grouping. Nested calls are ignored (the outer group wins).
    pub fn begin_transaction(&mut self, label: impl Into<String>) {
        if self.txn.is_none() {
            self.txn = Some((label.into(), Vec::new()));
        }
    }

    /// Commits the group as one undo step (nothing recorded if it is empty).
    pub fn end_transaction(&mut self) {
        if let Some((label, cmds)) = self.txn.take() {
            if cmds.is_empty() {
                return;
            }
            if !self.redo.is_empty() {
                self.redo.clear();
                if self.saved.map(|s| s > self.undo.len()).unwrap_or(false) {
                    self.saved = None;
                }
            }
            self.undo.push(Box::new(Composite { label, cmds }));
            self.sealed = true;
            self.trim();
        }
    }

    /// Reverts and discards everything done since `begin_transaction`.
    pub fn cancel_transaction(&mut self, ctx: &mut Ctx) {
        if let Some((_, mut cmds)) = self.txn.take() {
            while let Some(mut c) = cmds.pop() {
                c.undo(ctx);
            }
        }
    }
}

/// Several commands undone/redone as one.
pub struct Composite {
    label: String,
    cmds: Vec<Box<dyn Command>>,
}

impl Command for Composite {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        for (i, c) in self.cmds.iter_mut().enumerate() {
            if let Err(e) = c.execute(ctx) {
                for done in self.cmds[..i].iter_mut().rev() {
                    done.undo(ctx);
                }
                return Err(e);
            }
        }
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        for c in self.cmds.iter_mut().rev() {
            c.undo(ctx);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// -------------------------------------------------- subtree snapshot helper

struct SubtreeSnapshot {
    root: Entity,
    parent: Option<Entity>,
    index: usize,
    nodes: Vec<(Entity, BuiltEntityClone)>,
}

impl SubtreeSnapshot {
    fn capture(ctx: &Ctx, root: Entity) -> Result<Self, String> {
        if !ctx.world.contains(root) {
            return Err("entity does not exist".into());
        }
        let mut nodes = Vec::new();
        for e in subtree(ctx.world, root) {
            nodes.push((e, ctx.registry.snapshot(ctx.world, e).ok_or("entity does not exist")?));
        }
        Ok(Self {
            root,
            parent: parent_of(ctx.world, root),
            index: sibling_index(ctx.world, root).unwrap_or(0),
            nodes,
        })
    }

    fn remove(&self, ctx: &mut Ctx) {
        delete_subtree(ctx.world, self.root);
    }

    fn restore(&self, ctx: &mut Ctx) {
        for (e, built) in &self.nodes {
            ctx.world.spawn_at(*e, built);
        }
        if let Some(p) = self.parent.filter(|p| ctx.world.contains(*p)) {
            if ctx.world.get::<&Children>(p).is_err() {
                let _ = ctx.world.insert_one(p, Children::default());
            }
            let mut c = ctx.world.get::<&mut Children>(p).unwrap();
            let i = self.index.min(c.0.len());
            c.0.insert(i, self.root);
        }
    }
}

/// Lets the caller of a spawn/duplicate learn which entity was created.
#[derive(Clone, Default)]
pub struct EntityHandle(Rc<Cell<Option<Entity>>>);

impl EntityHandle {
    pub fn get(&self) -> Option<Entity> {
        self.0.get()
    }
}

/// Shared logic for commands that create a subtree (spawn, duplicate).
#[derive(Default)]
struct Produced {
    handle: EntityHandle,
    entity: Option<Entity>,
    snap: Option<SubtreeSnapshot>,
}

impl Produced {
    fn redo_or_create(
        &mut self,
        ctx: &mut Ctx,
        create: impl FnOnce(&mut Ctx) -> Result<Entity, String>,
    ) -> Result<(), String> {
        match self.snap.take() {
            Some(s) => s.restore(ctx),
            None => {
                let e = create(ctx)?;
                self.entity = Some(e);
                self.handle.0.set(Some(e));
            }
        }
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        if let Some(e) = self.entity {
            if let Ok(s) = SubtreeSnapshot::capture(ctx, e) {
                s.remove(ctx);
                self.snap = Some(s);
            }
        }
    }
}

// ---------------------------------------------------------------- commands

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub name: String,
    pub parent: Option<Entity>,
    pub shape: Option<PrimitiveShape>,
    pub position: Vec3,
}

impl SpawnSpec {
    pub fn empty(name: &str, parent: Option<Entity>) -> Self {
        Self { name: name.to_string(), parent, shape: None, position: Vec3::ZERO }
    }
    pub fn primitive(shape: PrimitiveShape, parent: Option<Entity>) -> Self {
        Self { name: shape.label().to_string(), parent, shape: Some(shape), position: Vec3::ZERO }
    }
}

pub struct SpawnCommand {
    spec: SpawnSpec,
    produced: Produced,
}

impl SpawnCommand {
    pub fn new(spec: SpawnSpec) -> (Self, EntityHandle) {
        let produced = Produced::default();
        let h = produced.handle.clone();
        (Self { spec, produced }, h)
    }
}

impl Command for SpawnCommand {
    fn label(&self) -> String {
        format!("Create {}", self.spec.name)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let s = self.spec.clone();
        self.produced
            .redo_or_create(ctx, |c| Ok(spawn_object(c.world, &s.name, s.parent, s.shape, s.position)))
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        self.produced.undo(ctx)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct DuplicateCommand {
    source: Entity,
    produced: Produced,
}

impl DuplicateCommand {
    pub fn new(source: Entity) -> (Self, EntityHandle) {
        let produced = Produced::default();
        let h = produced.handle.clone();
        (Self { source, produced }, h)
    }
}

impl Command for DuplicateCommand {
    fn label(&self) -> String {
        "Duplicate".into()
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let src = self.source;
        self.produced.redo_or_create(ctx, |c| {
            duplicate_subtree(c.world, c.registry, src).ok_or_else(|| "entity does not exist".to_string())
        })
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        self.produced.undo(ctx)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct DeleteCommand {
    root: Entity,
    name: String,
    snap: Option<SubtreeSnapshot>,
}

impl DeleteCommand {
    pub fn new(root: Entity) -> Self {
        Self { root, name: String::new(), snap: None }
    }
}

impl Command for DeleteCommand {
    fn label(&self) -> String {
        if self.name.is_empty() { "Delete".into() } else { format!("Delete {}", self.name) }
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let s = SubtreeSnapshot::capture(ctx, self.root)?;
        self.name = display_name(ctx.world, self.root);
        s.remove(ctx);
        self.snap = Some(s);
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        if let Some(s) = self.snap.take() {
            s.restore(ctx);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct ReparentCommand {
    entity: Entity,
    new_parent: Option<Entity>,
    index: Option<usize>,
    old: Option<(Option<Entity>, usize, Option<SpawnOrder>)>,
}

impl ReparentCommand {
    pub fn new(entity: Entity, new_parent: Option<Entity>, index: Option<usize>) -> Self {
        Self { entity, new_parent, index, old: None }
    }
}

impl Command for ReparentCommand {
    fn label(&self) -> String {
        "Reparent".into()
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let before = (
            parent_of(ctx.world, self.entity),
            sibling_index(ctx.world, self.entity).unwrap_or(0),
            ctx.world.get::<&SpawnOrder>(self.entity).ok().map(|o| *o),
        );
        hierarchy::reparent(ctx.world, self.entity, self.new_parent, self.index).map_err(|e| e.to_string())?;
        self.old.get_or_insert(before);
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        if let Some((p, i, order)) = self.old {
            let _ = hierarchy::reparent(ctx.world, self.entity, p, Some(i));
            if p.is_none() {
                match order {
                    Some(o) => {
                        let _ = ctx.world.insert_one(self.entity, o);
                    }
                    None => {
                        let _ = ctx.world.remove_one::<SpawnOrder>(self.entity);
                    }
                }
            }
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct RenameCommand {
    entity: Entity,
    new: String,
    old: Option<Option<String>>,
}

impl RenameCommand {
    pub fn new(entity: Entity, new: impl Into<String>) -> Self {
        Self { entity, new: new.into(), old: None }
    }
}

impl Command for RenameCommand {
    fn label(&self) -> String {
        format!("Rename to {}", self.new)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        if !ctx.world.contains(self.entity) {
            return Err("entity does not exist".into());
        }
        let prev = ctx.world.get::<&Name>(self.entity).ok().map(|n| n.0.clone());
        self.old.get_or_insert(prev);
        let _ = ctx.world.insert_one(self.entity, Name(self.new.clone()));
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        match self.old.clone() {
            Some(Some(n)) => {
                let _ = ctx.world.insert_one(self.entity, Name(n));
            }
            Some(None) => {
                let _ = ctx.world.remove_one::<Name>(self.entity);
            }
            None => {}
        }
    }
    fn merge_with(&mut self, next: &dyn Command) -> bool {
        match next.as_any().downcast_ref::<RenameCommand>() {
            Some(n) if n.entity == self.entity => {
                self.new = n.new.clone();
                true
            }
            _ => false,
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Sets one field on one or more entities (multi-select editing) as a single
/// undo step. Consecutive edits of the same field on the same targets merge.
pub struct SetFieldCommand {
    targets: Vec<Entity>,
    component: &'static str,
    field: &'static str,
    new: Value,
    /// (entity, old value) for targets that were actually changed.
    old: Option<Vec<(Entity, Value)>>,
}

impl SetFieldCommand {
    pub fn new(targets: Vec<Entity>, component: &'static str, field: &'static str, new: Value) -> Self {
        Self { targets, component, field, new, old: None }
    }
}

impl Command for SetFieldCommand {
    fn label(&self) -> String {
        format!("Set {}.{}", self.component, self.field)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let (c, f) = (self.component, self.field);
        let desc = ctx.registry.find(c).and_then(|d| d.field(f)).ok_or_else(|| format!("unknown field {c}.{f}"))?;
        match &self.old {
            None => {
                let mut old = Vec::new();
                for t in &self.targets {
                    if let Some(prev) = desc.get(ctx.world, *t) {
                        if desc.set(ctx.world, *t, &self.new) {
                            old.push((*t, prev));
                        }
                    }
                }
                if old.is_empty() {
                    return Err(format!("no target accepted {c}.{f}"));
                }
                self.old = Some(old);
            }
            Some(old) => {
                for (t, _) in old {
                    desc.set(ctx.world, *t, &self.new);
                }
            }
        }
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        if let (Some(old), Some(desc)) =
            (&self.old, ctx.registry.find(self.component).and_then(|d| d.field(self.field)))
        {
            for (t, v) in old {
                desc.set(ctx.world, *t, v);
            }
        }
    }
    fn merge_with(&mut self, next: &dyn Command) -> bool {
        match next.as_any().downcast_ref::<SetFieldCommand>() {
            Some(n) if n.component == self.component && n.field == self.field && n.targets == self.targets => {
                self.new = n.new.clone();
                true
            }
            _ => false,
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct AddComponentCommand {
    targets: Vec<Entity>,
    component: &'static str,
    added: Vec<Entity>,
}

impl AddComponentCommand {
    pub fn new(targets: Vec<Entity>, component: &'static str) -> Self {
        Self { targets, component, added: Vec::new() }
    }
}

impl Command for AddComponentCommand {
    fn label(&self) -> String {
        format!("Add {}", self.component)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let targets = if self.added.is_empty() { self.targets.clone() } else { self.added.clone() };
        self.added = targets
            .into_iter()
            .filter(|t| ctx.registry.add_component(ctx.world, *t, self.component))
            .collect();
        if self.added.is_empty() {
            Err(format!("cannot add {}", self.component))
        } else {
            Ok(())
        }
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        for t in &self.added {
            ctx.registry.remove_component(ctx.world, *t, self.component);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct RemoveComponentCommand {
    targets: Vec<Entity>,
    component: &'static str,
    removed: Vec<(Entity, ComponentSnapshot)>,
}

impl RemoveComponentCommand {
    pub fn new(targets: Vec<Entity>, component: &'static str) -> Self {
        Self { targets, component, removed: Vec::new() }
    }
}

impl Command for RemoveComponentCommand {
    fn label(&self) -> String {
        format!("Remove {}", self.component)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        if !ctx.registry.find(self.component).map(|c| c.removable).unwrap_or(false) {
            return Err(format!("{} cannot be removed", self.component));
        }
        let targets: Vec<Entity> = if self.removed.is_empty() {
            self.targets.clone()
        } else {
            self.removed.iter().map(|(e, _)| *e).collect()
        };
        let mut removed = Vec::new();
        for t in targets {
            if let Some(snap) = ctx.registry.snapshot_component(ctx.world, t, self.component) {
                if ctx.registry.remove_component(ctx.world, t, self.component) {
                    removed.push((t, snap));
                }
            }
        }
        if removed.is_empty() {
            return Err(format!("nothing to remove: {}", self.component));
        }
        self.removed = removed;
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        for (t, s) in &self.removed {
            ctx.registry.restore_component(ctx.world, *t, self.component, s);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Visible,
    Locked,
}

pub struct SetFlagCommand {
    targets: Vec<Entity>,
    flag: Flag,
    value: bool,
    old: Option<Vec<(Entity, Option<bool>)>>,
}

impl SetFlagCommand {
    pub fn new(targets: Vec<Entity>, flag: Flag, value: bool) -> Self {
        Self { targets, flag, value, old: None }
    }
    fn read(w: &World, e: Entity, flag: Flag) -> Option<bool> {
        match flag {
            Flag::Visible => w.get::<&Visible>(e).ok().map(|v| v.0),
            Flag::Locked => w.get::<&Locked>(e).ok().map(|v| v.0),
        }
    }
    fn write(w: &mut World, e: Entity, flag: Flag, v: Option<bool>) {
        match (flag, v) {
            (Flag::Visible, Some(b)) => {
                let _ = w.insert_one(e, Visible(b));
            }
            (Flag::Locked, Some(b)) => {
                let _ = w.insert_one(e, Locked(b));
            }
            (Flag::Visible, None) => {
                let _ = w.remove_one::<Visible>(e);
            }
            (Flag::Locked, None) => {
                let _ = w.remove_one::<Locked>(e);
            }
        }
    }
}

impl Command for SetFlagCommand {
    fn label(&self) -> String {
        format!("Set {:?} = {}", self.flag, self.value)
    }
    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        if self.old.is_none() {
            let old: Vec<_> = self
                .targets
                .iter()
                .filter(|t| ctx.world.contains(**t))
                .map(|t| (*t, Self::read(ctx.world, *t, self.flag)))
                .collect();
            if old.is_empty() {
                return Err("no such entity".into());
            }
            self.old = Some(old);
        }
        for (t, _) in self.old.as_ref().unwrap() {
            Self::write(ctx.world, *t, self.flag, Some(self.value));
        }
        Ok(())
    }
    fn undo(&mut self, ctx: &mut Ctx) {
        if let Some(old) = &self.old {
            for (t, v) in old {
                Self::write(ctx.world, *t, self.flag, *v);
            }
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::Transform;
    use crate::editor::hierarchy::*;
    use crate::editor::inspector::{Registry, Value};
    use glam::Vec3;
    use hecs::World;

    struct Fx {
        w: World,
        reg: Registry,
        st: UndoStack,
    }
    impl Fx {
        fn new() -> Self {
            Fx { w: World::new(), reg: Registry::with_defaults(), st: UndoStack::default() }
        }
        fn run(&mut self, c: impl Command + 'static) -> Result<(), String> {
            let mut ctx = Ctx { world: &mut self.w, registry: &self.reg };
            self.st.execute(Box::new(c), &mut ctx)
        }
        fn undo(&mut self) -> bool {
            let mut ctx = Ctx { world: &mut self.w, registry: &self.reg };
            self.st.undo(&mut ctx)
        }
        fn redo(&mut self) -> bool {
            let mut ctx = Ctx { world: &mut self.w, registry: &self.reg };
            self.st.redo(&mut ctx)
        }
        fn spawn(&mut self, name: &str, parent: Option<hecs::Entity>) -> hecs::Entity {
            let (c, h) = SpawnCommand::new(SpawnSpec::empty(name, parent));
            self.run(c).unwrap();
            h.get().unwrap()
        }
    }

    #[test]
    fn spawn_undo_redo_keeps_same_entity_id() {
        let mut f = Fx::new();
        let p = f.spawn("p", None);
        let e = f.spawn("child", Some(p));
        assert_eq!(children(&f.w, p), vec![e]);
        assert!(f.undo());
        assert!(!f.w.contains(e));
        assert!(children(&f.w, p).is_empty());
        assert!(f.redo());
        assert!(f.w.contains(e));
        assert_eq!(children(&f.w, p), vec![e]);
        assert_eq!(display_name(&f.w, e), "child");
        assert!(f.w.get::<&Transform>(e).is_ok());
    }

    #[test]
    fn delete_undo_restores_subtree_order_and_components() {
        let mut f = Fx::new();
        let p = f.spawn("p", None);
        let a = f.spawn("a", Some(p));
        let b = f.spawn("b", Some(p));
        let c = f.spawn("c", Some(p));
        let a1 = f.spawn("a1", Some(a));
        f.w.get::<&mut Transform>(a1).unwrap().position = Vec3::new(5.0, 0.0, 0.0);
        f.run(DeleteCommand::new(b)).unwrap();
        f.run(DeleteCommand::new(a)).unwrap();
        assert!(!f.w.contains(a) && !f.w.contains(a1));
        assert_eq!(children(&f.w, p), vec![c]);
        f.undo();
        assert_eq!(children(&f.w, p), vec![a, c]);
        f.undo();
        assert_eq!(children(&f.w, p), vec![a, b, c]);
        assert_eq!(children(&f.w, a), vec![a1]);
        assert_eq!(parent_of(&f.w, a1), Some(a));
        assert_eq!(f.w.get::<&Transform>(a1).unwrap().position.x, 5.0);
        f.redo();
        assert!(!f.w.contains(b));
    }

    #[test]
    fn delete_root_undo_restores_root_position() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", None);
        let c = f.spawn("c", None);
        f.run(DeleteCommand::new(b)).unwrap();
        f.undo();
        assert_eq!(roots(&f.w), vec![a, b, c]);
    }

    #[test]
    fn delete_missing_entity_errors_and_is_not_recorded() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        f.run(DeleteCommand::new(a)).unwrap();
        let before = f.st.undo_len();
        assert!(f.run(DeleteCommand::new(a)).is_err());
        assert_eq!(f.st.undo_len(), before);
    }

    #[test]
    fn reparent_undo_redo() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", None);
        let x = f.spawn("x", Some(a));
        let y = f.spawn("y", Some(a));
        f.run(ReparentCommand::new(x, Some(b), None)).unwrap();
        assert_eq!(parent_of(&f.w, x), Some(b));
        f.undo();
        assert_eq!(children(&f.w, a), vec![x, y]);
        f.redo();
        assert_eq!(children(&f.w, b), vec![x]);
        // to root and back
        f.run(ReparentCommand::new(y, None, Some(0))).unwrap();
        assert_eq!(roots(&f.w)[0], y);
        f.undo();
        assert_eq!(children(&f.w, a), vec![y]);
        assert_eq!(roots(&f.w), vec![a, b]);
    }

    #[test]
    fn reparent_cycle_is_error_not_recorded() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", Some(a));
        let n = f.st.undo_len();
        assert!(f.run(ReparentCommand::new(a, Some(b), None)).is_err());
        assert_eq!(f.st.undo_len(), n);
    }

    #[test]
    fn rename_merges_consecutive_edits_of_same_entity() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", None);
        f.st.seal();
        for s in ["x", "xy", "xyz"] {
            f.run(RenameCommand::new(a, s)).unwrap();
        }
        assert_eq!(f.st.undo_len(), 3); // 2 spawns + 1 rename
        f.run(RenameCommand::new(b, "other")).unwrap(); // different target: no merge
        assert_eq!(f.st.undo_len(), 4);
        f.undo();
        f.undo();
        assert_eq!(display_name(&f.w, a), "a");
    }

    #[test]
    fn set_field_drag_is_one_undo_step_until_sealed() {
        let mut f = Fx::new();
        let e = f.spawn("e", None);
        f.st.seal();
        let n = f.st.undo_len();
        for i in 1..=10 {
            f.run(SetFieldCommand::new(vec![e], "Transform", "position", Value::Vec3(Vec3::splat(i as f32)))).unwrap();
            assert_eq!(f.w.get::<&Transform>(e).unwrap().position, Vec3::splat(i as f32));
        }
        assert_eq!(f.st.undo_len(), n + 1);
        f.st.seal(); // mouse released
        f.run(SetFieldCommand::new(vec![e], "Transform", "position", Value::Vec3(Vec3::splat(99.0)))).unwrap();
        assert_eq!(f.st.undo_len(), n + 2);
        f.undo();
        assert_eq!(f.w.get::<&Transform>(e).unwrap().position, Vec3::splat(10.0));
        f.undo();
        assert_eq!(f.w.get::<&Transform>(e).unwrap().position, Vec3::ZERO);
        f.redo();
        assert_eq!(f.w.get::<&Transform>(e).unwrap().position, Vec3::splat(10.0));
    }

    #[test]
    fn different_fields_do_not_merge() {
        let mut f = Fx::new();
        let e = f.spawn("e", None);
        f.st.seal();
        let n = f.st.undo_len();
        f.run(SetFieldCommand::new(vec![e], "Transform", "position", Value::Vec3(Vec3::X))).unwrap();
        f.run(SetFieldCommand::new(vec![e], "Transform", "scale", Value::Vec3(Vec3::splat(2.0)))).unwrap();
        assert_eq!(f.st.undo_len(), n + 2);
    }

    #[test]
    fn multi_entity_set_field_is_one_step_and_restores_each_old_value() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", None);
        f.w.get::<&mut Transform>(a).unwrap().position = Vec3::X;
        f.w.get::<&mut Transform>(b).unwrap().position = Vec3::Y;
        f.st.seal();
        f.run(SetFieldCommand::new(vec![a, b], "Transform", "position", Value::Vec3(Vec3::splat(3.0)))).unwrap();
        f.undo();
        assert_eq!(f.w.get::<&Transform>(a).unwrap().position, Vec3::X);
        assert_eq!(f.w.get::<&Transform>(b).unwrap().position, Vec3::Y);
    }

    #[test]
    fn set_field_rejects_bad_input_without_recording() {
        let mut f = Fx::new();
        let e = f.spawn("e", None);
        let n = f.st.undo_len();
        assert!(f.run(SetFieldCommand::new(vec![e], "Transform", "position", Value::Bool(true))).is_err());
        assert!(f.run(SetFieldCommand::new(vec![e], "Camera", "near", Value::F32(1.0))).is_err()); // no Camera
        assert_eq!(f.st.undo_len(), n);
    }

    #[test]
    fn add_remove_component_undo() {
        let mut f = Fx::new();
        let e = f.spawn("e", None);
        f.run(AddComponentCommand::new(vec![e], "Camera")).unwrap();
        assert!(f.w.get::<&crate::ecs::Camera>(e).is_ok());
        f.run(SetFieldCommand::new(vec![e], "Camera", "near", Value::F32(2.5))).unwrap();
        f.st.seal();
        f.run(RemoveComponentCommand::new(vec![e], "Camera")).unwrap();
        assert!(f.w.get::<&crate::ecs::Camera>(e).is_err());
        f.undo();
        assert_eq!(f.w.get::<&crate::ecs::Camera>(e).unwrap().near, 2.5);
        f.undo();
        f.undo();
        assert!(f.w.get::<&crate::ecs::Camera>(e).is_err());
        assert!(f.run(RemoveComponentCommand::new(vec![e], "Camera")).is_err());
    }

    #[test]
    fn set_flag_undo_distinguishes_absent_from_true() {
        let mut f = Fx::new();
        let e = f.spawn("e", None);
        f.run(SetFlagCommand::new(vec![e], Flag::Visible, false)).unwrap();
        assert!(!is_visible(&f.w, e));
        f.undo();
        assert!(is_visible(&f.w, e));
        assert!(f.w.get::<&Visible>(e).is_err(), "component removed again, not left as Visible(true)");
        f.run(SetFlagCommand::new(vec![e], Flag::Locked, true)).unwrap();
        assert!(is_locked(&f.w, e));
    }

    #[test]
    fn duplicate_command_undo_redo() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let _k = f.spawn("k", Some(a));
        let (c, h) = DuplicateCommand::new(a);
        f.run(c).unwrap();
        let copy = h.get().unwrap();
        assert_eq!(subtree(&f.w, copy).len(), 2);
        f.undo();
        assert!(!f.w.contains(copy));
        assert_eq!(f.w.iter().count(), 2);
        f.redo();
        assert!(f.w.contains(copy));
        assert_eq!(subtree(&f.w, copy).len(), 2);
    }

    #[test]
    fn new_command_clears_redo() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        f.undo();
        assert!(f.st.can_redo());
        f.spawn("b", None);
        assert!(!f.st.can_redo());
        assert!(!f.redo());
        let _ = a;
    }

    #[test]
    fn transaction_groups_into_one_step_and_undoes_in_reverse() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        let b = f.spawn("b", None);
        f.st.seal();
        let n = f.st.undo_len();
        f.st.begin_transaction("Delete selection");
        f.run(DeleteCommand::new(a)).unwrap();
        f.run(DeleteCommand::new(b)).unwrap();
        assert!(f.st.in_transaction());
        f.st.end_transaction();
        assert_eq!(f.st.undo_len(), n + 1);
        assert_eq!(f.st.undo_label().as_deref(), Some("Delete selection"));
        assert!(f.w.iter().next().is_none());
        f.undo();
        assert_eq!(roots(&f.w), vec![a, b]);
        f.redo();
        assert!(f.w.iter().next().is_none());
    }

    #[test]
    fn empty_transaction_leaves_no_step() {
        let mut f = Fx::new();
        f.st.begin_transaction("nothing");
        f.st.end_transaction();
        assert_eq!(f.st.undo_len(), 0);
    }

    #[test]
    fn cancel_transaction_rolls_back() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        f.st.begin_transaction("oops");
        f.run(DeleteCommand::new(a)).unwrap();
        let mut ctx = Ctx { world: &mut f.w, registry: &f.reg };
        f.st.cancel_transaction(&mut ctx);
        assert!(f.w.contains(a));
        assert!(!f.st.in_transaction());
        assert_eq!(f.st.undo_len(), 1);
    }

    #[test]
    fn failure_inside_transaction_does_not_poison_it() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        f.st.begin_transaction("t");
        f.run(DeleteCommand::new(a)).unwrap();
        assert!(f.run(DeleteCommand::new(a)).is_err());
        f.st.end_transaction();
        f.undo();
        assert!(f.w.contains(a));
    }

    #[test]
    fn dirty_flag_tracks_saved_point() {
        let mut f = Fx::new();
        assert!(!f.st.is_dirty());
        let _a = f.spawn("a", None);
        assert!(f.st.is_dirty());
        f.st.mark_saved();
        assert!(!f.st.is_dirty());
        f.undo();
        assert!(f.st.is_dirty());
        f.redo();
        assert!(!f.st.is_dirty());
        // saving seals: later edits never merge into the saved step
        let e = f.spawn("e", None);
        f.run(RenameCommand::new(e, "e1")).unwrap();
        f.st.mark_saved();
        let n = f.st.undo_len();
        f.run(RenameCommand::new(e, "e2")).unwrap();
        assert_eq!(f.st.undo_len(), n + 1);
        assert!(f.st.is_dirty());
        f.undo();
        assert!(!f.st.is_dirty());
        assert_eq!(display_name(&f.w, e), "e1");
    }

    #[test]
    fn saved_state_lost_when_redo_branch_discarded() {
        let mut f = Fx::new();
        f.spawn("a", None);
        f.spawn("b", None);
        f.st.mark_saved();
        f.undo();
        f.undo();
        f.spawn("c", None); // new branch; old saved state unreachable
        f.redo();
        assert!(f.st.is_dirty());
        f.undo();
        f.undo();
        assert!(f.st.is_dirty());
    }

    #[test]
    fn history_limit_drops_oldest() {
        let mut f = Fx::new();
        f.st.set_limit(3);
        for i in 0..6 {
            f.spawn(&format!("n{i}"), None);
        }
        assert_eq!(f.st.undo_len(), 3);
        assert!(f.st.is_dirty());
    }

    #[test]
    fn labels() {
        let mut f = Fx::new();
        let a = f.spawn("a", None);
        assert_eq!(f.st.undo_label().as_deref(), Some("Create a"));
        f.run(RenameCommand::new(a, "b")).unwrap();
        assert!(f.st.undo_label().unwrap().starts_with("Rename"));
        f.undo();
        assert!(f.st.redo_label().unwrap().starts_with("Rename"));
    }
}
