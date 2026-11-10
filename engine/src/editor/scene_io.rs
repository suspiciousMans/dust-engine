//! Play-mode scene snapshot/restore and File-menu scene I/O.
//!
//! Both are traits so the host can substitute richer implementations: the
//! registry-backed [`RegistrySnapshot`] only knows components registered with
//! the inspector `Registry`; [`LevelSceneIo`] only round-trips what the
//! `level::Level` format can express (named primitives with a transform).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use glam::{EulerRot, Vec3};
use hecs::{BuiltEntityClone, Entity, World};

use super::hierarchy::{Name, Primitive, PrimitiveShape};
use super::inspector::Registry;
use crate::ecs::{euler_deg_to_quat, Transform};
use crate::level::{self, Level, LevelObject, MeshSource, PrimitiveKind};

// ------------------------------------------------------------------ snapshot

/// Captures the scene on Play and puts it back on Stop.
pub trait SceneSnapshot {
    fn capture(&mut self, world: &World, registry: &Registry);
    /// Returns `false` when nothing was captured.
    fn restore(&mut self, world: &mut World, registry: &Registry) -> bool;
    fn has_snapshot(&self) -> bool;
    fn discard(&mut self);
}

/// Snapshot of every entity's registered (and clone-only editor) components,
/// keyed by entity id so references (Parent/Children) stay valid.
///
/// Restore is surgical: entities spawned during play are despawned, deleted
/// ones are respawned under their old id, and for survivors the snapshotted
/// components are written back and registered components added during play
/// are removed. Components the registry doesn't know (GPU handles, behaviors,
/// physics bodies) are left untouched.
#[derive(Default)]
pub struct RegistrySnapshot {
    entities: Vec<(Entity, BuiltEntityClone, Vec<&'static str>)>,
    captured: bool,
}

impl SceneSnapshot for RegistrySnapshot {
    fn capture(&mut self, world: &World, registry: &Registry) {
        self.entities.clear();
        for e in world.iter().map(|r| r.entity()) {
            if let Some(b) = registry.snapshot(world, e) {
                let names = registry.present(world, e).iter().map(|c| c.name).collect();
                self.entities.push((e, b, names));
            }
        }
        self.captured = true;
    }

    fn restore(&mut self, world: &mut World, registry: &Registry) -> bool {
        if !self.captured {
            return false;
        }
        let keep: HashMap<Entity, ()> = self.entities.iter().map(|(e, _, _)| (*e, ())).collect();
        let extra: Vec<Entity> = world.iter().map(|r| r.entity()).filter(|e| !keep.contains_key(e)).collect();
        for e in extra {
            let _ = world.despawn(e);
        }
        for (e, built, names) in &self.entities {
            if world.contains(*e) {
                let added: Vec<&'static str> =
                    registry.present(world, *e).iter().map(|c| c.name).filter(|n| !names.contains(n)).collect();
                for n in added {
                    registry.remove_component(world, *e, n);
                }
                let _ = world.insert(*e, built);
            } else {
                let _ = world.spawn_at(*e, built);
            }
        }
        true
    }

    fn has_snapshot(&self) -> bool {
        self.captured
    }

    fn discard(&mut self) {
        self.entities.clear();
        self.captured = false;
    }
}

// ------------------------------------------------------------------- scene io

/// File-menu backend.
pub trait SceneIo {
    fn new_scene(&mut self, world: &mut World) -> Result<(), String>;
    fn open(&mut self, path: &Path, world: &mut World) -> Result<(), String>;
    fn save(&mut self, world: &World, path: &Path) -> Result<(), String>;
    /// File the scene was last opened from / saved to.
    fn current_path(&self) -> Option<&Path>;
}

/// Stub over `level.rs`: round-trips named primitives (name, shape, transform)
/// and preserves everything else in the loaded `Level` (lights, emitters,
/// physics params...) untouched. Sphere/Capsule are saved as Cube because
/// `PrimitiveKind` has no such variants.
#[derive(Default)]
pub struct LevelSceneIo {
    level: Level,
    path: Option<PathBuf>,
}

impl LevelSceneIo {
    pub fn level(&self) -> &Level {
        &self.level
    }
}

fn to_euler_deg(t: &Transform) -> [f32; 3] {
    let (x, y, z) = t.rotation.to_euler(EulerRot::XYZ);
    [x.to_degrees(), y.to_degrees(), z.to_degrees()]
}

impl SceneIo for LevelSceneIo {
    fn new_scene(&mut self, world: &mut World) -> Result<(), String> {
        world.clear();
        self.level = Level { name: "untitled".into(), ..Default::default() };
        self.path = None;
        Ok(())
    }

    fn open(&mut self, path: &Path, world: &mut World) -> Result<(), String> {
        let lvl = level::load_from_file(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        world.clear();
        for o in &lvl.objects {
            let shape = match o.mesh {
                MeshSource::Primitive(PrimitiveKind::Plane) => PrimitiveShape::Plane,
                _ => PrimitiveShape::Cube,
            };
            world.spawn((
                Name(o.name.clone()),
                Primitive(shape),
                Transform {
                    position: Vec3::from(o.position),
                    rotation: euler_deg_to_quat(Vec3::from(o.rotation_euler_deg)),
                    scale: Vec3::from(o.scale),
                },
            ));
        }
        self.level = lvl;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save(&mut self, world: &World, path: &Path) -> Result<(), String> {
        let mut objects = Vec::new();
        for (_, (name, prim, t)) in world.query::<(&Name, &Primitive, &Transform)>().iter() {
            objects.push(LevelObject {
                name: name.0.clone(),
                mesh: MeshSource::Primitive(if prim.0 == PrimitiveShape::Plane { PrimitiveKind::Plane } else { PrimitiveKind::Cube }),
                texture_path: None,
                position: t.position.to_array(),
                rotation_euler_deg: to_euler_deg(t),
                scale: t.scale.to_array(),
                is_dynamic: false,
                is_trigger: false,
                animation: None,
                script: None,
                class: None,
                level_transition: None,
                screen_effect: None,
                camera_shake: None,
            });
        }
        // Query order is archetype order; sort for stable, diffable files.
        objects.sort_by(|a, b| a.name.cmp(&b.name));
        self.level.objects = objects;
        level::save_to_file(&self.level, path).map_err(|e| format!("save {}: {e}", path.display()))?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn current_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{Light, LightKind};
    use crate::editor::hierarchy::{Children, Parent};

    fn snapshot_world(w: &World, reg: &Registry) -> Vec<(u32, String, Vec3)> {
        let mut v: Vec<_> = w
            .iter()
            .map(|r| {
                let e = r.entity();
                let name = w.get::<&Name>(e).map(|n| n.0.clone()).unwrap_or_default();
                let p = w.get::<&Transform>(e).map(|t| t.position).unwrap_or(Vec3::ZERO);
                let _ = reg;
                (e.id(), name, p)
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v
    }

    #[test]
    fn play_stop_restores_exactly() {
        let reg = Registry::with_defaults();
        let mut w = World::new();
        let a = w.spawn((Name("a".into()), Transform::from_position(Vec3::new(1.0, 2.0, 3.0)), Primitive(PrimitiveShape::Sphere)));
        let b = w.spawn((Name("b".into()), Transform::from_position(Vec3::X), Parent(a)));
        w.insert_one(a, Children(vec![b])).unwrap();
        let c = w.spawn((Name("c".into()),));
        let before = snapshot_world(&w, &reg);

        let mut snap = RegistrySnapshot::default();
        assert!(!snap.restore(&mut w, &reg), "nothing captured yet");
        snap.capture(&w, &reg);

        // "Play": mutate, delete, spawn, add/remove components.
        w.get::<&mut Transform>(a).unwrap().position = Vec3::splat(99.0);
        w.get::<&mut Name>(a).unwrap().0 = "changed".into();
        w.despawn(b).unwrap();
        w.insert_one(c, Light { kind: LightKind::Point { range: 5.0 }, color: Vec3::ONE, intensity: 1.0, is_static: false }).ok();
        let junk = w.spawn((Name("spawned in play".into()),));
        assert!(w.contains(junk));

        assert!(snap.restore(&mut w, &reg));
        assert_eq!(snapshot_world(&w, &reg), before);
        assert!(w.get::<&Light>(c).is_err(), "component added during play is removed");
        assert!(!w.contains(junk));
        assert_eq!(w.get::<&Parent>(b).unwrap().0, a);
        assert_eq!(w.get::<&Children>(a).unwrap().0, vec![b]);
        assert_eq!(w.get::<&Primitive>(a).unwrap().0, PrimitiveShape::Sphere);

        // Restorable again (Play twice in a row).
        w.get::<&mut Transform>(a).unwrap().position = Vec3::ZERO;
        assert!(snap.restore(&mut w, &reg));
        assert_eq!(snapshot_world(&w, &reg), before);
        snap.discard();
        assert!(!snap.has_snapshot());
    }

    #[test]
    fn restore_leaves_unregistered_components_alone() {
        struct Handle(u32);
        let reg = Registry::with_defaults();
        let mut w = World::new();
        let e = w.spawn((Name("x".into()), Handle(7)));
        let mut snap = RegistrySnapshot::default();
        snap.capture(&w, &reg);
        w.get::<&mut Name>(e).unwrap().0 = "y".into();
        snap.restore(&mut w, &reg);
        assert_eq!(w.get::<&Name>(e).unwrap().0, "x");
        assert_eq!(w.get::<&Handle>(e).unwrap().0, 7);
    }

    #[test]
    fn level_scene_io_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jame_sceneio_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lvl.ron");
        let mut io = LevelSceneIo::default();
        let mut w = World::new();
        w.spawn((Name("floor".into()), Primitive(PrimitiveShape::Plane), Transform { position: Vec3::new(0.0, -1.0, 0.0), rotation: glam::Quat::IDENTITY, scale: Vec3::splat(10.0) }));
        w.spawn((Name("box".into()), Primitive(PrimitiveShape::Cube), Transform::from_position(Vec3::new(2.0, 0.5, 1.0))));
        io.save(&w, &path).unwrap();
        assert_eq!(io.current_path(), Some(path.as_path()));

        let mut w2 = World::new();
        w2.spawn((Name("stale".into()),));
        let mut io2 = LevelSceneIo::default();
        io2.open(&path, &mut w2).unwrap();
        assert_eq!(w2.len(), 2);
        let mut found = Vec::new();
        for (_, (n, p, t)) in w2.query::<(&Name, &Primitive, &Transform)>().iter() {
            found.push((n.0.clone(), p.0, t.position, t.scale));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(found[0].0, "box");
        assert_eq!(found[0].2, Vec3::new(2.0, 0.5, 1.0));
        assert_eq!(found[1], ("floor".into(), PrimitiveShape::Plane, Vec3::new(0.0, -1.0, 0.0), Vec3::splat(10.0)));

        assert!(io2.open(&dir.join("missing.ron"), &mut w2).is_err());
        io2.new_scene(&mut w2).unwrap();
        assert_eq!(w2.len(), 0);
        assert!(io2.current_path().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
