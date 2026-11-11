//! Versioned, diff-friendly scene files.
//!
//! A [`Scene`] is plain data: a list of [`SceneEntity`] in hierarchy
//! pre-order (roots by `SpawnOrder`, children by `Children` order), each with
//! a **persistent [`EntityId`]** (not a hecs handle), its name, parent link and
//! every component the editor [`Registry`] knows, stored as named fields.
//!
//! Format guarantees (these are what make scenes reviewable in git):
//! * `format_version` is checked on load; older files go through a
//!   [`Migration`] chain, newer files are refused instead of silently mangled.
//! * Output is deterministic: entities in hierarchy order, component and
//!   field maps sorted, floats rounded to 6 decimals, atomic write
//!   (tmp + rename) so a crash never leaves a half-written scene.
//! * Components/fields this build doesn't know are **preserved** through load
//!   and save (see [`UnknownComponents`]) so opening a scene made by a newer
//!   build or by game code with extra components doesn't destroy data.
//!
//! The unit of "run the game then come back" is [`capture_world`] /
//! [`restore_world`]: the shell wraps that pair as its `SceneSnapshot`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use hecs::{BuiltEntityClone, Entity, World};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::hierarchy::{self, Children, Locked, Name, Parent, SpawnOrder, Visible};
use super::inspector::{FieldKind, Registry, Value};
use super::prefab::{MemberRef, PrefabInstance, PrefabMember};

pub const SCENE_FORMAT_VERSION: u32 = 1;

// --------------------------------------------------------------- EntityId

/// Persistent identity of an entity inside scene files. 64 random-ish bits,
/// written as 16 hex digits. Survives save/load, duplicate-with-fixup,
/// undo/redo and play-mode snapshots; hecs `Entity` handles do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(pub u64);

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl EntityId {
    /// Fresh id, unique across processes with overwhelming probability
    /// (clock + pid + counter, mixed).
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mixed = splitmix64(nanos ^ splitmix64(n) ^ ((std::process::id() as u64) << 32));
        Self(mixed.max(1))
    }

    /// Deterministic id derived from a string (FNV-1a + mix). Used when
    /// converting legacy files so that converting twice gives the same ids.
    pub fn from_key(key: &str) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(splitmix64(h).max(1))
    }
}

impl Default for EntityId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for EntityId {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        u64::from_str_radix(s, 16).map(Self)
    }
}

impl Serialize for EntityId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ------------------------------------------------------------ data model

/// A field value in a scene file. Enums are stored by **variant name**, not
/// index, so reordering variants in code doesn't corrupt old files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SceneValue {
    F32(f32),
    Vec3(f32, f32, f32),
    Bool(bool),
    Str(String),
    Enum(String),
    Color(f32, f32, f32),
    Asset(String),
}

impl SceneValue {
    /// Equality with a small tolerance for floats (file round trips and
    /// euler↔quaternion conversion are not bit exact).
    pub fn approx_eq(&self, other: &SceneValue) -> bool {
        const E: f32 = 1e-4;
        let c = |a: f32, b: f32| (a - b).abs() <= E * (1.0 + a.abs().max(b.abs()));
        match (self, other) {
            (Self::F32(a), Self::F32(b)) => c(*a, *b),
            (Self::Vec3(a, b, d), Self::Vec3(x, y, z)) | (Self::Color(a, b, d), Self::Color(x, y, z)) => {
                c(*a, *x) && c(*b, *y) && c(*d, *z)
            }
            _ => self == other,
        }
    }

    fn quantized(&self) -> SceneValue {
        let q = |x: f32| ((x as f64 * 1e6).round() / 1e6) as f32;
        match self {
            Self::F32(x) => Self::F32(q(*x)),
            Self::Vec3(a, b, c) => Self::Vec3(q(*a), q(*b), q(*c)),
            Self::Color(a, b, c) => Self::Color(q(*a), q(*b), q(*c)),
            other => other.clone(),
        }
    }
}

pub fn value_to_scene(v: &Value, kind: &FieldKind) -> SceneValue {
    match v {
        Value::F32(x) => SceneValue::F32(*x),
        Value::Vec3(v) => SceneValue::Vec3(v.x, v.y, v.z),
        Value::Bool(b) => SceneValue::Bool(*b),
        Value::Str(s) => SceneValue::Str(s.clone()),
        Value::Enum(i) => match kind {
            FieldKind::Enum(names) => SceneValue::Enum(names.get(*i).copied().unwrap_or("?").to_string()),
            _ => SceneValue::Enum(i.to_string()),
        },
        Value::Color(c) => SceneValue::Color(c[0], c[1], c[2]),
        Value::AssetPath(p) => SceneValue::Asset(p.clone()),
    }
}

/// `None` if the stored value doesn't fit the field (unknown enum variant,
/// type changed between versions, ...).
pub fn scene_to_value(v: &SceneValue, kind: &FieldKind) -> Option<Value> {
    Some(match (v, kind) {
        (SceneValue::F32(x), FieldKind::F32 { .. }) => Value::F32(*x),
        (SceneValue::Vec3(a, b, c), FieldKind::Vec3) => Value::Vec3(glam::Vec3::new(*a, *b, *c)),
        (SceneValue::Bool(b), FieldKind::Bool) => Value::Bool(*b),
        (SceneValue::Str(s), FieldKind::String) => Value::Str(s.clone()),
        (SceneValue::Enum(n), FieldKind::Enum(names)) => Value::Enum(names.iter().position(|x| x == n)?),
        (SceneValue::Color(a, b, c), FieldKind::Color) => Value::Color([*a, *b, *c]),
        (SceneValue::Asset(p), FieldKind::AssetPath) => Value::AssetPath(p.clone()),
        _ => return None,
    })
}

/// Raw RON text kept verbatim as one string per line, so a change inside an
/// opaque payload still shows up as a line diff in git.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawRon(pub Vec<String>);

impl RawRon {
    pub fn from_text(text: &str) -> Self {
        Self(text.lines().map(str::to_string).collect())
    }
    pub fn text(&self) -> String {
        self.0.join("\n")
    }
    pub fn from_value<T: Serialize>(v: &T) -> Result<Self, ron::Error> {
        Ok(Self::from_text(&ron::ser::to_string_pretty(v, pretty())?))
    }
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, ron::error::SpannedError> {
        ron::from_str(&self.text())
    }
}

pub type FieldMap = BTreeMap<String, SceneValue>;

/// Components the registry didn't recognise while loading. Kept on the entity
/// and written back on save so nothing is lost. Registered as a cloneable
/// component (see [`register_scene_components`]) so snapshots/undo keep it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnknownComponents {
    /// Unknown components with structured fields, plus unknown *fields* of
    /// known components (merged back into the same component on save).
    pub fields: BTreeMap<String, FieldMap>,
    /// Opaque payloads (e.g. legacy level data).
    pub raw: BTreeMap<String, RawRon>,
}

impl UnknownComponents {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.raw.is_empty()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}
fn is_empty<K, V>(m: &BTreeMap<K, V>) -> bool {
    m.is_empty()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneEntity {
    pub id: EntityId,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<EntityId>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub components: BTreeMap<String, FieldMap>,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub raw: BTreeMap<String, RawRon>,
    /// Set on the root entity of a prefab instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefab: Option<PrefabInstance>,
    /// Which prefab entities (innermost prefab first) this entity came from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefab_member: Vec<MemberRef>,
}

impl SceneEntity {
    pub fn new(id: EntityId, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            parent: None,
            hidden: false,
            locked: false,
            components: BTreeMap::new(),
            raw: BTreeMap::new(),
            prefab: None,
            prefab_member: Vec::new(),
        }
    }
    pub fn field(&self, comp: &str, field: &str) -> Option<&SceneValue> {
        self.components.get(comp)?.get(field)
    }
    pub fn set_field(&mut self, comp: &str, field: &str, v: SceneValue) {
        self.components.entry(comp.to_string()).or_default().insert(field.to_string(), v);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub name: String,
    /// Scene-wide opaque settings (physics params, music, ...).
    #[serde(default, skip_serializing_if = "is_empty")]
    pub settings: BTreeMap<String, RawRon>,
    #[serde(default)]
    pub entities: Vec<SceneEntity>,
}

impl Default for Scene {
    fn default() -> Self {
        Self::new("")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SceneError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] ron::error::SpannedError),
    #[error("serialize: {0}")]
    Serialize(#[from] ron::Error),
    #[error("file has format_version {found} but this build only reads up to {supported}")]
    TooNew { found: u32, supported: u32 },
    #[error("no migration from scene format version {0}")]
    NoMigration(u32),
    #[error("migration from version {0} failed: {1}")]
    Migration(u32, String),
}

/// Upgrades a scene from format `from` to `from + 1`.
pub struct Migration {
    pub from: u32,
    pub apply: fn(&mut Scene) -> Result<(), String>,
}

/// Migrations shipped with this build. Add one entry here (and bump
/// [`SCENE_FORMAT_VERSION`]) whenever the on-disk format changes meaning.
pub fn builtin_migrations() -> Vec<Migration> {
    Vec::new()
}

pub fn pretty() -> ron::ser::PrettyConfig {
    ron::ser::PrettyConfig::default().indentor("  ".to_string())
}

/// Things worth showing the user after a load.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadReport {
    pub warnings: Vec<String>,
}

pub struct SpawnResult {
    /// Spawned entities in file order.
    pub entities: Vec<Entity>,
    /// File id -> id the entity got in the world (identity unless remapped).
    pub id_map: HashMap<EntityId, EntityId>,
    pub report: LoadReport,
}

impl SpawnResult {
    pub fn roots(&self, world: &World) -> Vec<Entity> {
        self.entities.iter().copied().filter(|e| hierarchy::parent_of(world, *e).is_none()).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remap {
    Never,
    /// Remap only ids already present in the world (default).
    OnCollision,
    Always,
}

#[derive(Clone, Copy, Debug)]
pub struct SpawnOptions {
    pub remap: Remap,
    /// Attach the scene's root entities under this entity.
    pub parent: Option<Entity>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self { remap: Remap::OnCollision, parent: None }
    }
}

// ---------------------------------------------------------------- Scene

impl Scene {
    pub fn new(name: &str) -> Self {
        Self { format_version: SCENE_FORMAT_VERSION, name: name.to_string(), settings: BTreeMap::new(), entities: Vec::new() }
    }

    pub fn entity(&self, id: EntityId) -> Option<&SceneEntity> {
        self.entities.iter().find(|e| e.id == id)
    }
    pub fn entity_mut(&mut self, id: EntityId) -> Option<&mut SceneEntity> {
        self.entities.iter_mut().find(|e| e.id == id)
    }
    pub fn roots(&self) -> impl Iterator<Item = &SceneEntity> {
        self.entities.iter().filter(|e| e.parent.is_none())
    }

    /// Captures the whole world. Entities missing an [`EntityId`] get one
    /// (and duplicate ids from copy/paste are repaired), hence `&mut`.
    pub fn from_world(world: &mut World, reg: &Registry) -> Scene {
        normalize_ids(world);
        let order = hierarchy_order(world);
        Self::build(world, reg, &order)
    }

    /// Captures `root` and its descendants only; `root` becomes a scene root.
    pub fn from_subtree(world: &mut World, reg: &Registry, root: Entity) -> Scene {
        normalize_ids(world);
        let order = hierarchy::subtree(world, root);
        Self::build(world, reg, &order)
    }

    /// One entity as scene data (no parent link), without normalizing ids.
    pub(crate) fn capture_entity(world: &World, reg: &Registry, e: Entity) -> SceneEntity {
        let mut s = Self::build(world, reg, &[e]);
        s.entities.pop().unwrap_or_else(|| SceneEntity::new(EntityId(0), ""))
    }

    fn build(world: &World, reg: &Registry, order: &[Entity]) -> Scene {
        let included: HashSet<Entity> = order.iter().copied().collect();
        let mut out = Scene::new("");
        for &e in order {
            let id = world.get::<&EntityId>(e).map(|i| *i).unwrap_or(EntityId(0));
            let mut se = SceneEntity::new(id, &hierarchy::display_name(world, e));
            se.parent = hierarchy::parent_of(world, e)
                .filter(|p| included.contains(p))
                .and_then(|p| world.get::<&EntityId>(p).ok().map(|i| *i));
            se.hidden = world.get::<&Visible>(e).map(|v| !v.0).unwrap_or(false);
            se.locked = world.get::<&Locked>(e).map(|v| v.0).unwrap_or(false);
            for desc in reg.present(world, e) {
                let mut fields = FieldMap::new();
                for f in &desc.fields {
                    if let Some(v) = f.get(world, e) {
                        fields.insert(f.name.to_string(), value_to_scene(&v, &f.kind).quantized());
                    }
                }
                se.components.insert(desc.name.to_string(), fields);
            }
            if let Ok(u) = world.get::<&UnknownComponents>(e) {
                for (k, fields) in &u.fields {
                    let entry = se.components.entry(k.clone()).or_default();
                    for (fk, fv) in fields {
                        entry.entry(fk.clone()).or_insert_with(|| fv.clone());
                    }
                }
                se.raw = u.raw.clone();
            }
            se.prefab = world.get::<&PrefabInstance>(e).ok().map(|p| (*p).clone());
            se.prefab_member = world.get::<&PrefabMember>(e).map(|m| m.0.clone()).unwrap_or_default();
            out.entities.push(se);
        }
        out
    }

    /// Spawns into `world` next to whatever is already there (additive
    /// load); ids that collide with existing ones are remapped.
    pub fn spawn_into(&self, world: &mut World, reg: &Registry) -> SpawnResult {
        self.spawn_with(world, reg, &SpawnOptions::default())
    }

    /// Clears the world and loads this scene with ids unchanged.
    pub fn load_replace(&self, world: &mut World, reg: &Registry) -> SpawnResult {
        world.clear();
        self.spawn_with(world, reg, &SpawnOptions { remap: Remap::Never, parent: None })
    }

    pub fn spawn_with(&self, world: &mut World, reg: &Registry, opts: &SpawnOptions) -> SpawnResult {
        let mut report = LoadReport::default();
        let existing: HashSet<EntityId> = world.query::<&EntityId>().iter().map(|(_, i)| *i).collect();
        let mut id_map: HashMap<EntityId, EntityId> = HashMap::new();
        let mut taken = existing.clone();
        for se in &self.entities {
            let remap = match opts.remap {
                Remap::Never => false,
                Remap::Always => true,
                Remap::OnCollision => existing.contains(&se.id),
            };
            let mut id = if remap { EntityId::new() } else { se.id };
            if id_map.contains_key(&se.id) {
                report.warnings.push(format!("duplicate entity id {} in scene; second one renamed", se.id));
                id = EntityId::new();
            }
            while taken.contains(&id) && remap {
                id = EntityId::new();
            }
            taken.insert(id);
            id_map.insert(se.id, id);
        }
        // Dedup collisions that survived Remap::Never within the same file.
        let mut first_seen: HashSet<EntityId> = HashSet::new();

        let mut base = world.query::<&SpawnOrder>().iter().map(|(_, o)| o.0).fold(0.0, f64::max);
        let mut spawned: Vec<Entity> = Vec::with_capacity(self.entities.len());
        let mut by_file_id: HashMap<EntityId, Entity> = HashMap::new();
        for se in &self.entities {
            let mut id = id_map[&se.id];
            if !first_seen.insert(id) {
                id = EntityId::new();
            }
            base += 1.0;
            let e = world.spawn((id, Name(se.name.clone()), SpawnOrder(base)));
            if se.hidden {
                let _ = world.insert_one(e, Visible(false));
            }
            if se.locked {
                let _ = world.insert_one(e, Locked(true));
            }
            let mut unknown = UnknownComponents { fields: BTreeMap::new(), raw: se.raw.clone() };
            for (cname, fields) in &se.components {
                match reg.find(cname) {
                    None => {
                        report.warnings.push(format!("'{}': unknown component '{cname}' kept as-is", se.name));
                        unknown.fields.insert(cname.clone(), fields.clone());
                    }
                    Some(desc) => {
                        reg.add_component(world, e, cname);
                        for f in &desc.fields {
                            if let Some(sv) = fields.get(f.name) {
                                match scene_to_value(sv, &f.kind) {
                                    Some(v) if f.set(world, e, &v) => {}
                                    _ => report.warnings.push(format!(
                                        "'{}': {cname}.{} has an unusable value {sv:?}",
                                        se.name, f.name
                                    )),
                                }
                            }
                        }
                        let extra: FieldMap = fields
                            .iter()
                            .filter(|(k, _)| desc.field(k).is_none())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        if !extra.is_empty() {
                            report.warnings.push(format!("'{}': {cname} has unknown fields, kept as-is", se.name));
                            unknown.fields.insert(cname.clone(), extra);
                        }
                    }
                }
            }
            if !unknown.is_empty() {
                let _ = world.insert_one(e, unknown);
            }
            if let Some(p) = &se.prefab {
                let _ = world.insert_one(e, p.clone());
            }
            if !se.prefab_member.is_empty() {
                let _ = world.insert_one(e, PrefabMember(se.prefab_member.clone()));
            }
            by_file_id.insert(se.id, e);
            spawned.push(e);
        }
        // Parent links (all entities exist now, so file order is irrelevant).
        for (se, &e) in self.entities.iter().zip(&spawned) {
            let parent = match se.parent {
                Some(pid) => match by_file_id.get(&pid) {
                    Some(p) => Some(*p),
                    None => {
                        report.warnings.push(format!("'{}': parent {pid} not found; made a root", se.name));
                        opts.parent
                    }
                },
                None => opts.parent,
            };
            if let Some(p) = parent {
                if hierarchy::reparent(world, e, Some(p), None).is_err() {
                    report.warnings.push(format!("'{}': invalid parent link ignored", se.name));
                }
            }
        }
        SpawnResult { entities: spawned, id_map, report }
    }

    // ------------------------------------------------------------- text/io

    pub fn to_ron(&self) -> Result<String, SceneError> {
        let mut s = ron::ser::to_string_pretty(self, pretty())?;
        s.push('\n');
        Ok(s)
    }

    pub fn from_ron(text: &str) -> Result<Scene, SceneError> {
        Self::from_ron_with(text, &builtin_migrations(), SCENE_FORMAT_VERSION)
    }

    pub fn from_ron_with(text: &str, migrations: &[Migration], target: u32) -> Result<Scene, SceneError> {
        let mut scene: Scene = ron::from_str(text)?;
        if scene.format_version > target {
            return Err(SceneError::TooNew { found: scene.format_version, supported: target });
        }
        while scene.format_version < target {
            let v = scene.format_version;
            let m = migrations.iter().find(|m| m.from == v).ok_or(SceneError::NoMigration(v))?;
            (m.apply)(&mut scene).map_err(|e| SceneError::Migration(v, e))?;
            scene.format_version = v + 1;
        }
        Ok(scene)
    }

    pub fn save(&self, path: &Path) -> Result<(), SceneError> {
        write_atomic(path, self.to_ron()?.as_bytes())?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Scene, SceneError> {
        Self::from_ron(&std::fs::read_to_string(path)?)
    }

    /// Every asset path referenced by `Asset` fields (sorted, deduped).
    pub fn asset_refs(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .entities
            .iter()
            .flat_map(|e| e.components.values())
            .flat_map(|c| c.values())
            .filter_map(|v| match v {
                SceneValue::Asset(p) if !p.is_empty() => Some(p.clone()),
                _ => None,
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Rewrites asset paths after a rename/move. Returns how many changed.
    pub fn rewrite_asset_paths(&mut self, map: &HashMap<String, String>) -> usize {
        let mut n = 0;
        for e in &mut self.entities {
            for c in e.components.values_mut() {
                for v in c.values_mut() {
                    if let SceneValue::Asset(p) = v {
                        if let Some(new) = map.get(p.as_str()) {
                            *p = new.clone();
                            n += 1;
                        }
                    }
                }
            }
        }
        n
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Roots in display order, each followed by its subtree; entities whose
/// parent no longer exists are appended last so nothing is silently dropped.
fn hierarchy_order(world: &World) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut seen: HashSet<Entity> = HashSet::new();
    for r in hierarchy::roots(world) {
        for e in hierarchy::subtree(world, r) {
            if seen.insert(e) {
                out.push(e);
            }
        }
    }
    let mut stray: Vec<Entity> = world.iter().map(|e| e.entity()).filter(|e| !seen.contains(e)).collect();
    stray.sort_by_key(|e| e.id());
    for r in stray {
        for e in hierarchy::subtree(world, r) {
            if seen.insert(e) {
                out.push(e);
            }
        }
    }
    out
}

/// Gives every entity a unique [`EntityId`]: missing ones are created,
/// duplicates (from duplicate/paste) keep the id on the first entity in
/// hierarchy order and get a fresh id on the others.
pub fn normalize_ids(world: &mut World) {
    let order = hierarchy_order(world);
    let mut seen: HashSet<EntityId> = HashSet::new();
    for e in order {
        let cur = world.get::<&EntityId>(e).ok().map(|i| *i);
        match cur {
            Some(id) if seen.insert(id) => {}
            _ => {
                let mut id = EntityId::new();
                while !seen.insert(id) {
                    id = EntityId::new();
                }
                let _ = world.insert_one(e, id);
            }
        }
    }
}

pub fn find_by_id(world: &World, id: EntityId) -> Option<Entity> {
    world.query::<&EntityId>().iter().find(|(_, i)| **i == id).map(|(e, _)| e)
}

/// [`Registry::with_defaults`] plus the persistence components; use this
/// anywhere scenes/prefabs are saved, loaded or snapshotted.
pub fn scene_registry() -> Registry {
    let mut r = Registry::with_defaults();
    register_scene_components(&mut r);
    r
}

/// Registers the persistence components as clone-only so
/// [`Registry::snapshot`] (duplicate, undo-of-delete, play snapshot) keeps them.
pub fn register_scene_components(reg: &mut Registry) {
    reg.register_cloneable::<EntityId>();
    reg.register_cloneable::<UnknownComponents>();
    reg.register_cloneable::<PrefabInstance>();
    reg.register_cloneable::<PrefabMember>();
}

// ------------------------------------------------------ play-mode snapshot

/// Whole-world copy for "enter play mode / come back unchanged". Keeps hecs
/// handles stable, so the selection and undo history stay valid afterwards.
///
/// Limitation: only registry-known (registered or cloneable) components are
/// captured; runtime-only components added by play mode are dropped on
/// restore, which is the point.
pub struct WorldSnapshot {
    entities: Vec<(Entity, BuiltEntityClone)>,
}

impl WorldSnapshot {
    pub fn len(&self) -> usize {
        self.entities.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

pub fn capture_world(world: &mut World, reg: &Registry) -> WorldSnapshot {
    normalize_ids(world);
    let all: Vec<Entity> = world.iter().map(|e| e.entity()).collect();
    let entities = all.into_iter().filter_map(|e| reg.snapshot(world, e).map(|b| (e, b))).collect();
    WorldSnapshot { entities }
}

pub fn restore_world(world: &mut World, snap: &WorldSnapshot) {
    world.clear();
    for (e, built) in &snap.entities {
        world.spawn_at(*e, built);
    }
}

// ------------------------------------------------- document: dirty/autosave

pub fn autosave_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".autosave");
    PathBuf::from(s)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub autosave: PathBuf,
    pub autosave_modified: SystemTime,
    /// `None` if the scene itself was never saved / is gone.
    pub scene_modified: Option<SystemTime>,
}

/// A crash left newer work behind: the autosave exists and is newer than the
/// scene (or the scene is missing). A clean save/close removes the autosave,
/// so its mere presence next to an older scene means "unsaved work lost".
pub fn detect_recovery(path: &Path) -> Option<Recovery> {
    let auto = autosave_path(path);
    let am = std::fs::metadata(&auto).ok()?.modified().ok()?;
    let sm = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    match sm {
        Some(s) if s >= am => None,
        _ => Some(Recovery { autosave: auto, autosave_modified: am, scene_modified: sm }),
    }
}

pub fn discard_autosave(path: &Path) {
    let _ = std::fs::remove_file(autosave_path(path));
}

/// The open scene file: path, name, scene-level settings, dirty flag and
/// autosave pacing. Dirtiness is fed from the undo stack by the shell
/// (`doc.set_dirty(undo.is_dirty())`) or marked directly.
pub struct SceneDocument {
    pub path: Option<PathBuf>,
    pub name: String,
    pub settings: BTreeMap<String, RawRon>,
    dirty: bool,
    pub autosave_interval: Duration,
    last_autosave: Option<Instant>,
}

impl SceneDocument {
    pub fn new(name: &str) -> Self {
        Self {
            path: None,
            name: name.to_string(),
            settings: BTreeMap::new(),
            dirty: false,
            autosave_interval: Duration::from_secs(60),
            last_autosave: None,
        }
    }

    /// Loads `path` into an emptied world. If an autosave newer than the file
    /// exists, it's reported via [`detect_recovery`] first; this loads the
    /// file proper — call [`SceneDocument::open_recovery`] to take the autosave.
    pub fn open(path: &Path, world: &mut World, reg: &Registry) -> Result<(Self, SpawnResult), SceneError> {
        Self::open_from(path, path, world, reg, false)
    }

    /// Loads the autosave instead of the file; the document is dirty
    /// (the recovered state isn't saved yet).
    pub fn open_recovery(path: &Path, world: &mut World, reg: &Registry) -> Result<(Self, SpawnResult), SceneError> {
        Self::open_from(path, &autosave_path(path), world, reg, true)
    }

    fn open_from(
        path: &Path,
        read: &Path,
        world: &mut World,
        reg: &Registry,
        dirty: bool,
    ) -> Result<(Self, SpawnResult), SceneError> {
        let scene = Scene::load(read)?;
        let res = scene.load_replace(world, reg);
        let mut doc = Self::new(&scene.name);
        doc.settings = scene.settings;
        doc.path = Some(path.to_path_buf());
        doc.dirty = dirty;
        Ok((doc, res))
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
    pub fn set_dirty(&mut self, d: bool) {
        self.dirty = d;
    }

    pub fn capture(&self, world: &mut World, reg: &Registry) -> Scene {
        let mut s = Scene::from_world(world, reg);
        s.name = self.name.clone();
        s.settings = self.settings.clone();
        s
    }

    /// Saves to the document path (error if there is none), clears the dirty
    /// flag and removes the autosave.
    pub fn save(&mut self, world: &mut World, reg: &Registry) -> Result<(), SceneError> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| SceneError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "document has no path")))?;
        self.save_as(&path, world, reg)
    }

    pub fn save_as(&mut self, path: &Path, world: &mut World, reg: &Registry) -> Result<(), SceneError> {
        self.capture(world, reg).save(path)?;
        discard_autosave(path);
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    /// Pure pacing decision, split out so it's testable without sleeping.
    pub fn autosave_due(&self, now: Instant) -> bool {
        self.dirty
            && self.path.is_some()
            && self.last_autosave.map_or(true, |t| now.duration_since(t) >= self.autosave_interval)
    }

    /// Writes the sibling `.autosave` when due. The real file is untouched.
    /// Returns true if it wrote.
    pub fn autosave_if_due(&mut self, now: Instant, world: &mut World, reg: &Registry) -> Result<bool, SceneError> {
        if !self.autosave_due(now) {
            return Ok(false);
        }
        let path = self.path.clone().expect("checked by autosave_due");
        self.capture(world, reg).save(&autosave_path(&path))?;
        self.last_autosave = Some(now);
        Ok(true)
    }

    /// Call on a clean close so the next open doesn't offer recovery.
    pub fn close_clean(&self) {
        if let Some(p) = &self.path {
            discard_autosave(p);
        }
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    /// Unique scratch dir under $TMPDIR, removed on drop.
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("dust-{tag}-{}", super::EntityId::new()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;
    use crate::ecs::{Light, LightKind, Transform};
    use crate::editor::hierarchy::{spawn_object, PrimitiveShape};
    use glam::Vec3;

    fn reg() -> Registry {
        scene_registry()
    }

    fn sample_world() -> World {
        let mut w = World::new();
        let a = spawn_object(&mut w, "Crate", None, Some(PrimitiveShape::Cube), Vec3::new(1.0, 2.0, 3.0));
        let b = spawn_object(&mut w, "Lid", Some(a), Some(PrimitiveShape::Plane), Vec3::new(0.0, 0.5, 0.0));
        let _ = b;
        let l = spawn_object(&mut w, "Sun", None, None, Vec3::ZERO);
        w.insert_one(l, Light { color: Vec3::new(1.0, 0.5, 0.25), intensity: 2.0, kind: LightKind::Point { range: 7.5 }, is_static: true })
            .unwrap();
        w.insert_one(a, Visible(false)).unwrap();
        w
    }

    #[test]
    fn entity_id_text_roundtrip_and_uniqueness() {
        let a = EntityId::new();
        assert_eq!(a.to_string().len(), 16);
        assert_eq!(a.to_string().parse::<EntityId>().unwrap(), a);
        let ids: HashSet<_> = (0..1000).map(|_| EntityId::new()).collect();
        assert_eq!(ids.len(), 1000);
        assert_eq!(EntityId::from_key("x"), EntityId::from_key("x"));
        assert_ne!(EntityId::from_key("x"), EntityId::from_key("y"));
    }

    #[test]
    fn world_scene_world_preserves_data_and_hierarchy() {
        let r = reg();
        let mut w = sample_world();
        let scene = Scene::from_world(&mut w, &r);
        assert_eq!(scene.format_version, SCENE_FORMAT_VERSION);
        let names: Vec<_> = scene.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Crate", "Lid", "Sun"], "hierarchy pre-order");
        assert_eq!(scene.entities[1].parent, Some(scene.entities[0].id));

        let text = scene.to_ron().unwrap();
        let back = Scene::from_ron(&text).unwrap();
        assert_eq!(back, scene);

        let mut w2 = World::new();
        let res = back.load_replace(&mut w2, &r);
        assert!(res.report.warnings.is_empty(), "{:?}", res.report);
        let crate_e = find_by_id(&w2, scene.entities[0].id).unwrap();
        assert_eq!(hierarchy::display_name(&w2, crate_e), "Crate");
        assert!(!hierarchy::is_visible(&w2, crate_e));
        assert_eq!(w2.get::<&Transform>(crate_e).unwrap().position, Vec3::new(1.0, 2.0, 3.0));
        let lid = find_by_id(&w2, scene.entities[1].id).unwrap();
        assert_eq!(hierarchy::parent_of(&w2, lid), Some(crate_e));
        let sun = find_by_id(&w2, scene.entities[2].id).unwrap();
        let light = *w2.get::<&Light>(sun).unwrap();
        assert!(light.is_static);
        assert!(matches!(light.kind, LightKind::Point { range } if range == 7.5));
        assert_eq!(light.color, Vec3::new(1.0, 0.5, 0.25));
    }

    #[test]
    fn output_is_a_fixed_point_and_independent_of_hecs_order() {
        let r = reg();
        let mut w = sample_world();
        let t1 = Scene::from_world(&mut w, &r).to_ron().unwrap();
        let mut w2 = World::new();
        Scene::from_ron(&t1).unwrap().load_replace(&mut w2, &r);
        let t2 = Scene::from_world(&mut w2, &r).to_ron().unwrap();
        assert_eq!(t1, t2, "save(load(save)) must be byte identical");
        // Different insertion order of unrelated hecs entities / handles.
        let mut w3 = World::new();
        for _ in 0..7 {
            let e = w3.spawn((1u8,));
            w3.despawn(e).unwrap();
        }
        Scene::from_ron(&t1).unwrap().load_replace(&mut w3, &r);
        assert_eq!(Scene::from_world(&mut w3, &r).to_ron().unwrap(), t1);
    }

    #[test]
    fn file_looks_diffable() {
        let r = reg();
        let mut w = sample_world();
        let text = Scene::from_world(&mut w, &r).to_ron().unwrap();
        assert!(text.contains("format_version: 1"));
        assert!(text.lines().count() > 20, "multi-line, not one blob");
        assert!(text.contains("Vec3(1.0, 2.0, 3.0)"), "{text}");
        assert!(text.contains("Enum(\"Cube\")"));
        // floats are rounded: no 0.30000001-style noise
        let mut w = World::new();
        spawn_object(&mut w, "n", None, None, Vec3::new(0.1 + 0.2, 1.0 / 3.0, 0.0));
        let text = Scene::from_world(&mut w, &r).to_ron().unwrap();
        assert!(text.contains("Vec3(0.3, 0.333333, 0.0)"), "{text}");
    }

    #[test]
    fn unknown_components_and_fields_survive_roundtrip() {
        let r = reg();
        let text = r#"(
  format_version: 1,
  name: "future",
  entities: [
    (id: "00000000000000aa", name: "Thing",
     components: {
       "Transform": {"position": Vec3(1.0, 2.0, 3.0), "bogus_field": F32(9.0)},
       "HoverCar": {"lift": F32(3.5), "mode": Enum("Sport")},
     },
     raw: {"Legacy": ["(a: 1,", " b: 2)"]},
    ),
  ],
)"#;
        let scene = Scene::from_ron(text).unwrap();
        let mut w = World::new();
        let res = scene.load_replace(&mut w, &r);
        assert_eq!(res.report.warnings.len(), 2, "{:?}", res.report.warnings);
        let e = res.entities[0];
        assert_eq!(w.get::<&Transform>(e).unwrap().position, Vec3::new(1.0, 2.0, 3.0));
        let back = Scene::from_world(&mut w, &r);
        let ent = &back.entities[0];
        assert_eq!(ent.field("HoverCar", "lift"), Some(&SceneValue::F32(3.5)));
        assert_eq!(ent.field("HoverCar", "mode"), Some(&SceneValue::Enum("Sport".into())));
        assert_eq!(ent.field("Transform", "bogus_field"), Some(&SceneValue::F32(9.0)));
        assert_eq!(ent.raw["Legacy"].text(), "(a: 1,\n b: 2)");
        // and they survive a duplicate through the registry too
        let dup = hierarchy::duplicate_subtree(&mut w, &r, e).unwrap();
        assert!(w.get::<&UnknownComponents>(dup).is_ok());
    }

    #[test]
    fn bad_enum_variant_is_a_warning_not_a_failure() {
        let r = reg();
        let text = r#"(format_version: 1, entities: [(id: "01", name: "p", components: {"Primitive": {"shape": Enum("Torus")}})])"#;
        let mut w = World::new();
        let res = Scene::from_ron(text).unwrap().load_replace(&mut w, &r);
        assert_eq!(res.entities.len(), 1);
        assert_eq!(res.report.warnings.len(), 1);
    }

    #[test]
    fn version_too_new_is_refused_and_migrations_run_in_order() {
        let newer = "(format_version: 99, entities: [])";
        assert!(matches!(Scene::from_ron(newer), Err(SceneError::TooNew { found: 99, .. })));

        // Pretend format v1 -> v2 renamed component "Mover" to "Motor".
        fn rename(s: &mut Scene) -> Result<(), String> {
            for e in &mut s.entities {
                if let Some(c) = e.components.remove("Mover") {
                    e.components.insert("Motor".into(), c);
                }
            }
            Ok(())
        }
        fn tag(s: &mut Scene) -> Result<(), String> {
            s.name.push_str("+v3");
            Ok(())
        }
        let ms = [Migration { from: 1, apply: rename }, Migration { from: 2, apply: tag }];
        let old = r#"(format_version: 1, name: "n", entities: [(id: "05", name: "m", components: {"Mover": {"speed": F32(1.0)}})])"#;
        let s = Scene::from_ron_with(old, &ms, 3).unwrap();
        assert_eq!(s.format_version, 3);
        assert_eq!(s.name, "n+v3");
        assert!(s.entities[0].components.contains_key("Motor"));
        // gap in the chain is an error
        assert!(matches!(Scene::from_ron_with(old, &ms[1..], 3), Err(SceneError::NoMigration(1))));
        // failing migration reports its version
        fn boom(_: &mut Scene) -> Result<(), String> {
            Err("nope".into())
        }
        assert!(matches!(
            Scene::from_ron_with(old, &[Migration { from: 1, apply: boom }], 2),
            Err(SceneError::Migration(1, _))
        ));
    }

    #[test]
    fn additive_load_remaps_colliding_ids_and_keeps_links() {
        let r = reg();
        let mut w = sample_world();
        let scene = Scene::from_world(&mut w, &r);
        let before = w.len();
        let res = scene.spawn_into(&mut w, &r);
        assert_eq!(w.len(), before * 2);
        let ids: Vec<_> = w.query::<&EntityId>().iter().map(|(_, i)| *i).collect();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len(), "ids stay unique");
        // every file id was remapped to a different id
        assert!(res.id_map.iter().all(|(a, b)| a != b));
        // structure of the new copy is intact
        let new_crate = res.entities[0];
        let new_lid = res.entities[1];
        assert_eq!(hierarchy::parent_of(&w, new_lid), Some(new_crate));
        // roots appended after existing ones in display order
        let roots = hierarchy::roots(&w);
        assert_eq!(roots.len(), 4);
        assert_eq!(roots[2], new_crate);
        // loading into a non-colliding world keeps ids
        let mut w2 = World::new();
        let res2 = scene.spawn_into(&mut w2, &r);
        assert!(res2.id_map.iter().all(|(a, b)| a == b));
        // parent option attaches roots
        let host = hierarchy::spawn_node(&mut w2, "host", None);
        let res3 = scene.spawn_with(&mut w2, &r, &SpawnOptions { remap: Remap::Always, parent: Some(host) });
        assert_eq!(hierarchy::parent_of(&w2, res3.entities[0]), Some(host));
        assert_eq!(hierarchy::parent_of(&w2, res3.entities[2]), Some(host));
    }

    #[test]
    fn duplicate_entity_ids_are_repaired() {
        let r = reg();
        let mut w = sample_world();
        let first = hierarchy::roots(&w)[0];
        hierarchy::duplicate_subtree(&mut w, &r, first).unwrap();
        // registry snapshot copied the EntityId component, so ids collide
        let s = Scene::from_world(&mut w, &r);
        let ids: HashSet<_> = s.entities.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), s.entities.len());
    }

    #[test]
    fn dangling_and_cyclic_parent_links_degrade_gracefully() {
        let r = reg();
        let text = r#"(format_version: 1, entities: [
            (id: "01", name: "a", parent: "02"),
            (id: "02", name: "b", parent: "01"),
            (id: "03", name: "c", parent: "ff"),
        ])"#;
        let mut w = World::new();
        let res = Scene::from_ron(text).unwrap().load_replace(&mut w, &r);
        assert_eq!(res.entities.len(), 3);
        assert!(res.report.warnings.len() >= 2, "{:?}", res.report.warnings);
    }

    #[test]
    fn world_snapshot_restores_edits_and_keeps_handles() {
        let r = reg();
        let mut w = sample_world();
        let crate_e = hierarchy::roots(&w)[0];
        let id_before = *w.get::<&EntityId>(crate_e).ok().map(|i| i).as_deref().unwrap_or(&EntityId(0));
        let snap = capture_world(&mut w, &r);
        let id_before = if id_before.0 == 0 { *w.get::<&EntityId>(crate_e).unwrap() } else { id_before };
        // play mode: move things, delete things, add runtime junk
        w.get::<&mut Transform>(crate_e).unwrap().position = Vec3::splat(99.0);
        let lid = hierarchy::children(&w, crate_e)[0];
        hierarchy::delete_subtree(&mut w, lid);
        w.spawn((Name("bullet".into()),));
        w.insert_one(crate_e, 5u32).unwrap();
        restore_world(&mut w, &snap);
        assert_eq!(snap.len(), 3);
        assert_eq!(w.len(), 3);
        assert_eq!(w.get::<&Transform>(crate_e).unwrap().position, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(hierarchy::children(&w, crate_e), vec![lid], "same handles");
        assert_eq!(hierarchy::parent_of(&w, lid), Some(crate_e));
        assert_eq!(*w.get::<&EntityId>(crate_e).unwrap(), id_before);
        assert!(w.get::<&u32>(crate_e).is_err(), "runtime-only component dropped");
        assert!(!hierarchy::is_visible(&w, crate_e));
    }

    #[test]
    fn save_load_path_is_atomic_and_roundtrips() {
        let r = reg();
        let dir = TempDir::new("scene-io");
        let p = dir.path().join("nested/level.scene.ron");
        let mut w = sample_world();
        let s = Scene::from_world(&mut w, &r);
        s.save(&p).unwrap();
        assert!(!dir.path().join("nested/level.scene.ron.tmp").exists());
        assert_eq!(Scene::load(&p).unwrap(), s);
        assert!(matches!(Scene::load(&dir.path().join("missing.ron")), Err(SceneError::Io(_))));
        std::fs::write(&p, "(garbage").unwrap();
        assert!(matches!(Scene::load(&p), Err(SceneError::Parse(_))));
    }

    #[test]
    fn document_dirty_autosave_and_recovery() {
        let r = reg();
        let dir = TempDir::new("scene-doc");
        let p = dir.path().join("a.scene.ron");
        let mut w = sample_world();
        let mut doc = SceneDocument::new("a");
        assert!(doc.save(&mut w, &r).is_err(), "no path yet");
        doc.save_as(&p, &mut w, &r).unwrap();
        assert!(!doc.is_dirty());
        assert!(detect_recovery(&p).is_none());

        let t0 = Instant::now();
        assert!(!doc.autosave_due(t0), "clean docs never autosave");
        doc.mark_dirty();
        assert!(doc.autosave_due(t0));
        // make the autosave strictly newer than the saved file
        std::thread::sleep(Duration::from_millis(30));
        w.get::<&mut Transform>(hierarchy::roots(&w)[0]).unwrap().position = Vec3::splat(5.0);
        assert!(doc.autosave_if_due(t0, &mut w, &r).unwrap());
        assert!(!doc.autosave_due(t0 + Duration::from_secs(10)), "paced");
        assert!(doc.autosave_due(t0 + Duration::from_secs(61)));
        assert!(autosave_path(&p).exists());
        assert!(autosave_path(&p).to_string_lossy().ends_with("a.scene.ron.autosave"));
        // the real file did not change
        let on_disk = Scene::load(&p).unwrap();
        assert_ne!(on_disk.entities[0].field("Transform", "position"), Some(&SceneValue::Vec3(5.0, 5.0, 5.0)));

        // "crash": a new session sees recovery
        let rec = detect_recovery(&p).expect("recovery offered");
        assert_eq!(rec.autosave, autosave_path(&p));
        let mut w2 = World::new();
        let (doc2, _) = SceneDocument::open_recovery(&p, &mut w2, &r).unwrap();
        assert!(doc2.is_dirty());
        let e = hierarchy::roots(&w2)[0];
        assert_eq!(w2.get::<&Transform>(e).unwrap().position, Vec3::splat(5.0));

        // normal open ignores the autosave; discarding clears the offer
        let mut w3 = World::new();
        let (doc3, _) = SceneDocument::open(&p, &mut w3, &r).unwrap();
        assert!(!doc3.is_dirty());
        discard_autosave(&p);
        assert!(detect_recovery(&p).is_none());

        // saving removes a stale autosave
        doc.mark_dirty();
        doc.autosave_if_due(t0 + Duration::from_secs(200), &mut w, &r).unwrap();
        assert!(autosave_path(&p).exists());
        doc.save(&mut w, &r).unwrap();
        assert!(!autosave_path(&p).exists());
        assert!(!doc.is_dirty());
    }

    #[test]
    fn asset_refs_and_path_rewrite() {
        let mut s = Scene::new("x");
        let mut e = SceneEntity::new(EntityId(1), "a");
        e.set_field("Mesh", "texture", SceneValue::Asset("tex/a.png".into()));
        s.entities.push(e);
        assert_eq!(s.asset_refs(), ["tex/a.png"]);
        let m = HashMap::from([("tex/a.png".to_string(), "tex/b.png".to_string())]);
        assert_eq!(s.rewrite_asset_paths(&m), 1);
        assert_eq!(s.asset_refs(), ["tex/b.png"]);
    }
}
