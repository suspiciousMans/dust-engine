//! Headless end-to-end tests (no GL/SDL window): real `oxidized` + `rustc`
//! build a real script, which runs through the engine's `Behavior` hooks.
//! Skipped (with a note) when the `oxidized` compiler isn't installed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::behavior::Behavior;

fn toolchain() -> Option<build::Toolchain> {
    match build::Toolchain::discover() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("SKIPPED (no toolchain): {e}");
            None
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ox-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../games/sandbox/assets/scripts")
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

#[test]
fn spin_bob_runs_through_behavior_hooks() {
    let Some(tc) = toolchain() else { return };
    let out = scratch("spin");
    let rt = OxRuntime::with_toolchain(tc, example_dir(), &out);
    rt.reload(&example_dir().join("spin_bob.ox")).unwrap_or_else(|d| panic!("build failed:\n{d}"));

    let mut b = rt.behavior("spin_bob");
    b.entity = 7;
    let mut api = HeadlessApi { position: (1.0, 2.0, 3.0), ..Default::default() };

    b.on_ready(&mut api);
    assert_eq!(api.logs.len(), 1, "{:?}", api.logs);
    assert!(api.logs[0].starts_with("spin_bob ready at y=2"), "{:?}", api.logs);

    // 1 second of updates at 100 ms.
    for i in 1..=10 {
        api.elapsed = i as f32 * 0.1;
        b.on_update(&mut api, 0.1);
    }
    assert!(b.take_errors().is_empty());
    // spin: default 1.5 rad/s * 1 s about Y
    assert!(near(b.state.rotation[1], 1.5), "rot {:?}", b.state.rotation);
    // bob: base_y 2.0 + 0.4*sin(t/2*tau) at t=1.0 → sin(pi)=0
    assert!(near(api.position.1, 2.0), "pos {:?}", api.position);
    // at t=0.5 the offset would be 0.4 — check the HUD recorded the peak
    let peak = api.hud_bars.iter().map(|(_, f)| *f).fold(0.0, f32::max);
    assert!(peak > 0.9, "peak {peak}");
    assert_eq!((api.position.0, api.position.2), (1.0, 3.0), "x/z untouched");
    let cmds = rt.services().drain_commands();
    assert_eq!(cmds.iter().filter(|c| matches!(c, OxCommand::SetRotation { entity: 7, .. })).count(), 10);

    // input query: holding Space triples the spin speed
    let mut snap = InputSnapshot::default();
    snap.down.insert("Space".into());
    rt.services().set_input(snap);
    let before = b.state.rotation[1];
    b.on_update(&mut api, 0.1);
    assert!(near(b.state.rotation[1] - before, 0.45), "{}", b.state.rotation[1] - before);

    // tunable override
    b.state.tunables.insert("spin_speed".into(), 10.0);
    rt.services().set_input(InputSnapshot::default());
    let before = b.state.rotation[1];
    b.on_update(&mut api, 0.1);
    assert!(near(b.state.rotation[1] - before, 1.0));

    // interact: log + tone + sound
    b.on_interact(&mut api);
    assert_eq!(api.tones, vec![(500.0, 0.1)]);
    assert_eq!(api.sfx, vec!["sfx/demo_blip.wav".to_string()]);

    // trigger enter spawns above, exit despawns the same id
    rt.services().drain_commands();
    b.on_trigger_enter(&mut api, "Player");
    assert!(api.logs.last().unwrap().contains("trigger enter: Player"));
    let cmds = rt.services().drain_commands();
    let [OxCommand::Spawn { id, name, position, requested_by: 7 }] = &cmds[..] else { panic!("{cmds:?}") };
    assert_eq!(name, "pickup_spark");
    assert!(near(position[1], api.position.1 + 1.0));
    b.on_trigger_exit(&mut api, "Player");
    assert_eq!(rt.services().drain_commands(), vec![OxCommand::Despawn { id: *id }]);
}

#[test]
fn compile_errors_become_diagnostics_and_fix_hot_swaps() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("diag");
    let file = dir.join("hot.ox");
    std::fs::write(&file, "fn on_update(dt: Float) {\n    let x: Int = \"oops\"\n}\n").unwrap();
    let rt = OxRuntime::with_toolchain(tc, &dir, dir.join("out"));

    let err = rt.reload(&file).unwrap_err();
    assert_eq!(err.items.len(), 1, "{err}");
    let d = &err.items[0];
    assert!(d.file.ends_with("hot.ox"), "{d:?}");
    assert_eq!((d.line, d.col), (2, 5), "{d:?}");
    assert!(d.message.contains("expected `Int` but found `String`"), "{d:?}");
    assert_eq!(rt.diagnostics().items.len(), 1);

    // Behavior exists before any good build and is a no-op.
    let mut b = rt.behavior("hot");
    let mut api = HeadlessApi::default();
    b.on_update(&mut api, 0.1);
    assert!(api.logs.is_empty());

    // Fix it: live behavior picks the new build up, diagnostics clear.
    std::fs::write(&file, "fn on_update(dt: Float) {\n    log(\"v1 {dt}\")\n}\n").unwrap();
    rt.reload(&file).unwrap();
    assert!(rt.diagnostics().is_empty());
    b.on_update(&mut api, 0.5);
    assert_eq!(api.logs, vec!["v1 0.5"]);

    // Edit again; a *bad* edit keeps the last good build running.
    std::fs::write(&file, "fn on_update(dt: Float) {\n    log(\"v2\")\n").unwrap();
    assert!(rt.reload(&file).is_err());
    b.on_update(&mut api, 0.5);
    assert_eq!(api.logs.last().unwrap(), "v1 0.5");

    std::fs::write(&file, "fn on_update(dt: Float) {\n    log(\"v2\")\n}\n").unwrap();
    rt.reload(&file).unwrap();
    b.on_update(&mut api, 0.5);
    assert_eq!(api.logs.last().unwrap(), "v2");
}

#[test]
fn bad_hook_signature_is_reported_with_line() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("sig");
    let file = dir.join("sig.ox");
    std::fs::write(&file, "\nfn on_update(dt) {\n}\n").unwrap();
    let rt = OxRuntime::with_toolchain(tc, &dir, dir.join("out"));
    let err = rt.reload(&file).unwrap_err();
    assert_eq!(err.items[0].line, 2);
    assert!(err.items[0].message.contains("on_update(dt: Float)"));
}

#[test]
fn script_panic_is_contained() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("panic");
    let file = dir.join("boom.ox");
    std::fs::write(
        &file,
        "fn on_interact() {\n    assert(false, \"boom\")\n}\nfn on_update(dt: Float) {\n    log(\"alive\")\n}\n",
    )
    .unwrap();
    let rt = OxRuntime::with_toolchain(tc, &dir, dir.join("out"));
    rt.reload(&file).unwrap();
    let mut b = rt.behavior("boom");
    let mut api = HeadlessApi::default();
    b.on_interact(&mut api);
    let errs = b.take_errors();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("boom"), "{errs:?}");
    assert!(api.logs[0].starts_with("[script error]"));
    b.on_update(&mut api, 0.1); // still usable afterwards
    assert_eq!(api.logs.last().unwrap(), "alive");
}

#[test]
fn file_watcher_triggers_rebuild_and_swap() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("watch");
    let file = dir.join("w.ox");
    std::fs::write(&file, "fn on_interact() {\n    log(\"one\")\n}\n").unwrap();
    let mut rt = OxRuntime::with_toolchain(tc, &dir, dir.join("out"));
    rt.reload(&file).unwrap();
    rt.start_watching().unwrap();
    let mut b = rt.behavior("w");
    let mut api = HeadlessApi::default();
    b.on_interact(&mut api);
    assert_eq!(api.logs, vec!["one"]);

    std::fs::write(&file, "fn on_interact() {\n    log(\"two\")\n}\n").unwrap();
    let t = Instant::now();
    let mut reports = Vec::new();
    while reports.is_empty() && t.elapsed() < Duration::from_secs(60) {
        reports = rt.poll();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(reports.len(), 1, "no reload report within 60 s");
    assert!(reports[0].result.is_ok(), "{:?}", reports[0].result);
    b.on_interact(&mut api);
    assert_eq!(api.logs.last().unwrap(), "two");

    // A broken save surfaces diagnostics through poll() and diagnostics().
    std::fs::write(&file, "fn on_interact() {\n    nosuch(1)\n}\n").unwrap();
    let t = Instant::now();
    let mut reports = Vec::new();
    while reports.is_empty() && t.elapsed() < Duration::from_secs(60) {
        reports = rt.poll();
        std::thread::sleep(Duration::from_millis(50));
    }
    let d = reports[0].result.as_ref().unwrap_err();
    assert!(d.items[0].message.contains("nosuch"), "{d}");
    assert_eq!(d.items[0].line, 2);
    assert!(!rt.diagnostics().is_empty());
    b.on_interact(&mut api);
    assert_eq!(api.logs.last().unwrap(), "two", "last good build keeps running");
}

/// `cargo test -p engine --lib bench_spin_bob -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_spin_bob() {
    let Some(tc) = toolchain() else { return };
    let out = scratch("bench");
    let rt = OxRuntime::with_toolchain(tc, example_dir(), &out);
    rt.reload(&example_dir().join("spin_bob.ox")).unwrap();
    let mut b = rt.behavior("spin_bob");
    let mut api = HeadlessApi::default();
    b.on_ready(&mut api);
    let n = 20_000;
    let t = Instant::now();
    for _ in 0..n {
        b.on_update(&mut api, 0.016);
        api.hud_bars.clear();
        rt.services().drain_commands();
    }
    let per = t.elapsed().as_nanos() as f64 / n as f64;
    eprintln!("spin_bob on_update: {per:.0} ns/call ({n} calls, debug engine build, script opt-level=1)");
}

// ------------------------------------------------------------ smoke driver

const SMOKE_SRC: &str = r#"
fn tap(key: String) {
    press(key)
    release(key)
}

fn tick(frame: Int, t: Float) {
    let phase = get_var("phase", 0.0)
    if phase == 0.0 {
        tap("F3")
        set_var("phase", 1.0)
        set_var("started", t)
        log("entering phase " + str(1))
    } else if phase == 1.0 {
        if t - get_var("started", 0.0) > 0.5 {
            press("W")
            assert_true(frame > 1, "frame counter advances")
            assert_eq(phase, 1.0, "phase is 1")
            assert_eq_str("a", "a", "strings match")
            finish(true)
        }
    }
}
"#;

#[test]
fn smoke_driver_injects_keys_tracks_state_and_finishes() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("smoke");
    let file = dir.join("s.ox");
    std::fs::write(&file, SMOKE_SRC).unwrap();

    let mut d = SmokeDriver::load_with(&tc, &file, &dir.join("out")).unwrap_or_else(|e| panic!("{e}"));
    assert!(!d.cached);
    d.tick(0.0).unwrap();
    let ev = d.drain_events();
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert!(matches!(&ev[0], sdl2::event::Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::F3), .. }));
    assert!(matches!(&ev[1], sdl2::event::Event::KeyUp { keycode: Some(sdl2::keyboard::Keycode::F3), .. }));
    assert_eq!(d.result(), None);
    assert_eq!(d.state().log_lines, vec!["entering phase 1"]);

    d.tick(0.3).unwrap();
    assert_eq!(d.result(), None, "phase 1 waits 0.5 s");
    d.tick(0.7).unwrap();
    assert_eq!(d.result(), Some(true), "{:?}", d.state().failures);
    assert!(matches!(&d.drain_events()[..], [sdl2::event::Event::KeyDown { keycode: Some(sdl2::keyboard::Keycode::W), .. }]));
    assert_eq!(d.frames(), 3);

    // Second load of unchanged source is a cache hit (no oxidized/rustc run).
    let again = SmokeDriver::load_with(&tc, &file, &dir.join("out")).unwrap();
    assert!(again.cached);
    assert!(again.build_time < Duration::from_secs(1), "{:?}", again.build_time);
}

#[test]
fn smoke_assertion_failure_wins_over_finish_true() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("smokefail");
    let file = dir.join("f.ox");
    std::fs::write(
        &file,
        "fn tick(frame: Int, t: Float) {\n    assert_true(t > 5.0, \"too early\")\n    assert_eq(1.0, 2.0, \"nope\")\n    press(\"NoSuchKey\")\n    finish(true)\n}\n",
    )
    .unwrap();
    let mut d = SmokeDriver::load_with(&tc, &file, &dir.join("out")).unwrap();
    d.tick(1.0).unwrap();
    assert_eq!(d.result(), Some(false));
    let f = &d.state().failures;
    assert_eq!(f.len(), 3, "{f:?}");
    assert!(f[0].contains("too early") && f[1].contains("1 != 2") && f[2].contains("unknown key 'NoSuchKey'"), "{f:?}");
    assert!(d.drain_events().is_empty());
}

#[test]
fn smoke_script_without_tick_or_with_panic_is_reported() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("smokebad");
    let no_tick = dir.join("n.ox");
    std::fs::write(&no_tick, "fn other() {\n    log(\"x\")\n}\n").unwrap();
    let e = SmokeDriver::load_with(&tc, &no_tick, &dir.join("out")).err().expect("must fail");
    assert!(e.contains("must define `fn tick"), "{e}");

    let pss = dir.join("old.pss");
    std::fs::write(&pss, "fn tick() {}\n").unwrap();
    let e = SmokeDriver::load_with(&tc, &pss, &dir.join("out")).err().expect("must fail");
    assert!(e.contains("must be Oxidized `.ox`"), "{e}");

    let boom = dir.join("b.ox");
    std::fs::write(&boom, "fn tick(frame: Int, t: Float) {\n    assert(false, \"kaboom\")\n}\n").unwrap();
    let mut d = SmokeDriver::load_with(&tc, &boom, &dir.join("out")).unwrap();
    let err = d.tick(0.0).unwrap_err();
    assert!(err.contains("kaboom"), "{err}");
}

#[test]
fn behavior_build_cache_skips_the_compilers() {
    let Some(tc) = toolchain() else { return };
    let dir = scratch("cache");
    let file = dir.join("c.ox");
    std::fs::write(&file, "fn on_interact() {\n    log(\"hi\")\n}\n").unwrap();
    let opts = build::BuildOptions::default();
    let a = build::build_script(&tc, &file, &dir.join("out"), &opts).unwrap();
    let b = build::build_script(&tc, &file, &dir.join("out"), &opts).unwrap();
    assert!(!a.cached && b.cached);
    assert_eq!(a.lib_path, b.lib_path);
    std::fs::write(&file, "fn on_interact() {\n    log(\"changed\")\n}\n").unwrap();
    let c = build::build_script(&tc, &file, &dir.join("out"), &opts).unwrap();
    assert!(!c.cached && c.lib_path != a.lib_path);
}
