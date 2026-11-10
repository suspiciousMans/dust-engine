# Fork audit: engine vs Dreamscape's `engine/` copy

Compared: this repo at `3cdb9a9` (HEAD of `dust/p0-fork` before the merge) vs `/home/_l/dreamscape` branch `feat/ui-revamp` (`engine/`). Dates are the last commit touching the file in each repo.

## Method

Dreamscape's copy is rustfmt-formatted; this repo's is not. A raw diff shows 40+ files, but running `rustfmt --edition 2021` over both trees leaves only 12 files (plus `audio`, `perf.rs`, `mesh/instancing.rs`, `build.rs`, `Cargo.toml`, `LICENSE`) with real differences. In all of them Dreamscape is a strict superset: it adds code and removes nothing of ours. Formatting-only files keep this repo's text to avoid churn against concurrent branches (editor, ui_designer, script). Nothing in the Material-as-component change (`38d0ac5`: `ecs/components.rs`, `material.rs`) or the `pub use egui` restore differs between the two sides.

Build result after merge: `cargo test --workspace --offline -j2`: engine lib 62 passed; test_material 3, test_public_api 1, test_task4 3, test_tilemap 2; 0 failures; all 4 games + physics-sandbox compile. The emscripten paths (audio/web.rs, web_loop in app/mod.rs, GLES in window.rs, to_gles in program.rs) are cfg-gated and were NOT built here.

Verdicts: **format-only** = identical after rustfmt, kept ours. **DS newer** = took Dreamscape's file (semantic additions). **DS only** = new file copied. **merged** = hand-merged. **split** = restructured.

| File | This repo (last) | Dreamscape (last) | Verdict | Note |
|---|---|---|---|---|
| `Cargo.toml` | 2026-10-05 12a95ce | 2026-10-05 5b2cd0a | merged | merged: DS target-gated deps + license/publish; workspace sdl2 no longer forces bundled |
| `LICENSE` | n/a | 2026-10-05 6e42b3d | DS only | MIT for engine/; must survive the move |
| `build.rs` | 2026-09-17 39e3892 | 2026-09-25 755489b | DS newer | DS: uses CARGO_CFG_TARGET_OS (correct when cross-compiling); cfg! checks host |
| `src/ai.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/animation.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/app/mod.rs` | 2026-07-19 eb7315f | 2026-10-24 6346f68 | DS newer | DS: emscripten main loop, perf scopes |
| `src/audio/mod.rs` | n/a | 2026-09-25 90345c9 | DS only | native = rodio (superset of old audio.rs, adds music layer); web = SDL2 mixer, emscripten only |
| `src/audio/native.rs` | n/a | 2026-10-20 c8326b8 | DS only | native = rodio (superset of old audio.rs, adds music layer); web = SDL2 mixer, emscripten only |
| `src/audio/web.rs` | n/a | 2026-10-20 c8326b8 | DS only | native = rodio (superset of old audio.rs, adds music layer); web = SDL2 mixer, emscripten only |
| `src/behavior.rs` | 2026-07-19 0dd1123 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ecs/mod.rs` | 2026-07-18 83e3163 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/hotreload.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/input.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/level.rs` | 2026-07-22 8bc309b | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/lib.rs` | 2026-10-05 12a95ce | 2026-10-24 6346f68 | DS newer | DS: `pub mod perf`, cfg-gated module |
| `src/mesh/gltf_loader.rs` | 2026-07-22 8bc309b | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/mesh/instancing.rs` | n/a | 2026-10-25 b7a0769 | DS only | instanced draws |
| `src/mesh/mod.rs` | 2026-07-22 8bc309b | 2026-10-25 b7a0769 | DS newer | DS: `InstanceBuffer`, `draw_bound` |
| `src/net/framing.rs` | 2026-07-20 cc17505 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/net/mod.rs` | 2026-07-22 8bc309b | 2026-09-29 2b440dd | DS newer | DS: `pub use NetConnection`, MAX_PLAYERS 4→5 |
| `src/net/protocol.rs` | 2026-07-22 8bc309b | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/particles.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/pathfinding.rs` | 2026-07-19 246e35c | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/perf.rs` | n/a | 2026-10-25 9c10b71 | DS only | ENGINE_PROF frame profiler |
| `src/physics.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/platform/window.rs` | 2026-07-18 4283263 | 2026-10-24 e461aad | DS newer | DS: GLES3 on emscripten, ENGINE_NOVSYNC |
| `src/profile.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/renderer/mod.rs` | 2026-07-20 bea26be | 2026-10-25 b7a0769 | DS newer | DS: overlay, trails, grain texture; `present` takes `&mut self` |
| `src/renderer/particles.rs` | 2026-07-18 83e3163 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/renderer/post.rs` | 2026-07-19 d2de94a | 2026-10-25 b7a0769 | DS newer | DS: ScreenOverlay, TrailPass |
| `src/renderer/skybox.rs` | 2026-07-18 83e3163 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/rig.rs` | 2026-07-22 8bc309b | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/screen_effect.rs` | 2026-07-19 d2de94a | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/script/interpreter.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/script/lexer.rs` | 2026-07-18 83e3163 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/script/parser.rs` | 2026-07-18 83e3163 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/shader/mod.rs` | 2026-10-05 798c6b0 | 2026-10-25 b7a0769 | DS newer | DS: re-export INSTANCED_BIT |
| `src/shader/program.rs` | 2026-07-20 bea26be | 2026-09-25 90345c9 | DS newer | DS: GLSL ES conversion on emscripten |
| `src/shader/variant.rs` | 2026-07-18 4283263 | 2026-10-25 b7a0769 | DS newer | DS: INSTANCED_BIT, prewarm, tests |
| `src/testkit.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/texture.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/appearance_panel.rs` | 2026-07-22 8bc309b | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/asset_browser.rs` | 2026-07-20 bea26be | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/backend.rs` | 2026-07-19 eb7315f | 2026-10-24 e461aad | DS newer | DS: ui_scale, perf counters |
| `src/ui/hud.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/multiplayer_panel.rs` | 2026-07-20 cc17505 | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/output.rs` | 2026-11-10 3cdb9a9 | 2026-10-05 329d8fc | format-only | DS adds an unused `use PathBuf` (warns here); kept ours |
| `src/ui/panels.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `src/ui/profiler.rs` | 2026-07-19 eb7315f | 2026-09-25 a140089 | format-only | identical after rustfmt; kept ours |
| `tests/test_task4.rs` | 2026-11-10 3cdb9a9 | 2026-10-05 798c6b0 | format-only | DS adds an unused `use PathBuf` (warns here); kept ours |
