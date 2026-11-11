//! Two-way bridge between the legacy [`Level`] RON files and [`Scene`].
//!
//! Every legacy item becomes one scene entity with a stable id derived from
//! the level name, list and index (converting twice gives the same ids):
//! the editable bits (name, position, rotation, scale, light colour, ...)
//! are stored as registry fields so the editor can show and edit them, and
//! the full original item is kept verbatim in `raw[<LegacyType>]` so nothing
//! the editor doesn't model (scripts, classes, dialogue, particle defs, ...)
//! is lost. Going back, the raw item is the base and editable fields are
//! applied on top. Level-wide data (physics, music) lives in `settings`.

use std::path::Path;

use super::scene::{EntityId, RawRon, Scene, SceneEntity, SceneError, SceneValue};
use crate::level::{
    CharacterInstance, Level, LevelLight, LevelObject, LevelParticleEmitter, MeshSource, PrimitiveKind, RigInstance,
    SpawnerInstance,
};

const OBJ: &str = "LevelObject";
const LIGHT: &str = "LevelLight";
const RIG: &str = "RigInstance";
const EMITTER: &str = "LevelParticleEmitter";
const CHARACTER: &str = "CharacterInstance";
const SPAWNER: &str = "SpawnerInstance";

fn v3(a: [f32; 3]) -> SceneValue {
    SceneValue::Vec3(a[0], a[1], a[2])
}

fn get3(se: &SceneEntity, comp: &str, f: &str) -> Option<[f32; 3]> {
    match se.field(comp, f)? {
        SceneValue::Vec3(a, b, c) | SceneValue::Color(a, b, c) => Some([*a, *b, *c]),
        _ => None,
    }
}

fn getf(se: &SceneEntity, comp: &str, f: &str) -> Option<f32> {
    match se.field(comp, f)? {
        SceneValue::F32(x) => Some(*x),
        _ => None,
    }
}

fn entity<T: serde::Serialize>(level: &str, kind: &str, i: usize, name: &str, item: &T) -> Result<SceneEntity, SceneError> {
    let mut se = SceneEntity::new(EntityId::from_key(&format!("{level}:{kind}:{i}")), name);
    se.raw.insert(kind.to_string(), RawRon::from_value(item)?);
    Ok(se)
}

pub fn level_to_scene(level: &Level) -> Result<Scene, SceneError> {
    let mut s = Scene::new(&level.name);
    let n = &level.name;
    s.settings.insert("physics".into(), RawRon::from_value(&level.physics)?);
    if let Some(m) = &level.music_path {
        s.settings.insert("music_path".into(), RawRon::from_value(m)?);
    }
    for (i, o) in level.objects.iter().enumerate() {
        let mut e = entity(n, OBJ, i, &o.name, o)?;
        e.set_field("Transform", "position", v3(o.position));
        e.set_field("Transform", "rotation", v3(o.rotation_euler_deg));
        e.set_field("Transform", "scale", v3(o.scale));
        if let MeshSource::Primitive(k) = &o.mesh {
            let shape = match k {
                PrimitiveKind::Cube => "Cube",
                PrimitiveKind::Plane => "Plane",
            };
            e.set_field("Primitive", "shape", SceneValue::Enum(shape.into()));
        }
        s.entities.push(e);
    }
    for (i, l) in level.lights.iter().enumerate() {
        let mut e = entity(n, LIGHT, i, &l.name, l)?;
        e.set_field("Transform", "position", v3(l.position));
        e.set_field("Light", "color", SceneValue::Color(l.color[0], l.color[1], l.color[2]));
        e.set_field("Light", "intensity", SceneValue::F32(l.intensity));
        e.set_field("Light", "kind", SceneValue::Enum("Point".into()));
        e.set_field("Light", "range", SceneValue::F32(l.range));
        e.set_field("Light", "is_static", SceneValue::Bool(l.is_static));
        s.entities.push(e);
    }
    for (i, r) in level.rig_instances.iter().enumerate() {
        let mut e = entity(n, RIG, i, &r.name, r)?;
        e.set_field("Transform", "position", v3(r.position));
        e.set_field("Transform", "rotation", v3(r.rotation_euler_deg));
        s.entities.push(e);
    }
    for (i, p) in level.particle_emitters.iter().enumerate() {
        let mut e = entity(n, EMITTER, i, &p.name, p)?;
        e.set_field("Transform", "position", v3(p.position));
        s.entities.push(e);
    }
    for (i, c) in level.characters.iter().enumerate() {
        let mut e = entity(n, CHARACTER, i, &c.name, c)?;
        e.set_field("Transform", "position", v3(c.position));
        e.set_field("Transform", "scale", v3(c.scale));
        s.entities.push(e);
    }
    for (i, c) in level.spawners.iter().enumerate() {
        let mut e = entity(n, SPAWNER, i, &c.name, c)?;
        e.set_field("Transform", "position", v3(c.position));
        s.entities.push(e);
    }
    Ok(s)
}

/// Entities with none of the legacy markers (anything the editor created
/// from scratch) have no legacy equivalent and are reported in the second
/// tuple field instead of being silently dropped.
pub fn scene_to_level_lossy(scene: &Scene) -> Result<(Level, Vec<String>), SceneError> {
    let mut lv = Level { name: scene.name.clone(), ..Default::default() };
    if let Some(p) = scene.settings.get("physics") {
        lv.physics = p.parse()?;
    }
    if let Some(m) = scene.settings.get("music_path") {
        lv.music_path = m.parse()?;
    }
    let mut skipped = Vec::new();
    for se in &scene.entities {
        let pos = get3(se, "Transform", "position");
        if let Some(raw) = se.raw.get(OBJ) {
            let mut o: LevelObject = raw.parse()?;
            o.name = se.name.clone();
            if let Some(p) = pos {
                o.position = p;
            }
            if let Some(r) = get3(se, "Transform", "rotation") {
                o.rotation_euler_deg = r;
            }
            if let Some(sc) = get3(se, "Transform", "scale") {
                o.scale = sc;
            }
            lv.objects.push(o);
        } else if let Some(raw) = se.raw.get(LIGHT) {
            let mut l: LevelLight = raw.parse()?;
            l.name = se.name.clone();
            if let Some(p) = pos {
                l.position = p;
            }
            if let Some(c) = get3(se, "Light", "color") {
                l.color = c;
            }
            if let Some(x) = getf(se, "Light", "intensity") {
                l.intensity = x;
            }
            if let Some(x) = getf(se, "Light", "range") {
                l.range = x;
            }
            if let Some(SceneValue::Bool(b)) = se.field("Light", "is_static") {
                l.is_static = *b;
            }
            lv.lights.push(l);
        } else if let Some(raw) = se.raw.get(RIG) {
            let mut r: RigInstance = raw.parse()?;
            r.name = se.name.clone();
            if let Some(p) = pos {
                r.position = p;
            }
            if let Some(x) = get3(se, "Transform", "rotation") {
                r.rotation_euler_deg = x;
            }
            lv.rig_instances.push(r);
        } else if let Some(raw) = se.raw.get(EMITTER) {
            let mut p: LevelParticleEmitter = raw.parse()?;
            p.name = se.name.clone();
            if let Some(x) = pos {
                p.position = x;
            }
            lv.particle_emitters.push(p);
        } else if let Some(raw) = se.raw.get(CHARACTER) {
            let mut c: CharacterInstance = raw.parse()?;
            c.name = se.name.clone();
            if let Some(x) = pos {
                c.position = x;
            }
            if let Some(x) = get3(se, "Transform", "scale") {
                c.scale = x;
            }
            lv.characters.push(c);
        } else if let Some(raw) = se.raw.get(SPAWNER) {
            let mut c: SpawnerInstance = raw.parse()?;
            c.name = se.name.clone();
            if let Some(x) = pos {
                c.position = x;
            }
            lv.spawners.push(c);
        } else {
            skipped.push(se.name.clone());
        }
    }
    Ok((lv, skipped))
}

pub fn scene_to_level(scene: &Scene) -> Result<Level, SceneError> {
    Ok(scene_to_level_lossy(scene)?.0)
}

/// Loads either a native scene or a legacy level file as a [`Scene`].
pub fn load_scene_or_level(path: &Path) -> Result<Scene, SceneError> {
    let text = std::fs::read_to_string(path)?;
    // Scene's serde ignores unknown keys, so a level would "parse" as an
    // empty scene; Level has a required `objects` field and rejects scenes.
    match ron::from_str::<Level>(&text) {
        Ok(l) => level_to_scene(&l),
        Err(_) => Scene::from_ron(&text),
    }
}

/// Writes `scene` as a legacy level file (for games that still load them).
pub fn save_as_level(scene: &Scene, path: &Path) -> Result<Vec<String>, SceneError> {
    let (lv, skipped) = scene_to_level_lossy(scene)?;
    let text = ron::ser::to_string_pretty(&lv, ron::ser::PrettyConfig::default())?;
    super::scene::write_atomic(path, text.as_bytes())?;
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::scene::{scene_registry, testutil::TempDir};
    use hecs::World;

    fn real_levels() -> Vec<std::path::PathBuf> {
        let games = Path::new(env!("CARGO_MANIFEST_DIR")).join("../games");
        let mut v = Vec::new();
        for g in std::fs::read_dir(&games).unwrap().flatten() {
            let d = g.path().join("levels");
            if let Ok(rd) = std::fs::read_dir(d) {
                v.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ron")));
            }
        }
        v.sort();
        v
    }

    fn ron_of(l: &Level) -> String {
        ron::ser::to_string_pretty(l, ron::ser::PrettyConfig::default()).unwrap()
    }

    #[test]
    fn real_level_files_roundtrip_exactly_through_scene_text() {
        let files = real_levels();
        assert!(files.len() >= 5, "found {files:?}");
        for p in &files {
            let lv = crate::level::load_from_file(p).unwrap();
            let scene = level_to_scene(&lv).unwrap();
            let text = scene.to_ron().unwrap();
            let back = scene_to_level(&Scene::from_ron(&text).unwrap()).unwrap();
            assert_eq!(ron_of(&lv), ron_of(&back), "{p:?}");
            // deterministic ids: converting twice is identical
            assert_eq!(text, level_to_scene(&lv).unwrap().to_ron().unwrap(), "{p:?}");
        }
    }

    #[test]
    fn real_levels_survive_the_editor_world() {
        let r = scene_registry();
        for p in real_levels() {
            let lv = crate::level::load_from_file(&p).unwrap();
            let mut w = World::new();
            let scene = level_to_scene(&lv).unwrap();
            let res = scene.load_replace(&mut w, &r);
            assert!(res.report.warnings.is_empty() || res.report.warnings.iter().all(|m| m.contains("unknown")), "{p:?}: {:?}", res.report);
            // Level-wide settings live on the open document, not in the world.
            let mut captured = Scene::from_world(&mut w, &r);
            captured.settings = scene.settings.clone();
            let back = scene_to_level(&captured).unwrap();
            assert_eq!(back.objects.len(), lv.objects.len(), "{p:?}");
            assert_eq!(back.lights.len(), lv.lights.len());
            assert_eq!(back.characters.len(), lv.characters.len());
            assert_eq!(back.rig_instances.len(), lv.rig_instances.len());
            for (a, b) in lv.objects.iter().zip(&back.objects) {
                assert_eq!(a.name, b.name);
                assert_eq!(a.mesh, b.mesh);
                assert_eq!(a.script, b.script);
                assert_eq!(a.class, b.class);
                assert_eq!(a.is_trigger, b.is_trigger);
                for k in 0..3 {
                    assert!((a.position[k] - b.position[k]).abs() < 1e-4, "{p:?} {}", a.name);
                    assert!((a.scale[k] - b.scale[k]).abs() < 1e-4);
                }
                // euler -> quat -> euler is only equal modulo equivalence;
                // compare as rotations via sin/cos of each angle's quaternion
                let q = |e: [f32; 3]| crate::ecs::euler_deg_to_quat(glam::Vec3::from(e));
                assert!(q(a.rotation_euler_deg).dot(q(b.rotation_euler_deg)).abs() > 0.9999, "{p:?} {}", a.name);
            }
            assert_eq!(lv.music_path, back.music_path);
        }
    }

    #[test]
    fn editor_edits_flow_back_into_the_level() {
        let lv = crate::level::load_from_file(&real_levels()[0]).unwrap();
        let mut scene = level_to_scene(&lv).unwrap();
        let first = scene.entities.iter_mut().find(|e| e.raw.contains_key(OBJ)).unwrap();
        first.name = "Renamed".into();
        first.set_field("Transform", "position", SceneValue::Vec3(1.0, 2.0, 3.0));
        let back = scene_to_level(&scene).unwrap();
        assert_eq!(back.objects[0].name, "Renamed");
        assert_eq!(back.objects[0].position, [1.0, 2.0, 3.0]);
        // editor-made entities are reported, not dropped silently
        scene.entities.push(SceneEntity::new(EntityId(5), "Empty"));
        let (_, skipped) = scene_to_level_lossy(&scene).unwrap();
        assert_eq!(skipped, ["Empty"]);
    }

    #[test]
    fn load_any_and_write_legacy_files() {
        let dir = TempDir::new("compat");
        let src = &real_levels()[0];
        let scene = load_scene_or_level(src).unwrap();
        assert!(!scene.entities.is_empty());
        let native = dir.path().join("x.scene.ron");
        scene.save(&native).unwrap();
        assert_eq!(load_scene_or_level(&native).unwrap(), scene);
        let legacy = dir.path().join("x.ron");
        assert!(save_as_level(&scene, &legacy).unwrap().is_empty());
        let a = crate::level::load_from_file(src).unwrap();
        let b = crate::level::load_from_file(&legacy).unwrap();
        assert_eq!(ron_of(&a), ron_of(&b));
    }
}
