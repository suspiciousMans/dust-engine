//! Prefabs: reusable entity subtrees stored as `.prefab.ron` (a [`Scene`]
//! whose first entity is the root) and instantiated into worlds with
//! per-instance **overrides**.
//!
//! Model
//! * Instance root carries [`PrefabInstance`] `{ source, overrides }`.
//! * Every entity that came from a prefab carries [`PrefabMember`]: the chain
//!   of `(prefab, local id)` refs it belongs to, innermost prefab first, so
//!   nested prefabs work (an entity of prefab B placed inside prefab A has
//!   `[B:x, A:y]`).
//! * Overrides are **derived** by diffing an instance against its prefab
//!   ([`update_overrides`]); `SceneDocument` does this before saving when it
//!   has a [`PrefabLibrary`]. Edits therefore need no hooks.
//! * [`propagate`] re-applies a changed prefab onto every instance, keeping
//!   overridden fields.
//!
//! Limitations (documented, tested where they matter): components that an
//! instance added or the prefab removed are not tracked as add/remove
//! overrides (added ones survive refresh, removed ones are not removed);
//! a prefab whose root is itself an instance gets a wrapper root.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use glam::Vec3;
use hecs::{BuiltEntityClone, Entity, World};
use serde::{Deserialize, Serialize};

use super::command::{Command, Ctx};
use super::hierarchy::{self, Children, Locked, Name, Visible};
use super::inspector::Registry;
use super::scene::{
    scene_to_value, EntityId, Remap, Scene, SceneEntity, SceneError, SceneValue, SpawnOptions, UnknownComponents,
};

pub const PREFAB_EXT: &str = ".prefab.ron";

/// Pseudo component holding entity-level overridable data.
pub const ENTITY_COMP: &str = "@";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemberRef {
    /// Prefab path relative to the library root (`/` separators).
    pub source: String,
    /// Entity id inside that prefab file.
    pub id: EntityId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Override {
    /// Id of the prefab entity (in `PrefabInstance::source`'s file).
    pub target: EntityId,
    /// Component name, or [`ENTITY_COMP`] for `name`/`hidden`/`locked`.
    pub comp: String,
    pub field: String,
    pub value: SceneValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabInstance {
    pub source: String,
    #[serde(default)]
    pub overrides: Vec<Override>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrefabMember(pub Vec<MemberRef>);

#[derive(Debug, thiserror::Error)]
pub enum PrefabError {
    #[error(transparent)]
    Scene(#[from] SceneError),
    #[error("prefab '{0}' not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

// ------------------------------------------------------------- library

/// Loads/caches prefab files relative to a root directory.
#[derive(Debug, Default)]
pub struct PrefabLibrary {
    pub root: PathBuf,
    cache: HashMap<String, Scene>,
}

impl PrefabLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), cache: HashMap::new() }
    }

    pub fn path_of(&self, source: &str) -> PathBuf {
        self.root.join(source)
    }

    pub fn get(&mut self, source: &str) -> Result<Scene, PrefabError> {
        if let Some(s) = self.cache.get(source) {
            return Ok(s.clone());
        }
        let p = self.path_of(source);
        if !p.exists() {
            return Err(PrefabError::NotFound(source.to_string()));
        }
        let s = Scene::load(&p)?;
        self.cache.insert(source.to_string(), s.clone());
        Ok(s)
    }

    pub fn save(&mut self, source: &str, scene: &Scene) -> Result<(), PrefabError> {
        scene.save(&self.path_of(source))?;
        self.cache.insert(source.to_string(), scene.clone());
        Ok(())
    }

    /// Forget the cached copy (file changed on disk).
    pub fn invalidate(&mut self, source: &str) {
        self.cache.remove(source);
    }

    /// All `*.prefab.ron` under the root, relative, sorted.
    pub fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if p.to_string_lossy().ends_with(PREFAB_EXT) {
                    if let Ok(r) = p.strip_prefix(root) {
                        out.push(r.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        walk(&self.root, &self.root, &mut out);
        out.sort();
        out
    }
}

pub type SharedLibrary = Rc<RefCell<PrefabLibrary>>;

// ------------------------------------------------------------ queries

pub fn instance_of(world: &World, e: Entity) -> Option<PrefabInstance> {
    world.get::<&PrefabInstance>(e).ok().map(|p| (*p).clone())
}

/// Roots of all prefab instances, outermost (shallowest) first.
pub fn instance_roots(world: &World) -> Vec<Entity> {
    let mut v: Vec<(usize, Entity)> = world
        .query::<&PrefabInstance>()
        .iter()
        .map(|(e, _)| {
            let mut d = 0;
            let mut c = e;
            while let Some(p) = hierarchy::parent_of(world, c) {
                d += 1;
                c = p;
            }
            (d, e)
        })
        .collect();
    v.sort_by_key(|(d, e)| (*d, e.id()));
    v.into_iter().map(|(_, e)| e).collect()
}

/// The instance root `e` belongs to (nearest ancestor-or-self with a
/// [`PrefabInstance`]).
pub fn containing_instance(world: &World, e: Entity) -> Option<Entity> {
    let mut c = Some(e);
    while let Some(x) = c {
        if world.get::<&PrefabInstance>(x).is_ok() {
            return Some(x);
        }
        c = hierarchy::parent_of(world, x);
    }
    None
}

/// `local id -> entity` for the members of the instance rooted at `root`.
pub fn members_of(world: &World, root: Entity) -> HashMap<EntityId, Entity> {
    let Some(inst) = instance_of(world, root) else { return HashMap::new() };
    let mut out = HashMap::new();
    for e in hierarchy::subtree(world, root) {
        if let Ok(m) = world.get::<&PrefabMember>(e) {
            if let Some(r) = m.0.iter().find(|r| r.source == inst.source) {
                out.insert(r.id, e);
            }
        }
    }
    out
}

// ------------------------------------------------------ create / spawn

fn root_index(scene: &Scene) -> Option<usize> {
    scene.entities.iter().position(|e| e.parent.is_none())
}

/// Turns `root`'s subtree into a prefab file and converts the original
/// entities into an instance of it. Returns the library-relative path.
pub fn create_from_selection(
    world: &mut World,
    reg: &Registry,
    lib: &mut PrefabLibrary,
    root: Entity,
    source: &str,
) -> Result<Entity, PrefabError> {
    if !source.ends_with(PREFAB_EXT) {
        return Err(PrefabError::Invalid(format!("prefab path must end with {PREFAB_EXT}")));
    }
    if !world.contains(root) {
        return Err(PrefabError::Invalid("entity does not exist".into()));
    }
    // A root that is already an instance root can't carry two PrefabInstance
    // components, so wrap it.
    let root = if world.get::<&PrefabInstance>(root).is_ok() {
        let name = hierarchy::display_name(world, root);
        let parent = hierarchy::parent_of(world, root);
        let idx = hierarchy::sibling_index(world, root);
        let wrapper = hierarchy::spawn_node(world, &name, None);
        let _ = world.insert_one(wrapper, crate::ecs::Transform::default());
        hierarchy::reparent(world, wrapper, parent, idx).map_err(|e| PrefabError::Invalid(format!("{e:?}")))?;
        hierarchy::reparent(world, root, Some(wrapper), None).map_err(|e| PrefabError::Invalid(format!("{e:?}")))?;
        wrapper
    } else {
        root
    };
    let mut scene = Scene::from_subtree(world, reg, root);
    scene.name = hierarchy::display_name(world, root);
    let ids: Vec<EntityId> = scene.entities.iter().map(|e| e.id).collect();
    lib.save(source, &scene)?;
    let nodes = hierarchy::subtree(world, root);
    for (e, id) in nodes.iter().zip(&ids) {
        let mut chain = world.get::<&PrefabMember>(*e).map(|m| m.0.clone()).unwrap_or_default();
        chain.push(MemberRef { source: source.to_string(), id: *id });
        let _ = world.insert_one(*e, PrefabMember(chain));
    }
    let _ = world.insert_one(root, PrefabInstance { source: source.to_string(), overrides: Vec::new() });
    Ok(root)
}

/// Spawns `prefab` under `parent`. All entities get fresh ids.
pub fn instantiate(
    world: &mut World,
    reg: &Registry,
    prefab: &Scene,
    source: &str,
    parent: Option<Entity>,
) -> Result<Entity, PrefabError> {
    let ri = root_index(prefab).ok_or_else(|| PrefabError::Invalid(format!("prefab '{source}' is empty")))?;
    let res = prefab.spawn_with(world, reg, &SpawnOptions { remap: Remap::Always, parent });
    for (se, &e) in prefab.entities.iter().zip(&res.entities) {
        let mut chain = se.prefab_member.clone();
        chain.push(MemberRef { source: source.to_string(), id: se.id });
        let _ = world.insert_one(e, PrefabMember(chain));
    }
    let root = res.entities[ri];
    let _ = world.insert_one(root, PrefabInstance { source: source.to_string(), overrides: Vec::new() });
    Ok(root)
}

/// Shared slot telling the caller which entity an [`InstantiatePrefab`] made.
#[derive(Clone, Default)]
pub struct InstanceHandle(Rc<Cell<Option<Entity>>>);

impl InstanceHandle {
    pub fn get(&self) -> Option<Entity> {
        self.0.get()
    }
}

/// Undoable "drop a prefab into the scene".
pub struct InstantiatePrefab {
    lib: SharedLibrary,
    source: String,
    parent: Option<Entity>,
    position: Option<Vec3>,
    handle: InstanceHandle,
    entity: Option<Entity>,
    snap: Option<(Option<Entity>, usize, Vec<(Entity, BuiltEntityClone)>)>,
}

impl InstantiatePrefab {
    pub fn new(lib: SharedLibrary, source: &str, parent: Option<Entity>, position: Option<Vec3>) -> (Self, InstanceHandle) {
        let handle = InstanceHandle::default();
        (
            Self { lib, source: source.to_string(), parent, position, handle: handle.clone(), entity: None, snap: None },
            handle,
        )
    }
}

impl Command for InstantiatePrefab {
    fn label(&self) -> String {
        let n = self.source.rsplit('/').next().unwrap_or(&self.source);
        format!("Instantiate {}", n.trim_end_matches(PREFAB_EXT))
    }

    fn execute(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        if let Some((parent, index, nodes)) = self.snap.take() {
            // redo: put the exact same entities back
            for (e, b) in &nodes {
                ctx.world.spawn_at(*e, b);
            }
            if let (Some(p), Some(root)) = (parent.filter(|p| ctx.world.contains(*p)), self.entity) {
                if ctx.world.get::<&Children>(p).is_err() {
                    let _ = ctx.world.insert_one(p, Children::default());
                }
                let mut c = ctx.world.get::<&mut Children>(p).unwrap();
                let i = index.min(c.0.len());
                c.0.insert(i, root);
            }
            return Ok(());
        }
        let scene = self.lib.borrow_mut().get(&self.source).map_err(|e| e.to_string())?;
        let root = instantiate(ctx.world, ctx.registry, &scene, &self.source, self.parent).map_err(|e| e.to_string())?;
        if let Some(p) = self.position {
            if let Ok(mut t) = ctx.world.get::<&mut crate::ecs::Transform>(root) {
                t.position = p;
            }
        }
        self.entity = Some(root);
        self.handle.0.set(Some(root));
        Ok(())
    }

    fn undo(&mut self, ctx: &mut Ctx) {
        let Some(root) = self.entity else { return };
        let parent = hierarchy::parent_of(ctx.world, root);
        let index = hierarchy::sibling_index(ctx.world, root).unwrap_or(0);
        let nodes: Vec<_> = hierarchy::subtree(ctx.world, root)
            .into_iter()
            .filter_map(|e| ctx.registry.snapshot(ctx.world, e).map(|b| (e, b)))
            .collect();
        hierarchy::delete_subtree(ctx.world, root);
        self.snap = Some((parent, index, nodes));
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------- overrides

fn entity_pseudo_fields(se: &SceneEntity) -> Vec<(&'static str, SceneValue)> {
    vec![
        ("name", SceneValue::Str(se.name.clone())),
        ("hidden", SceneValue::Bool(se.hidden)),
        ("locked", SceneValue::Bool(se.locked)),
    ]
}

/// Differences between `cur` (live entity) and `base` (prefab entity).
fn diff_entity(target: EntityId, cur: &SceneEntity, base: &SceneEntity) -> Vec<Override> {
    let mut out = Vec::new();
    let basep: BTreeMap<_, _> = entity_pseudo_fields(base).into_iter().collect();
    for (f, v) in entity_pseudo_fields(cur) {
        if basep.get(f).map_or(true, |b| !b.approx_eq(&v)) {
            out.push(Override { target, comp: ENTITY_COMP.into(), field: f.into(), value: v });
        }
    }
    for (c, fields) in &cur.components {
        for (f, v) in fields {
            if base.field(c, f).map_or(true, |b| !b.approx_eq(v)) {
                out.push(Override { target, comp: c.clone(), field: f.clone(), value: v.clone() });
            }
        }
    }
    out
}

fn apply_override(se: &mut SceneEntity, o: &Override) {
    if o.comp == ENTITY_COMP {
        match (o.field.as_str(), &o.value) {
            ("name", SceneValue::Str(s)) => se.name = s.clone(),
            ("hidden", SceneValue::Bool(b)) => se.hidden = *b,
            ("locked", SceneValue::Bool(b)) => se.locked = *b,
            _ => {}
        }
    } else {
        se.set_field(&o.comp, &o.field, o.value.clone());
    }
}

/// Recomputes `PrefabInstance::overrides` for every instance in the world
/// from the current live values. Instances whose prefab can't be loaded are
/// left untouched. Returns the number of instances updated.
pub fn update_overrides(world: &mut World, reg: &Registry, lib: &mut PrefabLibrary) -> usize {
    instance_roots(world).into_iter().filter(|root| update_overrides_of(world, reg, lib, *root)).count()
}

/// Recomputes the overrides of one instance against its prefab's current
/// content. Returns false if the instance or its prefab can't be found.
pub fn update_overrides_of(world: &mut World, reg: &Registry, lib: &mut PrefabLibrary, root: Entity) -> bool {
    let Some(inst) = instance_of(world, root) else { return false };
    let Ok(prefab) = lib.get(&inst.source) else { return false };
    let members = members_of(world, root);
    let mut overrides = Vec::new();
    for base in &prefab.entities {
        let Some(&e) = members.get(&base.id) else { continue };
        let cur = Scene::capture_entity(world, reg, e);
        overrides.extend(diff_entity(base.id, &cur, base));
    }
    match world.get::<&mut PrefabInstance>(root) {
        Ok(mut i) => {
            i.overrides = overrides;
            true
        }
        Err(_) => false,
    }
}

// ------------------------------------------------------------ refresh

/// Writes `se` onto the existing entity `e` (known components set field by
/// field; unknown ones merged into [`UnknownComponents`]).
fn write_entity(world: &mut World, reg: &Registry, e: Entity, se: &SceneEntity) {
    let _ = world.insert_one(e, Name(se.name.clone()));
    let _ = world.insert_one(e, Visible(!se.hidden));
    let _ = world.insert_one(e, Locked(se.locked));
    for (cname, fields) in &se.components {
        match reg.find(cname) {
            Some(desc) => {
                if !desc.has(world, e) {
                    reg.add_component(world, e, cname);
                }
                for f in &desc.fields {
                    if let Some(v) = fields.get(f.name).and_then(|sv| scene_to_value(sv, &f.kind)) {
                        f.set(world, e, &v);
                    }
                }
            }
            None => {
                let has = world.get::<&UnknownComponents>(e).is_ok();
                if !has {
                    let _ = world.insert_one(e, UnknownComponents::default());
                }
                if let Ok(mut u) = world.get::<&mut UnknownComponents>(e) {
                    u.fields.insert(cname.clone(), fields.clone());
                }
            }
        }
    }
    if let Some(p) = &se.prefab {
        let _ = world.insert_one(e, p.clone());
    }
}

/// Re-applies `prefab` to the instance at `root`, then `overrides` on top.
/// Members missing from the prefab are deleted; prefab entities missing from
/// the instance are spawned.
pub fn refresh_instance(
    world: &mut World,
    reg: &Registry,
    root: Entity,
    prefab: &Scene,
    overrides: &[Override],
) {
    let Some(inst) = instance_of(world, root) else { return };
    let source = inst.source.clone();
    let mut members = members_of(world, root);
    let ids: std::collections::HashSet<EntityId> = prefab.entities.iter().map(|e| e.id).collect();
    // delete members the prefab no longer has (deepest first)
    let mut gone: Vec<Entity> = members.iter().filter(|(id, _)| !ids.contains(id)).map(|(_, e)| *e).collect();
    gone.retain(|e| *e != root);
    for e in gone {
        if world.contains(e) {
            hierarchy::delete_subtree(world, e);
        }
    }
    members.retain(|_, e| world.contains(*e));

    let root_local = members.iter().find(|(_, e)| **e == root).map(|(id, _)| *id);
    for base in &prefab.entities {
        let mut se = base.clone();
        for o in overrides.iter().filter(|o| o.target == base.id) {
            apply_override(&mut se, o);
        }
        // the instance root keeps its own PrefabInstance
        if Some(base.id) == root_local {
            se.prefab = None;
        }
        match members.get(&base.id).copied() {
            Some(e) => {
                write_entity(world, reg, e, &se);
                let mut chain = se.prefab_member.clone();
                chain.push(MemberRef { source: source.clone(), id: base.id });
                let _ = world.insert_one(e, PrefabMember(chain));
            }
            None => {
                let parent = base.parent.and_then(|p| members.get(&p).copied()).unwrap_or(root);
                let mut single = Scene::new("");
                let mut s1 = se.clone();
                s1.parent = None;
                single.entities.push(s1);
                let r = single.spawn_with(world, reg, &SpawnOptions { remap: Remap::Always, parent: Some(parent) });
                let e = r.entities[0];
                let mut chain = se.prefab_member.clone();
                chain.push(MemberRef { source: source.clone(), id: base.id });
                let _ = world.insert_one(e, PrefabMember(chain));
                members.insert(base.id, e);
            }
        }
    }
}

/// Re-applies the prefab `changed` to every instance of it, preserving each
/// instance's overrides. `lib` must already hold the new content (call
/// [`PrefabLibrary::save`] / `invalidate` first). Returns instances refreshed.
pub fn propagate(world: &mut World, reg: &Registry, lib: &mut PrefabLibrary, changed: &str) -> usize {
    let roots: Vec<Entity> =
        instance_roots(world).into_iter().filter(|r| instance_of(world, *r).is_some_and(|i| i.source == changed)).collect();
    let Ok(prefab) = lib.get(changed) else { return 0 };
    let mut n = 0;
    for root in roots {
        let ov = instance_of(world, root).map(|i| i.overrides).unwrap_or_default();
        refresh_instance(world, reg, root, &prefab, &ov);
        n += 1;
    }
    n
}

/// Edit a prefab file through `edit` and propagate to all live instances,
/// keeping overrides. Overrides are first derived against the **old**
/// content so only deliberate instance edits are preserved.
pub fn edit_prefab_and_propagate(
    world: &mut World,
    reg: &Registry,
    lib: &mut PrefabLibrary,
    source: &str,
    edit: impl FnOnce(&mut Scene),
) -> Result<usize, PrefabError> {
    update_overrides(world, reg, lib);
    let mut scene = lib.get(source)?;
    edit(&mut scene);
    lib.save(source, &scene)?;
    Ok(propagate(world, reg, lib, source))
}

// ----------------------------------------------- apply / revert / unpack

/// Writes the instance's current state back into its prefab file ("Apply
/// All"), clears its overrides and refreshes the *other* instances. The
/// root's own Transform placement is not written to the prefab.
pub fn apply_to_prefab(world: &mut World, reg: &Registry, lib: &mut PrefabLibrary, root: Entity) -> Result<usize, PrefabError> {
    let inst = instance_of(world, root).ok_or_else(|| PrefabError::Invalid("not a prefab instance".into()))?;
    let source = inst.source;
    let old = lib.get(&source)?;
    // other instances must keep their overrides relative to the OLD content
    update_overrides(world, reg, lib);

    let mut scene = Scene::from_subtree(world, reg, root);
    scene.name = old.name.clone();
    let nodes = hierarchy::subtree(world, root);
    // world EntityId -> local prefab id, and new chains
    let mut local: HashMap<EntityId, EntityId> = HashMap::new();
    let mut chains: Vec<Vec<MemberRef>> = Vec::new();
    for (se, e) in scene.entities.iter().zip(&nodes) {
        let chain = world.get::<&PrefabMember>(*e).map(|m| m.0.clone()).unwrap_or_default();
        let (lid, own) = match chain.iter().position(|r| r.source == source) {
            Some(p) => (chain[p].id, chain[..p].to_vec()),
            None => (se.id, chain),
        };
        local.insert(se.id, lid);
        chains.push(own);
    }
    for (se, chain) in scene.entities.iter_mut().zip(chains) {
        se.id = local[&se.id];
        se.parent = se.parent.map(|p| local[&p]);
        se.prefab_member = chain;
    }
    if let Some(r) = scene.entities.first_mut() {
        r.prefab = None;
        // keep the prefab's own root placement
        if let Some(old_root) = old.entities.iter().find(|e| e.parent.is_none()) {
            match old_root.components.get("Transform") {
                Some(t) => {
                    r.components.insert("Transform".into(), t.clone());
                }
                None => {
                    r.components.remove("Transform");
                }
            }
        }
    }
    lib.save(&source, &scene)?;
    // bring the in-world members in line (new entities become members)
    for (e, se) in nodes.iter().zip(&scene.entities) {
        let mut chain = se.prefab_member.clone();
        chain.push(MemberRef { source: source.clone(), id: se.id });
        let _ = world.insert_one(*e, PrefabMember(chain));
    }
    if let Ok(mut i) = world.get::<&mut PrefabInstance>(root) {
        i.overrides.clear();
    }
    // the applied instance's root placement stays as an override. Only this
    // instance: the others still hold OLD content and must keep the
    // overrides captured above, or refresh would treat the stale values as
    // deliberate edits.
    update_overrides_of(world, reg, lib, root);
    // refresh other instances
    let others: Vec<Entity> = instance_roots(world)
        .into_iter()
        .filter(|r| *r != root && instance_of(world, *r).is_some_and(|i| i.source == source))
        .collect();
    let mut n = 0;
    for o in others {
        let ov = instance_of(world, o).map(|i| i.overrides).unwrap_or_default();
        refresh_instance(world, reg, o, &scene, &ov);
        n += 1;
    }
    Ok(n)
}

/// Drops all overrides ("Revert All") except the root's placement.
pub fn revert_instance(world: &mut World, reg: &Registry, lib: &mut PrefabLibrary, root: Entity) -> Result<(), PrefabError> {
    let inst = instance_of(world, root).ok_or_else(|| PrefabError::Invalid("not a prefab instance".into()))?;
    let prefab = lib.get(&inst.source)?;
    update_overrides(world, reg, lib);
    let keep: Vec<Override> = instance_of(world, root)
        .map(|i| i.overrides)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| {
            o.comp == "Transform"
                && (o.field == "position" || o.field == "rotation")
                && root_local_id(world, root).is_some_and(|id| id == o.target)
        })
        .collect();
    refresh_instance(world, reg, root, &prefab, &keep);
    if let Ok(mut i) = world.get::<&mut PrefabInstance>(root) {
        i.overrides = keep;
    }
    Ok(())
}

fn root_local_id(world: &World, root: Entity) -> Option<EntityId> {
    let inst = instance_of(world, root)?;
    world.get::<&PrefabMember>(root).ok()?.0.iter().find(|r| r.source == inst.source).map(|r| r.id)
}

/// Reverts a single overridden field. Returns true if something changed.
pub fn revert_field(
    world: &mut World,
    reg: &Registry,
    lib: &mut PrefabLibrary,
    root: Entity,
    target: EntityId,
    comp: &str,
    field: &str,
) -> Result<bool, PrefabError> {
    let inst = instance_of(world, root).ok_or_else(|| PrefabError::Invalid("not a prefab instance".into()))?;
    let prefab = lib.get(&inst.source)?;
    update_overrides(world, reg, lib);
    let mut ov = instance_of(world, root).map(|i| i.overrides).unwrap_or_default();
    let before = ov.len();
    ov.retain(|o| !(o.target == target && o.comp == comp && o.field == field));
    let changed = ov.len() != before;
    refresh_instance(world, reg, root, &prefab, &ov);
    if let Ok(mut i) = world.get::<&mut PrefabInstance>(root) {
        i.overrides = ov;
    }
    Ok(changed)
}

/// Detaches one prefab level: the instance becomes plain entities (nested
/// instances inside stay instances).
pub fn unpack(world: &mut World, root: Entity) {
    let Some(inst) = instance_of(world, root) else { return };
    for e in hierarchy::subtree(world, root) {
        let chain = world.get::<&PrefabMember>(e).map(|m| m.0.clone()).unwrap_or_default();
        let kept: Vec<MemberRef> = chain.into_iter().filter(|r| r.source != inst.source).collect();
        if kept.is_empty() {
            let _ = world.remove_one::<PrefabMember>(e);
        } else {
            let _ = world.insert_one(e, PrefabMember(kept));
        }
    }
    let _ = world.remove_one::<PrefabInstance>(root);
}

/// Unpacks every nested level too.
pub fn unpack_completely(world: &mut World, root: Entity) {
    let nodes = hierarchy::subtree(world, root);
    let insts: Vec<Entity> = nodes.iter().copied().filter(|e| world.get::<&PrefabInstance>(*e).is_ok()).collect();
    for e in insts {
        unpack(world, e);
    }
}

// -------------------------------------------------------------- class

/// Converts an old-style [`crate::class::ObjectClass`] into a one-entity
/// prefab. The whole class is kept verbatim in `raw["ObjectClass"]` so
/// nothing (mesh source, script, animation, ...) is lost.
pub fn class_to_prefab(class: &crate::class::ObjectClass) -> Result<Scene, PrefabError> {
    use super::scene::RawRon;
    let mut se = SceneEntity::new(EntityId::from_key(&format!("class:{}", class.name)), &class.name);
    se.set_field("Transform", "position", SceneValue::Vec3(0.0, 0.0, 0.0));
    se.set_field("Transform", "rotation", SceneValue::Vec3(0.0, 0.0, 0.0));
    se.set_field("Transform", "scale", SceneValue::Vec3(class.scale[0], class.scale[1], class.scale[2]));
    if let crate::level::MeshSource::Primitive(k) = &class.mesh {
        let shape = match k {
            crate::level::PrimitiveKind::Cube => "Cube",
            crate::level::PrimitiveKind::Plane => "Plane",
        };
        se.set_field("Primitive", "shape", SceneValue::Enum(shape.into()));
    }
    se.raw.insert("ObjectClass".into(), RawRon::from_value(class).map_err(SceneError::from)?);
    let mut s = Scene::new(&class.name);
    s.entities.push(se);
    Ok(s)
}

/// Converts every `*.ron` class in `classes_dir` into
/// `<out>/<name>.prefab.ron` (library-relative `out_rel`). Returns the
/// created sources.
pub fn convert_classes_dir(classes_dir: &Path, lib: &mut PrefabLibrary, out_rel: &str) -> Result<Vec<String>, PrefabError> {
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(classes_dir)
        .map_err(SceneError::from)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ron"))
        .collect();
    paths.sort();
    for p in paths {
        let class = crate::class::load_from_file(&p).map_err(|e| PrefabError::Invalid(format!("{p:?}: {e}")))?;
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("class");
        let src = format!("{}/{}{}", out_rel.trim_end_matches('/'), stem, PREFAB_EXT);
        lib.save(&src, &class_to_prefab(&class)?)?;
        out.push(src);
    }
    Ok(out)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{Light, Transform};
    use crate::editor::command::UndoStack;
    use crate::editor::hierarchy::{spawn_object, PrimitiveShape};
    use crate::editor::scene::{scene_registry, testutil::TempDir};

    fn reg() -> Registry {
        scene_registry()
    }

    /// root "Lamp" (cube) with child "Bulb" (light).
    fn lamp(w: &mut World) -> Entity {
        let root = spawn_object(w, "Lamp", None, Some(PrimitiveShape::Cube), Vec3::new(1.0, 0.0, 0.0));
        let bulb = spawn_object(w, "Bulb", Some(root), None, Vec3::new(0.0, 2.0, 0.0));
        w.insert_one(bulb, Light { color: Vec3::ONE, intensity: 1.0, kind: crate::ecs::LightKind::Point { range: 5.0 }, is_static: false })
            .unwrap();
        root
    }

    fn child(w: &World, e: Entity, name: &str) -> Entity {
        hierarchy::subtree(w, e).into_iter().find(|c| hierarchy::display_name(w, *c) == name).unwrap()
    }

    fn intensity(w: &World, e: Entity) -> f32 {
        w.get::<&Light>(e).unwrap().intensity
    }

    #[test]
    fn create_save_instantiate_with_fresh_ids() {
        let dir = TempDir::new("prefab-a");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, root, "props/lamp.prefab.ron").unwrap();
        assert!(dir.path().join("props/lamp.prefab.ron").exists());
        assert_eq!(lib.list(), ["props/lamp.prefab.ron"]);
        assert_eq!(instance_of(&w, root).unwrap().source, "props/lamp.prefab.ron");
        assert!(members_of(&w, root).len() == 2);

        let prefab = lib.get("props/lamp.prefab.ron").unwrap();
        let inst = instantiate(&mut w, &r, &prefab, "props/lamp.prefab.ron", None).unwrap();
        assert_eq!(hierarchy::display_name(&w, inst), "Lamp");
        assert_eq!(members_of(&w, inst).len(), 2);
        let ids: Vec<EntityId> = w.query::<&EntityId>().iter().map(|(_, i)| *i).collect();
        let mut s = ids.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), ids.len());
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn instantiate_command_undo_redo() {
        let dir = TempDir::new("prefab-cmd");
        let lib: SharedLibrary = Rc::new(RefCell::new(PrefabLibrary::new(dir.path())));
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib.borrow_mut(), root, "lamp.prefab.ron").unwrap();
        let before = w.len();
        let mut stack = UndoStack::default();
        let mut ctx = Ctx { world: &mut w, registry: &r };
        let (cmd, h) = InstantiatePrefab::new(lib.clone(), "lamp.prefab.ron", None, Some(Vec3::new(9.0, 0.0, 0.0)));
        stack.execute(Box::new(cmd), &mut ctx).unwrap();
        let e = h.get().unwrap();
        assert_eq!(ctx.world.len(), before + 2);
        assert_eq!(ctx.world.get::<&Transform>(e).unwrap().position.x, 9.0);
        let id = *ctx.world.get::<&EntityId>(e).unwrap();
        assert!(stack.undo(&mut ctx));
        assert_eq!(ctx.world.len(), before);
        assert!(stack.redo(&mut ctx));
        assert_eq!(ctx.world.len(), before + 2);
        assert!(ctx.world.contains(e));
        assert_eq!(*ctx.world.get::<&EntityId>(e).unwrap(), id, "same identity after redo");
        assert_eq!(members_of(ctx.world, e).len(), 2);
        // missing prefab is an Err that records nothing
        let (bad, _) = InstantiatePrefab::new(lib.clone(), "nope.prefab.ron", None, None);
        assert!(stack.execute(Box::new(bad), &mut ctx).is_err());
    }

    #[test]
    fn overrides_are_derived_and_survive_propagation() {
        let dir = TempDir::new("prefab-ov");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, root, "lamp.prefab.ron").unwrap();
        let p = lib.get("lamp.prefab.ron").unwrap();
        let a = instantiate(&mut w, &r, &p, "lamp.prefab.ron", None).unwrap();
        let b = instantiate(&mut w, &r, &p, "lamp.prefab.ron", None).unwrap();
        // instance A: overridden intensity; instance B: untouched
        let bulb_a = child(&w, a, "Bulb");
        let bulb_b = child(&w, b, "Bulb");
        w.get::<&mut Light>(bulb_a).unwrap().intensity = 7.0;
        w.get::<&mut Transform>(a).unwrap().position = Vec3::new(5.0, 5.0, 5.0);
        assert_eq!(update_overrides(&mut w, &r, &mut lib), 3);
        let ov = instance_of(&w, a).unwrap().overrides;
        assert!(ov.iter().any(|o| o.comp == "Light" && o.field == "intensity" && o.value == SceneValue::F32(7.0)));
        assert!(ov.iter().any(|o| o.comp == "Transform" && o.field == "position"));
        assert!(instance_of(&w, b).unwrap().overrides.is_empty());

        // edit the prefab: new intensity + a new child entity
        let n = edit_prefab_and_propagate(&mut w, &r, &mut lib, "lamp.prefab.ron", |s| {
            let bulb = s.entities.iter_mut().find(|e| e.name == "Bulb").unwrap();
            bulb.set_field("Light", "intensity", SceneValue::F32(3.0));
            let root_id = s.entities[0].id;
            let mut shade = SceneEntity::new(EntityId::from_key("shade"), "Shade");
            shade.parent = Some(root_id);
            shade.set_field("Transform", "position", SceneValue::Vec3(0.0, 3.0, 0.0));
            s.entities.push(shade);
        })
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(intensity(&w, bulb_a), 7.0, "override kept");
        assert_eq!(intensity(&w, bulb_b), 3.0, "inherited value updated");
        assert_eq!(w.get::<&Transform>(a).unwrap().position, Vec3::new(5.0, 5.0, 5.0), "placement kept");
        assert_eq!(hierarchy::display_name(&w, child(&w, a, "Shade")), "Shade");
        assert_eq!(members_of(&w, b).len(), 3);
        // children of both instances, not duplicated
        assert_eq!(hierarchy::children(&w, a).len(), 2);

        // removing an entity from the prefab removes it from instances
        edit_prefab_and_propagate(&mut w, &r, &mut lib, "lamp.prefab.ron", |s| s.entities.retain(|e| e.name != "Shade"))
            .unwrap();
        assert_eq!(hierarchy::children(&w, a).len(), 1);
    }

    #[test]
    fn revert_apply_unpack() {
        let dir = TempDir::new("prefab-ra");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, root, "lamp.prefab.ron").unwrap();
        let p = lib.get("lamp.prefab.ron").unwrap();
        let a = instantiate(&mut w, &r, &p, "lamp.prefab.ron", None).unwrap();
        let b = instantiate(&mut w, &r, &p, "lamp.prefab.ron", None).unwrap();
        w.get::<&mut Transform>(a).unwrap().position = Vec3::splat(4.0);
        let bulb_a = child(&w, a, "Bulb");
        w.get::<&mut Light>(bulb_a).unwrap().intensity = 9.0;
        w.insert_one(bulb_a, Name("Bulb2".into())).unwrap();

        // revert one field
        assert!(revert_field(&mut w, &r, &mut lib, a, root_local_for(&p, "Bulb"), "Light", "intensity").unwrap());
        assert_eq!(intensity(&w, bulb_a), 1.0);
        assert_eq!(hierarchy::display_name(&w, bulb_a), "Bulb2", "other overrides stay");
        // revert all keeps placement but resets the name
        revert_instance(&mut w, &r, &mut lib, a).unwrap();
        assert_eq!(hierarchy::display_name(&w, bulb_a), "Bulb");
        assert_eq!(w.get::<&Transform>(a).unwrap().position, Vec3::splat(4.0));

        // apply: change in A flows to the file and to B
        w.get::<&mut Light>(bulb_a).unwrap().intensity = 2.5;
        let others = apply_to_prefab(&mut w, &r, &mut lib, a).unwrap();
        assert_eq!(others, 2); // original + b
        assert_eq!(intensity(&w, child(&w, b, "Bulb")), 2.5);
        assert_eq!(intensity(&w, child(&w, root, "Bulb")), 2.5);
        assert_eq!(w.get::<&Transform>(b).unwrap().position, Vec3::new(1.0, 0.0, 0.0), "placement not applied");
        let file = Scene::load(&dir.path().join("lamp.prefab.ron")).unwrap();
        assert_eq!(file.entities.len(), 2);
        assert_eq!(file.entities.iter().find(|e| e.name == "Bulb").unwrap().field("Light", "intensity"), Some(&SceneValue::F32(2.5)));
        assert!(file.entities[0].prefab.is_none());
        assert!(instance_of(&w, a).unwrap().overrides.iter().all(|o| o.comp == "Transform"));

        // unpack detaches
        unpack(&mut w, b);
        assert!(instance_of(&w, b).is_none());
        assert!(w.get::<&PrefabMember>(child(&w, b, "Bulb")).is_err());
        assert_eq!(propagate(&mut w, &r, &mut lib, "lamp.prefab.ron"), 2);
    }

    fn root_local_for(p: &Scene, name: &str) -> EntityId {
        p.entities.iter().find(|e| e.name == name).unwrap().id
    }

    #[test]
    fn nested_prefabs_and_propagation_through_levels() {
        let dir = TempDir::new("prefab-nest");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let lamp_root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, lamp_root, "lamp.prefab.ron").unwrap();
        // street = a pole with a lamp instance on it
        let street = spawn_object(&mut w, "Street", None, None, Vec3::ZERO);
        let lamp_p = lib.get("lamp.prefab.ron").unwrap();
        let inner = instantiate(&mut w, &r, &lamp_p, "lamp.prefab.ron", Some(street)).unwrap();
        w.get::<&mut Transform>(inner).unwrap().position = Vec3::new(0.0, 4.0, 0.0);
        create_from_selection(&mut w, &r, &mut lib, street, "street.prefab.ron").unwrap();

        let sp = lib.get("street.prefab.ron").unwrap();
        assert!(sp.entities.iter().any(|e| e.prefab.as_ref().is_some_and(|p| p.source == "lamp.prefab.ron")), "inner instance kept");
        let s1 = instantiate(&mut w, &r, &sp, "street.prefab.ron", None).unwrap();
        let bulb = child(&w, s1, "Bulb");
        let chain = w.get::<&PrefabMember>(bulb).unwrap().0.clone();
        assert_eq!(chain.iter().map(|m| m.source.as_str()).collect::<Vec<_>>(), ["lamp.prefab.ron", "street.prefab.ron"]);
        assert_eq!(members_of(&w, s1).len(), 3);
        let inner_in_s1 = child(&w, s1, "Lamp");
        assert_eq!(instance_of(&w, inner_in_s1).unwrap().source, "lamp.prefab.ron");
        assert_eq!(members_of(&w, inner_in_s1).len(), 2);

        // override at the outer instance level, then edit the INNER prefab
        w.get::<&mut Transform>(s1).unwrap().position = Vec3::splat(10.0);
        let n = edit_prefab_and_propagate(&mut w, &r, &mut lib, "lamp.prefab.ron", |s| {
            s.entities.iter_mut().find(|e| e.name == "Bulb").unwrap().set_field("Light", "intensity", SceneValue::F32(6.0));
        })
        .unwrap();
        assert_eq!(n, 3, "original lamp + inner in street + inner in s1");
        assert_eq!(intensity(&w, bulb), 6.0);
        assert_eq!(w.get::<&Transform>(s1).unwrap().position, Vec3::splat(10.0));
        assert_eq!(w.get::<&Transform>(inner_in_s1).unwrap().position, Vec3::new(0.0, 4.0, 0.0), "inner placement from outer prefab kept");

        // unpack one level leaves the inner instance alive
        unpack(&mut w, s1);
        assert!(instance_of(&w, s1).is_none());
        assert!(instance_of(&w, inner_in_s1).is_some());
        assert_eq!(w.get::<&PrefabMember>(bulb).unwrap().0.len(), 1);
        unpack_completely(&mut w, inner_in_s1);
        assert!(w.get::<&PrefabMember>(bulb).is_err());
    }

    #[test]
    fn instances_roundtrip_through_scene_files() {
        let dir = TempDir::new("prefab-scene");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, root, "lamp.prefab.ron").unwrap();
        let p = lib.get("lamp.prefab.ron").unwrap();
        let a = instantiate(&mut w, &r, &p, "lamp.prefab.ron", None).unwrap();
        let bulb_a = child(&w, a, "Bulb");
        w.get::<&mut Light>(bulb_a).unwrap().intensity = 8.0;
        update_overrides(&mut w, &r, &mut lib);
        let text = Scene::from_world(&mut w, &r).to_ron().unwrap();
        assert!(text.contains("lamp.prefab.ron"));
        assert!(text.contains("intensity"));

        // reload the scene "later", after the prefab changed on disk
        let mut w2 = World::new();
        Scene::from_ron(&text).unwrap().load_replace(&mut w2, &r);
        let a2 = instance_roots(&w2).into_iter().find(|e| hierarchy::display_name(&w2, *e) == "Lamp" && !instance_of(&w2, *e).unwrap().overrides.is_empty()).unwrap();
        assert!(!members_of(&w2, a2).is_empty());
        edit_prefab_and_propagate(&mut w2, &r, &mut lib, "lamp.prefab.ron", |s| {
            s.entities.iter_mut().find(|e| e.name == "Bulb").unwrap().set_field("Light", "intensity", SceneValue::F32(0.5));
        })
        .unwrap();
        assert_eq!(intensity(&w2, child(&w2, a2, "Bulb")), 8.0);
        let other = instance_roots(&w2).into_iter().find(|e| *e != a2 && hierarchy::display_name(&w2, *e) == "Lamp").unwrap();
        assert_eq!(intensity(&w2, child(&w2, other, "Bulb")), 0.5);
    }

    #[test]
    fn wrapper_root_when_selection_is_already_an_instance() {
        let dir = TempDir::new("prefab-wrap");
        let mut lib = PrefabLibrary::new(dir.path());
        let r = reg();
        let mut w = World::new();
        let root = lamp(&mut w);
        create_from_selection(&mut w, &r, &mut lib, root, "lamp.prefab.ron").unwrap();
        let wrapper = create_from_selection(&mut w, &r, &mut lib, root, "lamp2.prefab.ron").unwrap();
        assert_ne!(wrapper, root);
        assert_eq!(hierarchy::parent_of(&w, root), Some(wrapper));
        assert_eq!(instance_of(&w, root).unwrap().source, "lamp.prefab.ron");
        assert_eq!(instance_of(&w, wrapper).unwrap().source, "lamp2.prefab.ron");
        assert!(create_from_selection(&mut w, &r, &mut lib, wrapper, "bad.ron").is_err());
    }

    #[test]
    fn class_files_convert_to_prefabs() {
        let classes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../games/mushroom_man/classes");
        let dir = TempDir::new("prefab-class");
        let mut lib = PrefabLibrary::new(dir.path());
        let made = convert_classes_dir(&classes, &mut lib, "classes").unwrap();
        assert!(made.len() >= 3, "{made:?}");
        let r = reg();
        for src in &made {
            let p = lib.get(src).unwrap();
            let mut w = World::new();
            let e = instantiate(&mut w, &r, &p, src, None).unwrap();
            let class: crate::class::ObjectClass = p.entities[0].raw["ObjectClass"].parse().unwrap();
            assert_eq!(hierarchy::display_name(&w, e), class.name);
            assert_eq!(w.get::<&Transform>(e).unwrap().scale, Vec3::from(class.scale));
        }
    }
}
