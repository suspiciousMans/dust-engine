//! Single / multi selection with a primary (active) entity.
//!
//! The primary is the entity the inspector shows first and the one gizmos
//! anchor to. It is always a member of the selection (or `None` when empty).

use hecs::{Entity, World};

use super::hierarchy::is_descendant_of;

#[derive(Clone, Debug, Default)]
pub struct Selection {
    items: Vec<Entity>,
    primary: Option<Entity>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn contains(&self, e: Entity) -> bool {
        self.items.contains(&e)
    }
    pub fn primary(&self) -> Option<Entity> {
        self.primary
    }
    /// Selected entities in the order they were selected.
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.items.iter().copied()
    }
    pub fn to_vec(&self) -> Vec<Entity> {
        self.items.clone()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.primary = None;
    }

    /// Plain click: replace the selection.
    pub fn select_only(&mut self, e: Entity) {
        self.items.clear();
        self.items.push(e);
        self.primary = Some(e);
    }

    /// Adds `e` (if new) and makes it primary.
    pub fn add(&mut self, e: Entity) {
        if !self.items.contains(&e) {
            self.items.push(e);
        }
        self.primary = Some(e);
    }

    pub fn remove(&mut self, e: Entity) {
        self.items.retain(|x| *x != e);
        if self.primary == Some(e) {
            self.primary = self.items.last().copied();
        }
    }

    /// Ctrl+click.
    pub fn toggle(&mut self, e: Entity) {
        if self.contains(e) {
            self.remove(e);
        } else {
            self.add(e);
        }
    }

    /// Shift+click: select everything between the primary and `to`,
    /// following `visible_order` (the flattened tree as displayed).
    pub fn select_range(&mut self, visible_order: &[Entity], to: Entity) {
        let anchor = self.primary.and_then(|p| visible_order.iter().position(|e| *e == p));
        let target = visible_order.iter().position(|e| *e == to);
        match (anchor, target) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                for e in &visible_order[lo..=hi] {
                    if !self.items.contains(e) {
                        self.items.push(*e);
                    }
                }
                self.primary = Some(to);
            }
            _ => self.select_only(to),
        }
    }

    pub fn select_all(&mut self, all: impl IntoIterator<Item = Entity>) {
        self.items.clear();
        self.items.extend(all);
        self.primary = self.items.last().copied();
    }

    /// Drops entities that no longer exist.
    pub fn prune(&mut self, world: &World) {
        self.items.retain(|e| world.contains(*e));
        if self.primary.map(|p| !self.items.contains(&p)).unwrap_or(false) {
            self.primary = self.items.last().copied();
        }
    }

    /// Selected entities that have no selected ancestor. Use this for
    /// delete / duplicate / drag so a parent and its selected child aren't
    /// processed twice.
    pub fn top_level(&self, world: &World) -> Vec<Entity> {
        self.items
            .iter()
            .copied()
            .filter(|e| !self.items.iter().any(|o| o != e && is_descendant_of(world, *e, *o)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::hierarchy::{reparent, spawn_node};
    use hecs::World;

    fn three() -> (World, hecs::Entity, hecs::Entity, hecs::Entity) {
        let mut w = World::new();
        let a = spawn_node(&mut w, "a", None);
        let b = spawn_node(&mut w, "b", None);
        let c = spawn_node(&mut w, "c", None);
        (w, a, b, c)
    }

    #[test]
    fn select_only_replaces() {
        let (_w, a, b, _c) = three();
        let mut s = Selection::default();
        s.select_only(a);
        s.select_only(b);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![b]);
        assert_eq!(s.primary(), Some(b));
        assert!(!s.contains(a));
    }

    #[test]
    fn toggle_adds_removes_and_moves_primary() {
        let (_w, a, b, c) = three();
        let mut s = Selection::default();
        s.toggle(a);
        s.toggle(b);
        s.toggle(c);
        assert_eq!(s.len(), 3);
        assert_eq!(s.primary(), Some(c));
        s.toggle(c); // removing primary falls back to the previous one
        assert_eq!(s.primary(), Some(b));
        s.toggle(a);
        s.toggle(b);
        assert!(s.is_empty());
        assert_eq!(s.primary(), None);
    }

    #[test]
    fn add_is_idempotent_and_makes_primary() {
        let (_w, a, b, _c) = three();
        let mut s = Selection::default();
        s.add(a);
        s.add(b);
        s.add(a);
        assert_eq!(s.len(), 2);
        assert_eq!(s.primary(), Some(a));
    }

    #[test]
    fn range_select_uses_given_order() {
        let (_w, a, b, c) = three();
        let order = [a, b, c];
        let mut s = Selection::default();
        s.select_only(a);
        s.select_range(&order, c);
        assert_eq!(s.len(), 3);
        assert_eq!(s.primary(), Some(c));
        s.select_only(c);
        s.select_range(&order, a);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn prune_drops_dead_entities_and_fixes_primary() {
        let (mut w, a, b, _c) = three();
        let mut s = Selection::default();
        s.add(a);
        s.add(b);
        w.despawn(b).unwrap();
        s.prune(&w);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![a]);
        assert_eq!(s.primary(), Some(a));
    }

    #[test]
    fn top_level_excludes_entities_whose_ancestor_is_selected() {
        let (mut w, a, b, c) = three();
        reparent(&mut w, b, Some(a), None).unwrap();
        let mut s = Selection::default();
        s.add(a);
        s.add(b);
        s.add(c);
        assert_eq!(s.top_level(&w), vec![a, c]);
    }

    #[test]
    fn select_all_and_clear() {
        let (w, a, b, c) = three();
        let mut s = Selection::default();
        s.select_all(crate::editor::hierarchy::roots(&w));
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![a, b, c]);
        s.clear();
        assert!(s.is_empty());
    }
}
