//! A typed resource map: shared, singleton game state that systems read
//! without a god struct. At most one value per Rust type, keyed by `TypeId`
//! (so two different types that happen to share a name stay separate).
//!
//! The `Option`-returning methods never panic. `require` / `require_mut` are
//! conveniences for "this must exist" call sites and panic with a message
//! that names the missing type.

use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;

#[derive(Default)]
pub struct Resources {
    map: HashMap<TypeId, Box<dyn Any>>,
}

impl Resources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `value`, returning the previous value of the same type, if any.
    pub fn insert<T: 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|old| old.downcast::<T>().ok())
            .map(|b| *b)
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|b| b.downcast_ref::<T>())
    }

    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.map.get_mut(&TypeId::of::<T>()).and_then(|b| b.downcast_mut::<T>())
    }

    /// Removes and returns the value of type `T`, if present.
    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        self.map.remove(&TypeId::of::<T>()).and_then(|b| b.downcast::<T>().ok()).map(|b| *b)
    }

    pub fn contains<T: 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Returns the value of type `T`, inserting `make()` first if absent.
    pub fn get_or_insert_with<T: 'static>(&mut self, make: impl FnOnce() -> T) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(make()))
            .downcast_mut::<T>()
            .expect("Resources invariant: TypeId key always maps to a value of that type")
    }

    /// Like `get`, but panics naming the missing type.
    #[track_caller]
    pub fn require<T: 'static>(&self) -> &T {
        match self.get::<T>() {
            Some(v) => v,
            None => missing::<T>(),
        }
    }

    /// Like `get_mut`, but panics naming the missing type.
    #[track_caller]
    pub fn require_mut<T: 'static>(&mut self) -> &mut T {
        match self.get_mut::<T>() {
            Some(v) => v,
            None => missing::<T>(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cold]
#[track_caller]
fn missing<T: 'static>() -> ! {
    panic!(
        "resource `{}` not found; insert it with `resources.insert(..)` before running systems that need it",
        type_name::<T>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    mod a {
        #[derive(Debug, PartialEq)]
        pub struct Config(pub i32);
    }
    mod b {
        #[derive(Debug, PartialEq)]
        pub struct Config(pub i32);
    }

    #[derive(Debug, PartialEq)]
    struct Score(u32);
    #[derive(Debug, PartialEq)]
    struct Name(String);

    #[test]
    fn insert_and_get_by_type() {
        let mut r = Resources::new();
        assert!(r.is_empty());
        assert_eq!(r.insert(Score(3)), None);
        assert_eq!(r.get::<Score>(), Some(&Score(3)));
        assert!(r.contains::<Score>());
        assert!(!r.contains::<Name>());
        assert_eq!(r.get::<Name>(), None);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn different_types_are_isolated() {
        let mut r = Resources::new();
        r.insert(Score(1));
        r.insert(Name("x".into()));
        r.get_mut::<Score>().unwrap().0 = 9;
        assert_eq!(r.get::<Score>(), Some(&Score(9)));
        assert_eq!(r.get::<Name>(), Some(&Name("x".into())));
        assert_eq!(r.remove::<Score>(), Some(Score(9)));
        assert!(r.contains::<Name>());
        assert_eq!(r.remove::<Score>(), None);
    }

    #[test]
    fn same_named_types_in_different_modules_do_not_collide() {
        let mut r = Resources::new();
        r.insert(a::Config(1));
        r.insert(b::Config(2));
        assert_eq!(r.len(), 2);
        assert_eq!(r.get::<a::Config>(), Some(&a::Config(1)));
        assert_eq!(r.get::<b::Config>(), Some(&b::Config(2)));
    }

    #[test]
    fn insert_replaces_and_returns_old() {
        let mut r = Resources::new();
        r.insert(Score(1));
        assert_eq!(r.insert(Score(2)), Some(Score(1)));
        assert_eq!(r.len(), 1);
        assert_eq!(r.require::<Score>(), &Score(2));
    }

    #[test]
    fn get_or_insert_with_only_builds_once() {
        let mut r = Resources::new();
        r.get_or_insert_with(|| Score(5)).0 += 1;
        r.get_or_insert_with(|| Score(100)).0 += 1;
        assert_eq!(r.get::<Score>(), Some(&Score(7)));
    }

    #[test]
    #[should_panic(expected = "resource `dust_engine::resources::tests::Score` not found")]
    fn require_panics_naming_the_type() {
        let r = Resources::new();
        let _ = r.require::<Score>();
    }

    #[test]
    #[should_panic(expected = "not found")]
    fn require_mut_panics_when_missing() {
        let mut r = Resources::new();
        let _ = r.require_mut::<Name>();
    }
}
