# Scripting with Oxidized (`.ox`)

Status: **implemented and the only scripting system** (plan: Stage 5 item 4). The `.pss` interpreter, `ScriptBehavior`,
`testkit::SmokeTestHost`, the `legacy-pss` feature and `tools/script_editor` were removed (owner decision 2026-11-11).
Engine name stays "Jame" until Stage 6; paths below will move with the rename.

## 1. What Oxidized is today (measured, not assumed)

`/home/_l/oxidized` is a lexer → parser → AST → typecheck → **transpiler to Rust**; `rustc` does the rest.
There is **no interpreter or VM**. `src/repl.rs` says so itself: every evaluation "transpiles and runs a whole
program, so it is slow (hundreds of ms), by design".

The feature that makes embedding possible is `fn native` (README, "Calling Rust from oxidized"): a bodiless
declaration the *host* implements, resolved through `crate::__oxidized_natives`. Calls are emitted as plain Rust calls.

Probe of the generated code for a 9-line script with 3 natives:

```
$ oxidized check t.ox            -> oxidized: no errors
$ oxidized build t.ox -o t.rs    -> 4568 lines; no `fn main`; `use crate::__oxidized_natives::*;`
                                    script fn emitted as `fn on_update(dt: f64) -> ()`  (private, typed)
```

## 2. Options

| | (a) transpile at build time, link natively | (b) `.ox` → cdylib, `libloading`, hot reload | (c) embedded interpreter/VM |
|---|---|---|---|
| Works with oxidized today | yes | **yes** | **no — does not exist** |
| Compile cost / script | 6.1 s (`rustc -C opt-level=1`, bin) | **1.3–3.7 s** (cdylib, opt 1: 2.9 s, opt 0: 2.1 s) | n/a |
| Hot reload | no (relink the game) | **yes** | would be |
| Ship story | best (static, no `rustc` needed on player machine) | needs cdylibs next to the exe (or build them offline) | n/a |
| Crash isolation | script panic = engine panic | **`catch_unwind` in shim; verified** | |

Evidence for (b), spike done before writing any engine code (hand-written shim, called from Python `ctypes`):

```
$ rustc --edition 2021 --crate-type cdylib -C opt-level=0 -o libt0.so shim.rs   real 0m2.061s
$ rustc --edition 2021 --crate-type cdylib -C opt-level=1 -o libt1.so shim.rs   real 0m2.891s
$ python3 call.py            # ox_init(host_cb); ox_on_update(0.5)
log len 6.0
0 [0.5]                      # rc 0; host-side y updated by the script via the callback
```

Evidence for (a): same generated `.rs` included as `mod ox { include!("t.rs"); }` in a bin: `rustc -C opt-level=1`
took 6.1 s, runs, 4.4 MB. Works — but it is the same shim, minus reload.

### Decision

**(b) is the primary path (dev, editor, hot reload); (a) is the same shim built statically for shipping.**
Both consume the identical generated Rust, so nothing diverges. Only (b) is implemented; (a) is a follow-up:
`gen_shim` emits `mod ox`, then export hooks as plain `pub fn`s instead of `extern "C"`, and have `host_call`
call `runtime::dispatch` directly. `rustc` must be installed wherever scripts are (re)built — dev machines only.

## 3. Architecture (as built)

```
games/<g>/scripts/foo.ox         (asset root = the game dir; level files say `scripts/foo.ox`)
   │  oxc_scripts build  /  OxRuntime::reload   (engine/src/script/ox/build/)
   ▼
entry.ox = import engine_api.ox + import foo.ox      engine_api.ox is GENERATED from spec.rs
   │  oxidized build            (CLI; found via $OXIDIZED_BIN, PATH, ~/.local/bin, ~/oxidized/target/release)
   ▼
foo.rs  → hooks made `pub` → foo.shim.rs = [OxVal ABI + __oxidized_natives + mod ox{include foo.rs} + extern "C" ox_on_*]
   │  rustc --crate-type cdylib  (panic=unwind, opt-level 1)
   ▼
liboxs_foo.<cache key>.so  ──libloading──►  OxBehavior : Behavior   (engine/src/script/ox/runtime.rs)
```

* **One source of truth for the API:** `engine/src/script/ox/build/spec.rs::SPEC` (one line per native). From it we
  generate `engine_api.ox`, the shim's `__oxidized_natives`, and the host op table. A hash of the spec is baked into
  every `.so` and checked on load, so a stale cached library is rejected, never mis-dispatched.
* **ABI:** a single `extern "C" fn(op, *const OxVal, n, *mut OxVal)` host callback. No per-function symbols, no
  `--export-dynamic`. Math (`sin cos tan atan2 clamp lerp pi tau`) is inline in the shim — no host round trip.
* **State:** `.ox` has no globals and script fns are stateless, so per-entity state is held by the engine:
  `get_var/set_var` (floats) and read-only `tunable(name, default)` (designer overrides in `EntityState.tunables`).
* **Hot swap:** all `OxBehavior`s of a script share an `Arc<ScriptSlot>`; a successful rebuild replaces the slot's
  `LoadedScript`, so every live instance switches on its next hook with its state intact. A failed rebuild leaves the
  last good build running. Old libraries are leaked on purpose (unloading a Rust cdylib with TLS is unsafe; ~4 MB/reload, dev only).
* **Errors:** build errors → `ScriptDiagnostics { items: Vec<ScriptDiagnostic { file, line, col, message }> }`
  (`to_strings()` gives `file:line:col: message` for the console). Runtime panics in a script are caught in the
  shim, returned as an error string, logged through the entity's `ScriptApi::log` as `[script error] …`, and
  available from `OxBehavior::take_errors()`.
* **Watcher:** `OxRuntime::start_watching()` (notify, dep already present) + `poll()` each frame: 150 ms debounce,
  rebuild on a worker thread, swap, returns `Vec<ReloadReport>`; `diagnostics()` returns everything currently broken.

### Script-facing API (v1)

Hooks (declare with exactly these signatures; unannotated params are rejected with a line-numbered diagnostic):
`on_ready()` (alias `on_start()`), `on_update(dt: Float)`, `on_interact()`, `on_trigger_enter(other: String)`,
`on_trigger_exit(other: String)` — these map 1:1 to `behavior::Behavior`.

| area | natives |
|---|---|
| transform | `pos(axis)`, `set_pos(x,y,z)`, `move_by(dx,dy,dz)`, `rot(axis)`, `set_rot(x,y,z)`, `scale(axis)`, `set_scale(x,y,z)` (axis 0/1/2) |
| input | `key_down(key) -> Int`, `key_pressed(key) -> Int`, `axis(name) -> Float` |
| world | `spawn(name,x,y,z) -> Int`, `despawn(id)` |
| log/time/audio | `log(msg)`, `time()`, `play_sound(path)`, `play_tone(hz,secs)` |
| data | `tunable(name,default)`, `get_var(name,default)`, `set_var(name,value)` |
| HUD/camera | `hud_bar(name,f)`, `toast(msg,secs)`, `camera_shake(i,secs)` |
| math (inline) | `sin cos tan atan2 clamp lerp pi tau` |

Rust side: trait `OxApi` (the full surface), `BehaviorCtx` (adapter over the existing `behavior::ScriptApi`),
`OxServices` (input snapshot in; `OxCommand` queue out: `Spawn`, `Despawn`, `SetRotation`, `SetScale`).
`behavior::ScriptApi` has no rotation/scale/spawn/input, and `behavior.rs` is owned by another change, so those go
through `OxServices`; the app calls `services.set_input(..)` before hooks and `services.drain_commands()` after.

## 4. Example & verification

(Output below is from the original vertical slice, before `.pss` was removed; the legacy-interpreter test counts no longer apply.)

`games/sandbox/assets/scripts/spin_bob.ox` — spin (tunable speed, Space = 3×) + bob (base_y captured in `on_ready`) +
interact tone/sound + `on_trigger_enter` logs and spawns `pickup_spark`, `on_trigger_exit` despawns it.

Headless tests (no SDL/GL; real `oxidized` + `rustc`; skipped with a message if `oxidized` is missing):

```
$ CARGO_TARGET_DIR=$PWD/target cargo test -p engine --lib script:: --offline -j2
test script::ox::tests::spin_bob_runs_through_behavior_hooks ... ok
test script::ox::tests::compile_errors_become_diagnostics_and_fix_hot_swaps ... ok
test script::ox::tests::bad_hook_signature_is_reported_with_line ... ok
test script::ox::tests::script_panic_is_contained ... ok
test script::ox::tests::file_watcher_triggers_rebuild_and_swap ... ok
(+ 7 build/diag/spec/codegen unit tests and the 10 legacy interpreter tests)  test result: ok. 22 passed
$ cargo test -p engine --no-default-features --lib script::   -> 12 passed (no legacy-pss)
$ cargo test -p oxc_scripts                                    -> 9 passed
$ oxc_scripts build games/sandbox/assets/scripts
ok    bob_demo -> …/liboxs_bob_demo.0.so [hooks: on_ready, on_update, on_interact] (3.7s)
ok    spin_bob -> …/liboxs_spin_bob.0.so [hooks: on_ready, on_update, on_interact, on_trigger_enter, on_trigger_exit] (5.3s)
2 script(s), 0 failed
```

Cost: `bench_spin_bob` (ignored test): ~36 µs per `on_update` (debug engine build, script at opt 1). Most of that is the
transpiler's `DynamicValue`/`String` allocation for string literals (`"base_y".to_string()` per call) — fine for tens of
scripted entities, not for thousands. Only a release-engine measurement would be authoritative; not taken.

## 5. Migration `.pss` → `.ox` (done; `.pss` is no longer loadable)

* All bundled scripts are `.ox`: `games/{sandbox,walkaround_demo,mushroom_man}/scripts/bob_demo.ox` (converted with
  `oxc_scripts convert`, then hand-edited: amplitude/period became `tunable(...)`s with the same defaults, state is
  `get_var/set_var`) and the six smoke drivers under `games/*/scripts/smoke/*.ox` (ported by hand, see §8).
* `oxc_scripts convert foo.pss [--out-dir D]` stays as a one-way migration aid for scripts outside this repo: hook
  renames (`ready/update/interact/trigger_*` → `on_*` with typed params), `get_x/y/z()` → `pos(0/1/2)`,
  `set_position` → `set_pos`, `play_sfx` → `play_sound`, `6.283185307` → `tau()`, top-level `let` → `get_var/set_var`,
  semicolons dropped. It prints warnings (untyped helper fns, string `+`, `screen_flash` which has no `.ox` API yet) and
  refuses smoke-driver scripts (port those by hand). Its fixtures live in `tools/oxc_scripts/tests/fixtures/`.
* Removed: `engine/src/script/{lexer,parser,interpreter}.rs`, `behavior::ScriptBehavior`, `testkit.rs`, the `legacy-pss`
  cargo feature (engine now has no features), `tools/script_editor` (it only edited/syntax-checked `.pss` through the
  removed interpreter; nothing was ported — use any text editor, errors show as `file:line:col` in the log/console).
* A level/class entry pointing at a non-`.ox` script is logged as unsupported and spawns without behavior.

## 6. What Oxidized cannot do yet (and the minimal change)

Verified against `target/release/oxidized` (built 2026-10-16):

1. **A native returning `Bool` cannot be used as a condition.** `if key_down("x") {…}` transpiles to
   `(key_down("x".to_string())).is_truthy()` and rustc fails: `no method named is_truthy found for type bool`. `== true`
   fails too (`bool == DynamicValue`). *Workaround used:* key queries return `Int` and scripts compare `== 1`.
   *Minimal fix (oxidized):* in the runtime prelude, add `trait __OxTruthy { fn is_truthy(&self) -> bool; } impl __OxTruthy for bool { fn is_truthy(&self) -> bool { *self } }`
   (or make `emit_bool_expr` skip `.is_truthy()` when the callee is a native with `-> Bool`). Not applied — workaround is adequate.
2. **No math builtins**: `sin`, `cos`, `clamp`, `PI` are "undefined function/variable" (checked). Provided as inline shim natives.
   Proper fix: add them to `stdlib.rs`.
3. **No globals/statics** (top level allows declarations only) → state is engine-held vars. No struct field assignment from
   outside methods, so a `State` struct round-trip is awkward; `get_var/set_var` is the pragmatic answer. A typed per-entity
   `Map` argument (`on_update(dt, state)`) would need nothing from oxidized but hook-signature work here.
4. **Flat namespace**: all functions share one scope per compilation, so each script is its own cdylib (done) and shared helper
   files must be `import`ed (textual include), which duplicates them per script.
5. **No attributes/`extern "C"`/`pub`** in `.ox`: the shim is generated Rust and a textual `\nfn <hook>(` → `\npub fn <hook>(` rewrite
   exports hooks. Cleanest oxidized change: a `pub fn` modifier (or `#[export]`) that is passed through to the transpiler.
6. **Hook params must be typed** (`dt: Float`); untyped params become `DynamicValue` and do not match the shim — caught up front.
7. `List.map(f)` is not a method (only struct methods exist); closures and traits do work (checked) but weren't needed.
8. No per-statement source map for runtime panics: the panic location is generated Rust (`foo.pub.rs:951`), the message is
   the script's own (`assert(false, "boom")` → `boom`). Compile errors do carry `.ox` line/col.

## 7. Not done / next

* Static ship build, option (a): games currently need `oxidized` + `rustc` on the machine that *builds* scripts (first run);
  a player machine can run from a pre-filled `.ox-build` cache only if the `oxidized` binary is still discoverable (the cache key
  hashes the compiler binaries). A build-time/`build.rs` path or a keyed-by-source cache that does not need the compiler is open.
* The bundled games apply `set_rot`/`set_scale` but only log `spawn`/`despawn` commands as unsupported.
* Natives for `screen_flash` are missing; `.ox` cannot trigger it.
* `oxidized` discovery uses the CLI; if/when oxidized is vendored the same code could call `oxidized::pipeline::compile_file` in-process.
* Windows/macOS lib naming is handled in `lib_file_name` but untested.
* `[profile.release] panic = "abort"` applies to the engine only; script libraries are built by `rustc` with `panic=unwind`
  so script panics stay contained, but the host trampoline's `catch_unwind` is a no-op in a release engine.

## 8. Smoke-test driver (`tick`)

Replaces `App::run_scripted` + `SmokeTestHost` (see GUIDE.md "Scripted smoke tests" for the user-facing description).

* **Script shape:** `fn tick(frame: Int, t: Float)`; called once per frame by `App::run_scripted(.., SmokeDriver)`.
* **Profile:** `build::spec::Profile::Smoke` (`SMOKE_SPEC` + `SMOKE_PRELUDE`) has its own native table, its own API hash
  (a behavior library cannot load as a smoke library and vice versa) and one hook, `tick`. Natives: `log time press release
  assert_eq assert_eq_str get_var set_var clamp`, plus ox-level wrappers `assert_true(cond: Bool, msg)` and `finish(ok: Bool)`.
  Why wrappers: oxidized wraps a `Bool` argument in `DynamicValue::Bool(..)`, which a native declared with a `bool` parameter
  rejects in rustc; an ox-defined `fn` taking `Bool` accepts it (checked), and forwards to `check_fail(msg)` / `finish_with(0|1)`.
* **State:** ox has no globals, so phase counters use `get_var/set_var`, backed by a host-side map (`SmokeState`).
* **Events:** `press/release` queue `sdl2::event::Event::KeyDown/KeyUp`; `run_scripted` drains them after `tick` into
  `ctx.input.handle_event` + `game.handle_event`. Real window, exit code 0/1 as before.
* **Loading without a hand-built cdylib:** `SmokeDriver::load(script)` runs the same `build_script` pipeline as hot reload
  (`oxidized build` → shim → `rustc` cdylib) into `<script dir>/.ox-build`, then `libloading`s it. A content-hash cache
  (`oxs_<name>.<key>.so`, key = profile API hash, opt level, source, sibling `.ox` files when the script imports, size+mtime of
  `oxidized`/`rustc`) makes repeat runs skip the compilers; the library is written as `*.part` and renamed, so an interrupted
  build is never trusted. `OX_NO_CACHE=1` disables it. A `build.rs` step was not needed.
* **Measured (debug engine, this machine):** cold build of a smoke script 0.9–1.9 s (`built in 1.9s` first sandbox run,
  `1.2–1.3 s` walkaround_demo / mushroom_man, `0.0s (cached)` afterwards); `check_docked_editor.sh` end to end 8.4 s including the
  cold build and a ~6 s run. The build happens before the window opens, so it does not distort the game clock.
* **Errors:** build errors print `file:line:col: message` and exit 1; a script panic in `tick` (including a failing
  `assert(..)`) logs `smoke test script error: <name>::tick: <msg>` and exits 1; a `.pss` path is rejected.
* **Tests:** `script::ox::tests::smoke_*` (headless: keys injected, phase state, assertion-wins-over-finish, missing `tick`, panic,
  `.pss` rejection) and `behavior_build_cache_skips_the_compilers`.
