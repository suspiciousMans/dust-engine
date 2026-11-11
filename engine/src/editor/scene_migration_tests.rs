//! Worked example of scene format versioning: how a v1 scene file is
//! upgraded to v2 through a custom [`Migration`] list passed to
//! [`Scene::from_ron_with`], and the three ways a load can be refused.
//!
//! The real [`SCENE_FORMAT_VERSION`] is untouched (still 1, with no built-in
//! migrations); these tests use a *pretend* target of 2 so a future change
//! can copy this pattern. To ship a real format change:
//!
//! 1. write a `fn(&mut Scene) -> Result<(), String>` that rewrites a scene
//!    from format `n` to `n + 1`,
//! 2. add `Migration { from: n, apply: that_fn }` to
//!    [`builtin_migrations`],
//! 3. bump [`SCENE_FORMAT_VERSION`] to `n + 1`.
//!
//! `from_ron_with` applies the chain one step at a time (`from == current
//! version`) and stamps `format_version = n + 1` after each step, so a
//! migration never sets the version itself.

use super::scene::{
    builtin_migrations, Migration, Scene, SceneError, SceneValue, SCENE_FORMAT_VERSION,
};

/// v1 stored a `Health` component with a single `hp` field.
const V1_SCENE: &str = r#"(
    format_version: 1,
    name: "arena",
    entities: [
        (id: "0000000000000001", name: "player",
         components: {"Health": {"hp": F32(40.0)}}),
        (id: "0000000000000002", name: "crate"),
    ],
)"#;

/// The example v1 -> v2 change: `Health.hp` becomes `Health.current`, and a
/// new `Health.max` field is added (defaulting to the old value so existing
/// entities start at full health).
fn health_hp_to_current_and_max(scene: &mut Scene) -> Result<(), String> {
    for entity in &mut scene.entities {
        let Some(health) = entity.components.get_mut("Health") else {
            continue; // entities without the component are untouched
        };
        match health.remove("hp") {
            Some(SceneValue::F32(hp)) => {
                health.insert("current".into(), SceneValue::F32(hp));
                health.insert("max".into(), SceneValue::F32(hp));
            }
            Some(other) => return Err(format!("Health.hp on '{}' is not an F32: {other:?}", entity.name)),
            None => {}
        }
    }
    Ok(())
}

fn v1_to_v2() -> Vec<Migration> {
    vec![Migration { from: 1, apply: health_hp_to_current_and_max }]
}

#[test]
fn v1_scene_upgrades_to_v2_through_a_custom_migration_list() {
    let scene = Scene::from_ron_with(V1_SCENE, &v1_to_v2(), 2).unwrap();

    assert_eq!(scene.format_version, 2, "stamped by from_ron_with, not by the migration");
    assert_eq!(scene.name, "arena");
    let player = &scene.entities[0];
    assert_eq!(player.field("Health", "hp"), None, "old field is gone");
    assert_eq!(player.field("Health", "current"), Some(&SceneValue::F32(40.0)));
    assert_eq!(player.field("Health", "max"), Some(&SceneValue::F32(40.0)));
    // An entity without the component passes through unchanged.
    assert!(scene.entities[1].components.is_empty());
}

#[test]
fn upgraded_scene_saves_as_v2_and_reloads_without_migrations() {
    let upgraded = Scene::from_ron_with(V1_SCENE, &v1_to_v2(), 2).unwrap();
    let text = upgraded.to_ron().unwrap();
    assert!(text.contains("format_version: 2"), "{text}");

    // A file already at the target version needs no migration at all.
    let reloaded = Scene::from_ron_with(&text, &[], 2).unwrap();
    assert_eq!(reloaded, upgraded);
}

#[test]
fn migrations_chain_one_version_at_a_time() {
    // v2 -> v3 renames the scene; it only runs after v1 -> v2.
    fn rename(scene: &mut Scene) -> Result<(), String> {
        assert_eq!(scene.format_version, 2, "previous step has already been stamped");
        scene.name = format!("{}-v3", scene.name);
        Ok(())
    }
    let mut chain = v1_to_v2();
    chain.push(Migration { from: 2, apply: rename });

    let scene = Scene::from_ron_with(V1_SCENE, &chain, 3).unwrap();
    assert_eq!(scene.format_version, 3);
    assert_eq!(scene.name, "arena-v3");
    assert!(scene.entities[0].field("Health", "current").is_some());
}

#[test]
fn too_new_scene_is_refused_not_mangled() {
    let from_the_future = "(format_version: 3, name: \"x\", entities: [])";
    match Scene::from_ron_with(from_the_future, &v1_to_v2(), 2) {
        Err(SceneError::TooNew { found: 3, supported: 2 }) => {}
        other => panic!("expected TooNew {{ found: 3, supported: 2 }}, got {other:?}"),
    }

    // Same through the real entry point: this build supports SCENE_FORMAT_VERSION.
    let newer = format!("(format_version: {}, entities: [])", SCENE_FORMAT_VERSION + 1);
    assert!(matches!(
        Scene::from_ron(&newer),
        Err(SceneError::TooNew { found, supported })
            if found == SCENE_FORMAT_VERSION + 1 && supported == SCENE_FORMAT_VERSION
    ));
}

#[test]
fn missing_migration_step_is_a_no_migration_error() {
    // A v1 file but a list that only knows how to leave v2.
    fn noop(_: &mut Scene) -> Result<(), String> {
        Ok(())
    }
    let only_v2 = [Migration { from: 2, apply: noop }];
    assert!(matches!(
        Scene::from_ron_with(V1_SCENE, &only_v2, 3),
        Err(SceneError::NoMigration(1))
    ));

    // An empty list can't upgrade anything.
    assert!(matches!(Scene::from_ron_with(V1_SCENE, &[], 2), Err(SceneError::NoMigration(1))));

    // A file with no `format_version` key reads as version 0, which no
    // migration list in this example covers.
    let unversioned = "(name: \"old\", entities: [])";
    let err = Scene::from_ron_with(unversioned, &v1_to_v2(), 2).unwrap_err();
    assert!(matches!(err, SceneError::NoMigration(0)), "{err:?}");
    assert_eq!(err.to_string(), "no migration from scene format version 0");
}

#[test]
fn failing_migration_reports_its_source_version_and_message() {
    // `hp` holding the wrong type makes the example migration refuse.
    let bad = V1_SCENE.replace("F32(40.0)", "Str(\"lots\")");
    match Scene::from_ron_with(&bad, &v1_to_v2(), 2) {
        Err(SceneError::Migration(1, msg)) => assert!(msg.contains("not an F32"), "{msg}"),
        other => panic!("expected Migration(1, ..), got {other:?}"),
    }
}

#[test]
fn builtin_migrations_cover_every_version_below_current() {
    // Invariant for whoever bumps SCENE_FORMAT_VERSION: exactly one built-in
    // migration leaves each older version, so any old file can reach current.
    let migrations = builtin_migrations();
    for v in 1..SCENE_FORMAT_VERSION {
        assert_eq!(
            migrations.iter().filter(|m| m.from == v).count(),
            1,
            "need exactly one migration from scene format v{v}"
        );
    }
    assert!(migrations.iter().all(|m| m.from < SCENE_FORMAT_VERSION));
}
