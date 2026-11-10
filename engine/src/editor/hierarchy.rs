//! Scene-graph components and structural operations (hecs-based).
//!
//! Every function here keeps `Parent` and `Children` consistent; callers
//! (commands, UI) never edit those two components directly.

use glam::Vec3;
use hecs::{Entity, World};

use super::inspector::Registry;

/// Human-readable entity name shown in the hierarchy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name(pub String);

/// Link to the parent entity. Absent on roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parent(pub Entity);

/// Ordered child list. Order is the display order and is preserved by
/// duplicate/undo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Children(pub Vec<Entity>);

/// Own visibility flag (absent = visible). See [`is_visible`] for the
/// effective value that includes ancestors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible(pub bool);

/// Own lock flag (absent = unlocked). Locked entities can't be picked or
/// edited through the viewport; see [`is_locked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Locked(pub bool);

/// Sort key for root entities (hecs iteration order is not stable across
/// despawns). `f64` so an item can be inserted between two neighbours
/// without renumbering the others.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpawnOrder(pub f64);

/// Built-in primitive shape marker for "Create > Primitive". The renderer
/// bridge decides how to turn it into a mesh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveShape {
    Cube,
    Sphere,
    Plane,
    Capsule,
}

impl PrimitiveShape {
    pub const ALL: [PrimitiveShape; 4] = [Self::Cube, Self::Sphere, Self::Plane, Self::Capsule];
    pub fn label(self) -> &'static str {
        match self {
            Self::Cube => "Cube",
            Self::Sphere => "Sphere",
            Self::Plane => "Plane",
            Self::Capsule => "Capsule",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Primitive(pub PrimitiveShape);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HierarchyError {
    NoSuchEntity,
    Cycle,
}

impl std::fmt::Display for HierarchyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchEntity => write!(f, "entity does not exist"),
            Self::Cycle => write!(f, "cannot make an entity a child of itself or its descendant"),
        }
    }
}
impl std::error::Error for HierarchyError {}

// ---------------------------------------------------------------- queries

pub fn parent_of(world: &World, e: Entity) -> Option<Entity> {
    world.get::<&Parent>(e).ok().map(|p| p.0)
}

pub fn children(world: &World, e: Entity) -> Vec<Entity> {
    world.get::<&Children>(e).map(|c| c.0.clone()).unwrap_or_default()
}

fn sort_key(world: &World, e: Entity) -> (f64, u32) {
    let o = world.get::<&SpawnOrder>(e).map(|o| o.0).unwrap_or(f64::INFINITY);
    (o, e.id())
}

/// Entities with no parent, in stable display order.
pub fn roots(world: &World) -> Vec<Entity> {
    let mut v: Vec<Entity> = world
        .iter()
        .map(|e| e.entity())
        .filter(|e| !world.satisfies::<&Parent>(*e).unwrap_or(false))
        .collect();
    v.sort_by(|a, b| sort_key(world, *a).partial_cmp(&sort_key(world, *b)).unwrap());
    v
}

/// `e` followed by all descendants, depth-first pre-order.
pub fn subtree(world: &World, e: Entity) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(n) = stack.pop() {
        if !world.contains(n) {
            continue;
        }
        out.push(n);
        stack.extend(children(world, n).into_iter().rev());
    }
    out
}

pub fn is_descendant_of(world: &World, e: Entity, ancestor: Entity) -> bool {
    let mut cur = parent_of(world, e);
    let mut guard = 0;
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = parent_of(world, p);
        guard += 1;
        if guard > 100_000 {
            break; // corrupted (cyclic) data; don't hang the editor
        }
    }
    false
}

pub fn display_name(world: &World, e: Entity) -> String {
    match world.get::<&Name>(e) {
        Ok(n) => n.0.clone(),
        Err(_) => format!("Entity {}", e.id()),
    }
}

fn ancestors_any(world: &World, e: Entity, f: impl Fn(&World, Entity) -> bool) -> bool {
    let mut cur = Some(e);
    let mut guard = 0;
    while let Some(n) = cur {
        if f(world, n) {
            return true;
        }
        cur = parent_of(world, n);
        guard += 1;
        if guard > 100_000 {
            break;
        }
    }
    false
}

/// Effective visibility: hidden if the entity or any ancestor is hidden.
pub fn is_visible(world: &World, e: Entity) -> bool {
    !ancestors_any(world, e, |w, n| w.get::<&Visible>(n).map(|v| !v.0).unwrap_or(false))
}

/// Effective lock: locked if the entity or any ancestor is locked.
pub fn is_locked(world: &World, e: Entity) -> bool {
    ancestors_any(world, e, |w, n| w.get::<&Locked>(n).map(|v| v.0).unwrap_or(false))
}

fn next_order(world: &World) -> f64 {
    world
        .query::<&SpawnOrder>()
        .iter()
        .map(|(_, o)| o.0)
        .fold(0.0, f64::max)
        + 1.0
}

// -------------------------------------------------------------- mutations

/// Spawns `Name + SpawnOrder` (no Transform; callers add what they need) and
/// links it under `parent` at the end of its child list.
pub fn spawn_node(world: &mut World, name: &str, parent: Option<Entity>) -> Entity {
    let order = next_order(world);
    let e = world.spawn((Name(name.to_string()), SpawnOrder(order)));
    if let Some(p) = parent {
        // A freshly spawned entity can't create a cycle; a missing parent
        // just leaves it as a root.
        let _ = reparent(world, e, Some(p), None);
    }
    e
}

fn unlink_from_parent(world: &mut World, e: Entity) -> Option<usize> {
    let p = parent_of(world, e)?;
    let mut idx = None;
    if let Ok(mut c) = world.get::<&mut Children>(p) {
        if let Some(i) = c.0.iter().position(|x| *x == e) {
            c.0.remove(i);
            idx = Some(i);
        }
    }
    let _ = world.remove_one::<Parent>(e);
    idx
}

/// Position of `e` among its siblings (roots included).
pub fn sibling_index(world: &World, e: Entity) -> Option<usize> {
    match parent_of(world, e) {
        Some(p) => children(world, p).iter().position(|x| *x == e),
        None => roots(world).iter().position(|x| *x == e),
    }
}

/// Re-parents `e` under `new_parent` (`None` = root) at `index` among the new
/// siblings, counted **after** `e` has been taken out of its old position
/// (so "move to end" is `Some(len_without_e)`; `None`/too large = end).
/// Rejects cycles and missing entities without modifying anything.
pub fn reparent(
    world: &mut World,
    e: Entity,
    new_parent: Option<Entity>,
    index: Option<usize>,
) -> Result<(), HierarchyError> {
    if !world.contains(e) {
        return Err(HierarchyError::NoSuchEntity);
    }
    if let Some(p) = new_parent {
        if !world.contains(p) {
            return Err(HierarchyError::NoSuchEntity);
        }
        if p == e || is_descendant_of(world, p, e) {
            return Err(HierarchyError::Cycle);
        }
    }
    unlink_from_parent(world, e);
    match new_parent {
        Some(p) => {
            if world.get::<&Children>(p).is_err() {
                let _ = world.insert_one(p, Children::default());
            }
            let mut c = world.get::<&mut Children>(p).unwrap();
            let i = index.unwrap_or(usize::MAX).min(c.0.len());
            c.0.insert(i, e);
            drop(c);
            let _ = world.insert_one(e, Parent(p));
        }
        None => {
            if let Some(i) = index {
                place_root(world, e, i);
            }
        }
    }
    Ok(())
}

/// Gives root `e` a `SpawnOrder` so it lands at `index` among the other roots.
fn place_root(world: &mut World, e: Entity, index: usize) {
    let others: Vec<Entity> = roots(world).into_iter().filter(|x| *x != e).collect();
    let ord = |w: &World, x: Entity| sort_key(w, x).0;
    let i = index.min(others.len());
    let prev = if i > 0 { Some(ord(world, others[i - 1])) } else { None };
    let next = others.get(i).map(|x| ord(world, *x));
    let new = match (prev, next) {
        (Some(a), Some(b)) if b.is_finite() => (a + b) / 2.0,
        (Some(a), _) => a + 1.0,
        (None, Some(b)) if b.is_finite() => b - 1.0,
        _ => next_order(world),
    };
    let _ = world.insert_one(e, SpawnOrder(new));
}

/// Despawns `e` and all descendants; returns what was removed (pre-order).
pub fn delete_subtree(world: &mut World, e: Entity) -> Vec<Entity> {
    let all = subtree(world, e);
    unlink_from_parent(world, e);
    for n in &all {
        let _ = world.despawn(*n);
    }
    all
}

fn strip_copy_suffix(name: &str) -> &str {
    if let Some(stripped) = name.strip_suffix(')') {
        if let Some(pos) = stripped.rfind(" (") {
            if !stripped[pos + 2..].is_empty() && stripped[pos + 2..].bytes().all(|b| b.is_ascii_digit()) {
                return &stripped[..pos];
            }
        }
    }
    name
}

/// "Cube" -> "Cube (1)"; "Cube (1)" -> next free "Cube (n)" among `e`'s siblings.
pub fn unique_copy_name(world: &World, e: Entity) -> String {
    let base = strip_copy_suffix(&display_name(world, e)).to_string();
    let siblings = match parent_of(world, e) {
        Some(p) => children(world, p),
        None => roots(world),
    };
    let taken: Vec<String> = siblings.iter().map(|s| display_name(world, *s)).collect();
    (1..)
        .map(|n| format!("{base} ({n})"))
        .find(|c| !taken.contains(c))
        .unwrap()
}

/// Deep-copies `e` and its descendants (all components the registry knows how
/// to clone) and inserts the copy right after the original. Returns the new
/// root, or `None` if `e` doesn't exist.
pub fn duplicate_subtree(world: &mut World, reg: &Registry, e: Entity) -> Option<Entity> {
    if !world.contains(e) {
        return None;
    }
    let copy_name = unique_copy_name(world, e);
    let src = subtree(world, e);
    let mut map: Vec<(Entity, Entity)> = Vec::with_capacity(src.len());
    for s in &src {
        let built = reg.snapshot(world, *s)?;
        let n = world.spawn(&built);
        map.push((*s, n));
    }
    let lookup = |old: Entity| map.iter().find(|(o, _)| *o == old).map(|(_, n)| *n);
    for (old, new) in &map {
        let kids: Vec<Entity> = children(world, *old).into_iter().filter_map(lookup).collect();
        if kids.is_empty() {
            let _ = world.remove_one::<Children>(*new);
        } else {
            let _ = world.insert_one(*new, Children(kids));
        }
        match parent_of(world, *old).and_then(lookup) {
            Some(p) => {
                let _ = world.insert_one(*new, Parent(p));
            }
            None => {
                let _ = world.remove_one::<Parent>(*new);
            }
        }
        let order = next_order(world);
        let _ = world.insert_one(*new, SpawnOrder(order));
    }
    let root_copy = lookup(e)?;
    let _ = world.insert_one(root_copy, Name(copy_name));
    let idx = sibling_index(world, e).map(|i| i + 1);
    let orig_parent = parent_of(world, e);
    let _ = reparent(world, root_copy, orig_parent, idx);
    Some(root_copy)
}

/// Default transform-bearing node used by "Create Empty" / primitives.
pub fn spawn_object(
    world: &mut World,
    name: &str,
    parent: Option<Entity>,
    shape: Option<PrimitiveShape>,
    position: Vec3,
) -> Entity {
    let e = spawn_node(world, name, parent);
    let _ = world.insert_one(e, crate::ecs::Transform::from_position(position));
    if let Some(s) = shape {
        let _ = world.insert_one(e, Primitive(s));
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::inspector::Registry;
    use hecs::World;

    fn names(w: &World, es: &[hecs::Entity]) -> Vec<String> {
        es.iter().map(|e| display_name(w, *e)).collect()
    }

    fn tree() -> (World, hecs::Entity, hecs::Entity, hecs::Entity, hecs::Entity) {
        // a
        // ├─ b
        // │  └─ c
        // d (root)
        let mut w = World::new();
        let a = spawn_node(&mut w, "a", None);
        let b = spawn_node(&mut w, "b", Some(a));
        let c = spawn_node(&mut w, "c", Some(b));
        let d = spawn_node(&mut w, "d", None);
        (w, a, b, c, d)
    }

    #[test]
    fn spawn_links_parent_and_children_in_order() {
        let (mut w, a, b, _c, _d) = tree();
        let b2 = spawn_node(&mut w, "b2", Some(a));
        assert_eq!(children(&w, a), vec![b, b2]);
        assert_eq!(parent_of(&w, b), Some(a));
    }

    #[test]
    fn roots_are_stable_in_spawn_order_even_after_deletes() {
        let mut w = World::new();
        let r: Vec<_> = (0..5).map(|i| spawn_node(&mut w, &format!("r{i}"), None)).collect();
        delete_subtree(&mut w, r[1]);
        delete_subtree(&mut w, r[3]);
        let n = spawn_node(&mut w, "new", None); // reuses a freed slot
        let got = roots(&w);
        assert_eq!(names(&w, &got), ["r0", "r2", "r4", "new"]);
        assert_eq!(*got.last().unwrap(), n);
    }

    #[test]
    fn reparent_moves_and_updates_both_sides() {
        let (mut w, _a, b, c, d) = tree();
        reparent(&mut w, c, Some(d), None).unwrap();
        assert!(children(&w, b).is_empty());
        assert_eq!(children(&w, d), vec![c]);
        assert_eq!(parent_of(&w, c), Some(d));
    }

    #[test]
    fn reparent_to_root_removes_parent() {
        let (mut w, _a, b, _c, _d) = tree();
        reparent(&mut w, b, None, None).unwrap();
        assert_eq!(parent_of(&w, b), None);
        assert!(roots(&w).contains(&b));
    }

    #[test]
    fn reparent_rejects_cycles_and_self() {
        let (mut w, a, b, c, _d) = tree();
        assert_eq!(reparent(&mut w, a, Some(c), None), Err(HierarchyError::Cycle));
        assert_eq!(reparent(&mut w, a, Some(a), None), Err(HierarchyError::Cycle));
        assert_eq!(reparent(&mut w, b, Some(c), None), Err(HierarchyError::Cycle));
        // untouched
        assert_eq!(parent_of(&w, b), Some(a));
        assert_eq!(parent_of(&w, a), None);
    }

    #[test]
    fn reparent_missing_entity_errors() {
        let (mut w, a, _b, c, _d) = tree();
        delete_subtree(&mut w, c);
        assert_eq!(reparent(&mut w, a, Some(c), None), Err(HierarchyError::NoSuchEntity));
    }

    #[test]
    fn reparent_with_index_and_within_same_parent() {
        let mut w = World::new();
        let p = spawn_node(&mut w, "p", None);
        let x = spawn_node(&mut w, "x", Some(p));
        let y = spawn_node(&mut w, "y", Some(p));
        let z = spawn_node(&mut w, "z", Some(p));
        // move x to the end: index is interpreted against the list *after* removal
        reparent(&mut w, x, Some(p), Some(2)).unwrap();
        assert_eq!(children(&w, p), vec![y, z, x]);
        reparent(&mut w, x, Some(p), Some(0)).unwrap();
        assert_eq!(children(&w, p), vec![x, y, z]);
        // out-of-range index clamps
        reparent(&mut w, x, Some(p), Some(99)).unwrap();
        assert_eq!(children(&w, p), vec![y, z, x]);
    }

    #[test]
    fn delete_subtree_removes_descendants_and_unlinks_parent() {
        let (mut w, a, b, c, d) = tree();
        let removed = delete_subtree(&mut w, b);
        assert_eq!(removed, vec![b, c]);
        assert!(!w.contains(b) && !w.contains(c));
        assert!(children(&w, a).is_empty());
        assert!(w.contains(d));
    }

    #[test]
    fn subtree_is_preorder() {
        let (mut w, a, b, c, _d) = tree();
        let b2 = spawn_node(&mut w, "b2", Some(a));
        assert_eq!(subtree(&w, a), vec![a, b, c, b2]);
    }

    #[test]
    fn duplicate_subtree_copies_structure_and_inserts_after_original() {
        let (mut w, a, b, c, _d) = tree();
        let reg = Registry::with_defaults();
        let copy = duplicate_subtree(&mut w, &reg, b).unwrap();
        assert_ne!(copy, b);
        assert_eq!(children(&w, a), vec![b, copy]);
        assert_eq!(display_name(&w, copy), "b (1)");
        let copy_kids = children(&w, copy);
        assert_eq!(copy_kids.len(), 1);
        assert_ne!(copy_kids[0], c);
        assert_eq!(parent_of(&w, copy_kids[0]), Some(copy));
        assert_eq!(display_name(&w, copy_kids[0]), "c"); // only the root gets renamed
        // original untouched
        assert_eq!(children(&w, b), vec![c]);
    }

    #[test]
    fn duplicate_copies_registered_component_values() {
        use crate::ecs::Transform;
        let (mut w, _a, b, _c, _d) = tree();
        w.insert_one(b, Transform::from_position(glam::Vec3::new(1.0, 2.0, 3.0))).unwrap();
        let reg = Registry::with_defaults();
        let copy = duplicate_subtree(&mut w, &reg, b).unwrap();
        assert_eq!(w.get::<&Transform>(copy).unwrap().position, glam::Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn duplicate_root_places_copy_after_it() {
        let (mut w, a, _b, _c, d) = tree();
        let reg = Registry::with_defaults();
        let copy = duplicate_subtree(&mut w, &reg, a).unwrap();
        assert_eq!(roots(&w), vec![a, copy, d]);
    }

    #[test]
    fn reparent_to_root_at_index_reorders_roots() {
        let mut w = World::new();
        let a = spawn_node(&mut w, "a", None);
        let b = spawn_node(&mut w, "b", None);
        let c = spawn_node(&mut w, "c", None);
        reparent(&mut w, c, None, Some(0)).unwrap();
        assert_eq!(roots(&w), vec![c, a, b]);
        reparent(&mut w, c, None, Some(1)).unwrap();
        assert_eq!(roots(&w), vec![a, c, b]);
        reparent(&mut w, a, None, Some(2)).unwrap();
        assert_eq!(roots(&w), vec![c, b, a]);
        // repeated insertion at the front never collides
        for _ in 0..20 {
            reparent(&mut w, a, None, Some(0)).unwrap();
            reparent(&mut w, b, None, Some(0)).unwrap();
        }
        assert_eq!(roots(&w), vec![b, a, c]);
    }

    #[test]
    fn unique_name_increments() {
        let mut w = World::new();
        let a = spawn_node(&mut w, "Cube", None);
        let reg = Registry::with_defaults();
        let b = duplicate_subtree(&mut w, &reg, a).unwrap();
        let c = duplicate_subtree(&mut w, &reg, a).unwrap();
        assert_eq!(display_name(&w, b), "Cube (1)");
        assert_eq!(display_name(&w, c), "Cube (2)");
        let d = duplicate_subtree(&mut w, &reg, b).unwrap();
        assert_eq!(display_name(&w, d), "Cube (3)");
    }

    #[test]
    fn effective_visibility_and_lock_inherit_from_ancestors() {
        let (mut w, a, _b, c, d) = tree();
        assert!(is_visible(&w, c));
        w.insert_one(a, Visible(false)).unwrap();
        assert!(!is_visible(&w, c));
        assert!(is_visible(&w, d));
        w.insert_one(a, Locked(true)).unwrap();
        assert!(is_locked(&w, c));
        assert!(!is_locked(&w, d));
    }

    #[test]
    fn entities_without_hierarchy_components_are_roots_with_fallback_name() {
        let mut w = World::new();
        let e = w.spawn((42u32,));
        assert_eq!(roots(&w), vec![e]);
        assert!(display_name(&w, e).starts_with("Entity "));
        // can be adopted
        let p = spawn_node(&mut w, "p", None);
        reparent(&mut w, e, Some(p), None).unwrap();
        assert_eq!(children(&w, p), vec![e]);
    }
}
