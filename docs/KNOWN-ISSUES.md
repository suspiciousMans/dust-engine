# Known issues

Each item names the plan stage that owns it (see `docs/ROADMAP.md`).
Updated 2026-11-11 after merging the legacy-editor retirement in all three games, scene/prefab I/O, input maps, skeletal animation and docs/packaging.

## Engine

| Issue | Owner | Notes |
|---|---|---|
| Engine forked from Dreamscape (40 files differ at audit time) | Stage 1 / P0 | `docs/fork-audit.md`; confirm the reconcile actually landed |
| Phases 1-3 and 5 exist in the engine (effects, audio tracks, Schedule/Resources, Tunables/Telemetry) but no game uses them yet; the Dreamscape ports (plan tasks 1.4, 2.4, 3.4, 5.2, 5.3) are open | Stage 1 | Dreamscape is not in this repo |
| Fade alpha is stored on `Fade` only; `MeshRenderer` has no tint/alpha so the renderer ignores it | Stage 1 / P1 | |
| Games must call `audio.update()` each frame for track fades to advance | Stage 1 / P2 | web audio path type-checked only, never built |
| `physics::sim::tests::five_hundred_body_benchmark` asserts <50 ms/step in debug and fails under heavy machine load | Stage 0 | flaky, not a logic bug |
| Skeletal animation (`engine::skeletal`): CPU skinning + glTF import are unit-tested only; `SkinnedMesh` GL upload and drawing have never run on a real GL context, no game/editor draws it yet, no GPU-skinning path, no inspector registration, glTF imports the first skin only | Stage 5 | `engine/src/skeletal/` |
| Physics: see the **Physics audit** table below (what exists/tested/missing: no convex mesh, no angular dynamics, no body CCD, no kinematic flag, no islands) | Stage 5 | `engine/src/physics/` |
| `new_game.sh` / `package.sh` (bash) exist alongside the PowerShell scripts; `new_game.sh` was checked by scaffolding a copy and `cargo check`ing it, `package.sh` only by `bash -n` and a dry run. No web (emscripten) packaging: no build recipe is documented in the repo | Stage 5 | `new_game.sh`, `package.sh` |
| CI has Windows and Linux jobs (`.github/workflows/ci.yml`; the Linux job was validated as YAML only, never run on GitHub); web is not gated | Stage 0 | |
| `cargo clippy --workspace` reports ~100 style warnings (not gated) | Stage 0 | mostly `map_or`, complex types |

## Physics audit

Status per feature in `engine/src/physics/`. "tested" = a unit test in the module exercises it and passes (`cargo test -p engine --lib physics`: 72 tests).

| Feature | Status | Notes |
|---|---|---|
| Shape: AABB | exists + tested | `ColliderShape::Aabb`, always axis-aligned (rotation ignored); `narrow.rs` tests |
| Shape: sphere | exists + tested | |
| Shape: capsule | exists + tested | local +Y axis, rotates with `Transform::rotation` |
| Shape: oriented box | exists + tested | `ColliderShape::Box`; SAT incl. edge-edge, `narrow.rs` tests |
| Shape: convex mesh | **missing** | no hull/mesh collider type; only the four primitives |
| Raycast (`raycast`, `raycast_filtered`, `raycast_all`) | exists + tested | all four shapes, layers, triggers, exclusion, started-inside, deterministic ties |
| Sweeps (`shape_cast`, `spherecast`, `capsule_sweep`, `box_sweep`) | exists + tested | thin-wall no-tunnel test on a long cast |
| Overlap (`overlap_shape/sphere/box/capsule`) | exists + tested | |
| Layers / masks (`CollisionLayers`, `LayerNames` RON) | exists + tested | mutual rule; queries filter by layer mask |
| Trigger events (`PhysicsEvent`, `TriggerEvents` resource, `physics_system`) | exists + tested | Enter/Stay/Exit, stable order; Headless wired. Events are body-vs-collider only, no contact (non-trigger) events |
| Joints (distance, rope, pin, spring; `Joint` component) | exists + tested | pendulum length over 600 steps and period, spring vs analytic frequency + reduced mass, 5-link chain, rope slack/taut, pin, determinism (bit-identical in both modes). Iterative position-based, unit-mass bodies; no hinge/angle limits/motors; jointed bodies never sleep |
| Angular velocity / rotation dynamics | **missing** | `RigidBody` is `{ velocity, grounded }`; rotation is set by the game and only affects collision shape. No torque, no spin from contacts |
| Friction / restitution (`PhysicsMaterial`) | exists + tested | one combined test (bounce + slow-down); sqrt/max combine; no rolling friction (no angular dynamics) |
| Sleeping | exists + tested (per body) | `SleepConfig`; `stack_settles_and_sleeps`. **Islands: not done** — each body sleeps on its own timer and wakes when moved/touched by an awake body |
| CCD for dynamic bodies | **missing** | bodies integrate discretely, so fast small bodies can tunnel. Queries and the character controller do swept tests (`tiny_sphere_does_not_tunnel_thin_wall_on_long_cast`); fixed 1/60 s step with up to 8 substeps per frame |
| Kinematic bodies | **missing** (as a body type) | no flag, no velocity transfer to touching bodies. A collider without `RigidBody` is static and can be moved by writing its `Transform`, but riders are not carried. See character controller for the kinematic capsule |
| Character controller (`CharacterController::move_and_slide`) | exists + tested | slide, step offset, slope limit, ceiling, grounded; does not push dynamic bodies |
| Broadphase | exists, benchmarked only | `five_hundred_body_benchmark`: ~7.9 ms/step on this branch vs ~8.3 ms/step on dust/main (debug build, 2 jobs, idle machine); no perf regression, no scaling test beyond 500 bodies |

## Editor

| Issue | Owner | Notes |
|---|---|---|
| Docked editor is hosted in all four games and is the ONLY editor in `sandbox`, `walkaround_demo` and `mushroom_man` (F1, F2, F4, `DUST_EDITOR=1` open it; `physics-sandbox` uses F1). Game panels are host tabs (Level / Selected / Create / Render, mushroom: Level (F2) / Render (F1) / Multiplayer). `main.rs` after retirement: sandbox 7,631, walkaround_demo 7,324, mushroom_man 7,643 lines (the rest is game logic and the panels kept on purpose). The game panels still use a whole-level snapshot undo that is separate from the shell's command undo (labelled in the tabs; using it clears the shell's undo). Rig instances cannot be moved with gizmos (rig parts are locked). Mouse picking, gizmo drags and File-menu clicks have never been driven by hand | Stage 2.2 / 3 | |
| File menu: the shell default `RonSceneIo` (New/Open/Save/Save As/Revert, `*` dirty marker, unsaved-changes / autosave-recovery / legacy-level prompts) is wired. The three games replace it with a `GameSceneIo` that saves and loads their own `levels/*.ron` (queued until after the shell's UI pass; save errors reach the console only); the `*` marker follows the shell's undo only, not panel edits; window close goes through `request_quit` / `QuitApproved`. `physics-sandbox` uses the default | Stage 3.8 | clicks not hand-tested |
| Prefab UX is wired (Create Prefab from Selection, Instantiate Prefab picker, hierarchy ◆/◇ markers, inspector Apply / Revert All / Unpack, per-field override marker + Revert field, all undoable via `PrefabOpCommand`) but only with a project dir (`<dir>/prefabs`); without one the entries are disabled. Opening a scene does not re-propagate prefab edits made since it was saved; Apply/Create undo restores the prefab file but not other tooling's changes to it; components outside the inspector `Registry` are not part of the undo snapshot | Stage 3.8 | `EditorRuntime::new` registers the scene components; `EditorState::new()` alone (no `scene_registry()`) loses `PrefabInstance` on undo |
| Legacy `level::Level` files open through `level_compat` (non-primitive data kept as raw payload) and are written back only in the Scene format after a confirmation; there is no "Save as Level" menu entry (`level_compat::save_as_level` exists) | Stage 3.8 | |
| Assets tab is wired (`editor/asset_tab.rs`, `asset_drop.rs`): folder browse, search, select, double click -> `ShellEvent::AssetOpened`, drag to viewport/hierarchy -> `ShellEvent::AssetDropped`. The three big games set the asset root and apply a dropped image to the object under the cursor / open a dropped level `.ron`; `physics-sandbox` does neither. Thumbnails are never freed; only png/jpg are thumbnailed; drags never driven by hand | Stage 3.7 | |
| UI Designer tab is wired (`editor/designer_tab.rs`): designer panel, document picker over `EditorRuntime::set_ui_dir` (the three big games set `<asset root>/ui`), built-in examples, undo, live canvas preview. The preview is the designer's own canvas, not the game's UI layer; games still draw their HUD by hand | Stage 4.2 | |
| Click-through of select / gizmo / undo / Play / Stop has only been unit tested and smoke-tested via F3/F4 scripts, not driven by hand | Stage 3 | |
| Play/Stop restores only components registered in the inspector `Registry` | Stage 3 | register game components with `register_cloneable` |
| Camera orbit input is not suppressed while a gizmo drag is active | Stage 3.4 | |
| Scene `format_version` has no migrations yet (still version 1) | Stage 5 | hook exists in `scene::builtin_migrations`; worked v1 -> v2 example and error cases in `editor/scene_migration_tests.rs`, documented in GUIDE.md |

## Games

| Issue | Owner | Notes |
|---|---|---|
| `physics-sandbox` logs "Frame dt" every frame after the first second | Stage 0 | existing behaviour, noisy in editor mode |

## Naming

The project is still called Jame in code, paths and docs. The rename to Dust
is the last stage and waits on four owner decisions listed in the plan.
