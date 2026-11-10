//! Reflection-lite: components describe their editable fields once, and the
//! inspector UI / undo commands work from those descriptions.
//!
//! Register a component with [`ComponentBuilder`]:
//! ```ignore
//! reg.register(
//!     ComponentBuilder::new("Health", Health::default)
//!         .f32("max", |h| h.max, |h, v| h.max = v).range(1.0, 1000.0).tooltip("Starting HP")
//! );
//! ```
//! Registered components must be `Clone` (needed for duplicate / undo of
//! delete). Components that should survive duplicate/undo but have no editable
//! fields use [`Registry::register_cloneable`].

use std::any::{Any, TypeId};
use std::rc::Rc;

use glam::Vec3;
use hecs::{Component, Entity, EntityBuilderClone, BuiltEntityClone, World};

use super::hierarchy::{Children, Locked, Name, Parent, Primitive, PrimitiveShape, SpawnOrder, Visible};

/// A dynamically-typed field value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    F32(f32),
    Vec3(Vec3),
    Bool(bool),
    Str(String),
    /// Index into the variants of [`FieldKind::Enum`].
    Enum(usize),
    /// Linear RGB, 0..1.
    Color([f32; 3]),
    AssetPath(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    F32 { range: Option<(f32, f32)>, speed: f32 },
    Vec3,
    Bool,
    String,
    Enum(Vec<&'static str>),
    Color,
    AssetPath,
}

type Getter = Rc<dyn Fn(&World, Entity) -> Option<Value>>;
type Setter = Rc<dyn Fn(&mut World, Entity, &Value) -> bool>;

#[derive(Clone)]
pub struct FieldDesc {
    pub name: &'static str,
    pub kind: FieldKind,
    pub tooltip: &'static str,
    get: Getter,
    set: Setter,
}

impl FieldDesc {
    pub fn get(&self, world: &World, e: Entity) -> Option<Value> {
        (self.get)(world, e)
    }
    /// Returns false when the entity lacks the component or the value's type
    /// doesn't match the field.
    pub fn set(&self, world: &mut World, e: Entity, v: &Value) -> bool {
        (self.set)(world, e, v)
    }
}

#[derive(Clone)]
pub struct ComponentDesc {
    pub name: &'static str,
    pub fields: Vec<FieldDesc>,
    /// Whether the inspector offers "Remove Component".
    pub removable: bool,
    type_id: TypeId,
    has: Rc<dyn Fn(&World, Entity) -> bool>,
    add_default: Rc<dyn Fn(&mut World, Entity) -> bool>,
    remove: Rc<dyn Fn(&mut World, Entity) -> bool>,
    snapshot: Rc<dyn Fn(&World, Entity) -> Option<Box<dyn Any>>>,
    restore: Rc<dyn Fn(&mut World, Entity, &dyn Any) -> bool>,
    clone_into: Rc<dyn Fn(&World, Entity, &mut EntityBuilderClone)>,
}

impl ComponentDesc {
    pub fn has(&self, w: &World, e: Entity) -> bool {
        (self.has)(w, e)
    }
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }
    pub fn field(&self, name: &str) -> Option<&FieldDesc> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// Typed builder; erased into a [`ComponentDesc`] by [`Registry::register`].
pub struct ComponentBuilder<T: Component + Clone> {
    name: &'static str,
    default: fn() -> T,
    fields: Vec<FieldDesc>,
    removable: bool,
}

impl<T: Component + Clone> ComponentBuilder<T> {
    pub fn new(name: &'static str, default: fn() -> T) -> Self {
        Self { name, default, fields: Vec::new(), removable: true }
    }

    /// Hide the "Remove Component" button (e.g. Transform).
    pub fn not_removable(mut self) -> Self {
        self.removable = false;
        self
    }

    fn push(
        mut self,
        name: &'static str,
        kind: FieldKind,
        get: impl Fn(&T) -> Option<Value> + 'static,
        set: impl Fn(&mut T, &Value) -> bool + 'static,
    ) -> Self {
        self.fields.push(FieldDesc {
            name,
            kind,
            tooltip: "",
            get: Rc::new(move |w, e| w.get::<&T>(e).ok().and_then(|c| get(&c))),
            set: Rc::new(move |w, e, v| match w.get::<&mut T>(e) {
                Ok(mut c) => set(&mut c, v),
                Err(_) => false,
            }),
        });
        self
    }

    pub fn f32(self, name: &'static str, get: fn(&T) -> f32, set: fn(&mut T, f32)) -> Self {
        self.push(
            name,
            FieldKind::F32 { range: None, speed: 0.1 },
            move |t| Some(Value::F32(get(t))),
            move |t, v| match v {
                Value::F32(x) => {
                    set(t, *x);
                    true
                }
                _ => false,
            },
        )
    }

    pub fn vec3(self, name: &'static str, get: fn(&T) -> Vec3, set: fn(&mut T, Vec3)) -> Self {
        self.push(
            name,
            FieldKind::Vec3,
            move |t| Some(Value::Vec3(get(t))),
            move |t, v| match v {
                Value::Vec3(x) => {
                    set(t, *x);
                    true
                }
                _ => false,
            },
        )
    }

    pub fn bool(self, name: &'static str, get: fn(&T) -> bool, set: fn(&mut T, bool)) -> Self {
        self.push(
            name,
            FieldKind::Bool,
            move |t| Some(Value::Bool(get(t))),
            move |t, v| match v {
                Value::Bool(x) => {
                    set(t, *x);
                    true
                }
                _ => false,
            },
        )
    }

    pub fn string(self, name: &'static str, get: fn(&T) -> String, set: fn(&mut T, String)) -> Self {
        self.push(
            name,
            FieldKind::String,
            move |t| Some(Value::Str(get(t))),
            move |t, v| match v {
                Value::Str(x) => {
                    set(t, x.clone());
                    true
                }
                _ => false,
            },
        )
    }

    pub fn asset_path(self, name: &'static str, get: fn(&T) -> String, set: fn(&mut T, String)) -> Self {
        self.push(
            name,
            FieldKind::AssetPath,
            move |t| Some(Value::AssetPath(get(t))),
            move |t, v| match v {
                Value::AssetPath(x) => {
                    set(t, x.clone());
                    true
                }
                _ => false,
            },
        )
    }

    pub fn color(self, name: &'static str, get: fn(&T) -> [f32; 3], set: fn(&mut T, [f32; 3])) -> Self {
        self.push(
            name,
            FieldKind::Color,
            move |t| Some(Value::Color(get(t))),
            move |t, v| match v {
                Value::Color(x) => {
                    set(t, *x);
                    true
                }
                _ => false,
            },
        )
    }

    pub fn enumeration(
        self,
        name: &'static str,
        variants: &[&'static str],
        get: fn(&T) -> usize,
        set: fn(&mut T, usize),
    ) -> Self {
        let n = variants.len();
        self.push(
            name,
            FieldKind::Enum(variants.to_vec()),
            move |t| Some(Value::Enum(get(t))),
            move |t, v| match v {
                Value::Enum(x) if *x < n => {
                    set(t, *x);
                    true
                }
                _ => false,
            },
        )
    }

    /// Restrict the most recently added f32 field (slider/drag clamp).
    pub fn range(mut self, min: f32, max: f32) -> Self {
        if let Some(FieldDesc { kind: FieldKind::F32 { range, .. }, .. }) = self.fields.last_mut() {
            *range = Some((min, max));
        }
        self
    }

    /// Drag speed of the most recently added f32 field.
    pub fn speed(mut self, s: f32) -> Self {
        if let Some(FieldDesc { kind: FieldKind::F32 { speed, .. }, .. }) = self.fields.last_mut() {
            *speed = s;
        }
        self
    }

    /// Hover text for the most recently added field.
    pub fn tooltip(mut self, text: &'static str) -> Self {
        if let Some(f) = self.fields.last_mut() {
            f.tooltip = text;
        }
        self
    }

    fn build(self) -> ComponentDesc {
        let default = self.default;
        ComponentDesc {
            name: self.name,
            fields: self.fields,
            removable: self.removable,
            type_id: TypeId::of::<T>(),
            has: Rc::new(|w, e| w.satisfies::<&T>(e).unwrap_or(false)),
            add_default: Rc::new(move |w, e| {
                !w.satisfies::<&T>(e).unwrap_or(true) && w.insert_one(e, default()).is_ok()
            }),
            remove: Rc::new(|w, e| w.remove_one::<T>(e).is_ok()),
            snapshot: Rc::new(|w, e| {
                w.get::<&T>(e).ok().map(|c| Box::new((*c).clone()) as Box<dyn Any>)
            }),
            restore: Rc::new(|w, e, any| match any.downcast_ref::<T>() {
                Some(c) => w.insert_one(e, c.clone()).is_ok(),
                None => false,
            }),
            clone_into: Rc::new(|w, e, b| {
                if let Ok(c) = w.get::<&T>(e) {
                    b.add((*c).clone());
                }
            }),
        }
    }
}

/// Opaque saved copy of one component (for undoing a removal).
pub struct ComponentSnapshot(Box<dyn Any>);

#[derive(Default, Clone)]
pub struct Registry {
    components: Vec<ComponentDesc>,
    /// Clone-only components: copied by duplicate/undo but not shown.
    hidden: Vec<Rc<dyn Fn(&World, Entity, &mut EntityBuilderClone)>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Editor components + all engine components.
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register_cloneable::<Name>();
        r.register_cloneable::<Parent>();
        r.register_cloneable::<Children>();
        r.register_cloneable::<Visible>();
        r.register_cloneable::<Locked>();
        r.register_cloneable::<SpawnOrder>();
        r.register(
            ComponentBuilder::new("Primitive", || Primitive(PrimitiveShape::Cube)).enumeration(
                "shape",
                &["Cube", "Sphere", "Plane", "Capsule"],
                |p| PrimitiveShape::ALL.iter().position(|s| *s == p.0).unwrap_or(0),
                |p, i| p.0 = PrimitiveShape::ALL[i],
            ),
        );
        crate::ecs::register_engine_components(&mut r);
        r
    }

    pub fn register<T: Component + Clone>(&mut self, b: ComponentBuilder<T>) {
        let desc = b.build();
        if let Some(existing) = self.components.iter_mut().find(|c| c.name == desc.name) {
            *existing = desc;
        } else {
            self.components.push(desc);
        }
    }

    pub fn register_cloneable<T: Component + Clone>(&mut self) {
        self.hidden.push(Rc::new(|w, e, b| {
            if let Ok(c) = w.get::<&T>(e) {
                b.add((*c).clone());
            }
        }));
    }

    pub fn components(&self) -> &[ComponentDesc] {
        &self.components
    }

    pub fn find(&self, name: &str) -> Option<&ComponentDesc> {
        self.components.iter().find(|c| c.name == name)
    }

    /// Components present on `e`.
    pub fn present(&self, w: &World, e: Entity) -> Vec<&ComponentDesc> {
        self.components.iter().filter(|c| c.has(w, e)).collect()
    }

    /// Components addable to `e`, optionally filtered by a case-insensitive substring.
    pub fn addable(&self, w: &World, e: Entity, filter: &str) -> Vec<&ComponentDesc> {
        let f = filter.to_lowercase();
        self.components
            .iter()
            .filter(|c| !c.has(w, e) && c.name.to_lowercase().contains(&f))
            .collect()
    }

    pub fn get_value(&self, w: &World, e: Entity, comp: &str, field: &str) -> Option<Value> {
        self.find(comp)?.field(field)?.get(w, e)
    }

    pub fn set_value(&self, w: &mut World, e: Entity, comp: &str, field: &str, v: &Value) -> bool {
        match self.find(comp).and_then(|c| c.field(field)) {
            Some(f) => f.set(w, e, v),
            None => false,
        }
    }

    pub fn add_component(&self, w: &mut World, e: Entity, comp: &str) -> bool {
        self.find(comp).map(|c| (c.add_default)(w, e)).unwrap_or(false)
    }

    pub fn remove_component(&self, w: &mut World, e: Entity, comp: &str) -> bool {
        self.find(comp).map(|c| (c.remove)(w, e)).unwrap_or(false)
    }

    pub fn snapshot_component(&self, w: &World, e: Entity, comp: &str) -> Option<ComponentSnapshot> {
        (self.find(comp)?.snapshot)(w, e).map(ComponentSnapshot)
    }

    pub fn restore_component(&self, w: &mut World, e: Entity, comp: &str, s: &ComponentSnapshot) -> bool {
        self.find(comp).map(|c| (c.restore)(w, e, s.0.as_ref())).unwrap_or(false)
    }

    /// Clones every registered / cloneable component of `e`. Components the
    /// registry doesn't know about are NOT included.
    pub fn snapshot(&self, w: &World, e: Entity) -> Option<BuiltEntityClone> {
        if !w.contains(e) {
            return None;
        }
        let mut b = EntityBuilderClone::new();
        for h in &self.hidden {
            h(w, e, &mut b);
        }
        for c in &self.components {
            (c.clone_into)(w, e, &mut b);
        }
        Some(b.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{Camera, Light, LightKind, PlayerController, Transform};

    #[derive(Clone, Debug, PartialEq)]
    struct Health {
        max: f32,
        alive: bool,
        label: String,
        mode: usize,
    }
    impl Default for Health {
        fn default() -> Self {
            Self { max: 10.0, alive: true, label: "hp".into(), mode: 0 }
        }
    }

    fn reg() -> Registry {
        let mut r = Registry::new();
        r.register(
            ComponentBuilder::new("Health", Health::default)
                .f32("max", |h| h.max, |h, v| h.max = v)
                .range(1.0, 100.0)
                .tooltip("starting hp")
                .bool("alive", |h| h.alive, |h, v| h.alive = v)
                .string("label", |h| h.label.clone(), |h, v| h.label = v)
                .enumeration("mode", &["a", "b"], |h| h.mode, |h, v| h.mode = v),
        );
        r
    }

    #[test]
    fn field_metadata_is_recorded() {
        let r = reg();
        let f = r.find("Health").unwrap().field("max").unwrap();
        assert_eq!(f.kind, FieldKind::F32 { range: Some((1.0, 100.0)), speed: 0.1 });
        assert_eq!(f.tooltip, "starting hp");
        assert_eq!(r.find("Health").unwrap().fields.len(), 4);
    }

    #[test]
    fn get_set_roundtrip_and_type_mismatch_rejected() {
        let r = reg();
        let mut w = World::new();
        let e = w.spawn((Health::default(),));
        assert_eq!(r.get_value(&w, e, "Health", "max"), Some(Value::F32(10.0)));
        assert!(r.set_value(&mut w, e, "Health", "max", &Value::F32(50.0)));
        assert_eq!(w.get::<&Health>(e).unwrap().max, 50.0);
        assert!(!r.set_value(&mut w, e, "Health", "max", &Value::Bool(true)));
        assert!(!r.set_value(&mut w, e, "Health", "mode", &Value::Enum(5)), "enum out of range");
        assert!(!r.set_value(&mut w, e, "Nope", "max", &Value::F32(1.0)));
        assert!(!r.set_value(&mut w, e, "Health", "nope", &Value::F32(1.0)));
    }

    #[test]
    fn get_on_entity_without_component_is_none() {
        let r = reg();
        let mut w = World::new();
        let e = w.spawn(());
        assert_eq!(r.get_value(&w, e, "Health", "max"), None);
        assert!(!r.set_value(&mut w, e, "Health", "max", &Value::F32(1.0)));
    }

    #[test]
    fn add_remove_component() {
        let r = reg();
        let mut w = World::new();
        let e = w.spawn(());
        assert_eq!(r.addable(&w, e, "hea").len(), 1);
        assert!(r.add_component(&mut w, e, "Health"));
        assert!(!r.add_component(&mut w, e, "Health"), "no double add");
        assert!(r.addable(&w, e, "").is_empty());
        assert_eq!(r.present(&w, e).len(), 1);
        assert!(r.remove_component(&mut w, e, "Health"));
        assert!(!r.remove_component(&mut w, e, "Health"));
    }

    #[test]
    fn snapshot_and_restore_component() {
        let r = reg();
        let mut w = World::new();
        let e = w.spawn((Health { max: 77.0, ..Default::default() },));
        let snap = r.snapshot_component(&w, e, "Health").unwrap();
        r.remove_component(&mut w, e, "Health");
        assert!(r.restore_component(&mut w, e, "Health", &snap));
        assert_eq!(w.get::<&Health>(e).unwrap().max, 77.0);
    }

    #[test]
    fn engine_components_registered_with_defaults() {
        let r = Registry::with_defaults();
        for n in ["Transform", "Camera", "Light", "PlayerController", "Primitive"] {
            assert!(r.find(n).is_some(), "{n} missing");
        }
        assert!(!r.find("Transform").unwrap().removable);
    }

    #[test]
    fn transform_rotation_edits_as_euler_degrees() {
        let r = Registry::with_defaults();
        let mut w = World::new();
        let e = w.spawn((Transform::default(),));
        assert!(r.set_value(&mut w, e, "Transform", "rotation", &Value::Vec3(Vec3::new(0.0, 90.0, 0.0))));
        let got = match r.get_value(&w, e, "Transform", "rotation").unwrap() {
            Value::Vec3(v) => v,
            _ => panic!(),
        };
        assert!((got - Vec3::new(0.0, 90.0, 0.0)).abs().max_element() < 1e-3, "{got:?}");
        let q = w.get::<&Transform>(e).unwrap().rotation;
        assert!(q.angle_between(crate::ecs::euler_deg_to_quat(Vec3::new(0.0, 90.0, 0.0))) < 1e-2);
    }

    #[test]
    fn camera_fov_is_degrees_and_player_fields_edit() {
        let r = Registry::with_defaults();
        let mut w = World::new();
        let e = w.spawn((Camera::default(), PlayerController::default()));
        assert!(r.set_value(&mut w, e, "Camera", "fov_y_degrees", &Value::F32(90.0)));
        assert!((w.get::<&Camera>(e).unwrap().fov_y_radians - 90f32.to_radians()).abs() < 1e-5);
        assert!(r.set_value(&mut w, e, "PlayerController", "move_speed", &Value::F32(9.0)));
        assert_eq!(w.get::<&PlayerController>(e).unwrap().move_speed, 9.0);
    }

    #[test]
    fn light_kind_switch_keeps_other_params_sane() {
        let r = Registry::with_defaults();
        let mut w = World::new();
        let e = w.spawn((Light {
            color: Vec3::ONE,
            intensity: 1.0,
            kind: LightKind::Point { range: 8.0 },
            is_static: false,
        },));
        assert_eq!(r.get_value(&w, e, "Light", "kind"), Some(Value::Enum(1)));
        assert_eq!(r.get_value(&w, e, "Light", "range"), Some(Value::F32(8.0)));
        r.set_value(&mut w, e, "Light", "kind", &Value::Enum(0));
        assert!(matches!(w.get::<&Light>(e).unwrap().kind, LightKind::Directional { .. }));
        r.set_value(&mut w, e, "Light", "kind", &Value::Enum(1));
        assert!(matches!(w.get::<&Light>(e).unwrap().kind, LightKind::Point { .. }));
        r.set_value(&mut w, e, "Light", "color", &Value::Color([1.0, 0.5, 0.0]));
        assert_eq!(w.get::<&Light>(e).unwrap().color, Vec3::new(1.0, 0.5, 0.0));
    }

    #[test]
    fn full_snapshot_clones_hidden_and_registered() {
        let r = Registry::with_defaults();
        let mut w = World::new();
        let e = crate::editor::hierarchy::spawn_object(&mut w, "x", None, None, Vec3::X);
        let s = r.snapshot(&w, e).unwrap();
        let n = w.spawn(&s);
        assert_eq!(w.get::<&Name>(n).unwrap().0, "x");
        assert_eq!(w.get::<&Transform>(n).unwrap().position, Vec3::X);
    }
}
