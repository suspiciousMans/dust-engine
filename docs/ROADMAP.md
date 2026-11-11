# Jame Engine roadmap

Goal: make games on Jame faster to build and safer to change. The pressure
comes from Dreamscape, the largest game on the engine (~34k lines, an 8k-line
`main.rs`, one `DreamscapeGame` struct holding most state). Each phase below
ships on its own and is judged by a concrete game change it makes smaller.

The detailed plan, with tasks and acceptance tests, is in
[PLAN-engine-ergonomics.md](PLAN-engine-ergonomics.md).

## Where things stand

Updated 2026-11-11. Short version: the engine-side work for phases 0-5 exists
and is tested, but **no game consumes it yet** and **none of the Dreamscape
ports has been done**. Each phase's exit test is a Dreamscape change, so no
phase is "done" by the roadmap's own rule.

- The engine (this repo, ~46k lines under `engine/src`) has ECS, renderer,
  shader profiles, legacy editor panels, a docked editor (`EditorRuntime`,
  RON scenes, prefabs, Assets and UI Designer tabs), physics, pathfinding, AI,
  rigs, scripting, net, audio with named tracks, hot reload, a profiler, and
  the new `Schedule`/`Resources`, effects, `Tunables`/`Telemetry`, input
  action maps, skeletal animation and headless harness.
- Dreamscape's divergent engine copy was audited and merged into this repo
  (`docs/fork-audit.md`). Pointing Dreamscape at this repo as a dependency
  (tasks 0.3/0.4) is not done here and cannot be verified from this repo;
  `docs/KNOWN-ISSUES.md` says to confirm it.
- The `Game` trait is still `update(ctx, dt)` + `render(ctx)`. `Schedule`
  exists, but the bundled games (`sandbox`, `mushroom_man`, `walkaround_demo`,
  `physics-sandbox`) still hand-order their work in `main.rs`
  (7-8k lines each for the first three).
- Not built or exercised: the emscripten/web paths (cfg-gated, type-checked
  at best), `SkinnedMesh` on a real GL context, and click-through use of the
  docked editor. CI now has a Linux job alongside Windows; web is not gated.

### Status by phase

| # | Phase | Engine side | Still open | Status |
|---|---|---|---|---|
| 0 | Reconcile the fork | Audit written, Dreamscape superset merged, audio split into `audio/{native,web}.rs` | 0.3 Dreamscape on this repo as a dependency; 0.4 `nn_viz`, `dungeon_oxide`; 0.5 downstream-game CI job | Partial |
| 1 | Effect primitives | `engine::effects`: `Lifetime`, `Fade`, `TextureOverride`, `tick`; unit-tested | 1.4 port dash ghosts/bursts/STILLNESS/PHASE; `MeshRenderer` has no alpha so `Fade` alpha is not rendered | Partial (engine done, no consumer) |
| 2 | Audio buses | `AudioEngine` named tracks, gain/fade/mute, shared loop clock, `AudioBackend` trait, `NullBackend` for tests | 2.4 port adaptive music; web backend never built; games must call `audio.update()` | Partial (engine done, no consumer) |
| 3 | Systems and scheduling | `Schedule` (stages, before/after, run conditions, cycle errors) and `Resources` | 3.4 run three Dreamscape features as registered systems; split `main.rs`; no bundled game runs a `Schedule` | Partial (engine done, no consumer) |
| 4 | Headless test harness | `Headless`, `HeadlessInput`, `EditorHeadless`, `fakes`; used by the engine's own tests | The exit test (Stillness restores enemy textures) lives in Dreamscape; no game has headless tests | Partial |
| 5 | Tunables and telemetry | `Tunables` (mtime/watcher reload, `for_build`), `Telemetry` CSV hook | 5.2/5.3 port Dreamscape's constants and balance CSV; no game loads a tunables file | Partial (engine done, no consumer) |
| 6 | Data-driven unlocks | - | Not started (optional, game-side first) | Not started |

Work outside the original phases that also landed in the engine (all
unit-tested, none hand-driven end to end): the docked editor and its scene /
prefab I/O, input action maps (`input_map`), skeletal animation, the UI
designer, and POSIX `new_game.sh` / `package.sh`. See `docs/KNOWN-ISSUES.md`
for the per-feature gaps.

## Phases

| # | Phase | Why | Size | Depends on |
|---|---|---|---|---|
| 0 | Reconcile the fork | One engine, one source of truth | M | none |
| 1 | Effect primitives | `Lifetime`/`Fade` and `TextureOverride` replace three hand-rolled spawn/fade/despawn patterns | S | 0 |
| 2 | Audio buses | N named looping tracks sharing a clock; native and web from one trait | M | 0 |
| 3 | Systems and scheduling | Features plug in; `main.rs` can be split | L | 0, 1 |
| 4 | Headless test harness | Test game behaviour without a GL context | M | 3 helps, not required |
| 5 | Tunables and telemetry | Retune constants without a rebuild; shared balance logging | S | 0 |
| 6 | Data-driven unlocks (optional, game-side first) | Adding a form/record becomes a data edit | M | none |

Suggested order: **0 → 1 → 2 → 5**, then **3 → 4**. Phases 1, 2 and 5 are
small and independent once 0 is done, so they can run in parallel.

## Phase summaries

**0. Reconcile the fork.** Work out, file by file, which side is newer, merge
into this repo, then make Dreamscape (and `nn_viz`, `dungeon_oxide`) consume
the engine as a dependency instead of a copy. Exit: `diff` between the
Dreamscape build's engine and this repo is empty because there is only one.

**1. Effect primitives.** Dash ghosts, burst cubes and echoes each spawn an
entity, record a birth time, shrink it, then despawn it. Phase and Stillness
swap a texture and remember the old one to restore it. Make both components
and ticking systems in the engine. Exit: Dreamscape's `update_ability_fx` and
burst code shrink to spawning the component.

**2. Audio buses.** The recent adaptive-music change added a second
hard-coded sink (`layer`, `layer_gain`) and had to be written twice, for
native and web. Replace it with named tracks that have a gain and share one
loop clock. Exit: a third layer is one line of game code and works on both
backends.

**3. Systems and scheduling.** A registry of systems with explicit ordering
and run conditions, so a feature adds a system instead of editing the main
loop. Exit: at least three Dreamscape features (adaptive music, ability FX,
echoes) run as registered systems and `main.rs` loses their fields.

**4. Headless test harness.** A world plus a stub context that skips GL so
tests can advance N frames and assert on entities. Exit: a test proves
Stillness restores enemy textures when it ends.

**5. Tunables and telemetry.** Load named constants (music `NEAR`/`FAR`, form
multipliers, `GHOST_LIFE`) from a RON file watched by hot reload, and turn the
balance CSV logger into an engine hook. Exit: edit a value, see it in the
running game.

**6. Data-driven unlocks.** A condition → reward table in RON for forms,
records and attunements. Prove it in Dreamscape before deciding whether it
belongs in the engine at all.

## Non-goals

- A new renderer, editor rewrite, or scripting-language changes.
- Breaking the public API without a migration note in the PR.
- Anything that stops existing seeds from replaying in Dreamscape. New
  randomness uses its own seeded stream.

## Working rules

- One PR per phase, tests first, commit per green task.
- Each phase is verified by porting a real Dreamscape feature, not by a toy.
- Native and web (emscripten) must both build before a phase is done.
