# Known issues

Each item names the plan stage that owns it (see `docs/ROADMAP.md`).
Updated 2026-11-10 after merging shell, gizmos, scenes and physics.

## Engine

| Issue | Owner | Notes |
|---|---|---|
| Engine forked from Dreamscape (40 files differ at audit time) | Stage 1 / P0 | `docs/fork-audit.md`; confirm the reconcile actually landed |
| Phases 1-3 and 5 exist in the engine (effects, audio tracks, Schedule/Resources, Tunables/Telemetry) but no game uses them yet; the Dreamscape ports (plan tasks 1.4, 2.4, 3.4, 5.2, 5.3) are open | Stage 1 | Dreamscape is not in this repo |
| Fade alpha is stored on `Fade` only; `MeshRenderer` has no tint/alpha so the renderer ignores it | Stage 1 / P1 | |
| Games must call `audio.update()` each frame for track fades to advance | Stage 1 / P2 | web audio path type-checked only, never built |
| `physics::sim::tests::five_hundred_body_benchmark` asserts <50 ms/step in debug and fails under heavy machine load | Stage 0 | flaky, not a logic bug |
| Skeletal/skinned animation missing (segmented rigs only) | Stage 5 | |
| Physics ray/sweep queries and joints: check coverage after the physics rewrite | Stage 5 | |
| `new_game` scaffolding and packaging are PowerShell only | Stage 5 | `new_game.ps1`, `package.ps1` |
| CI builds on Windows only; Linux and web are not gated | Stage 0 | |
| `cargo clippy --workspace` reports ~100 style warnings (not gated) | Stage 0 | mostly `map_or`, complex types |

## Editor

| Issue | Owner | Notes |
|---|---|---|
| Docked editor (F4) is hosted in all four games, but the legacy F1/F2 editor is still in `sandbox`, `walkaround_demo`, `mushroom_man` (main.rs still ~7.7-8k lines each); two undo stacks; rig instances not editable by gizmo; HUD anchors to window not viewport; mouse picking and gizmo drag never hand-tested | Stage 2.2 / 3 | retire F2 to shrink main.rs |
| File menu uses `LevelSceneIo` (primitives only), not the RON `Scene`/prefab path | Stage 3.8 | `editor/scene.rs`, `prefab.rs` are not wired to the shell |
| Assets tab is wired (`editor/asset_tab.rs`, `asset_drop.rs`): folder browse, search, select, double click -> `ShellEvent::AssetOpened`, drag to viewport/hierarchy -> `ShellEvent::AssetDropped`. Games must set `EditorRuntime::set_asset_root` and handle the events; the hosted games do neither yet. Thumbnails are never freed; only png/jpg are thumbnailed; only hand-driven checks of the real drag are missing (headless tests only) | Stage 3.7 | no folder is configured by default -> empty state |
| UI Designer tab is wired (`editor/designer_tab.rs`): designer panel, document picker over `EditorRuntime::set_ui_dir`, built-in examples, undo, live canvas preview. Games must set the directory; the preview is the designer's own canvas, not the game's UI layer; never hand-driven | Stage 4.2 | no directory by default -> examples only |
| Click-through of select / gizmo / undo / Play / Stop has only been unit tested and smoke-tested via F3/F4 scripts, not driven by hand | Stage 3 | |
| Play/Stop restores only components registered in the inspector `Registry` | Stage 3 | register game components with `register_cloneable` |
| Camera orbit input is not suppressed while a gizmo drag is active | Stage 3.4 | |
| Scene `format_version` has no migrations yet | Stage 5 | hook exists in `scene::builtin_migrations` |

## Games

| Issue | Owner | Notes |
|---|---|---|
| `physics-sandbox` logs "Frame dt" every frame after the first second | Stage 0 | existing behaviour, noisy in editor mode |
| `walkaround_demo` has unused-variable warnings in some builds | Stage 0 | recheck |

## Naming

The project is still called Jame in code, paths and docs. The rename to Dust
is the last stage and waits on four owner decisions listed in the plan.
