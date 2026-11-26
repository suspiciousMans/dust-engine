# Dust Engine Guide

A lightweight Rust game engine (SDL2 + OpenGL) for building multiple games with
a PS2-era visual style: low internal resolution upscaled with nearest-neighbor
filtering, per-vertex lighting, fog, optional vertex snapping and affine
texture mapping, and posterize/dither post-processing — all controlled by
savable "shader profiles" you can tweak live in an in-game editor GUI.

## Workspace layout

```
dust-engine/
  Cargo.toml              workspace root (members = engine, games/sandbox, games/mushroom_man,
                          games/walkaround_demo, games/physics-sandbox, tools/oxc_scripts)
  .cargo/config.toml       env vars needed to build vendored SDL2 with a recent CMake/GCC
  new_game.ps1 / .sh       scaffold a new game from the sandbox template
  package.ps1 / .sh        release build + dist/<game>/ folder
  docs/                    ROADMAP.md (status), KNOWN-ISSUES.md, examples/input_map.ron, ...
  engine/                  the reusable library — everything below lives here
    src/
      app/                 Game trait + App::run main loop
      platform/            SDL2 window + GL context setup
      input.rs, time.rs    keyboard/mouse/gamepad state, delta-time clock
      input_map.rs         named actions bound to devices in RON, rebinding, contexts
      camera.rs            OrbitCamera, FirstPersonCamera and camera FX
      ecs/                 hecs::World + Transform/MeshRenderer/Camera/Light/LevelObjectMeta/PlayerController components
      resources.rs         typed singleton map (Resources)
      schedule.rs          named systems with stages, before/after and run conditions (Schedule)
      effects.rs           Lifetime / Fade / TextureOverride components + tick
      mesh/, texture.rs    OBJ/glTF loading, GPU upload, primitives
      shader/, renderer/   GLSL compile/cache, low-res offscreen FBO + composite dither/posterize pass
      profile.rs           ShaderProfile/RenderParams + RON save/load/cycle
      level.rs             Level/LevelObject + RON save/load (the legacy F2 editor's data model)
      physics/             hand-rolled RigidBody/Collider simulation
      audio/               AudioEngine: SFX, music, named loop tracks (rodio natively, SDL2 mixer on web)
      tunables.rs          named constants in RON, live reload (Tunables)
      telemetry.rs         CSV event log hook (Telemetry)
      headless.rs          window-less test harness (Headless, EditorHeadless)
      skeletal/            skinned animation: skeleton, clips, state machine, CPU skinning, glTF import
      rig.rs, animation.rs older rigid-part rigs and orbit/bob motion (unchanged)
      editor/              docked editor core: shell, hierarchy, inspector, gizmos, scenes, prefabs, runtime
      ui/, ui_designer/    egui bridge, profiler/HUD widgets; UI document designer
  games/
    sandbox/                the demo game — also your template for new games
      src/main.rs
      assets/shaders/        mesh.vert, mesh.frag, post_composite.frag
      assets/models/, assets/textures/
      profiles/              *.ron shader profiles (auto-generated on first run)
      levels/                *.ron levels (auto-generated on first run)
    mushroom_man/, walkaround_demo/   larger demos built from the same template
    physics-sandbox/         small particle/physics toy; the reference host for EditorRuntime
  tools/
    oxc_scripts/             workspace member
```

## Quick start

```
cd dust-engine
cargo run -p sandbox
```

The `.cargo/config.toml` already sets the env vars this machine needs to build
vendored SDL2 (an old CMake-version pin plus a C-standard flag for the GCC
toolchain) — you don't need to set anything yourself. The first build compiles
SDL2 from source and is slow (a minute or two); subsequent builds are fast.

**Controls** (Edit mode): left-drag orbits the camera, **WASD** flies the
orbit target around on the horizontal plane (relative to the direction
you're looking), **Space**/**Shift** raise/lower it, scroll wheel zooms,
**F1** toggles the shader-profile panel, **F2** toggles the level editor
panel, **Tab** cycles shader profiles, **F5** saves the current profile,
**F3** enters Play mode (see "Running the game" below), **Escape** quits.
**F4** opens the docked editor instead (viewport, hierarchy, inspector,
gizmos, scenes and prefabs — see "The docked editor" below). Release builds
have no editor at all and boot straight into Play.

> **Key note.** F1/F2/Tab/F5/F3 are the *legacy* panels (the sandbox's own
> egui code in `main.rs`); F4 is the engine's `EditorRuntime`. The dock key is
> chosen by each game's `handle_event`, not by the engine: at this commit it
> is F4 in `sandbox`, `mushroom_man` and `walkaround_demo`, and F1 in
> `physics-sandbox` (which has no legacy panels). A change that reworks which
> of F1/F2/F4 open the dock in the games is in flight; check the game's
> `handle_event` if a key here does not match.

## The `Game` trait and the main loop

Every game implements `dust_engine::app::Game`:

```rust
trait Game {
    fn init(&mut self, ctx: &mut Context) -> anyhow::Result<()> { Ok(()) }
    fn handle_event(&mut self, ctx: &mut Context, event: &sdl2::event::Event) {}
    fn update(&mut self, ctx: &mut Context, dt: f32) -> anyhow::Result<()>;
    fn render(&mut self, ctx: &mut Context) -> anyhow::Result<()>;
}
```

`App::run("Title", width, height, YourGame::new())` owns the window, event
pump, and timing, and calls into your `Game` impl each frame in this order:
drain SDL events (`handle_event` per event) → `update(dt)` → `render()` →
swap buffers. `Context` gives you `ctx.gl()` (the `glow::Context`), `ctx.input`
(keyboard/mouse state), `ctx.time` (delta/elapsed), and `ctx.drawable_size()`.
Set `ctx.should_quit = true` to exit.

See `games/sandbox/src/main.rs`'s `Sandbox` struct for a complete example.

## Scene basics: ECS

The engine uses `hecs` (re-exported as `dust_engine::ecs`). A `World` holds
entities; each entity is a bundle of components. The two you'll use constantly:

```rust
world.spawn((
    Transform::from_position(Vec3::new(0.0, 0.0, 0.0)),
    MeshRenderer { mesh: Arc::new(gpu_mesh), texture: Some(Arc::new(gpu_texture)) },
));
```

- `Transform { position, rotation, scale }` — `.matrix()` gives the model matrix.
- `MeshRenderer { mesh: Arc<GpuMesh>, texture: Option<Arc<GpuTexture>> }` — `Arc`
  so one loaded mesh/texture can back many entities without re-uploading.
- `Camera` exists as a data shape for future camera work driven off the ECS;
  the sandbox currently drives the camera via a standalone `OrbitCamera`
  (simpler for a single-camera game) rather than an ECS-driven one.
- `Light { color, intensity, kind: LightKind::{Directional{direction},
  Point{range}} }` — the directional sun/ambient light is still a hardcoded
  uniform, but `Point` lights are real, placeable ECS entities; see "Point
  lights" below.
- `LevelObjectMeta { name, mesh_source, texture_path, rotation_euler_deg }` —
  tags an entity as a level-editor-placed object and carries enough info to
  serialize it back into a `LevelObject` (see "Level editor" below). Only
  entities you want listed/editable/saved in the level editor need this;
  plain `Transform`/`MeshRenderer` entities work fine without it.
- `PlayerController { move_speed, jump_speed, interact_radius,
  interact_impulse }` — tags the player entity spawned in Play mode (see
  "Physics" and "Running the game" below). Plain tunable fields.

Load meshes/textures with `dust_engine::mesh::load_obj(path)` (returns one
`MeshData` per OBJ sub-object) and `dust_engine::mesh::GpuMesh::upload(gl, &data)`,
and `dust_engine::texture::GpuTexture::load_from_file(gl, path, TextureFilter::Nearest)`.

Draw by querying: `world.query::<(&Transform, &MeshRenderer)>().iter()`.

## Camera

`dust_engine::camera::OrbitCamera::new(target, distance)` — orbits a point via
`.orbit(delta_yaw, delta_pitch)` and `.zoom(delta)`; `.view_matrix()` and
`.projection_matrix(aspect)` feed straight into your shader uniforms.

## The render pipeline

Each frame: `Renderer::begin_scene` binds a **low-resolution offscreen
framebuffer** (sized `window_size * resolution_scale`) and draws your scene
into it — this is where the vertex shader applies lighting/fog/vertex-snapping
and where the compiled shader variant (affine vs. perspective-correct UVs) is
selected. `Renderer::present` then blits that small texture up to the full
window with `GL_NEAREST` filtering (the actual pixelation) through a composite
shader that layers on posterize + ordered-dither. The editor GUI is drawn
*after* `present`, directly at full window resolution, so the UI itself always
stays crisp regardless of how retro the 3D looks.

### `RenderParams` fields

| Field | Effect |
|---|---|
| `resolution_scale` | Internal render resolution as a fraction of the window (lower = blockier) |
| `fog_color` / `fog_start` / `fog_end` | Linear distance fog blended in per-vertex |
| `color_levels` | Posterize steps per color channel in the composite pass |
| `dither_strength` | Ordered-dither blended into the posterize step (breaks up banding) |
| `lighting_mode` | `Unlit` or `VertexLit` (Lambertian, computed per-vertex — flat "faceted" look) |
| `ambient_color` | Added to lit surfaces so shadowed faces aren't pure black |
| `light_dir` | Direction *toward* the directional "sun" light used by `VertexLit` shading (not required to be normalized — the shader normalizes it) |
| `vertex_snap_amount` | Clip-space grid size vertices are quantized to (0 = off); PS1-style geometric wobble |
| `affine_texture_mapping` | Toggles `noperspective` UV interpolation — the classic warped-texture look |
| `texture_filter` | `Nearest` (crisp/blocky) or `Bilinear` (smoothed) |
| `backface_culling` | Skips drawing a triangle's interior-facing side — see below |

**`backface_culling`**, when on, wraps just the opaque mesh loop in
`Sandbox::render` with `gl.enable(CULL_FACE)`/`gl.cull_face(BACK)` (off
again before particles draw — skybox and particles are unaffected either
way). For a closed mesh this changes nothing visible from a normal
distance — the front face already occludes the back one via the depth
buffer — but it's what keeps the camera from rendering a wall's or
object's *interior* surface when it clips very close to or slightly
inside solid geometry (the simple collision resolver in `dust_engine::physics`
doesn't guarantee zero interpenetration, so this happens in practice).
`#[serde(default)]` (`false`) so profiles saved before this field existed
keep their exact old look; the checkbox lives in the F1 panel's
**Geometry / Texturing** section, right under "Affine texture mapping".

## Shadows

Opt-in directional shadow map for the mesh shader (a single sun-style light).
**Off by default**: a game that never calls `set_shadow_settings` keeps
byte-identical shaders and allocates nothing. Rendering is a depth pass from
the light into a square map, then a lookup in the mesh fragment shader that
darkens the final colour by `1 - strength * facing * (1 - lit)`.

```rust
use dust_engine::renderer::{patch_mesh_shaders, ShadowBounds, ShadowFilter, ShadowSettings};

// 1. Setup: patch YOUR mesh shaders in memory (files on disk are untouched).
//    Contract: vert declares `uModel`, `aPos` (loc 0), `aNormal`; both have
//    `void main()`; frag writes the colour output named by the 3rd argument.
let (vert, frag) = patch_mesh_shaders(&vert_src, &frag_src, "FragColor")?; // -> anyhow::Result<(String, String)>
program = link_program(gl, &vert, &frag)?;
renderer.set_shadow_settings(gl, ShadowSettings::smooth()); // or ::ps2() / struct literal

// 2. Each frame, BEFORE begin_scene: shadow pass over the casters.
if renderer.shadows_active() {
    let bounds = ShadowBounds::camera(view, proj, renderer.shadow_settings());
    if renderer.begin_shadow_pass(gl, sun_toward_light, &bounds) { // -> bool
        for (model, mesh) in casters { renderer.shadow_draw(gl, &model, mesh); }
        renderer.end_shadow_pass(gl);
    }
}
renderer.begin_scene(gl);
// 3. Scene draw: set your uniforms with the SAME light vector, then
renderer.bind_shadows(gl, program); // uploads uniforms, binds the map to unit 7, makes `program` current
```

* `light_dir` points *toward* the light. Pass the same vector to your
  shader's light uniform so lit side and shadow side agree.
* `bind_shadows` sets `uShadowStrength = 0` when shadows are off or no pass
  ran, so it is always safe to call. It uses texture unit
  `renderer::SHADOW_TEXTURE_UNIT` (7); don't leave your own texture there.
* `ShadowBounds`: `Frustum` via `ShadowBounds::camera(view, proj, &settings)`
  (fits the visible part of the camera frustum, up to `max_distance`),
  `Aabb { min, max }` or `Sphere { center, radius }` for a fixed region.
* The shadow pass is separate from `RenderParams`/the vertex-snap look: the
  map is depth-only and drawn at its own size; the result is then affected by
  the low-res framebuffer and composite like everything else.

`ShadowSettings` (`#[serde(default)]`, also stored as `RenderParams.shadows`,
so old profile files load with shadows off):

| Field | Default | Effect |
|---|---|---|
| `enabled` | `false` | Master switch; `false` frees the map |
| `resolution` | 1024 | Map side in texels (clamped 64..=4096) |
| `bias` | 0.75 | Constant depth bias in map texels |
| `normal_bias` | 1.0 | Receiver pushed along its normal, in texels |
| `strength` | 0.55 | 0 = invisible, 1 = black umbra |
| `softness` | 1.0 | PCF radius in texels (0 = one tap, capped at 2) |
| `filter` | `Pcf` | `Pcf` soft, `Hard` one tap, `Dithered` Bayer-stippled (PS2 look; pair with a low `resolution`) |
| `max_distance` | 40.0 | Frustum fit: how far from the eye is covered |
| `caster_extend` | 30.0 | Frustum fit: how far toward the light casters are still included |
| `texel_snap` | `true` | Snap the map to the texel grid so edges don't shimmer when the camera moves |

Presets: `ShadowSettings::smooth()` (2048, PCF, softness 1.5) and
`ShadowSettings::ps2()` (256, `Dithered`, strength 0.6). The F1 render panel
has a **Shadows** section for all of the above. Demo:
`DUST_SHADOWS=1 cargo run -p physics-sandbox` (or `DUST_SHADOWS=ps2`) adds
five static cubes and draws the ground as a receiver.

Limits: one directional light, no cascades (a single map fitted to the
frustum, so large `max_distance` softens detail), no point-light shadows, only
meshes you submit with `shadow_draw` cast, and only programs built from
`patch_mesh_shaders` receive.

## Hi-fi rendering (opt-in)

For a clean modern look instead of the PS2 one, the engine ships a second
render path. Nothing changes for a game that does not opt in: the default
scene target is still RGBA8, single-sampled and `GL_NEAREST`.

```rust
use dust_engine::renderer::{patch_mesh_shaders, Renderer, ShadowSettings, TargetOptions};
use dust_engine::shader::builtin::{PbrMaterial, PointLight, SceneLighting};
use dust_engine::shader::{ShaderVariantCache, PBR_MESH_FRAG, PBR_MESH_VERT, TONEMAP_COMPOSITE_FRAG};

let mut renderer = Renderer::new(gl, size, 1.0, TONEMAP_COMPOSITE_FRAG)?;
renderer.set_target_options(gl, TargetOptions::hifi())?; // HDR + 4x MSAA + linear
renderer.set_shadow_settings(gl, ShadowSettings::smooth());
renderer.set_exposure(1.0);
let (v, f) = patch_mesh_shaders(PBR_MESH_VERT, PBR_MESH_FRAG, "FragColor")?;
let mut pbr = ShaderVariantCache::new(v, f);

// each frame, after the shadow pass and begin_scene:
let program = pbr.get_or_compile(gl, 0)?;
renderer.bind_shadows(gl, program);
lighting.upload(gl, program, camera_pos);        // SceneLighting
// set uView / uProj, then per draw: uModel + PbrMaterial::upload
```

* **`TargetOptions`** (`renderer::framebuffer`): `hdr` (RGBA16F, falls back
  to RGBA8 with a warning), `samples` (MSAA, resolved in `present`), `linear`
  (smooth upscale). `retro()` is the default, `hifi()` turns all three on.
  `Renderer::set_target_options` recreates the target; `scene_target()`
  reports what the driver actually gave (`is_hdr`, `is_multisampled`).
* **`PBR_MESH_VERT` / `PBR_MESH_FRAG`**: per-pixel GGX + Lambert with
  roughness/metallic, sRGB base texture, optional tangent-space normal map
  (no tangents needed, frame from derivatives), optional glTF metal/rough
  map, emissive, sky/ground hemisphere ambient with a matching cheap
  specular, one sun, up to 8 point lights (`MAX_POINT_LIGHTS`), linear fog.
  Same attribute layout as every Dust mesh, `AFFINE_UV` and `INSTANCED`
  variants, and it passes the shadow patcher's contract.
* **`TONEMAP_COMPOSITE_FRAG`**: exposure (`Renderer::set_exposure`, uniform
  `uExposure`), ACES filmic curve, sRGB encode, the damage-flash tint, and the
  usual posterize/dither uniforms (off at `PostParams::default()`).
* **Primitives**: `mesh::primitives::uv_sphere` and `cylinder` join `cube`,
  `plane` and `quad`.

Demo: `cargo run -p dust_engine --example hifi_showcase` (R toggles the
retro target). `DUST_SCREENSHOT=out.png` saves the third frame and exits,
which also works headless under `xvfb-run`.

Limits: the sun's shadow darkens the whole fragment (ambient included);
there is no image-based lighting or bloom yet; tracers (`set_trails`) blend
through RGBA8 targets, so they clip HDR highlights.

## Shader profiles

A `ShaderProfile` (in `dust_engine::profile`) bundles shader file paths + a
`RenderParams`. They're saved as human-editable `.ron` files under a game's
`profiles/` folder — open one in a text editor and change numbers directly if
you don't want to use the GUI. On first run, if a game's `profiles/` folder is
empty, `games/sandbox/src/main.rs`'s `default_demo_profiles()` writes out three
starter looks (`ps2_classic`, `ps1_wobble`, `clean_lowpoly`) so there's always
something to load and cycle through — copy that pattern (or just copy the
generated `.ron` files) into a new game.

`dust_engine::profile::ProfileCycler` manages a loaded set: `.next()`/`.prev()` for
hotkey cycling, `.add()` for a runtime "save as new profile", `.select(i)` for
GUI list clicks. `load_dir`/`load_from_file`/`save_to_file` handle the RON I/O.

## The editor GUI

> This section describes the **legacy** F1/F2 panels that live in each game's
> `main.rs`. The engine also ships a full docked editor (`EditorRuntime`,
> described in "The docked editor" below); the legacy panels are not the only
> editor, and new games should host the docked one.

`dust_engine::ui::EguiState` bridges SDL2 input to `egui` and paints it via
`egui_glow` — any game gets this by owning one `EguiState`, forwarding every
SDL event to `handle_event`, and wrapping its panel code in `.run(...)` /
`.paint(...)` once per frame (see `Sandbox::render` for the exact pattern,
including gating camera-drag and hotkeys on `ui.wants_pointer_input()` /
`ui.wants_keyboard_input()` so interacting with either panel doesn't also spin
the camera or trigger Tab/F5). `dust_engine::ui::render_params_editor(ui, &mut
params)` draws sliders/checkboxes for every `RenderParams` field and is
reusable as-is in any game.

The sandbox shows two independent panels, each toggled by its own hotkey:
a right-hand **shader profile panel** (F1 — profile list, Save/Save-As-New)
and a left-hand **level editor panel** (F2 — described next).

## Level editor

A `Level` (in `dust_engine::level`) is a named list of `LevelObject`s — name, mesh
source, optional texture, and a position/rotation/scale — saved as a
human-editable `.ron` file under a game's `levels/` folder, the same
auto-bootstrap-if-empty convention as shader profiles: on first run
`games/sandbox/src/main.rs`'s `default_level()` writes the classic 3-cube demo
scene to `levels/default.ron` so there's always something to load.

A `LevelObject`'s mesh is either a built-in **primitive** (`Cube`/`Plane`,
generated procedurally by `dust_engine::mesh::primitives` and uploaded once, then
shared via `Arc<GpuMesh>` across every instance — scale is applied through the
`Transform`, not by regenerating geometry) or an **`ObjFile`** path (only the
first sub-mesh of a multi-object OBJ is used per level object — author
multi-part models as separate level objects instead). Texture/OBJ paths are
stored relative to the game's asset root when possible
(`dust_engine::level::relativize`), the same portability fix applied to shader
profile paths, so saved levels don't break when the game is shipped to another
machine.

The **F2 panel** (`Sandbox::draw_level_editor_ui`/`draw_selected_object_ui` in
`games/sandbox/src/main.rs`) gives you:

- **Save** / **Save As New** / a clickable list of saved levels to load —
  same pattern as the shader-profile panel.
- **Add Cube** / **Add Plane** — spawns a primitive at a simple
  non-overlapping default position, auto-selected.
- **Import Model as Object...** — opens a native file picker (`rfd`) for an
  `.obj`, then a second picker for an optional texture (a solid white 1×1
  fallback texture is used if you cancel), and adds it as a **new** object
  — unlike early versions of this button, it no longer clears the scene.
- A **scene outliner** (every entity with a `LevelObjectMeta`) — click a name
  to select it.
- For the selected object: an editable name, **Position**/**Rotation**/
  **Scale** as drag-value fields (click and drag left/right to change, or
  click once to type an exact number), **Assign Texture...**, and **Delete**.
  Rotation is edited as Euler degrees and only converted to the `Transform`'s
  quaternion on change (`dust_engine::glam`'s `Quat::from_euler`), so repeated
  edits don't drift the value the way a naive Euler↔quaternion round-trip
  would.

The F2 panel has no 3D transform gizmo or click-to-select-in-viewport — it
is deliberately a *simple* editor (the docked editor, F4, has both). Vertex/face-level mesh editing is also out of
scope; "editing geometry" here means placing/transforming whole objects built
from primitives or imported meshes.

Each object also has a **Dynamic Rigid Body** checkbox (see "Physics" below)
— checking it inserts a `RigidBody` on the live entity so it falls/collides
in Play mode; unchecking it removes the component, making the object static
again. New primitives default sensibly (`Cube` → dynamic, `Plane`/imported →
static) but every object is overridable.

## Undo/redo

The F2 editor keeps `undo_stack`/`redo_stack: Vec<Level>` on `Sandbox` (capped
at 50 entries), reusing `build_level_from_ecs`/`apply_level`'s existing exact
round-trip as the snapshot/restore mechanism — no separate diff/patch format.
`push_undo_snapshot` is called before every discrete edit (an "Add X" button,
Delete, Duplicate) and whenever the selected entity changes to a *different*
one than last frame, so a slider-drag session on one object collapses into a
single undo step instead of one per frame. **Ctrl+Z** pops `undo_stack`,
pushes the current state to `redo_stack`, and re-applies the popped level;
**Ctrl+Shift+Z** mirrors this for redo. Selection isn't part of `Level`, so
undo/redo deselects afterward — a known, accepted simplification. `apply_level`
only restarts/stops music if `level.music_path` actually changed, specifically
so undo/redo doesn't glitch music that was already playing correctly.

## Multi-select and duplicate

Scoped to **Scene Objects and Characters** (not rigs/classes — a rig instance
is a multi-entity hierarchy, out of scope for this pass). `Sandbox` gains
`selected_entities`/`selected_characters: HashSet<Entity>` alongside the
existing singular `selected_entity`/`selected_character` (which stay "which
one's detail panel is shown," always the most recently clicked). A plain
click on an outliner row clears the set and selects just that entity — the
same behavior as before this feature existed; **Ctrl+click** toggles
membership instead. **Ctrl+D** duplicates every entity in the active
selection set (nudging position, appending "Copy" to the name, and selecting
the new copies); **Delete** removes every entity in whichever set is
non-empty. Both push an undo snapshot first.

## In-game profiler overlay

`dust_engine::ui::draw_profiler_overlay` draws a small always-on `egui::Area`
anchored top-right (the HUD lives top-left, so they never overlap): FPS and
frame time (averaged over the last ~30 frames via `Sandbox.frame_times: VecDeque<f32>`),
live entity count (`hecs::World::len()`), and a draw-call counter incremented
once per mesh draw in `Sandbox::render`'s opaque-mesh loop. Available in both
Edit and Play mode — it's a dev tool, not gameplay — and toggled off via a
"Show Profiler" checkbox in the F1 panel (`profiler_visible: bool`, defaults
`true`).

## Hot reload on file change

`dust_engine::hotreload::HotReloadWatcher` wraps a `notify::RecommendedWatcher`
watching a game's asset root recursively over a non-blocking channel;
`poll_events()` drains and de-duplicates same-path events from the same save
(editors often write a file 2-3 times per save). `Sandbox.hot_reload:
Option<HotReloadWatcher>` is built in `init` and degrades silently to `None`
if the watcher fails to start, the same convention as `audio: Option<AudioContext>`.
**Scoped to shaders, scripts, and textures** — not level/profile RON, since
those are edited live in the F2 panel and watching them risks fighting an
in-progress edit or the undo stack. Each frame, a changed path is matched
against: the active shader profile's three shader files (re-applies the
profile), or
any live `MeshRenderer`'s texture path (reloads and swaps the GPU texture in
place). Edit a shader or script in your text editor while the game is
running and the change appears within a frame or two, no manual F2 "Reload"
click needed.

## Asset browser

A drop-in replacement for the native file picker at every **texture** picker
call site (`Assign Texture...`, the model importer's texture step, class
texture assignment) — the only asset kind where a thumbnail is cheap and
worth showing; model/audio/script pickers keep the plain `rfd::FileDialog`.
`dust_engine::ui::AssetBrowserState` recursively scans a root folder for
`png`/`jpg`/`jpeg` files and lazily uploads each thumbnail the first time
it's visible. Texture registration with `egui_glow`'s painter has to happen
*outside* the painter's own closure (it's exclusively borrowed during
`EguiState::run`), so thumbnail loading is split in two:
`ensure_thumbnails_loaded` (needs `&mut EguiState`, called before the egui
frame) and `show` (read-only, called inside it). Since egui is immediate-mode
and a pick doesn't resolve until a later frame, each call site carries a
small `pending_texture_pick: Option<PendingTexturePick>` across frames to
remember "who asked" until the browser returns a clicked path.

## glTF import

`dust_engine::mesh::load_gltf` (mirrors the existing OBJ loader) reads a `.gltf`/
`.glb` file's node tree, requiring the `TRIANGLES` primitive mode per mesh
(other modes are skipped with a warning, same as OBJ's discarded materials)
and mapping POSITION/NORMAL/TEXCOORD_0 onto `dust_engine::mesh::Vertex` (missing
attributes default the same way OBJ's loader does). Embedded and
external images are decoded to RGBA8 and normalized from whatever glTF pixel
format they were authored in. **No vertex skinning and no animation-channel
import** — the rig system is rigid-part hierarchy only, so glTF import is
static-pose geometry, explicitly logged as a heads-up rather than silently
dropped.

Two importer entry points in the F2 panel: **"Import Model as Object..."**
now also accepts `.gltf`/`.glb` (taking the first mesh-carrying node, same
flow as OBJ import — `MeshSource::GltfFile`), and a new **"Import glTF as
Rig..."** button under "Rigs" for multi-node files, which builds a
`RigAsset` with one `RigPartDef` per mesh node (parented by the node's own
glTF parent) and saves it into `rigs/` — immediately usable through the
existing "Add instance of" flow, no new placement UI needed.

## Point lights

Beyond the single hardcoded directional light (sun + ambient), the engine
supports up to **4 point lights** per scene, placed and edited right in the
level editor. A `LevelLight` (in `dust_engine::level`) is a name, position, color,
intensity, and range, saved as part of a `Level`'s `lights` list — same RON
file, same portability rules as everything else in the level.

A placed light is an entity like any other: `Transform` + `Light` +
`LevelObjectMeta` (`Sandbox::spawn_level_light`), but with no `MeshRenderer` —
lights are invisible in both Edit and Play mode, there's no gizmo/billboard
marking their position (a known simplification; move the object and watch the
lighting change if you need to find it). Because it carries `LevelObjectMeta`
like a mesh object does, a light shows up in the **same F2 scene outliner**
as everything else and can be renamed/repositioned/deleted the same way;
`draw_selected_object_ui` checks for a `Light` component and, if present,
dispatches to `draw_selected_light_ui` instead of the mesh-object fields.

**F2 panel**: **Add Light** (next to Add Cube/Add Plane) drops a white,
intensity-1.0, range-5.0 light at the origin, auto-selected. The selected-light
panel gives you Position, a color picker, an Intensity slider (0–5), and a
Range slider (0.5–30).

Rendering: `mesh.vert`/`mesh.frag` declare fixed-size arrays —
`uPointLightPos[4]`, `uPointLightColor[4]`, `uPointLightIntensity[4]`,
`uPointLightRange[4]`, `uPointLightCount` — a plain uniform array rather than
a UBO/SSBO, which is all a handful of level lights need. Each frame,
`Sandbox::render` collects up to 4 `(Transform, Light)` entities (extra
lights past 4 are silently ignored — keep scenes within the budget) and
uploads them; the vertex shader sums each light's contribution into the
existing per-vertex Lambertian term using simple linear-falloff attenuation
(`clamp(1 - distance/range, 0, 1)`), so point lights only affect
`VertexLit`-mode surfaces (same as the directional light — `Unlit` mode
ignores all lighting).

`build_level_from_ecs` gathers lights back out with a
`.without::<&Light>()` filter on the object query (so a light entity, which
also carries `LevelObjectMeta`, doesn't get double-counted as a mesh object)
and a separate `(&Transform, &LevelObjectMeta, &Light)` query for the
`lights` list.

## Physics

`dust_engine::physics` is a small, readable simulation, not a general solver: translation-only bodies, four collider shapes, sweep/ray/overlap queries, simple joints, a character controller. The "Physics audit" table in `docs/KNOWN-ISSUES.md` lists what exists, what is tested and what is missing (no convex meshes, no angular dynamics, no CCD for bodies, no kinematic body type, no sleeping islands). The examples below are compiled and run by `physics::guide_examples` in `engine/src/physics/mod.rs`.

### Bodies and colliders

```rust
pub struct PhysicsParams { pub gravity: f32, pub linear_damping: f32 }   // default 9.8, 0.15
pub struct RigidBody { pub velocity: Vec3, pub grounded: bool }
pub enum ColliderShape {
    Sphere { radius: f32 },
    Aabb { half_extents: Vec3 },                      // axis-aligned, rotation ignored
    Capsule { radius: f32, half_height: f32 },        // local +Y
    Box { half_extents: Vec3 },                       // oriented by Transform::rotation
}
pub struct Collider { pub shape: ColliderShape, pub is_trigger: bool }
// Collider::new / ::trigger(shape) / ::sphere(r) / ::capsule(r, hh) / ::aabb(he) / ::oriented_box(he)
```

A `Collider` with no `RigidBody` is **static**; with one it is dynamic. Optional components: `CollisionLayers { layer, mask }` (two entities collide only if each one's mask contains the other's layer; default layer 0, mask all) and `PhysicsMaterial { friction, restitution }` (pair values combine as `sqrt(a*b)` and `max`; an entity without one counts as frictionless).

```rust
let mut world = hecs::World::new();
let floor = world.spawn((Transform::from_position(Vec3::new(0.0, -0.5, 0.0)),
                         Collider::aabb(Vec3::new(20.0, 0.5, 20.0))));
let ball = world.spawn((Transform::from_position(Vec3::new(0.0, 3.0, 0.0)),
                        Collider::sphere(0.5), RigidBody::default(),
                        PhysicsMaterial { friction: 0.4, restitution: 0.3 }));
let sensor = world.spawn((Transform::from_position(Vec3::new(0.0, 0.5, 0.0)),
                          Collider::trigger(ColliderShape::Aabb { half_extents: Vec3::splat(1.0) })));
```

### Stepping

Three ways, from simplest to richest:

* `physics::step(&mut world, dt, &params) -> Vec<(Entity, Entity)>`: stateless, one pass per call, returns the (dynamic, other) trigger overlaps. No sleeping.
* `PhysicsWorld::new(params)` then `pw.update(&mut world, frame_dt) -> Vec<PhysicsEvent>`: fixed 1/60 s timestep with an accumulator (at most `max_substeps` = 8 per call), sleeping (`SleepConfig`), joints, and native Enter/Stay/Exit events. `pw.step_fixed(&mut world)` runs exactly one step; `pw.is_sleeping(e)`, `pw.interpolated_position(&world, e)` and `pw.alpha()` help rendering.
* Schedule/Resources: `schedule.add_system("physics", physics::physics_system)` (stage it as you like). It reads the `PhysicsParams` resource, uses a `PhysicsWorld` resource if you inserted one (otherwise the stateless `step`), and fills a `TriggerEvents` resource every run. `Headless::with_default_systems` already does this.

```rust
let mut pw = PhysicsWorld::new(PhysicsParams::default());
for ev in pw.update(&mut world, 1.0 / 60.0) {
    if ev.kind == TriggerEventKind::Enter { /* ev.entity (dynamic) entered ev.other */ }
}
// or, from any system after physics_system ran:
// for ev in ctx.res::<TriggerEvents>().entered() { ... }   // also .stayed(), .exited(), .involving(e)
```

Events are replaced every run, ordered by `(entity, other)` with all Enter/Stay before all Exit.

### Queries

`PhysicsQueries` is implemented for `hecs::World`; import it with `use dust_engine::physics::PhysicsQueries`. Every query honours layers (`layer_mask: u32`, `u32::MAX` = everything); triggers are skipped unless you use a `QueryFilter` with `.with_triggers()`.

```rust
let hit: Option<Hit> = world.raycast(origin, Vec3::NEG_Y, 100.0, u32::MAX);   // nearest
let all: Vec<Hit> = world.raycast_all(origin, Vec3::NEG_Y, 100.0,
        &QueryFilter::default().excluding(ball).with_triggers());               // nearest first
let sweep = world.box_sweep(center, Vec3::splat(0.5), Quat::IDENTITY, Vec3::NEG_Y, 10.0, u32::MAX);
// also: raycast_filtered, shape_cast, spherecast, capsule_sweep
let near: Vec<Entity> = world.overlap_sphere(center, 2.0, u32::MAX);           // also overlap_box, overlap_capsule, overlap_shape
```

`Hit { entity, point, normal, distance, started_inside }`: for rays `distance` is along the unit direction; for shape casts it is how far the shape's centre travelled before touching. `normal` faces back toward the query. Ties break on the lowest entity id, so results are deterministic.

### Joints

A `Joint` is a component on its own entity. Constructors: `distance`, `rope`, `pin`, `spring` (and `*_to_point` variants that attach to a fixed world point), plus `.with_anchors(local_a, local_b)` and `.with_collide_connected(bool)`. Joints are solved inside `PhysicsWorld` steps and inside the stateless `step`. Jointed bodies do not sleep, and jointed pairs do not collide unless asked to.

```rust
let anchor = Vec3::new(5.0, 6.0, 0.0);
let bob = world.spawn((Transform::from_position(anchor + Vec3::X * 2.0),
                       Collider::sphere(0.25), RigidBody::default()));
world.spawn((Joint::distance_to_point(bob, anchor, 2.0),));      // pendulum
// Joint::spring(a, b, rest_length, stiffness, damping), Joint::rope(a, b, max_length), Joint::pin(a, b)
```

Bodies have no angular state, so a joint constrains positions only (a pin is a point constraint, not a hinge).

### Character controller

```rust
let mut cc = CharacterController::default();   // radius .35, half_height .5, step .35, slope 45 deg
let r: MoveResult = cc.move_and_slide(&mut world, player, Vec3::new(0.0, -5.0, 0.0)).unwrap();
// r.position, r.grounded, r.ground_normal, r.blocked, r.stepped, r.last_hit
```

Call it once per fixed step with the displacement you want (velocity * dt, gravity included). It moves the entity's `Transform` directly and the entity should not have a `RigidBody`. It slides along walls, steps up ledges, refuses slopes steeper than `max_slope_deg`, and stops at ceilings. It does not push dynamic bodies.

### Debug drawing

`PhysicsDebug::build(&world, &PhysicsDebugOptions::default(), |e| pw.is_sleeping(e))` returns wireframe `lines` (colour by static / dynamic / sleeping / trigger) for any line renderer.

`spawn_level_object` in `games/sandbox/src/main.rs` derives each object's `Collider` from its `Transform::scale`. `PhysicsParams` lives on the loaded `Level` (`Level::physics`), so gravity and damping save, load and revert with the level.

## Audio

`dust_engine::audio::AudioContext` wraps `rodio` (with its `symphonia-all`
feature, which already decodes WAV/OGG/MP3/FLAC — no extra dependency
needed) behind a small, purpose-specific API:

```rust
// AudioContext = AudioEngine<PlatformBackend>: rodio natively, the SDL2 mixer on emscripten
pub fn new() -> anyhow::Result<Self>;                       // fails if no output device
pub fn play_sfx_file(&self, path: &Path) -> anyhow::Result<()>;   // one-shot, fire-and-forget
pub fn play_tone(&self, frequency_hz: f32, duration_secs: f32);   // procedural sine blip, no file
pub fn play_music_file(&mut self, path: &Path, looped: bool) -> anyhow::Result<()>;
pub fn stop_music(&mut self);
pub fn set_music_volume(&mut self, volume: f32);
```

`play_tone` needs no asset at all (a `SineWave` source built on the fly) —
handy while prototyping a jump/interact/UI sound before a real asset
exists, and what the jump/push demos still use. `play_sfx_file`/
`play_music_file` decode a real file via `rodio::Decoder`; looped music is
fully decoded into an in-memory `SamplesBuffer` and repeated (fine for
typical track lengths, not streamed). `Sandbox.audio: Option<AudioContext>`
degrades silently if `AudioContext::new()` fails (no output device) —
every call site checks `Some` first, so a game runs fine, just silent, on
a machine with no audio hardware.

**Scripts and native `Behavior`s** reach SFX through the same `ScriptApi`
trait as everything else: `play_sound(path)` (an `.ox` script call) /
`ScriptApi::play_sfx(&mut self, path: &str)` (native), resolved relative to
the asset root the same way script/texture paths are. See
`bob_demo.ox`'s `on_interact()`, which calls both `play_tone` and
`play_sound("sfx/demo_blip.wav")` side by side — procedural and
sample-based, from the same script.

**Background music is level-wide**, not per-object: `Level.music_path:
Option<PathBuf>` (`#[serde(default)]`). `apply_level` starts it looping
whenever a level loads (or calls `stop_music()` if `None`, so switching to
a level without music doesn't leak the previous track), and
`Sandbox.current_music_path` mirrors it so `build_level_from_ecs` writes
it back out. **F2 panel**: the **Level** section gains "Assign Music..."
(same `rfd` file-picker pattern as "Assign Texture..."), "Stop Music", and
a volume slider — `Sandbox.music_volume` persists across level loads since
`play_music_file` itself has no volume parameter.

**Demo content**: no audio assets shipped in the repo, and pulling in an
encoder crate just for placeholder sounds isn't worth a new dependency —
`games/sandbox/src/main.rs`'s `write_wav`/`generate_blip_samples`/
`generate_ambient_samples` hand-roll a minimal mono 16-bit PCM WAV file
instead (a short faded sine blip, and a three-note chord sized to loop
with no click — every frequency completes a whole number of cycles within
the loop duration). Generated once into `sfx/demo_blip.wav`/
`music/demo_ambient.wav` if missing, same bootstrap-if-empty convention as
profiles/rigs/classes/levels.

### Named tracks (buses)

Besides the single `play_music_file` slot, an `AudioContext` holds any number
of named **looping tracks** with their own gain, fades and mute. They share
one loop clock, so equal-length loops added with the same offset stay in time
(adaptive music: add the layers silent, fade them in when the game state
changes). The same API runs natively and on the web backend.

```rust
use std::path::Path;

use dust_engine::audio::AudioContext;

fn start_music(drums: Vec<f32>, pad: Vec<f32>) -> anyhow::Result<AudioContext> {
    let mut audio = AudioContext::new()?; // Err if there is no output device
    // Tracks join one shared loop clock, so equal-length loops stay in time.
    audio.add_track("pad", (pad, 44_100)).set_gain(0.6);
    audio.add_track("drums", (drums, 44_100)).set_gain(0.0);
    audio.add_track_file("bass", Path::new("music/bass.ogg"))?.set_gain(0.5);
    Ok(audio)
}

fn each_frame(audio: &mut AudioContext, enemy_near: bool) {
    audio.update(); // advances the clock and any fades
    if enemy_near {
        audio.fade_track("drums", 1.0, 2.0); // to gain 1.0 over 2 s
    } else {
        audio.fade_track("drums", 0.0, 4.0);
    }
    audio.set_track_muted("bass", false);
}
```

`add_track` returns a `TrackHandle` whose `set_gain` / `fade_to` / `set_muted`
chain; the track starts when the handle is dropped, so the gain you chain
applies before any sound plays. `set_track_gain`, `fade_track`,
`set_track_muted`, `stop_track`, `has_track`, `track_names` and
`track_position` work by name. **Call `audio.update()` once per frame** or
fades never advance. `play_music_*` use the track named `dust_engine::audio::MUSIC_TRACK`
(`"music"`) and `play_music_layer_samples` uses `LAYER_TRACK` (`"layer"`).
For tests, `AudioEngine::with_manual_time(NullBackend::new())` plus
`advance(dt)` runs the same logic with no device. The web backend has only
been type-checked, never built.

## Spatial audio and mixer

Everything below lives in `dust_engine::audio` and works on any backend; with
`NullBackend` it runs with no device (use `AudioEngine::with_manual_time`).

### Mixer buses

Buses form a tree under `master` (`MASTER_BUS`); built-ins are `music`,
`sfx` and `ui` (`MUSIC_BUS`, `SFX_BUS`, `UI_BUS`). Each has gain 0..=1, mute
and solo. A track's effective gain is **track gain × mute × every bus gain on
its chain up to master**, with solo silencing everything outside the soloed
scope (`track_effective_gain(name)` reports exactly what the backend is told).

```rust
audio.add_bus("footsteps", SFX_BUS)?;                // Result<(), MixerError>
audio.set_bus_gain("sfx", 0.5);                      // -> bool (false: unknown bus)
audio.set_bus_muted("music", true);
audio.set_bus_solo("sfx", true);
audio.assign_track_bus("drums", "sfx");              // -> bool; also applies to a track of that name created later
audio.set_track_solo("drums", true);
audio.track_effective_gain("drums");                 // Option<f32>
audio.play_samples_on("ui", &samples, 44_100);       // one-shot through a bus
audio.save_mixer_settings(path)?;                    // RON: buses + track routes
audio.load_mixer_settings(path)?;                    // -> Vec<String> warnings
```

### Spatial voices

```rust
pub fn set_listener(&mut self, listener: Listener);
pub fn play_at(&mut self, source: impl Into<LoopBuffer>, position: Vec3, params: Emitter) -> SpatialHandle;
pub fn add_spatial_track(&mut self, name: &str, source: impl Into<LoopBuffer>, emitter: Emitter) -> SpatialHandle;
pub fn set_max_spatial_voices(&mut self, max: usize);   // default DEFAULT_MAX_SPATIAL_VOICES = 32
pub fn spatial_params(&self, handle: &SpatialHandle) -> Option<SpatialParams>;
```

`Listener { position, forward, up, velocity }` (`Listener::from_rotation(pos,
quat)` for a camera: forward is -Z, up +Y). `Emitter` carries
`min_distance`/`max_distance`, `Rolloff::{Inverse, Linear, Exponential}` +
`rolloff_factor`, an optional `Cone`, `doppler_factor`, optional `velocity`
and `rear_attenuation`; build it with `Emitter::at(pos).with_distances(1.0,
30.0).with_rolloff(Rolloff::Linear, 1.0)`. `SpatialHandle` (`Clone`,
`Send + Sync`) offers `set_position`, `set_velocity`, `set_emitter`,
`is_alive`, `stop`, `stop_when_dropped`. `add_spatial_track` loops on the
shared loop clock like `add_track`. The listener and handle positions are
applied by the next `audio.update()` / `advance(dt)`.

Per voice the engine computes distance/cone gain (folded into the track
volume, so buses and solo apply), an equal-power pan (`left² + right² = 1`)
and a doppler pitch. At most `max_spatial_voices` play at once; a new voice
over the bound steals the one with the lowest *effective* gain (ties: the
oldest); a new voice quieter than everything already playing is not started (its handle is dead).
Spatial voices default to the `sfx` bus unless `assign_track_bus` says otherwise.
One-shot `play_at` voices free their slot when they finish.

Backend support (`AudioBackend` additive methods, all default-bodied):

| Backend | pan | pitch | `track_level` |
|---|---|---|---|
| `NativeBackend` (rodio) | yes: mono buffers are expanded to 2 channels, per-channel gain | yes (`Sink::set_speed`, 0.1..4) | yes, post-fader peak |
| `NullBackend` | recorded (`pan(id)`, `pitch(id)`) | recorded | settable via `levels` |
| `WebBackend` (SDL2 mixer, mono) | **no: gain/distance only** | no | `None` |

### ECS: `AudioEmitter` and the listener

```rust
pub fn audio_spatial_system<B: AudioBackend + 'static>(ctx: &mut SystemCtx<'_>);
pub const AUDIO_SPATIAL_SYSTEM: &str = "audio_spatial";
```

```rust
resources.insert(AudioContext::new()?);               // skip when there is no device: the system is then a no-op
schedule.add_system(AUDIO_SPATIAL_SYSTEM, audio_spatial_system::<PlatformBackend>);

world.spawn((Transform::from_position(Vec3::new(4.0, 0.0, 0.0)),
             AudioEmitter::looped((samples, 44_100)).on_bus("sfx")));
world.spawn((Transform::default(), AudioListener)); // else the first Camera + Transform is the listener
// per frame, after schedule.run(..):
resources.get_mut::<AudioContext>().unwrap().update();
```

`AudioEmitter::{new, looped, once, on_bus, with_emitter}` with public
`source`, `looped`, `emitter`, `bus`, `playing`, `track_velocity`, `handle`.
The system starts a `playing` emitter, moves its voice with the entity's
`Transform` (velocity measured from the position change, for doppler), clears
`playing` when a one-shot ends or the voice is stolen (it never restarts by
itself), and sets the listener from the `AudioListener` entity (or camera)
with measured velocity. Despawning the entity stops its looping voice. The
system does not call `update()`; the host does, once per frame.

### Mixer panel (egui)

```rust
pub fn draw_mixer_panel<B: AudioBackend>(ui: &mut egui::Ui, audio: &mut AudioEngine<B>, state: &mut MixerPanelState) -> Vec<MixerEdit>;
```

Bus strips (vertical fader, M, S, resulting gain) and one row per named track
(bus picker, gain, M, S, effective gain, level meter). Meters are drawn only
when the backend reports `track_level` (native); otherwise the panel shows
gains only. Edits apply to the engine immediately and are also returned;
`state.queue.push(MixerEdit::BusGain { .. })` feeds an edit in from code. Call
it from a host tab (the module docs of `dust_engine::audio` show the full
`ShellHost::tab_ui` wiring):

```rust
fn tab_ui(&mut self, tab: &EditorTab, _world: &mut World, ui: &mut egui::Ui) -> bool {
    match (tab, self.audio.as_mut()) {
        (EditorTab::Host(name), Some(audio)) if name == "Mixer" => {
            draw_mixer_panel(ui, audio, &mut self.mixer_ui);
            true
        }
        _ => false,
    }
}
```

Tracks whose names start with `@` (spatial one-shots, emitters) are not
listed. Nothing in the repo adds a "Mixer" tab yet; hosts opt in.

## Trigger volumes

A `Collider` can be marked `is_trigger: true` — it's never physically solid:
overlapping it never pushes anything out or zeroes velocity. Instead,
`physics::step` returns every `(dynamic_entity, other_entity)` pair that
overlapped that frame, trigger or not, so it's up to the caller to act on the
ones that matter.

`Sandbox::update` (Play mode only) filters that list down to pairs involving
`player_entity`, diffs the current frame's set against a stored
`trigger_overlaps: HashSet<(Entity, Entity)>` from the previous frame, and
calls `on_trigger_entered`/`on_trigger_exited` for whatever newly
appeared/disappeared. Those two methods are the extension point — the demo
behavior logs the trigger's name and plays a tone (same `AudioContext` used
for jump/interact), showing triggers + physics + audio working together.
Replace them with your own game's trigger logic (checkpoints, level
transitions, damage zones, ...).

`LevelObject.is_trigger` (`#[serde(default)]`, so old levels still load as
non-triggers) saves/loads the flag with the level. **F2 panel**: an
**"Is Trigger (non-solid)"** checkbox next to "Dynamic Rigid Body" on any
selected object, and an **"Add Trigger Zone"** button (a `Cube`,
`is_trigger: true`, static by default) alongside Add Cube/Add Plane/Add
Light. A trigger renders as a solid bright cyan box
(`solid_color_texture`/`TRIGGER_COLOR` in `games/sandbox/src/main.rs`)
instead of its assigned/white texture, in both Edit and Play mode, so it
stays visually identifiable — true see-through would need alpha blending,
which the render pipeline doesn't have yet (a known simplification).

### Level transitions

A trigger can also load a *different* level and place the player at a
specific spot in it — e.g. a doorway that leads from one room's level file
into another's. This reuses the exact same `on_trigger_entered` extension
point above rather than adding a parallel mechanism: it's just one more
thing that method does after the log/tone/behavior dispatch, if the trigger
entity has a transition configured.

```rust
pub struct LevelTransition {
    pub target_level: String,   // matched against Level::name, same as the F2 "Load:" list
    pub spawn_position: [f32; 3],
    pub spawn_yaw_deg: f32,
}
```

`LevelObject.level_transition: Option<LevelTransition>` (`#[serde(default)]`)
saves/loads it with the level, mirrored onto `LevelObjectMeta` the same way
`script_path` is, so the F2 panel and `build_level_from_ecs` can round-trip
it. Only meaningful when `is_trigger` is true.

When `on_trigger_entered` finds a `LevelTransition` on the entered trigger,
`Sandbox::transition_to_level` looks up `target_level` by name in
`self.levels`, calls `apply_level` (the same full level swap the F2 "Load:"
list uses — wipes the world, respawns objects/lights/rigs, switches music),
then respawns the player at `spawn_position` facing `spawn_yaw_deg` and
shows a toast. `pre_play_snapshot` (what F3/Escape-stop restores) is
deliberately left pointing at whichever level Play mode actually started
from, so stopping mid-transition returns the editor to the level you were
editing, not wherever the player ended up.

**F2 panel**: selecting a trigger object shows a **"Level Transition"**
section — a target-level picker (same list as the Level section's "Load:"),
`DragValue`s for spawn position and yaw, and "Add"/"Clear Transition"
buttons. The shipped default level demonstrates this: its "Level Exit"
trigger leads to a small "second_room" level, whose own "Return" trigger
leads back.

### Screen effects

A trigger (or a script/native `Behavior`) can also flash a full-screen
color tint — a damage flash, a fade, a "portal" flash on a level
transition, etc. — composited directly into the rendered frame rather than
drawn as UI on top of it, so it stays native to the posterize/dither retro
look instead of reading as a crisp modern overlay.

```rust
// dust_engine::screen_effect
pub struct ScreenEffectSpec {
    pub color: [f32; 3],
    pub strength: f32,        // peak tint strength, 0..1
    pub fade_in_secs: f32,
    pub hold_secs: f32,
    pub fade_out_secs: f32,
}
```

`ScreenEffectState` (owned by `Sandbox` as `screen_effects`, like `hud`)
holds at most one playing effect, ticked once per frame alongside
`hud.tick(dt)`. Its `current()` evaluates the fade-in/hold/fade-out
envelope into a `(color, strength)` pair, fed into `PostParams` right
before `renderer.present(...)` in `render()`. `CompositePass`/
`post_composite.frag` mix the tint into the color **before** the
posterize/dither step, so a flash gets quantized/dithered along with
everything else rather than looking pasted on. A custom composite shader
that doesn't declare the `uTintColor`/`uTintStrength` uniforms just no-ops
the tint (missing uniform locations are already handled as `Option`s) —
no crash, the effect simply doesn't show under that profile.

`LevelObject.screen_effect: Option<ScreenEffectSpec>` (`#[serde(default)]`)
saves/loads it with the level, mirrored onto `LevelObjectMeta` the same way
`level_transition` is. Only meaningful when `is_trigger` is true, and
independent of `level_transition` — a trigger can have either, both, or
neither. `on_trigger_entered` checks for a `screen_effect` right alongside
its `level_transition` check (before it, so a portal trigger flashes and
then swaps levels) and calls `self.screen_effects.trigger(...)`.

Scripts and native `Behavior`s can fire the same effect directly via
`screen_flash(r, g, b, strength, fade_in, hold, fade_out)` (see
"Scripting" below) — useful for e.g. a damage flash driven by gameplay
logic rather than authored on a trigger.

**F2 panel**: selecting a trigger object shows a **"Screen Effect"**
section — a color picker, a strength slider, `DragValue`s for fade-in/
hold/fade-out, and "Add"/"Preview"/"Clear Screen Effect" buttons. "Preview"
plays the effect immediately without entering Play mode. The shipped
default level demonstrates this: "Trigger Zone" flashes cyan (matching its
own render color) and "Level Exit" flashes white alongside its level swap.

## Characters and AI

`dust_engine::pathfinding::NavGrid` bakes a walkability grid (XZ plane, default
cell size `0.5`) from every **static** (no `RigidBody`), non-trigger
`Collider` in the world — `Sandbox::apply_level` calls
`NavGrid::bake(&self.world, 0.5)` once per level load, after every other
spawn loop, and stores it on `Sandbox.nav_grid`. A collider only counts as
an obstruction if its Y half-extent (or sphere radius) is at least `0.3` —
this is what keeps a paper-thin floor plane from blocking every cell while
walls/pillars still block correctly. `NavGrid::find_path(start, goal)` runs
8-directional A* (octile heuristic, no cutting a corner diagonally through
two blocked orthogonal cells) and returns waypoint centers in world space,
or `None` if unreachable.

`dust_engine::ai` is the character framework, deliberately reusing existing
systems rather than building parallel ones — feedback goes through
`ScreenEffectState`/`CameraShakeState`, dialogue/health through `HudState`,
and death particles through `spawn_burst_at`:

- **`Disposition`** — `Passive` (wanders, flees the player), `Hostile`
  (wanders, chases and attacks), `Friendly` (wanders, but only ever reacts
  to `interact()` — never chases or flees).
- **`CharacterMeta`** — the authored tuning (name, color, disposition,
  move speed, wander radius, sight range, and combat fields `damage`/
  `attack_range`/`attack_cooldown_secs`, `damage: None` meaning the
  character never attacks). Round-tripped into
  `dust_engine::level::CharacterInstance`.
- **`CharacterBrain`** — runtime-only state machine (`Idle`/`Wandering`/
  `Chasing`/`Fleeing`/`Attacking`), never serialized. Perception is a plain
  distance check against the player's position (not a line-of-sight
  raycast) with hysteresis on losing track (keeps tracking out to
  `1.5 × sight_range` before giving up, so a target sitting right at the
  boundary doesn't flicker between states).
- **`Health`** — `current`/`max` hit points, used for the player too (the
  player gets one via `Health::new(100.0)` at spawn, in both
  `enter_play_mode` and `transition_to_level`). `damage()`/`is_dead()`/
  `fraction()` (for HUD bars).
- **`Dialogue`** — a small branching node graph for `Friendly` characters:
  `Dialogue { nodes: Vec<DialogueNode>, current: usize }` where
  `DialogueNode { text: String, choices: Vec<(String, usize)> }` (choice
  label, target node index). A node with no choices behaves exactly like the
  old flat cycling dialogue — auto-advances to the next node, wrapping — so
  non-branching NPCs are just every node having zero choices. When
  `interact()` advances into a node that *has* choices, `Sandbox.active_dialogue`
  is set and a small bottom-center `draw_dialogue_choices` overlay lists them
  with number-key hints; pressing **1**-**9** jumps to the chosen node's
  target and shows its text as a toast, same as a normal advance.
- **`dust_engine::ai::step(world, dt, player_position, nav_grid) ->
  Vec<AiEvent>`** — advances every `CharacterBrain`'s perception/state/
  steering (writes horizontal `RigidBody.velocity`; physics' existing
  gravity/collision handles the rest, same as the player), then scans every
  `Health` for `is_dead()`. Mirrors `physics::step`'s "return events, let
  the caller handle them" contract: `AiEvent::AttackedPlayer { damage }`
  and `AiEvent::CharacterDied { entity, position }` — the caller (not this
  function) despawns a dead entity, since it also needs the position for
  death feedback first.

**Game-side wiring** (`games/*/src/main.rs`, same shape in both games):
`Game::update`'s Play-mode block calls `dust_engine::ai::step` right after
`update_player_input` (steering decided before `physics::step` applies it
that same frame, the same ordering `update_player_input` itself uses), then
handles the returned events — `AttackedPlayer` damages the player's
`Health`, triggers a red `ScreenEffectSpec` + `CameraShakeSpec`, and on
death shows a toast and calls `restart_level`; `CharacterDied` despawns the
entity, spawns a particle burst, and plays a tone. The player's `Health`
fraction is written to `hud.set_bar("Health", ...)` every Play-mode frame,
rendered by the existing generic HUD bar UI — no new UI code needed there.
`interact()`'s nearest-target search also scans `CharacterMeta` entities:
a character with `Dialogue` shows/advances its next line as a toast; a
`Hostile` character with `Health` takes a fixed `10.0` damage instead (the
only player-facing damage source in this vertical slice — there's no
dedicated attack input, so the interact key doubles as a melee hit).

A placed character is authored as `dust_engine::level::CharacterInstance` — a
sibling list on `Level` (`Level.characters`) like `RigInstance`/
`LevelLight`, since a character's shape doesn't fit `LevelObject`'s generic
mesh/texture/trigger schema. `Sandbox::spawn_character` spawns it as a
cube — a sphere `Collider` (matching the player's own convention) +
`RigidBody` (an ordinary dynamic body, no special-casing needed in
`dust_engine::physics`) + `CharacterMeta`/`CharacterBrain`, plus `Health`/
`Dialogue` if the instance was authored with combat/dialogue. The cube is
solid-`color`-filled by default; setting `CharacterMeta::texture_path`
(mirroring `LevelObjectMeta::texture_path`) wraps it in a texture instead —
`color` stays meaningful as the fallback used if the texture fails to load.

**F2 panel**: a **"Characters"** section (mirrors "Rigs") — **Add
Character**, a clickable outliner, and for the selected character: name,
position, color, the current texture path (or "(solid color)") with
**Assign Texture...** (native file picker) / **Browse Texture...** (the
same in-editor thumbnail asset browser textures on level objects use) /
**Clear Texture** buttons, a disposition dropdown, move speed/wander
radius/sight range drag values, an **Add/Clear Combat** pair (damage/attack
range/cooldown, same optional-field pattern as a trigger's Screen Effect/
Camera Shake), an **Add/Clear Health** pair, and an **Add/Clear Dialogue**
pair with a growable line list and a "Preview" button.

**Stated simplifications**: perception is distance-only (no line-of-sight
raycast); the `NavGrid` bakes once per level load and never re-bakes
mid-level, so it assumes static rooms, not moving/destructible geometry.

Mushroom Man's dungeon demonstrates all three dispositions: a `Friendly`
"Cave Sprite" near the entrance spawn point (dialogue hints at the Bounce
gate), a `Passive` "Tunnel Crawler" wandering the tunnels corridor, and a
`Hostile` "Spore Guardian" guarding the approach to the Heart of the Grove.

## Spawners

A placed periodic spawner (`dust_engine::level::SpawnerInstance`, a sibling list
on `Level` like `LevelLight`/`CharacterInstance`) periodically spawns copies
of a `CharacterInstance` template around its own position. `dust_engine::ai::SpawnerConfig`/
`SpawnerState` hold the authored config (interval, max alive, an optional
total-spawned cap, spawn radius) and runtime bookkeeping (elapsed time,
which spawned entities are still alive); `dust_engine::ai::step_spawners(world,
dt) -> Vec<SpawnRequest>` ticks every spawner and — mirroring `ai::step`'s
"return events, let the caller act" contract, since `dust_engine::ai` has no `gl`
access to actually spawn a GPU-backed entity — emits a `SpawnRequest` when
it's time. `Game::update` calls `step_spawners` right after the main
`ai::step` block and fulfills each request via `Sandbox::spawn_character`,
tracking the new entity back onto the spawner's `SpawnerState`.

**F2 panel**: a **"Spawners"** section (mirrors "Characters") — **Add
Spawner**, a clickable outliner, and for the selected spawner: name,
position, interval/max-alive/spawn-radius drag values, an optional "Limit
total spawned" cap, and a compact editor for the template character it
spawns (name, color, disposition, move speed, sight range, optional combat/
health). Mushroom Man's `tunnels` level demonstrates one: a "Crawler Nest"
that tops back up to 2 alive Tunnel Crawlers every 12 seconds if the
original wanders off or is defeated.

## Animation

`dust_engine::animation` gives level objects procedural motion — two kinds, not a
full keyframe/timeline system:

```rust
pub enum AnimationKind {
    Orbit { axis: Vec3, speed_deg_per_sec: f32 },          // continuous rotation
    Bob { axis: Vec3, amplitude: f32, period_secs: f32 },  // sinusoidal position offset
}
pub struct Animator { pub kind: AnimationKind, pub base_position: Vec3, pub base_rotation: Quat, pub elapsed: f32 }
pub fn step(world: &mut hecs::World, dt: f32);
```

`Animator` recomputes `Transform` from a **fixed base pose** (`base_position`/
`base_rotation`) plus `kind` and `elapsed` every call — it never mutates the
transform incrementally, so the motion can't drift. `Sandbox::update` calls
`dust_engine::animation::step` unconditionally, in both Edit and Play mode — a
live preview while editing costs nothing extra and is a nice default (try
it: the shipped default level's "Orbiting Cube" spins continuously, visible
right away without entering Play mode).

An animated object with **no `RigidBody`** (the common case — decorative
motion, moving platforms) still pushes anything that touches it in Play
mode, with no special-casing needed: the physics collision loop already
checks every `Collider` regardless of `RigidBody` presence, so a static
animated object is a free kinematic mover.

`dust_engine::level::AnimationSpec` mirrors `AnimationKind` (serializable) and
`LevelObject.animation: Option<AnimationSpec>` (`#[serde(default)]`) saves it
with the level. `spawn_level_object` attaches an `Animator` when present,
using the object's authored transform as the base pose. **Important:**
`build_level_from_ecs` reads `Animator::base_position` (not the live,
currently-oscillating `Transform::position`) for animated objects, so saving
mid-animation doesn't capture a random instant — rotation is unaffected
since it's already saved from `LevelObjectMeta::rotation_euler_deg`, which
animation doesn't touch.

**F2 panel**: an **Animation** dropdown (None/Orbit/Bob) on the selected
object, plus an Axis and the kind's parameters (Speed for Orbit; Amplitude/
Period for Bob). Switching **None → Orbit/Bob** captures the object's
*current* transform as the new base pose; switching **back to None** just
removes the `Animator`, leaving the object wherever it last was. Tweaking
axis/parameters *while already animating*, or switching between Orbit and
Bob, preserves the existing base pose rather than re-capturing the
(possibly mid-motion) live transform — otherwise every parameter edit would
make the object jump.

## Running the game (Play mode)

**F3** toggles between **Edit** mode (everything above — orbit camera, both
GUI panels, nothing simulated) and **Play** mode, which is meant to feel like
hitting Play in a normal game engine:

- The level is **snapshotted** before entering Play (`build_level_from_ecs`)
  and **restored exactly** on Stop (`apply_level` with that snapshot) — physics
  knocking objects around, or you pushing them, never corrupts your edits.
  **F3** always stops immediately, back to the editor. Plain **Escape**
  pauses instead of stopping (see "Pausing" below) while in Play, and quits
  the app while already in Edit mode.
- Both GUI panels hide automatically (F1/F2/Tab/F5/orbit-drag are gated to
  Edit mode) and a small "Play Mode" banner appears instead. The mouse is
  captured (SDL2 relative mouse mode) for unbounded FPS look, released again
  on Stop.
- An invisible player (a sphere `Collider` + `RigidBody` + `PlayerController`,
  no mesh) spawns above the level and falls onto whatever's there. The camera
  becomes `dust_engine::camera::FirstPersonCamera`, positioned at the player's
  `Transform::position` plus a fixed eye-height offset each frame, plus an
  optional walking view-bob (see below).
- **WASD** moves relative to look direction, **mouse** looks freely, **Space**
  jumps when `RigidBody::grounded` is true, **E** interacts. A connected
  **gamepad** blends in automatically — see "Gamepad support" below.

**Head bob** (`dust_engine::camera::HeadBob`): a vertical bob plus a lighter
side-to-side sway, added to the eye position in `player_eye_position`. It's
driven by *actual* horizontal speed and `grounded` (updated once per frame
in `update_player_input`, right where movement velocity is set), not just
"is a key held," so it naturally speeds up/slows down with movement and
eases in/out via an internal `intensity` value rather than snapping on/off
or freezing mid-cycle when you stop. `enabled` toggles it off as a strict
no-op with zero per-frame cost. Toggle and tune `enabled`/`amplitude`/
`sway_amplitude`/`cycles_per_unit` live in the F1 panel's **Camera**
section — a play-session setting, not saved with the level or profile.

**Three more `dust_engine::camera` FX**, all applied the same way (added into
`player_eye_position` or written onto `fp_camera` each frame in
`update_player_input`), each an `enabled` toggle plus a couple of tuning
fields, live-editable in the same F1 **Camera** section:

- **`LandingDip`** — a critically-damped spring, kicked downward by
  `land(fall_speed)` the exact frame `RigidBody::grounded` transitions
  false→true (detected in `Game::update`, right around the
  `dust_engine::physics::step` call — landing zeroes out `velocity.y` as part of
  collision resolution, so the fall speed has to be captured *before*
  `step` runs), then `update(dt)`'d every frame regardless of grounded
  state so it always settles back to rest rather than getting stuck.
- **`StrafeTilt`** — eases `fp_camera.roll` toward a target proportional to
  how much of the current frame's movement is sideways
  (`move_dir.dot(right_flat)`, the same signed value that already blends
  WASD/left-stick input), and back to level when you stop strafing.
- **`SpeedFov`** — an additive FOV kick at speed, deliberately *not*
  written straight into `fp_camera.fov_y_radians` (that field is instead
  driven by `base_fov_radians + speed_fov.kick_radians()` each frame) so it
  never fights with a game's own ability/zoom logic that might want to set
  a different base FOV outright.

**`CameraShakeSpec`/`CameraShakeState`** (also `dust_engine::camera`): a
momentary jitter, mirroring `dust_engine::screen_effect::ScreenEffectSpec`'s
role but for the eye position instead of a color tint — a few
incommensurate sine waves rather than a `rand` dependency, decaying
linearly to zero over `duration_secs`. Fireable three ways, all converging
on the same `CameraShakeState::trigger`: a trigger's
`LevelObjectMeta::camera_shake` (checked in `on_trigger_entered` right
alongside `screen_effect`), `ScriptApi::camera_shake(intensity, duration)`
from a script or native `Behavior`, or the F1 panel's "Test camera shake"
button. The F2 panel's per-trigger inspector has a matching **Camera
Shake** section (Add/Preview/Clear), right under **Screen Effect**.

### Pausing

**Escape** while playing pauses instead of stopping — `Sandbox::pause`/
`resume` flip a `paused: bool` (only meaningful while `mode ==
EditorMode::Play`) and release/re-capture the mouse so a real cursor can
click the pause menu's buttons (relative mouse mode otherwise swallows
clicks as look-deltas). `dust_engine::ui::draw_pause_menu` — a small, stateless
function in the same spirit as `draw_hud`, just returning whichever button
was clicked — draws a centered **Resume** / **Exit to Editor** / **Quit
Game** window.

Pausing is a real freeze, not just an input lock: `Game::update`'s
"always on, live preview" block (`animation::step`, rig clip stepping,
`particles::step`, `hud.tick`) — which normally *also* runs in Edit mode
so editing has a live preview — skips entirely while `mode == Play &&
paused`, so particles/rig poses/HUD toast timers actually stop rather
than keep animating behind the menu. The frozen HUD and world are still
drawn, though, so pausing looks exactly like hitting pause, not like the
screen going blank. F9 checkpoint-save, **E** interact, and the gamepad
interact button are all disabled while paused, alongside the existing
Play-mode-only gating.

**Exit to Editor** calls the same `exit_play_mode` F3 already uses (player
despawned, pre-Play snapshot restored). **Quit Game** sets
`ctx.should_quit`. Both are here so a pause menu built this way behaves
like a real shipped game's, not just a debug convenience. In a release
build there's no editor to exit to, so that button becomes **"Restart
Level"** instead (`Sandbox::restart_level`) — see "Exporting a game"
below for exactly when that switch happens.

A forced title/start screen before Edit mode was considered and
deliberately left out: a debug build boots straight into the dev-time
level editor by design, and a menu you have to click through on every
launch would fight that workflow (a release build already skips it,
straight to Play — see "Exporting a game" below). The pause menu
demonstrates the same reusable `dust_engine::ui` modal-overlay pattern a title
or game-over screen would reuse — add one the same way once a game built
on this engine actually needs it.

**This is the place to add your own game's input** — two methods on
`Sandbox` in `games/sandbox/src/main.rs`, both heavily commented as the
extension points:

- `update_player_input` (called from `Game::update`, Play mode only) — reads
  `ctx.input`/mouse-delta every frame and drives continuous movement/look.
  Swap the movement scheme, add sprint/crouch, bind new keys, etc. here.
- `interact` (bound to `KeyDown(E)` in `handle_event`) — the example
  "interact with the environment" hook: finds the nearest entity with a
  `RigidBody` within `PlayerController::interact_radius` and pushes it with
  an outward impulse (`PlayerController::interact_impulse`), tying the
  physics engine and the input hook together in one visible action. Replace
  this with your own game's interaction (pickup, dialogue, opening a door,
  ...) — it's a one-shot action, not a per-frame poll, so it's the natural
  place for anything triggered by a single keypress.

## Gamepad support

`dust_engine::input::Input` mirrors its keyboard down/just-pressed/just-released
pattern for a controller: `left_stick()`/`right_stick() -> (f32, f32)` (each
axis `-1.0..=1.0`, deadzoned), `controller_button_down`/
`controller_just_pressed(sdl2::controller::Button) -> bool`. Only **one**
controller is tracked — the first one connected — fine for a single local
player; a second one plugged in while the first is still connected is
ignored.

`Platform` owns the `GameControllerSubsystem` (must stay alive for opened
controllers to remain valid); `Input::handle_event` takes it as a second
argument so it can open a `GameController` on `Event::ControllerDeviceAdded`
and drop it on `..Removed`, tracking axes/buttons the same way keyboard
state is tracked. No extra `sdl2` Cargo feature is needed — controller/
joystick support is already in the base crate, with a built-in mapping
database that recognizes common Xbox/PlayStation-layout pads via XInput on
Windows out of the box.

In the sandbox, `update_player_input` blends the **left stick** additively
with WASD (whichever is actually being pushed drives movement — note this
means the stick's analog magnitude is *not* preserved, since keyboard input
is a normalized unit vector; a known simplification) and the **right
stick** additively with mouse-look. **A** jumps (same as Space), **X**
interacts (same as E).

**Caveat, stated plainly**: unlike every other feature in this guide, this
one could not be end-to-end tested with real hardware in the environment
this was built in (no physical controller, no virtual joystick driver) — it
was built carefully against SDL2's documented event/API surface and the app
was confirmed to start up cleanly with the subsystem initialized, but
**please confirm on real hardware** before relying on it.

## Scripting

Per-object behavior is written in **Oxidized** (`.ox`), a small typed
language that transpiles to Rust (the `oxidized` compiler lives outside this
repo). The old interpreted `.pss` language is retired: there is no `.pss`
interpreter, no `legacy-pss` feature and no standalone script editor any
more. Full design, measurements and limits: **`docs/SCRIPTING-OXIDIZED.md`**.

How a script runs: `foo.ox` -> `oxidized build` -> Rust -> `rustc --crate-type
cdylib` -> loaded with `libloading` -> wrapped as an `dust_engine::behavior::Behavior`
(`dust_engine::script::ox::OxBehavior`). Builds are cached on disk by content hash
(`<asset root>/.ox-build`, git-ignored): a cold build is ~1-6 s, an unchanged
script loads instantly. Saving a script while the game runs rebuilds it on a
worker thread and live-swaps every entity using it, keeping per-entity state; a
broken save logs `file:line:col: message` and the last good build keeps running.
`oxidized` and `rustc` must be installed on the machine that builds scripts
(`$OXIDIZED_BIN`, `PATH`, `~/.local/bin`, `~/oxidized/target/release`).

### Language (what scripts look like)

```
# scripts/bob_demo.ox
fn on_ready() { log("ready") }                  # hooks; unannotated params are rejected
fn on_update(dt: Float) {
    let phase = get_var("phase", 0.0) + dt      # script functions are stateless: per-entity
    set_var("phase", phase)                     #   state lives in engine-held variables
    let wave = sin(phase / tunable("period_secs", 2.0) * tau())
    move_by(0.0, tunable("amplitude", 0.4) * wave, 0.0)
    hud_bar("Wobble", wave * 0.5 + 0.5)
}
fn on_interact() { play_tone(500.0, 0.1) }
fn on_trigger_enter(other: String) { log("hello " + other) }
fn on_trigger_exit(other: String) { }
```

Hooks (all optional): `on_ready()` (alias `on_start()`), `on_update(dt: Float)`,
`on_interact()`, `on_trigger_enter(other: String)`, `on_trigger_exit(other: String)`.

**Script-facing API** (single source of truth: `engine/src/script/ox/build/spec.rs`):
transform `pos(axis) set_pos move_by rot(axis) set_rot scale(axis) set_scale`
(axis 0/1/2); input `key_down(key) key_pressed(key)` (return `Int` 1/0) and
`axis(name)`; world `spawn despawn`; `log time play_sound play_tone`;
data `tunable(name, default) get_var(name, default) set_var(name, value)`;
HUD/camera `hud_bar toast camera_shake`; math `sin cos tan atan2 clamp lerp pi tau`.
`oxc_scripts api` prints the generated declarations. Not available from
`.ox`: `screen_flash` (there is no native for it yet).

### `Behavior`/`ScriptApi` — using Rust the same way a script would

```rust
// dust_engine::behavior
pub trait ScriptApi {
    fn log(&mut self, message: &str);
    fn position(&self) -> (f32, f32, f32);
    fn set_position(&mut self, x: f32, y: f32, z: f32);
    fn move_by(&mut self, dx: f32, dy: f32, dz: f32);
    fn play_tone(&mut self, frequency_hz: f32, duration_secs: f32);
    fn play_sfx(&mut self, path: &str);
    fn elapsed(&self) -> f32;
    fn set_hud_bar(&mut self, name: &str, fraction: f32);
    fn show_toast(&mut self, message: &str, seconds: f32);
    fn screen_flash(&mut self, r: f32, g: f32, b: f32, strength: f32, fade_in_secs: f32, hold_secs: f32, fade_out_secs: f32);
    fn camera_shake(&mut self, intensity: f32, duration_secs: f32);
}
pub trait Behavior: Send + Sync {
    fn on_ready(&mut self, api: &mut dyn ScriptApi) { }
    fn on_update(&mut self, api: &mut dyn ScriptApi, dt: f32) { }
    fn on_interact(&mut self, api: &mut dyn ScriptApi) { }
    fn on_trigger_enter(&mut self, api: &mut dyn ScriptApi, other_name: &str) { }
    fn on_trigger_exit(&mut self, api: &mut dyn ScriptApi, other_name: &str) { }
}
pub struct BehaviorSlot(pub Box<dyn Behavior>);  // the ECS component
```

`OxBehavior` implements `Behavior` by calling the compiled script's hooks.
Crucially, **a plain Rust type can implement `Behavior` directly** — see
`dust_engine::behavior::NativeBobBehavior` — and gets attached to an entity via the
exact same `BehaviorSlot` component. The game dispatches through the `Behavior`
trait uniformly and never needs to know whether a given entity's logic came
from a script or from compiled Rust: **prototype as a script, port to native
Rust later, and nothing about how it's wired into the game has to change.**

The default level's **"Script Demo Cube"** (running `scripts/bob_demo.ox`)
and **"Native Behavior Cube"** (running `NativeBobBehavior`, spawned in
`Sandbox::spawn_native_behavior_demo`) stand side by side and do the
identical sine-bob motion + interact log/tone — one from a script, one from
compiled Rust. The native one is a compiled-in demo, not saved level data
(`NativeBehaviorDemoMarker` tells `build_level_from_ecs` to skip it).

### Runtime wiring (`games/*/src/main.rs`, `dust_engine::script::ox::OxScripts`)

- `LevelObject.script: Option<PathBuf>` points at an `.ox` file relative to the
  asset root (`scripts/bob_demo.ox`). `Game::make_script_behavior` asks
  `OxScripts` (created lazily, so a game without scripted objects never needs
  `oxidized`) for a behavior; a build error is logged and the object still
  spawns, with an idle behavior that a fixed save hot-swaps in. A non-`.ox`
  path (e.g. an old `.pss`) is logged as unsupported and ignored.
- Each frame `update` calls `OxScripts::begin_frame(&ctx.input)` (the input
  snapshot scripts read with `key_down`/`axis`), logs finished background
  rebuilds from `poll()`, and applies queued `set_rot`/`set_scale` commands to
  `Transform`s with `apply_commands`. `spawn`/`despawn` commands are returned
  to the game; the bundled games log them as unsupported.
- `Game::with_behavior(entity, |behavior, api| ...)` is the one place that
  bridges `Behavior`/`ScriptApi` to the ECS (unchanged).
- `on_update` is dispatched for every `BehaviorSlot` entity once per frame,
  **Play mode only**. `on_interact` extends the existing `interact()` hook: a
  nearby entity with a `BehaviorSlot` gets its own `on_interact` called
  *instead of* the generic "push it" demo; `on_trigger_enter`/`on_trigger_exit`
  extend the existing trigger log+tone additively.

### Migrating an old `.pss` script

`cargo run -p oxc_scripts -- convert path/to/foo.pss [--out-dir DIR]` converts
the behavior subset (hook renames, `get_x/y/z` -> `pos`, top-level `let` ->
`get_var/set_var`, `play_sfx` -> `play_sound`, ...) and prints warnings for
what needs hand work. Check the result with `oxc_scripts build <dir>`.

### Editing scripts

The inspector's Script section (**Attach/Change Script...** picks an `.ox`
file, **Reload**, **Edit Inline** + **Save**) is unchanged; Save writes the file
and the watcher rebuilds it. Use any text editor otherwise; there is no
dedicated script editor app.

## Character rigs

`dust_engine::rig` gives level objects skeletal-*ish* animation using the classic
PS1/PS2-era technique: a character is several separate rigid meshes (torso,
head, arms, legs) parented in a hierarchy, each moving as a whole — **not**
per-vertex skin deformation. Joints don't bend smoothly (a raised arm is a
rigid box swinging from its shoulder, not a soft-deforming sleeve); that's
an inherent limitation of the technique, not a bug, and it's why this is
the right-sized approach for a lightweight PS2-style engine rather than
importing pre-skinned glTF characters or building an in-engine
weight-painting tool.

### Data shapes

```rust
// dust_engine::rig — a reusable character definition, its own `rigs/*.ron` files
pub struct RigPartDef { pub name: String, pub parent: Option<String>, pub mesh: MeshSource, pub texture_path: Option<PathBuf>, pub local_position: [f32; 3], pub local_rotation_euler_deg: [f32; 3], pub scale: [f32; 3] }
pub struct Keyframe { pub time: f32, pub rotation_euler_deg: [f32; 3] }
pub struct JointTrack { pub joint_name: String, pub keyframes: Vec<Keyframe> }
pub struct RigClip { pub name: String, pub duration: f32, pub looping: bool, pub tracks: Vec<JointTrack> }
pub struct RigAsset { pub name: String, pub parts: Vec<RigPartDef>, pub clips: Vec<RigClip> }

// runtime ECS components
pub struct RigPart { pub parent: Option<Entity>, pub local_position: Vec3, pub local_rotation_euler_deg: Vec3, pub local_rotation: Quat }
pub struct Rig { pub parts_by_name: HashMap<String, Entity> }       // lives on the root entity
pub struct RigAnimator {
    pub clips: Vec<RigClip>, pub current_clip: Option<usize>, pub time: f32, pub playing: bool, pub speed: f32,
    // blend-out state for whatever clip `current_clip` just replaced — see "Clip blending" below
    pub previous_clip: Option<usize>, pub previous_time: f32, pub blend_elapsed: f32, pub blend_duration: f32,
}
```

A rig part is just an entity with a `Transform` + `MeshRenderer`, exactly
like any other level object — the render loop needed **zero changes** for
this feature. The only new problem was computing each part's *world*
transform from its parent chain: `RigPart::local_position`/
`local_rotation` are the authoritative local pose (set at spawn, hand-posed
in the F2 panel, or driven by clip playback); `Transform` is treated as
pure output for a rigged entity, overwritten every frame — the same
pattern `Animator` already uses for `base_position`/`base_rotation` vs. the
live `Transform`.

Two systems, both run unconditionally every frame from `Game::update`
(Edit and Play mode, same "live preview" philosophy as
`dust_engine::animation::step`):

- `step_rig_animation` advances each playing `RigAnimator`'s clock
  (wrapping or clamping against the clip's `duration`) and **slerps**
  between a track's two bracketing keyframes (not raw Euler lerp — avoids
  gimbal artifacts), writing the sampled rotation into the named part's
  `RigPart::local_rotation`. It's gated on `playing` specifically:
  pausing lets you hand-pose a part via the F2 panel without the sampler
  immediately overwriting it — that hand-pose *is* the keyframe-authoring
  workflow.
- `update_world_transforms` resolves every `RigPart`'s world
  position/rotation via a memoized parent-first recursive walk
  (`parent_world_pos + parent_world_rot * local_pos`, `parent_world_rot *
  local_rot`; a root's world pose *is* its local pose) and writes it into
  `Transform`.

### The F2 workflow

The level editor panel gains a **Rigs** section, below Scene Objects:

- **New Rig** (name + button) creates a fresh `RigAsset` with one root
  part named "Root", saves it to `rigs/<name>.ron`, and places an instance
  immediately so there's something to build on.
- **Add instance of: \<rig name\>** places another instance of an
  already-loaded rig asset.
- Clicking a placed instance's name opens its panel: **Root Local
  Position/Rotation** (editing `RigPart` directly, *not* `Transform` —
  same reasoning as animated objects, since `update_world_transforms`
  overwrites `Transform` every frame), a **Clips** list with **New
  Clip...**, **Play/Pause**, a **Loop** checkbox, a **Speed** slider, and a
  **Time (scrub)** slider (dragging it pauses playback, since scrubbing
  against a running clip is useless), a **Parts** list, **Add Part...**
  (always a `Cube` primitive — resize/retexture afterward like any other
  primitive; picks a parent from the rig's existing part names, falling
  back to the root if the name doesn't match one), **Save Rig**, and
  **Delete Rig Instance**.
- Selecting a part shows *its* Local Position/Rotation and a **Set
  Keyframe Here** button that upserts `(current scrub time, current local
  rotation)` into the selected clip's track for that part. The whole
  authoring loop is: pause the clip, scrub to a time, pose a part by hand
  with the drag-values, click **Set Keyframe Here**, scrub to the next
  time, repeat, **Save Rig**.

`dust_engine::level::RigInstance` (a *placement*: name, `rig_path`, position,
rotation, which clip is playing) is a sibling list on `Level` —
`Level.rig_instances`, the same pattern as `Level.lights` — since a rig is
structurally a multi-entity hierarchy, not a single mesh like
`LevelObject`. No per-instance scale: scale is authored per-part inside
the rig asset itself, since scaling only the root's own mesh (not the
whole hierarchy's offsets) would be more confusing than useful.

### Clip blending

Switching clips instantly (cutting `current_clip` and `time` straight to
the new values) makes every part's pose jump on the very next frame — a
visible pop, worst on a part mid-swing. `RigAnimator::play_clip(index)`
avoids that by snapshotting whatever was playing (`previous_clip`,
`previous_time`) before switching, then blending: for `blend_duration`
seconds (default 0.2), `step_rig_animation` samples **both** the old
clip (frozen at `previous_time` — it isn't advancing anymore, just
providing a fixed starting pose) and the new clip (advancing normally)
per track and `Quat::slerp`s between them by `blend_elapsed /
blend_duration`. Once the blend window elapses, `previous_clip` is
cleared and the part reads purely from the new clip. Calling `play_clip`
with the already-current clip index is a no-op — nothing to blend from
itself.

The F2 panel's clip list routes through `play_clip` when you click a
different clip, so hand-testing an authored transition always gets the
blend; nothing else needed it (the "Time (scrub)" slider still snaps
`time` directly within one clip — scrubbing is meant to jump exactly
where you drag it, not ease into it). Since a full blend finishes inside
0.2 seconds, verifying the interpolation visually frame-by-frame isn't
practical — see `dust_engine::rig`'s `play_clip_blends_smoothly_instead_of_popping`
unit test for the actual verification: it steps the simulation at fixed
`dt`s and asserts the sampled rotation sits at the old pose right at the
switch, at the slerp midpoint halfway through the blend window, and
exactly at the new pose once it's elapsed.

## Object classes

`dust_engine::class` lets many placed objects share one definition — a
**class** — instead of duplicating mesh/texture/scale/physics/animation/
script settings on every instance by hand. It mirrors `dust_engine::profile`'s
save/load pattern exactly, its own `classes/*.ron` folder:

```rust
pub struct ObjectClass {
    pub name: String,
    pub mesh: MeshSource,
    pub texture_path: Option<PathBuf>,
    pub scale: [f32; 3],
    pub is_dynamic: bool,
    pub is_trigger: bool,
    pub animation: Option<AnimationSpec>,
    pub script: Option<PathBuf>,
}
```

`LevelObject` gains `class: Option<PathBuf>`. When set, `spawn_level_object`
resolves mesh/texture/scale/dynamic/trigger/animation/script **from the
class** instead of the object's own copies of those fields — only
position/rotation/name stay per-instance. A `ClassMember` marker component
tags every class-spawned entity so the game can find them again later.

Editing a class does **not** live-update its instances automatically —
that would make "why did this object just change?" hard to reason about.
Instead, **Apply to All Instances** (F2 panel) is an explicit action: click
it after editing a class and every live `ClassMember` entity referencing
that class path is re-resolved from the new values. Until you click it,
existing instances keep whatever they were spawned with.

### F2 workflow

A **Classes** section (below Rigs) gives you: **New Class** (name +
button), a list of loaded classes with a **+ Instance** button per row
(spawns a new object referencing that class at a default position), and a
per-class editor (mesh/texture/scale/dynamic/trigger/animation/script —
the same fields the plain object editor exposes, plus **Save Class** and
**Apply to All Instances**).

Selecting a class-spawned object in Scene Objects shows a reduced panel —
Name/Position/Rotation plus "Class: `<name>`", **Edit Class** (jumps to it
in the Classes section), and **Unlink from Class** (clears `class`,
freezing the object's current resolved values as its own independent
copy — from then on it's a plain object again).

## Skybox

A simple vertical-gradient sky, drawn as `dust_engine::renderer::SkyboxPass` —
no cubemap or dome mesh. It reuses the fullscreen-triangle trick already
in `post.rs`'s `CompositePass`: a single triangle covering the whole
screen via `gl_VertexID`, no vertex buffer needed. The fragment shader
reconstructs each pixel's world-space view ray from the inverse
view-projection matrix and mixes a horizon color and a zenith color by the
ray's Y component.

Drawn **first**, before any mesh, with `gl.depth_mask(false)` so it never
writes depth — normal opaque geometry drawn afterward occludes it exactly
like it would occlude a real sky. It renders into the same low-res
offscreen target as everything else, so it's pixelated/dithered along
with the rest of the scene rather than looking like a crisp UI overlay.

`RenderParams` gains `sky_horizon_color`/`sky_zenith_color: [f32; 3]`,
editable in the Render Params panel's new **Sky** section. Both are
`#[serde(default)]` (falling back to black) so profiles saved before this
feature still load — re-save them once to pick up the default blue
gradient.

## Particle effects

`dust_engine::particles` is a small, non-instanced particle system — each live
particle is one draw call, deliberately simple and PS2-chunky rather than
built for thousands of particles at once (easy to swap for instancing
later if a scene needs that).

```rust
pub struct ParticleEmitterDef {
    pub rate_per_sec: f32,                      // 0 = burst-only, never auto-emits
    pub lifetime_min: f32, pub lifetime_max: f32,
    pub speed_min: f32, pub speed_max: f32,
    pub spread_deg: f32,                         // cone half-angle around +Y
    pub gravity_scale: f32,
    pub start_size: f32, pub end_size: f32,
    pub start_color: [f32; 4], pub end_color: [f32; 4],  // lerped by age
    pub max_particles: u32,
}
```

A `ParticleEmitter` runtime component owns a `Vec<Particle>` pool and a
tiny hand-rolled xorshift32 PRNG (seeded from the OS via
`std::hash::RandomState` — no `rand` dependency for something this small).
`dust_engine::particles::step(world, dt) -> Vec<Entity>` ages/culls particles,
auto-emits for continuous emitters (`rate_per_sec > 0`), and returns
one-shot burst emitters (`rate_per_sec <= 0`) that have gone empty, so
`Game::update` can despawn them — this is how a triggered burst cleans
itself up without becoming permanent level clutter.

Rendering (`dust_engine::renderer::ParticlePass`) draws each particle as a
camera-facing billboarded quad (`mesh::primitives::quad()`), rotation
taken from the camera's world-space orientation (extracted from the
inverse view matrix), with GL blending enabled around just this pass — the
**first thing in the engine to use blending** — so particles fade instead
of punching hard-edged holes in whatever's behind them.

Placed emitters live in `Level.particle_emitters`
(`LevelParticleEmitter { name, position, def }`), the same
sibling-list-on-`Level` pattern as lights/rig instances. The F2 panel gets
**Add Particle Emitter**; selecting one shows every `ParticleEmitterDef`
field as sliders/drag-values, a live **"Live particles: N"** count, a
**Test Burst (12)** button, and **Delete**. `Sandbox::spawn_burst_at` is
the transient-burst path used by `interact()`'s push demo — no
`LevelObjectMeta`, so it never shows up as level data, just particles that
age out and clean themselves up.

## In-game HUD

`dust_engine::hud::HudState` is deliberately tiny — a title, named progress
bars, and fading toast messages:

```rust
pub struct HudState {
    pub title: Option<String>,
    pub bars: Vec<(String, f32)>,   // (name, fraction 0..=1)
    // toasts: private — go through set_bar/show_toast/tick
}
impl HudState {
    pub fn set_bar(&mut self, name: &str, fraction: f32);   // clamped 0..=1
    pub fn show_toast(&mut self, message: &str, seconds: f32);
    pub fn tick(&mut self, dt: f32);                        // ages/removes expired toasts
}
```

> **Sandbox no longer calls `draw_hud`/`draw_pause_menu`.** Its HUD and pause
> menu are RON documents (see "UI Designer: RON documents in a game" below);
> `HudState` still feeds them. `draw_hud`/`draw_pause_menu` stay for
> `walkaround_demo` and `mushroom_man`, which still draw them by hand.

`dust_engine::ui::draw_hud(ctx, hud)` renders it as a fixed top-left
`egui::Area` — title, each bar as an `egui::ProgressBar`, toasts stacked
below and fading out as their remaining time runs low. The sandbox calls
it from `Game::render`'s egui closure **gated to Play mode only** — the
one deliberate gap in the "editor panels only show in Edit mode" rule,
since a HUD is a gameplay thing with nothing to show while you're editing.
`Sandbox::update` only calls `self.hud.tick(dt)` inside the Play-mode
branch too, so a toast's timer doesn't run out while the game is paused in
Edit mode.

Both scripts and native `Behavior`s reach the HUD through the same
`ScriptApi` trait everything else in the scripting story uses —
`set_hud_bar`/`show_toast` are two more required methods alongside
`log`/`move_by`/etc., and `HostAdapter::call_native` forwards a script's
`hud_bar(name, value)`/`toast(message, seconds)` calls to them. See the
Script Demo Cube's `update` (sets a "Wobble" bar from its bob phase) and
`interact()`'s push demo (`show_toast("Pushed!", 1.5)`) for one example
from each call site.

### Editing the HUD's look

The above is *content* (what the HUD shows); its *look* — position, bar
width/color, title size, toast color — is a separate, editable
`dust_engine::hud::HudStyle`:

```rust
pub struct HudStyle {
    pub anchor_offset: [f32; 2],   // pixels from the top-left corner
    pub bar_width: f32,
    pub bar_fill_color: [f32; 3],
    pub title_font_size: f32,
    pub toast_color: [f32; 3],
}
```

It's bundled directly into `dust_engine::profile::RenderParams` (a `hud:
HudStyle` field, `#[serde(default)]` so profiles saved before this field
existed keep their old look) rather than living in its own file — this
means it's saved/loaded/cycled with the rest of a `ShaderProfile`
automatically, no separate plumbing needed. `draw_hud`'s signature is
`draw_hud(ctx, hud, style)`; call it with `&self.render_params.hud`.

**F1 panel**: a **"HUD"** section right under "Render Params" —
`dust_engine::ui::hud_style_editor(ui, &mut style)` draws drag values for the
anchor offset, a slider for bar width, color pickers for the bar fill and
toast text, and a slider for title font size. Like Render Params, changes
apply live (you'll see it update in Play mode immediately) and are
captured by "Save"/"Save As New" on the shader profile.

## UI Designer: RON documents in a game

The in-game HUD and pause menu can be plain RON documents designed in the
editor's **UI Designer** tab and loaded by the game. `games/sandbox` does this
for both screens: nothing in its Rust code draws them.

**Workflow (RON -> game):**

1. The game keeps its documents in `<asset root>/assets/ui/` (sandbox:
   `games/sandbox/assets/ui/hud.ron`, `pause_menu.ron`) and loads each with
   `dust_engine::ui_designer::UiScreen::load(name, path, fallback)` at startup. A
   missing or invalid file falls back to `examples::hud_doc()` /
   `pause_menu_doc()` and logs a warning; it never aborts the game.
2. Every frame the game calls `screen.poll()` (one `stat`), publishes its state
   as bindings (`UiBindings::set_f32/set_bool/set_text`), then calls
   `screen.show(ctx, &bindings, area)`. `area` is the whole window, or the
   Viewport tab's rect when the editor is docked (the HUD is drawn into the
   viewport there). `show` returns a `UiFrameResult` whose `events` are the
   button/toggle/slider events of this frame.
3. The game maps event names to actions (`games/sandbox/src/game_ui.rs`,
   `action_from_event`): `resume_game`, `exit_to_editor`, `restart_level`,
   `quit_game`, `toggle_stats` (carries the new bool).
4. `EditorRuntime::set_ui_dir(<asset root>/assets/ui)` points the UI Designer
   tab at the same folder: pick `hud.ron` in the Document box, edit, Save.
   `poll()` sees the changed file and the running game swaps the document in
   without a restart. Edits made with an external text editor reload the same
   way. A file that fails to parse (for example saved half-written) is
   reported in the log once and the previous document stays live until the next
   good save.
5. `DUST_PLAY=1` starts the sandbox directly in Play and `DUST_PAUSED=1` in the
   pause menu, which is handy for checking a document without keyboard input.

**Bindings.** A node's `bindings` map (`property -> value name`) connects it to
values the game publishes. Property names the designer understands:

| Property | Node kinds | Value / template |
|---|---|---|
| `text` | Label, Button, Toggle, ProgressBar overlay | template: `"HP {player.health:.0}/{player.max}"`; `{name:.N}` fixes N decimals, `{{`/`}}` escape braces; unknown names stay visible as written |
| `value`, `min`, `max` | ProgressBar, Slider | value name (a number); the bar fill is `(value-min)/(max-min)` |
| `checked` | Toggle | value name (truthy) |
| `visible`, `enabled` | any | value name, `!name` negates; a hidden node takes no layout space |
| `opacity` | any | value name, number 0..1 (multiplied down the tree) |
| `class` | any | value name holding text; its words are added as style classes after the node's own `classes` (a bar turns red with `bar_low`) |

Layout inside a document: anchors + offsets (free layout), `margin`/`padding`,
`HStack`/`VStack`/`Grid`, `Fixed`/`Fill`/`Fit` sizes, `ScaleWithScreen` scaling.

**Values the sandbox publishes** (`game_ui.rs`, `Sandbox::publish_game_ui`):

| Value | Meaning |
|---|---|
| `hud.title`, `hud.has_title` | `HudState::title` |
| `hud.bar.<name>` (0..1), `hud.bar.<name>_pct`, `hud.bar.<name>.visible` | each `HudState` bar, name lower-cased (`health`) |
| `hud.bar.health.class` | `"bar_low"` below 30 % health, else empty |
| `hud.toast.{0,1,2}.text / .alpha / .visible` | first three `HudState` toasts; alpha is the old fade (`min(remaining, 1)`) |
| `play.show_hint`, `play.esc_hint`, `play.f3_hint` | controls chip words (hidden when docked) |
| `level.name`, `perf.fps` (smoothed), `perf.ms` | frame stats |
| `hud.show_stats` | state of the pause menu's "Show frame stats" toggle |
| `net.active`, `net.status` | hosting/joined state and the multiplayer status line |
| `editor_available` | pause menu: "Exit to Editor" vs "Restart Level" |

Limits to know: there is no repeater/list node, so toasts use a fixed number of
slots (three); keyboard/gamepad focus navigation exists in the designer
(`UiFocus`) but the sandbox does not wire it to its pause menu, so the menu is
mouse-only; the multiplayer and player-appearance panels that open next to the
pause menu are still hand-coded egui windows, which is why the sandbox pause
menu has no full-screen dim layer.

### Editor Settings tab

The shell's **Settings** tab edits `EditorPrefs`: keymap preset, UI scale, font
size and font family (default or monospace), viewport grid, the seven theme
colours, layout presets, **Reset theme** and **Reset all settings to defaults**.
Changes are written to `<config dir>/editor_prefs.ron` shortly after you let
go of the mouse (`PrefsAutosave`), and the dock layout is written when the
editor closes (`EditorRuntime::save_layout`). The config dir is chosen by the
game (sandbox: `<asset root>/.editor`), so settings are per game, not per user.

## Save/load (checkpoints)

`dust_engine::save` is intentionally small — a single gameplay checkpoint, not a
full save-file system:

```rust
pub struct SaveData {
    pub level_name: String,
    pub player_position: [f32; 3],
    pub player_yaw: f32,
    pub player_pitch: f32,
    pub saved_at_elapsed: f32,
    pub player_health: Option<(f32, f32)>,      // (current, max)
    pub despawned_names: Vec<String>,
    pub extra: HashMap<String, f32>,
}
```

Same `load_from_file`/`save_to_file` RON pattern as every other asset type
in the engine, written to a game's `saves/` folder — but unlike
profiles/rigs/classes/levels, there's exactly one file
(`saves/checkpoint.ron`), overwritten each time, since this is "get back
to roughly where I was," not a multi-slot save system. The three fields
above (all `#[serde(default)]`, so an old checkpoint file still loads) carry
just enough world-state delta to make a reload feel right without a full
per-object snapshot: `player_health` restores current/max HP;
`despawned_names` (tracked live on `Sandbox.despawned_since_load`, pushed
whenever a named object/character is despawned — an eaten mushroom pickup,
`AiEvent::CharacterDied`) is replayed on load by re-despawning whatever
`apply_level` just freshly respawned; `extra` is a small game-defined scalar
bag the engine never reads itself (Mushroom Man stashes which mushroom
ability is currently active there, keyed `"active_mushroom"`). **Full
per-object/script custom state (inventory, arbitrary world flags, quest
progress, ...) is still explicitly out of scope** — add it once a concrete
game actually needs that, rather than guessing at a shape now that would
likely be wrong for whatever that game turns out to be.

**F9** saves — Play mode only, since there's no player entity to snapshot
in Edit mode. **F10** loads, entering Play mode first if you're currently
in Edit mode (re-using the exact same `enter_play_mode` the F3 hotkey
calls, so a loaded checkpoint gets a freshly spawned player/physics state
under it, not a stale one). Both show a HUD toast ("Checkpoint saved"/
"Checkpoint loaded") so the otherwise-silent snapshot is visible — the
same `hud.show_toast` call site as the interact-push demo. If the
checkpoint's `level_name` doesn't match whatever level is currently
loaded, F10 logs a warning and repositions the player anyway rather than
switching levels for you — a checkpoint this simple isn't trying to
reconstruct "which level should be active," just where you were standing.

## Scripted smoke tests

`dust_engine::app::App::run_scripted` is a separate loop from `App::run` (not a
flag on it, so the production path stays completely untouched) that drives
a game with an Oxidized script instead of a human — a real SDL2
window still opens (there's no headless mode in this engine). Each frame,
after polling real SDL events (so window-close still works), it calls the
script's `fn tick(frame: Int, t: Float)` once (`frame` counts calls from 0,
`t` is `ctx.time.elapsed`). The script is compiled with the **smoke** profile
(`dust_engine::script::ox::SmokeDriver`, spec in `build/spec.rs::SMOKE_SPEC`), whose
whole "standard library" is:

- **`press(key_name)`** / **`release(key_name)`** — queues a synthetic SDL
  key event (by SDL key name, e.g. `"W"`, `"F3"`), fed through the exact
  same `ctx.input.handle_event` + `game.handle_event` path a real keypress
  takes, right after `tick` returns. An unknown key name fails the run.
- **`time()`** — same value as `t`.
- **`assert_true(cond: Bool, message)`** / **`assert_eq(a: Float, b: Float, message)`** /
  **`assert_eq_str(a, b, message)`** — a failure logs the message and marks the
  run failed; the run ends at the end of that frame.
- **`finish(ok: Bool)`** — ends the run. `run_scripted` returns `Ok(0)` if the
  run passed (and no assertion failed), `Ok(1)` otherwise — including the
  window closing early or `tick` panicking, both treated as failures. An
  earlier assertion failure wins over a later `finish(true)`.
- **`log(msg)`** — logged as `[smoke test] msg`.
- **`get_var(name, default)`** / **`set_var(name, value)`** — script functions
  are stateless, so a script drives itself as an explicit phase state machine
  through these (see the `enter_phase`/`seconds_in_phase` helpers in the
  bundled scripts) across many `tick` calls.

See `games/sandbox/scripts/smoke/basic_playthrough.ox` pattern (enter Play mode,
walk, assert time advanced, finish), `games/mushroom_man/scripts/smoke/talk_to_sprite.ox`
(adds a turn-and-interact phase) and the three `docked_editor.ox` scripts. Smoke
scripts cannot read game state; checks are log lines (`games/sandbox/scripts/smoke/check_docked_editor.sh`
greps them).

Each game's `main()` checks for a `--smoke-test <script.ox>` argument before
falling through to the normal interactive `App::run`; when present, it builds
(or reuses the cached build, `<script dir>/.ox-build`) and loads the script,
calls `App::run_scripted`, then `std::process::exit`s with the returned code.
Build cost: ~1-2 s cold (oxidized + rustc, opt-level 1), ~0 s cached. A `.pss`
path is rejected with an error.

```
cargo run -p sandbox -- --smoke-test scripts/smoke/basic_playthrough.ox
cargo run -p mushroom_man -- --smoke-test scripts/smoke/talk_to_sprite.ox
```

## Multiplayer

`dust_engine::net` adds listen-server multiplayer — one player's own instance
hosts (`NetHost`), 2-4 total players connect directly over LAN
(`NetClient`), full world state syncs so everyone sees the same AI
characters, physics props, dialogue, and level transitions. No dedicated
server, no NAT traversal/matchmaking, no async runtime: plain non-blocking
`std::net::{TcpListener, TcpStream}`, polled once per `Game::update()` call
alongside everything else in the engine's single-threaded frame loop.

**Transport & framing**: length-prefixed `bincode` frames (`u32` LE length
+ payload) over TCP — TCP's head-of-line blocking is a non-issue at LAN
latencies and a 20Hz snapshot rate, and its built-in ordering/reliability
is free, so there's no hand-rolled UDP netcode. `dust_engine::net::framing::NetConnection`
wraps one `TcpStream`: reads accumulate into a buffer (`WouldBlock` just
means "nothing more this frame," not an error) and drain complete frames,
leaving any trailing partial frame for next frame; writes queue into a
`VecDeque<Vec<u8>>` and drain opportunistically, so a send never blocks.
Same "poll once per frame, never block" contract as `HotReloadWatcher::poll_events`.

**Protocol** (`dust_engine::net::protocol`): `ClientMessage::{Hello, Input,
Interact, DialogueChoice, Disconnect}` and `ServerMessage::{Welcome,
Reject, PlayerJoined, PlayerLeft, CharacterSpawned, Snapshot, Dialogue,
DialogueClosed, LevelTransition}`. `Hello`/`Welcome` carry a
`PROTOCOL_VERSION` that must match exactly — unlike `Level`/`SaveData`'s
RON + `#[serde(default)]` fields, `bincode`'s wire format is positional,
so there's no cross-version compatibility; a mismatch gets `Reject`.
`Snapshot` broadcasts host→all at a fixed **20 Hz** (`NetHost::should_send_snapshot`'s
own `dt` accumulator, decoupled from render frame rate) with **full state,
no delta compression** — the simplest thing that works at 2-4 players.

**Entity replication**: `NetId(u32)` is a host-assigned identity,
independent of `hecs::Entity` (which is process-local and not
serde-serializable). Every player, `CharacterMeta` character, and dynamic
physics prop gets one via a `Networked(NetId)` component; static level
geometry never does — the `Level` sent whole in `Welcome`/`LevelTransition`
plus the existing `apply_level` already reconstructs it identically on
every peer. A joining/transitioning client learns which of its own
freshly-`apply_level`'d entities map to which `NetId` via `Welcome`/
`LevelTransition`'s `named_net_ids: Vec<(String, NetId)>` side-list,
matched by name (`Sandbox::find_entity_by_name`) rather than spawn order —
robust to `apply_level`'s internal ordering, at the cost of requiring
object/character names to be unique within a level (not currently
enforced by the F2 editor). Despawned networked entities (an
`AiEvent::CharacterDied`) get flushed into the next `Snapshot`'s `removed`
list via `Sandbox.net_removed`.

**Movement is host-authoritative**, but a straight 20Hz-snapshot-only
approach read as visibly choppy, so two layers of client-side smoothing
sit on top of it:

- **The local player's own X/Z** is dead-reckoned every frame: a client
  computes its own camera-relative `move_dir` (`Sandbox::compute_move_dir`,
  factored out of `update_player_input`) and both sends it as
  `ClientMessage::Input` *and* integrates it directly into its own
  `Transform.position` locally, so movement feels instantly responsive
  instead of stepping once per snapshot. Both X/Z and **Y** (jump/fall,
  which has no local prediction of its own) are then reconciled against
  the latest `Snapshot`'s value (`Sandbox.net_local_target`, updated in
  `update_networking`), but not the same way: a hard snap on arrival was
  tried first and made the joining client's own view visibly wobble
  every ~50ms (even tiny natural client/host timing differences show up
  as a full pop that way); a continuous per-frame blend toward the
  target was tried next and *still* wobbled, because dead reckoning
  keeps pushing X/Z forward every frame while the blend simultaneously
  pulls it back toward a target that's merely a few frames stale —
  fighting the player's own momentum reads as a persistent drag/wobble
  even with no real desync at all. The fix (`update_player_input`):
  **Y** blends every frame unconditionally (`NET_LOCAL_SMOOTH_RATE`),
  since it has nothing else pulling on it; **X/Z** only blends once the
  gap from `net_local_target` exceeds a small dead zone
  (`NET_LOCAL_XZ_DEADZONE`), so ordinary tracking noise is left alone
  entirely and correction only ever kicks in for a genuine desync — the
  host stopping the player at a wall the local prediction doesn't know
  about.
- **Everything else** (other players, AI characters, dynamic physics
  props) gets a `NetTarget` component instead of a hard `Transform`
  overwrite: `Sandbox::smooth_networked_transforms` lerps position/rotation
  toward the latest snapshot value every frame, so a pushed box or a
  remote player's avatar reads as continuous motion instead of a 20Hz
  flipbook.

A client's own `fp_camera.yaw`/`pitch` still update instantly and locally
(pure view direction, nothing to reconcile).

**Join flow**: host clicks **Host Game** (new multiplayer panel, drawn
alongside the pause menu — `dust_engine::ui::draw_multiplayer_panel`) →
`NetHost::bind(dust_engine::net::DEFAULT_PORT)`. A joiner types `IP:port`,
clicks **Join** → `NetClient::connect` (one bounded ~2s blocking call on
the button click, not per-frame — an acceptable one-time hitch). The
client sends `Hello`; the host checks `PROTOCOL_VERSION` and
`dust_engine::net::MAX_PLAYERS` (4), assigns a `NetId`, spawns a visible
`RemotePlayer`-tagged avatar (`Sandbox::spawn_remote_player_at` — same
physical shape as the local player plus a cube `MeshRenderer`, since a
remote player needs to actually be seen), and replies `Welcome` with the
host's current `Level` plus `named_net_ids`. The host also synthesizes
`PlayerJoined` for its own player and every other already-connected remote,
so a late joiner learns about everyone. `spawn_ring_offset(base_position,
base_yaw_deg, net_id)` gives every player a small deterministic spread
around a shared spawn point (initial join or a level transition) so 2-4
players don't stack on top of each other — computed identically host- and
client-side from just a `NetId`, so no extra position data travels over
the wire for it.

**Level transitions are host-only authoritative by default**, and this
falls out of existing code almost for free: `transition_to_level` was
already only ever triggered by `player_entity`-specific trigger-overlap
checks, so a remote player's entity overlapping the same trigger in the
host's own simulation was never checked — walking into an exit trigger as
a non-hosting client does nothing by default. When the host's own trigger
fires a transition, it broadcasts `ServerMessage::LevelTransition` and
repositions every remote player via `spawn_ring_offset`, same as the
initial join.

**Per-player level-switch permission**: the host can grant individual
connected players the ability to trigger transitions themselves, via a
**"Players"** list in the multiplayer panel (host-only — a client isn't
shown the roster, it just discovers the exit works once granted) with a
**"Can switch levels"** checkbox per name, backed by
`Sandbox.remote_level_switch_permission: HashMap<NetId, bool>`. This is
enforced entirely host-side with no protocol change:
`check_remote_level_switch_triggers` (called from `Game::update`
alongside the host's own `trigger_overlaps` handling) separately checks
whether any *permitted* remote player's entity is overlapping a
level-transition trigger this frame, deliberately bypassing the full
`on_trigger_entered` dispatch (sound/behavior-script/screen-flash/
camera-shake) since those are tied to what the *host's* own screen is
showing, not a remote player's.

**AI perception is aware of every connected player**, not just the
host's own local one: `dust_engine::ai::step` takes `player_positions: &[(Entity,
Vec3)]` — the host's own local player plus every `remote_players` entity
— and each character reacts to whichever one is physically nearest,
recomputed every call so retargeting mid-chase (or mid-attack-cooldown)
follows whoever gets closer. `AiEvent::AttackedPlayer` now carries which
player entity was actually hit (`player: Entity`), so damage lands on the
right one; the host only flashes/shakes its *own* screen when its own
local player was hit (a remote player taking damage has no bearing on
what the host is looking at, same reasoning as the level-switch-permission
check above). There's still no shared death/respawn flow for anyone but
the host's own local player — a remote player reaching 0 HP is just fully
healed in place with a toast, rather than being left stuck or restarting
the level for everyone over one player's death.

**Every flavor of `interact()` replicates** — dialogue, hitting a hostile
character, a scripted object's own `on_interact`, and pushing a dynamic
object — all through the same single `ClientMessage::Interact { target:
NetId }` message. `Sandbox::interact_or_send` dispatches — offline or
hosting, it's the normal local `interact()`; under `NetMode::Client`, it
searches the client's own locally-mirrored `Networked` characters *and*
objects for the nearest one in range and sends the target's `NetId`
instead of touching anything directly. The host resolves what that
target actually means, using the exact same order `interact()` itself
checks locally:

1. Has a `Dialogue` component → unicast dialogue (advances/replies with
   `Dialogue`/`DialogueClosed` to just that one connection).
2. A `Hostile` character with `Health` → the same fixed
   `PLAYER_ATTACK_DAMAGE` hit; death is handled next frame via the usual
   `AiEvent::CharacterDied` path, same as any other death.
3. Has a `BehaviorSlot` → its own `on_interact` runs, exactly as if the
   host's own player had interacted with it.
4. Otherwise → a push, using the same outward impulse `interact()` uses
   locally, computed from the *requesting client's own* remote-avatar
   position (`self.remote_players.get(&requester)`) rather than the
   host's own player, so the shove direction is correct.

For a scripted/behavior object to ever be reachable this way, it has to
be tagged `Networked` while hosting — `start_hosting`/`transition_to_level`'s
object-tagging loop covers anything `interact()` itself would consider
interactable (`RigidBody` *or* `BehaviorSlot`), not just dynamic physics
props, specifically so this resolves for non-dynamic scripted objects
too. None of these resulting state changes (`Health.current`,
`RigidBody.velocity`, a `Dialogue`'s current node) need an extra
broadcast — they reach every client for free via the next periodic
`Snapshot` (or, for dialogue, the direct unicast reply above).
`Sandbox.net_dialogue` is the client-side sibling to `active_dialogue`,
driving the same `draw_dialogue_choices` overlay and 1-9 number-key
handler.

**Particle bursts replicate too**, via `ServerMessage::ParticleBurst
{ position, def, count }` — a cosmetic-only `ParticleEmitter` (a pushed
object's puff, a hostile hit, a character death) has no `Networked`
entity of its own to ride along on a `Snapshot`, so `Sandbox::spawn_burst_at`
itself broadcasts to every connected client whenever it's called while
hosting (`broadcast_if_hosting` — a silent no-op offline/as a client),
and the client's own handler for the message just calls the same
`spawn_burst_at` again locally. Every one of `spawn_burst_at`'s existing
call sites gets this for free with no changes needed at the call site
itself — without it, particle feedback for a network-triggered action
(a client pushing an object, a hostile character dying to a client's
attack) would only ever have shown up on the *host's* own screen.

**Disconnect & automatic reconnect**: the host detects a dropped
connection, despawns that player's avatar, and broadcasts `PlayerLeft`. A
client whose connection unexpectedly drops shows a toast and
automatically retries — `update_networking` counts down
`Sandbox.net_reconnect_attempts_left` (5 attempts, 3s apart) and re-runs
the exact same `NetClient::connect` call `join_game` makes from a button
click, reusing the last-known `net_join_address`/`net_player_name`; the
multiplayer panel shows "Reconnecting... (N attempts left)" with a
**Cancel** button in place of the normal buttons while this runs. A
successful reconnect rejoins as a brand-new player (fresh avatar, fresh
spawn point) — this is a convenience retry, not a session/identity resume,
so no health/position/inventory state survives the drop. A manual
**Disconnect** click, or clicking **Join**/**Host** again, always cancels
any in-progress reconnect sequence. Deliberately rejected connections
(`ServerMessage::Reject`, e.g. a version mismatch or a full lobby) don't
trigger a retry, since retrying would just hit the same rejection again.
There's still no host migration: if the host quits, the session ends for
everyone.

**Stated simplifications**: no client-side prediction/interpolation for
anything but position (see the smoothing section above — mouse-look is
still instant, but there's no rewind/replay reconciliation for e.g. a
missed dodge), no delta compression, no host migration, no NAT
traversal/matchmaking/LAN discovery (direct `IP:port` only), no F2 level
editing while a session is active, no chat/voice, and a trust-the-LAN-peer
model with no anti-cheat beyond what `physics::step` already imposes.

`games/sandbox`, `games/mushroom_man`, and `games/walkaround_demo` all
carry the identical implementation. To try it: run two copies of the same
game, **Esc** to pause in one, click **Host Game**, then in the other type
`127.0.0.1:7777` (or the host's LAN IP) and click **Join**.

## Systems: `Schedule` and `Resources`

> **Status.** Implemented and unit-tested in the engine; no game in this
> workspace runs its frame through a `Schedule` yet. The games still hand-order
> work in `Game::update`.

`dust_engine::resources::Resources` is a typed singleton map — at most one value per
Rust type, keyed by `TypeId`. `get`/`get_mut`/`remove` return `Option`;
`require`/`require_mut` panic with a message naming the missing type.
`dust_engine::schedule::Schedule` is a registry of named systems. A system is a
`fn` or closure taking a `SystemCtx { world, resources, dt }`; the schedule
decides when each runs:

- **`Stage`** gives hard ordering: `First < PreUpdate < Update < PostUpdate < Last`
  (default `Update`).
- **`before(name)` / `after(name)`** give fine ordering by name. A constraint
  that contradicts stage order is a cycle.
- Ties run in stage order, then registration order — fully deterministic.
- **`run_if(|world, res| ..)`** / **`run_if_resource::<T>(|t| ..)`** skip a
  system without removing it from the ordering; `set_enabled(name, bool)`
  toggles one at runtime.

The schedule does not own the world and does not touch the `Game` trait: a
game keeps its `World` and `Resources` and calls `schedule.run(..)` from its
own `update`, moving one feature at a time.

```rust
use dust_engine::ecs::{Transform, World};
use dust_engine::glam::Quat;
use dust_engine::resources::Resources;
use dust_engine::schedule::{Schedule, Stage, SystemCtx};

struct Score(u32);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Edit,
    Play,
}

// A system is a plain fn or closure over a SystemCtx { world, resources, dt }.
fn spin(ctx: &mut SystemCtx) {
    let dt = ctx.dt;
    for (_, t) in ctx.world.query_mut::<&mut Transform>() {
        t.rotation = Quat::from_rotation_y(dt) * t.rotation;
    }
}

fn build_schedule() -> Schedule {
    let mut schedule = Schedule::new();
    schedule
        .add_system("spin", spin)
        .stage(Stage::Update) // the default; First < PreUpdate < Update < PostUpdate < Last
        .run_if_resource::<Mode>(|m| *m == Mode::Play);
    schedule.add_system("score", |ctx| ctx.res_mut::<Score>().0 += 1).after("spin");
    schedule
}

fn setup() -> Resources {
    let mut res = Resources::new();
    res.insert(Score(0));
    res.insert(Mode::Play);
    *res.get_or_insert_with(|| 0u32) += 1; // one value per Rust type
    res
}

// Call from Game::update. Ordering mistakes (cycles, unknown names, duplicate
// names) come back as ScheduleError, never a panic.
fn tick(schedule: &mut Schedule, world: &mut World, res: &mut Resources, dt: f32) -> anyhow::Result<()> {
    let ran = schedule.run(world, res, dt)?;
    log::trace!("{ran} systems ran");
    Ok(())
}
```

Cycles, unknown names in `before`/`after`, and duplicate names are reported as
`ScheduleError` from `Schedule::build()` / `run()`; `try_add_system` returns the
duplicate error instead of panicking like `add_system`.

## Effects: `Lifetime`, `Fade`, `TextureOverride`

> **Status.** Implemented and unit-tested. Games still use their own
> spawn/age/despawn bookkeeping; none has been ported (see
> [docs/ROADMAP.md](docs/ROADMAP.md)).

`dust_engine::effects` moves "spawn something short-lived, shrink it, despawn it"
and "swap a texture, remember the old one" into ECS components:

- `Lifetime { age, max }` — the entity is despawned by `effects::tick` when
  `age >= max`.
- `Fade` — scale and/or alpha animated over the entity's `Lifetime`
  (`Fade::shrink()`, `Fade::fade_out()`, `with_scale`, `with_alpha`,
  `with_curve(FadeCurve::{Linear, EaseOut})`). Scale is written into the
  `Transform`; alpha is stored on the `Fade` (`fade.alpha()`) for your
  renderer to read — **`MeshRenderer` has no alpha channel yet**, so the
  engine renderer ignores it. A `Fade` without a `Lifetime` does nothing.
- `TextureOverride { original, remaining }` — created by
  `apply_texture_override(world, entity, texture, duration)`; the first original
  is kept if you apply twice, and `restore_texture` puts it back. Despawning the
  entity drops the saved texture with it.

```rust
use std::sync::Arc;

use dust_engine::ecs::{Entity, MeshRenderer, Transform, World};
use dust_engine::effects::{self, Fade, FadeCurve, Lifetime};
use dust_engine::glam::Vec3;
use dust_engine::mesh::GpuMesh;
use dust_engine::texture::GpuTexture;

// A dash ghost: shrinks over 0.4 s, then despawns itself.
fn spawn_ghost(world: &mut World, mesh: Arc<GpuMesh>, tex: Arc<GpuTexture>, at: Vec3) -> Entity {
    world.spawn((
        Transform::from_position(at),
        MeshRenderer { mesh, texture: Some(tex) },
        Lifetime::new(0.4),
        Fade::shrink().with_curve(FadeCurve::EaseOut),
    ))
}

// Swap an enemy's texture for 3 s; the original is remembered and put back.
fn freeze(world: &mut World, enemy: Entity, frozen: Arc<GpuTexture>) {
    effects::apply_texture_override(world, enemy, Some(frozen), Some(3.0));
}

fn thaw_now(world: &mut World, enemy: Entity) {
    effects::restore_texture(world, enemy);
}

// Once per frame: advance lifetimes, apply fades, tick overrides, despawn the expired.
fn update_effects(world: &mut World, dt: f32) {
    let despawned: Vec<Entity> = effects::tick(world, dt);
    let _ = despawned;
}
```

`effects::tick` runs the three steps in order (`tick_lifetimes`, `apply_fades`,
`tick_texture_overrides`) and then despawns expired entities; call the pieces
individually if you need a different order.

## Tunables and telemetry

> **Status.** Implemented and unit-tested; not yet used by a game.

`dust_engine::tunables::Tunables` loads a flat RON map of name → value
(`{ "music.near": 12.0, "ghost.life": 4, "debug.god": true, "music.track": "a" }`)
and lets you retune a running game: getters (`f32`, `i64`, `bool`, `string`)
take a compiled-in default and never fail — a missing name or wrong type
returns the default and is recorded in `Tunables::misses()`. An integer is
accepted by `f32` getters. `reload_if_changed()` (one `metadata` call per
frame) or `reload_if_touched(paths)` (feed it `HotReloadWatcher::poll_events`)
reloads; a file that fails to parse keeps the old values and sets
`last_error()`; `changes()` / `take_changes()` list what moved.
`Tunables::for_build(path)` ignores the file in release builds unless the
`DUST_TUNABLES` env var is set, so players can't retune a shipped game.

`dust_engine::telemetry::Telemetry` is the balance-CSV hook: `record(event,
&[(field, f64)])` appends rows, writing an `event,<fields>` header the first
time an event name (or its field list) appears. Output is buffered (`flush()`
or drop). `Telemetry::from_env(var)` is disabled unless the variable is set, and
a disabled hook costs one branch per call. IO errors disable it rather than
panic.

```rust
use dust_engine::telemetry::Telemetry;
use dust_engine::tunables::Tunables;

// assets/tunables.ron:   { "music.near": 12.0, "ghost.life": 0.4, "debug.god": false }
fn setup() -> (Tunables, Telemetry) {
    (
        // Debug builds (or DUST_TUNABLES set) read the file; release uses the defaults below.
        Tunables::for_build("assets/tunables.ron"),
        // Disabled unless MYGAME_BALANCE=path/to/balance.csv is set.
        Telemetry::from_env("MYGAME_BALANCE"),
    )
}

fn each_frame(tun: &mut Tunables, tel: &mut Telemetry, t: f32, hp: f32) {
    if tun.reload_if_changed() {
        for c in tun.take_changes() {
            log::info!("tunable {} changed: {:?} -> {:?}", c.name, c.old, c.new);
        }
    }
    // Getters never fail: a missing or wrong-typed name returns the default.
    let near = tun.f32("music.near", 12.0);
    let ghost_life = tun.f32("ghost.life", 0.4);
    let god = tun.bool("debug.god", false);
    let _ = (near, ghost_life, god);

    // Header row is written the first time an event name appears.
    tel.record("kill", &[("t", t as f64), ("hp", hp as f64)]);
}
```

## Input action maps

> **Status.** Implemented and unit-tested (including a headless input
> source); the games' own input code does not use it yet.

`dust_engine::input_map` lets you name an *action* once and bind it to devices in
data. A map is RON — see the fully commented `docs/examples/input_map.ron`:

```ron
(version: 1,
 contexts: [(name: "gameplay"), (name: "menu", enabled: false, exclusive: true)],
 actions: [
   (name: "jump", context: "gameplay", bindings: [Key("Space"), PadButton("A")]),
   (name: "move", context: "gameplay", bindings: [
       Composite(up: Key("W"), down: Key("S"), left: Key("A"), right: Key("D")),
       Stick(x: "LeftX", y: "LeftY")]),
 ])
```

Bindings are `Key`, `Mouse`, `PadButton`, `PadAxis`, `Pair` (1-D, e.g. Q/E),
`Composite` (WASD-style vec2) and `Stick` (radial deadzone). `ActionState` is
resolved once per frame from any `InputSource` (`dust_engine::input::Input` and
`headless::HeadlessInput` both implement it) and answers `pressed`,
`just_pressed`, `just_released`, `axis` and `vec2` by action name; unknown
actions read as idle. Contexts are layers: an enabled `exclusive` context mutes
every context declared before it, and actions with no context are always live.
vec2 uses +y up.

```rust
use std::path::Path;

use dust_engine::input::Input;
use dust_engine::input_map::{ActionMap, ActionState};

struct Controls {
    map: ActionMap,
    state: ActionState,
}

fn load_controls() -> anyhow::Result<Controls> {
    // Shipped defaults, with the player's rebinds layered on top (missing file is fine).
    let (map, warnings) = ActionMap::load_layered(
        Path::new("assets/input_map.ron"),
        Path::new("assets/input_overrides.ron"),
    )?;
    for w in warnings {
        log::warn!("{w}");
    }
    let state = ActionState::new(&map);
    Ok(Controls { map, state })
}

// Once per frame, e.g. at the top of Game::update.
fn update(c: &mut Controls, input: &Input, paused: bool) {
    // An enabled `exclusive` context mutes every context declared before it.
    c.state.set_context_enabled("menu", paused);
    c.state.resolve(&c.map, input);

    if c.state.just_pressed("jump") {
        // ...
    }
    let movement = c.state.vec2("move"); // +x right, +y up, length <= 1
    let throttle = c.state.axis("throttle"); // -1..1
    let _ = (movement, throttle);
}
```

Rebinding is `ActionMap::rebind` with a `ConflictPolicy` (`Reject`, `Swap`,
`Allow`) and `RebindState::poll` to capture the next input; player rebinds are
saved as a separate overrides RON (`save_overrides` writes only actions that
differ from the shipped defaults) and layered back with `load_layered`.
`draw_rebind_panel` / `show_rebind_window` provide a ready-made egui settings UI.

## Skeletal animation

> **Status.** The math, state machine, CPU skinning, glTF import and RON asset
> format are unit-tested. **`SkinnedMesh`'s GL upload and drawing have never run
> on a real GL context**, no game or editor draws a skinned mesh yet, there is
> no GPU-skinning path, and glTF import reads the first skin only.

`dust_engine::skeletal` is separate from the older `dust_engine::rig` (rigid parts) and
`dust_engine::animation` (orbit/bob). Layers, bottom up: `pose` (joint transforms,
shortest-arc blending, additive layers), `skeleton` (hierarchy, bind pose,
inverse-bind, joint palette), `clip` (keyframed `AnimationClip`s, optional root
motion), `animator` (`AnimatorController` asset + `SkeletalAnimator`
component), `skin` (per-vertex joints/weights, `SkinnedMeshData::skin`,
`SkinnedMesh` GPU wrapper) and `import` (`load_gltf_skinned`,
`AnimationSet::load_from_file/save_to_file`, `convert_gltf_to_ron`).

A controller names its parameters (float/bool/trigger), states (a clip or a
1-D `Blend1D`), and transitions with conditions, crossfade duration and an
optional `exit_time`. `SkeletalAnimator::update` is a pure function of state
and `dt`, so a fixed `dt` sequence gives bit-identical output.

```rust
use std::path::Path;
use std::sync::Arc;

use dust_engine::ecs::{Transform, World};
use dust_engine::schedule::Schedule;
use dust_engine::skeletal::import::load_gltf_skinned;
use dust_engine::skeletal::{
    skeletal_animation_system, AnimationSet, AnimatorController, Compare, Condition, Motion, ParamDef,
    ParamValue, SkeletalAnimator, StateDef, TransitionDef, SKELETAL_ANIMATION_SYSTEM,
};

fn state(name: &str) -> StateDef {
    StateDef { name: name.into(), motion: Motion::Clip { clip: name.into() }, speed: 1.0 }
}

fn controller() -> AnimatorController {
    let cond = |compare| vec![Condition { parameter: "speed".into(), compare }];
    AnimatorController {
        name: "hero".into(),
        parameters: vec![ParamDef { name: "speed".into(), default: ParamValue::Float(0.0) }],
        states: vec![state("idle"), state("walk")],
        transitions: vec![
            TransitionDef { from: Some("idle".into()), to: "walk".into(), conditions: cond(Compare::Greater(0.1)), duration: 0.2, exit_time: None },
            TransitionDef { from: Some("walk".into()), to: "idle".into(), conditions: cond(Compare::Less(0.1)), duration: 0.2, exit_time: None },
        ],
        default_state: "idle".into(),
    }
}

fn spawn_hero(world: &mut World, schedule: &mut Schedule) -> anyhow::Result<()> {
    // Skeleton + clips (+ skinned meshes) straight from a glTF/GLB...
    let model = load_gltf_skinned(Path::new("assets/models/hero.glb"))?;
    // ...or from the RON form written by `convert_gltf_to_ron`:
    // let set = AnimationSet::load_from_file(Path::new("assets/anims/hero.set.ron"))?;
    let set: Arc<AnimationSet> = Arc::new(model.set);
    let anim = SkeletalAnimator::new(set, Arc::new(controller()))?;
    world.spawn((Transform::default(), anim));

    // Updates every animator and applies root motion to the Transform.
    schedule.add_system(SKELETAL_ANIMATION_SYSTEM, skeletal_animation_system);
    Ok(())
}

fn drive(world: &mut World, speed: f32) {
    for (_, anim) in world.query_mut::<&mut SkeletalAnimator>() {
        anim.set_float("speed", speed);
        // anim.palette() is the joint palette for SkinnedMesh::update(gl, palette)
        let _palette = anim.palette();
    }
}
```

## Headless testing

`dust_engine::headless` runs game and editor logic frame by frame with no window,
no GL context and no SDL video: `Headless` owns a `World`, `Resources` and a
`Schedule` and steps them with a fixed `dt`. `with_default_systems()` adds
`"physics"` (`Stage::PostUpdate`) and `"effects"` (`effects::tick`,
`Stage::Last`) so your `Update` systems run before both. Scripted input —
`press(frame, key)`, `release`, `tap`, `set_axis` — is applied at the start of
the step where `clock().frame == frame` and read through the `HeadlessInput`
resource. `step`, `step_n`, `run_until`, `spawn`, `count`, `entities_with`,
`get`, `position` and friends cover the rest; `headless::fakes` provides
GL-free stand-ins for components that hold GPU handles. `EditorHeadless` adds
an `EditorState` + `EditorShell` so Play / Stop / undo can be tested too (it
only steps the simulation while in Play).

```rust
use dust_engine::ecs::Transform;
use dust_engine::effects::Lifetime;
use dust_engine::headless::{EditorHeadless, Headless, HeadlessInput};

#[test]
fn lifetime_entity_expires_after_four_frames() {
    let mut h = Headless::with_default_systems(); // adds "physics" and "effects" systems
    h.set_dt(0.25); // powers of two keep the f32 sums exact
    let spark = h.spawn((Transform::default(), Lifetime::new(1.0)));

    // Scripted input: press "jump" on frame 2, release on frame 5.
    h.press(2, "jump");
    h.release(5, "jump");
    h.add_system("count_held", |ctx| {
        if ctx.res::<HeadlessInput>().is_down("jump") {
            *ctx.res_mut::<u32>() += 1;
        }
    });
    h.resources.insert(0u32);

    assert_eq!(h.run_until(100, |h| !h.contains(spark)), Some(4));
    h.step_n(6, 0.25);
    assert_eq!(h.clock().frame, 10);
    assert_eq!(*h.resources.require::<u32>(), 3); // held on frames 2, 3, 4
}

#[test]
fn editor_play_and_stop_without_a_window() {
    let mut e = EditorHeadless::new();
    assert_eq!(e.step(1.0 / 60.0), false); // only steps while in Play
    e.play();
    assert_eq!(e.step_n(10, 1.0 / 60.0), 10);
    e.stop(); // restores the pre-Play snapshot
}
```

`dust_engine::script::ox::SmokeDriver` is a different tool: it drives a *running*
game from an `.ox` script (see "Scripted smoke tests").

## The docked editor

The engine has two editors. The **legacy** F1/F2 panels live in each game's
`main.rs` (see "The editor GUI"). The **docked editor** is
`dust_engine::editor::runtime::EditorRuntime`: a dockable window with Viewport,
Hierarchy, Inspector, Assets, Console, Profiler, UI Designer and Settings tabs
plus any host tabs, gizmos (move/rotate/scale), viewport picking, undo/redo,
Play / Pause / Stop with a world snapshot, and a File menu for scenes. Nothing
in `dust_engine::editor` touches SDL or GL except the runtime's viewport target, so
the logic is testable headless.

**Opening it.** The key is chosen by the host game. **F4** opens the dock in
`sandbox`, `mushroom_man` and `walkaround_demo` (the legacy F1/F2 panels stay on
their own keys); `physics-sandbox` binds F1. *Note:* a change that reworks which
of F1/F2/F4 open the dock in the games is in flight — the engine only exposes
`EditorRuntime::open: bool`, so check the game's `handle_event`. Inside the dock
**F2 is Rename**, not the legacy level panel. Release builds never start the
runtime (`editor_available()` is `cfg!(debug_assertions)`).

### Hosting recipe

Per frame the game does: (1) `handle_event` for every SDL event, (2)
`ensure_target` + `view_proj` to learn where and how to render, (3) render its
scene into `target` (`Renderer::present_to(gl, rt.target.framebuffer(), ..)`),
(4) `run_ui` to draw the docked editor around it and run gizmos/picking. See
`games/physics-sandbox/src/main.rs` for the smallest complete host.

```rust
use std::path::PathBuf;

use anyhow::Result;
use dust_engine::app::{Context, Game};
use dust_engine::ecs::World;
use dust_engine::editor::runtime::{EditorRuntime, NoHost};
use dust_engine::editor::shell::PerfSnapshot;
use dust_engine::editor::Registry;
use dust_engine::sdl2::event::Event;
use dust_engine::sdl2::keyboard::Keycode;

#[derive(Clone)]
struct Health(f32);

struct MyGame {
    world: World,
    editor: Option<EditorRuntime>,
}

impl Game for MyGame {
    fn init(&mut self, ctx: &mut Context) -> Result<()> {
        // Components the editor must clone/restore (duplicate, undo, Play -> Stop).
        let mut registry = Registry::with_defaults();
        registry.register_cloneable::<Health>();

        let project = PathBuf::from("project");
        let rt = EditorRuntime::new(ctx.gl_arc(), registry, Some(project.join(".editor")), Vec::new())?
            .with_asset_root(project.join("assets")) // Assets tab
            .with_ui_dir(project.join("ui")) // UI Designer tab
            .with_project_dir(project); // <dir>/scenes, <dir>/prefabs
        self.editor = Some(rt);
        Ok(())
    }

    fn handle_event(&mut self, _ctx: &mut Context, event: &Event) {
        if let Some(rt) = self.editor.as_mut() {
            rt.handle_event(event); // forwards to egui only while open
            if matches!(event, Event::KeyDown { keycode: Some(Keycode::F4), repeat: false, .. }) {
                rt.open = !rt.open;
                if !rt.open {
                    rt.save_layout();
                }
            }
        }
    }

    fn update(&mut self, _ctx: &mut Context, _dt: f32) -> Result<()> {
        // Closed -> plain game; open -> only simulate while the editor is in Play.
        let simulating = self.editor.as_ref().map_or(true, |e| e.is_simulating());
        if simulating {
            // game logic / schedule.run(...)
        }
        Ok(())
    }

    fn render(&mut self, ctx: &mut Context) -> Result<()> {
        let gl = ctx.gl();
        let drawable = ctx.drawable_size();
        let Some(mut rt) = self.editor.take() else { return Ok(()) };
        if rt.open {
            let size = rt.ensure_target(gl, drawable)?; // Viewport tab framebuffer
            let (_view, _proj) = rt.view_proj(); // editor camera while not simulating
            // Render the scene into rt.target.framebuffer() at `size`, e.g.
            // renderer.present_to(gl, rt.target.framebuffer(), size, &Default::default());
            let _ = size;
            let dt = ctx.time.delta.max(1e-4);
            let perf = PerfSnapshot { fps: 1.0 / dt, frame_ms: dt * 1000.0, entities: self.world.len() as usize, draw_calls: 0 };
            rt.run_ui(gl, drawable, &mut self.world, &mut NoHost, perf);
        } else {
            // normal game rendering
        }
        self.editor = Some(rt);
        Ok(())
    }
}
```

- `Registry::with_defaults()` knows `Transform`, names, hierarchy, etc.; call
  `register_cloneable::<T>()` for each game component, otherwise duplicate,
  undo and Play → Stop will not restore it.
- `EditorRuntime::new` registers the scene components (`EntityId`, prefab
  links) for you. `config_dir` stores preferences and the dock layout;
  `save_layout()` writes them.
- When the dock is closed the game runs as a plain game; when open, simulate
  only while `is_simulating()` (Play) and use `wants_pointer()` /
  `wants_keyboard()` to keep the game from reacting to UI clicks and typing.
- Game-specific tabs: pass tab names to `EditorRuntime::new` and implement
  `ShellHost::tab_ui` (also `viewport_overlay`, `selection_bounds`,
  `accepts_asset_drop`). `NoHost` is the do-nothing host.

### Assets and UI Designer tabs

Both have built-in content that is used when the host's `tab_ui` declines the
tab. **Assets** browses a folder (`set_asset_root`), with search, selection,
double-click (`ShellEvent::AssetOpened`) and drag to the viewport/hierarchy
(`ShellEvent::AssetDropped`); only png/jpg get thumbnails. The game must handle
those events itself (`EditorShell::take_events`). **UI Designer** edits `.ron`
UI documents in the directory given to `set_ui_dir` (built-in `pause_menu` and
`hud` examples otherwise) with a live canvas preview, undo/redo and Load/Save.
With no root/directory set, Assets shows an empty state and the designer shows
only the examples.

### RON scenes and prefabs

With `with_project_dir(dir)` (or `set_project_dir`), the File menu works on
`<dir>/scenes/*.scene.ron`, and prefabs live in `<dir>/prefabs/*.prefab.ron`;
without a project dir scene paths are used as typed and the prefab entries are
disabled. `RonSceneIo` (`editor::ron_scene_io`) is the shell's default scene
backend: New / Open / Save / Save As / Revert, a `*` dirty marker driven by the
undo stack, `<scene>.autosave` with crash recovery, and unsaved-changes prompts.
Legacy `level::Level` files open through `level_compat` and are only written
back in the scene format after a confirmation. **Caveat:** `sandbox`,
`mushroom_man` and `walkaround_demo` currently replace the shell's scene backend
with `DisabledSceneIo` in their `docked.rs` (their levels live in the legacy Level
tab); drop that line and call `set_project_dir` to turn File > Open/Save on.

A `Scene` is deterministic, diff-friendly RON: entities in hierarchy order with
persistent `EntityId`s, components stored as named fields (`SceneValue`),
unknown components/fields preserved through load and save. Prefabs are scene
files whose first entity is the root; instances carry per-field overrides
(derived by diffing against the prefab) and nest. In the Hierarchy: *Create
Prefab from Selection*, *Instantiate Prefab*, and on an instance *Apply to
Prefab*, *Revert All*, *Unpack*; overridden fields are marked in the Inspector
with a per-field *Revert*. Known gaps are tracked in `docs/KNOWN-ISSUES.md`.

### Scene format versioning

Every scene file starts with `format_version` (currently
`SCENE_FORMAT_VERSION = 1`, with no built-in migrations yet). A load goes
through `Scene::from_ron_with(text, migrations, target)`:

- a file **newer** than `target` is refused with `SceneError::TooNew`, never
  silently mangled;
- an older file is upgraded one step at a time: while `version < target`, the
  `Migration { from: version, apply }` is looked up and run, then the version is
  stamped `version + 1` by the loader (a migration never sets it itself). A gap
  in the list — including a file with no `format_version` at all, which reads as
  version 0 — is `SceneError::NoMigration(version)`; a migration returning
  `Err(msg)` becomes `SceneError::Migration(version, msg)`.

To ship a format change: write a `fn(&mut Scene) -> Result<(), String>`, add it
to `builtin_migrations()` in `editor/scene.rs`, and bump `SCENE_FORMAT_VERSION`.
The worked example (v1 → v2 renaming `Health.hp`, a two-step chain, and the
TooNew / NoMigration / failure cases) is tested in
`engine/src/editor/scene_migration_tests.rs`:

```rust
use std::path::Path;

use dust_engine::ecs::World;
use dust_engine::editor::scene::{Migration, Scene, SceneDocument, SceneError, SceneValue};
use dust_engine::editor::Registry;

// v1 -> v2: Health.hp became Health.current (+ a new Health.max).
fn health_hp_to_current(scene: &mut Scene) -> Result<(), String> {
    for entity in &mut scene.entities {
        let Some(health) = entity.components.get_mut("Health") else { continue };
        match health.remove("hp") {
            Some(SceneValue::F32(hp)) => {
                health.insert("current".into(), SceneValue::F32(hp));
                health.insert("max".into(), SceneValue::F32(hp));
            }
            Some(other) => return Err(format!("Health.hp is not an F32: {other:?}")),
            None => {}
        }
    }
    Ok(())
}

// `Migration::apply` is a plain fn pointer; from_ron_with stamps the new version itself.
fn load_as_v2(text: &str) -> Result<Scene, SceneError> {
    let chain = [Migration { from: 1, apply: health_hp_to_current }];
    Scene::from_ron_with(text, &chain, 2)
}

// Loading a scene file into a world (what File > Open does), and saving it back.
fn open_and_save(world: &mut World, reg: &Registry) -> Result<(), SceneError> {
    let (mut doc, result) = SceneDocument::open(Path::new("project/scenes/arena.scene.ron"), world, reg)?;
    for w in &result.report.warnings {
        log::warn!("{w}"); // unknown components/fields are kept, but reported
    }
    doc.save(world, reg)
}
```

## Adding a new, separate game

**Fastest path:** `.\new_game.ps1 -Name mygame` (Windows) or
`./new_game.sh mygame` (Linux/macOS, bash; add `--no-build` to skip the final
`cargo build -p mygame` check) copies `games/sandbox` as a
template, renames its crate to `mygame`, registers it in the workspace
root `Cargo.toml`, retitles its window/error-dialog strings, and builds it
to confirm the scaffold actually compiles. Everything comes along except
the bootstrap-generated `profiles/`/`levels/`/`rigs/`/`classes/`/`sfx/`/
`music/`/`saves/` folders (those regenerate fresh for the new game via the
same `default_level()`/`default_demo_profiles()`/etc. functions it
inherits in `src/main.rs`) — so `cargo run -p mygame` gives you the
sandbox's full working demo scene as a starting point to replace piece by
piece, rather than a truly blank slate.

**From scratch instead**, if you don't want any of the demo content:

1. Create the folder and manifest:
   ```
   games/mygame/
     Cargo.toml
     src/main.rs
   ```
   ```toml
   # games/mygame/Cargo.toml
   [package]
   name = "mygame"
   version = "0.1.0"
   edition = "2021"

   [dependencies]
   engine = { path = "../../engine" }
   sdl2.workspace = true
   glow.workspace = true
   glam.workspace = true
   egui.workspace = true
   rfd.workspace = true
   anyhow.workspace = true
   log.workspace = true
   env_logger.workspace = true
   ```
2. Add it to the workspace root `Cargo.toml`: `members = ["engine", "games/sandbox", "games/mygame"]`.
3. Copy `games/sandbox/assets/shaders/{mesh.vert,mesh.frag,post_composite.frag}`
   into `games/mygame/assets/shaders/` as your starting shader set (they're
   plain GLSL — edit freely; `mesh.vert`/`mesh.frag` must stay `#version`-line-free
   since `ShaderVariantCache` prepends it).
4. Implement `Game` for a struct in `src/main.rs` (start from `Sandbox` in
   `games/sandbox/src/main.rs` and delete what you don't need — the profile
   and level bootstrap/apply logic, ECS setup, render loop, and Play-mode
   wiring (physics step, first-person camera, `update_player_input`/
   `interact`) are all reusable as-is — a new game mostly means rewriting
   what's *inside* those last two methods for its own input/interactions).
5. `fn main() { App::run("My Game", 1280, 720, MyGame::new()) }`
6. `cargo run -p mygame` — empty `profiles/`/`levels/` folders mean you'll
   want your own `default_demo_profiles()`/`default_level()` (or just
   hand-write one `.ron` file matching the `ShaderProfile`/`RenderParams`
   shape in `dust_engine::profile`, or the `Level`/`LevelObject` shape in
   `dust_engine::level`).

## Exporting a game (shipping a standalone `.exe`)

```
.\package.ps1 -Game sandbox      # Windows
./package.sh sandbox             # Linux/macOS (bash); --skip-build reuses target/release/<game>
```

This builds `-p sandbox` in release mode and assembles `dist/sandbox/` with
the `.exe` and every asset folder that exists next to it — `assets/`,
`profiles/`, `levels/`, `rigs/`, `classes/`, `scripts/`, `sfx/`, `music/`
(not `saves/`: checkpoints are pure runtime state, re-created empty on
first launch, so shipping the dev's own `checkpoint.ron` would just hand
players a stale position instead of a fresh game) — zip that folder (or
hand it to an installer builder like Inno Setup/WiX if you want a
"Setup.exe") and it runs standalone on another machine. Substitute your
own game's crate name for `-Game`; if your game adds its own asset
subfolder beyond this list, add it to the copy loop in `package.ps1` and
`package.sh` too. On Linux/macOS the binary is `target/release/<game>` (no
`.exe`); `package.sh` also honours `CARGO_TARGET_DIR`. There is no web
packaging: building for `wasm32-unknown-emscripten` is not documented in this
repo beyond cfg-gated code paths, so neither script has a web mode.

A few things make this work, already wired up for every game in the workspace:

- **Assets resolve relative to the `.exe`, not the dev machine.** Early on,
  asset/profile paths were resolved via `env!("CARGO_MANIFEST_DIR")` — a
  compile-time constant baking in the *source folder on the machine that
  built it*. That's fine for `cargo run` but breaks the instant you copy the
  binary anywhere else. Each game should resolve its asset root at runtime via
  something like `Sandbox`'s `resolve_asset_root()` in
  `games/sandbox/src/main.rs`: look next to `std::env::current_exe()` first
  (where a shipped build's assets live), and only fall back to
  `CARGO_MANIFEST_DIR` in debug builds (`#[cfg(debug_assertions)]`) so
  `cargo run` keeps working without copying assets into `target/debug` on
  every change — and so that fallback, and the dev machine's path, don't even
  exist in a release binary.
- **No console window in release.** `#![cfg_attr(not(debug_assertions),
  windows_subsystem = "windows")]` at the top of `main.rs` hides the console
  for players while keeping it for `cargo run`. Since that also means no
  visible `log::info!`/panic output, release builds instead log to a
  `game.log` file next to the `.exe` and show a native error dialog (via
  `rfd::MessageDialog`, already a dependency) on panic or a fatal `Err`
  returned from `main` — copy the `init_logging`/`show_fatal_error_dialog`
  pattern from the sandbox into a new game.
- **`[profile.release]`** in the root `Cargo.toml` (`lto = true`,
  `codegen-units = 1`, `strip = true`, `panic = "abort"`) applies to every
  game automatically — smaller binary, no unwinding tables, better codegen.
- **Custom icon (optional):** drop an `icon.ico` next to a game's `Cargo.toml`
  and add the same `build.rs` + `winres` build-dependency pattern from
  `games/sandbox/build.rs`/`Cargo.toml` — it embeds the icon if present and
  silently falls back to the default icon (with a build warning) if not.
- **The level/shader editor is automatically gone in release.** `main.rs`'s
  `editor_available()` returns `cfg!(debug_assertions)`, and every editor
  affordance (both `SidePanel`s, F1/F2/Tab/F5/F3, orbit-camera drag/zoom)
  was already gated on "currently in `EditorMode::Edit`" — so a release
  build, which never enters `Edit` mode (`Sandbox::init` calls
  `enter_play_mode` immediately when `!editor_available()`), simply never
  reaches any of it. `package.ps1`/`package.sh` already build `--release`, so this
  needs no packaging changes (the docked F4 editor is gated the same way —
  the game only starts its `EditorRuntime` when `editor_available()`): what a player gets is exactly the game,
  booting straight into Play. The pause menu's middle button becomes
  **"Restart Level"** instead of "Exit to Editor" (see "Pausing" above) —
  there being no editor to exit to.

Caveat: the in-game editors save profiles/levels back into the `profiles/`/
`levels/` folders next to the `.exe`. If you install to a location that needs
admin rights (e.g. `Program Files`), those writes will fail for a normal
user in a *debug* build — ship to a user-writable folder (Desktop,
`Documents`, a portable zip), which is moot for a real `--release` shipment
anyway since the editor (and its writes) aren't reachable there at all.


## Turn-based multiplayer (`dust_engine::turn`)

For board and card games. Host-authoritative action log: implement `TurnGame`
(`State`, `Action`, pure `validate` and `apply`), then run a `TurnHost` on one
machine and a `TurnClient` on the others. Clients `submit` an action; the host
validates, applies, numbers it and broadcasts it; every peer applies the same
actions in the same order. A client that detects a gap in the sequence asks for
a `Resync`. `apply` must be deterministic: roll dice on the host with
`TurnHost::submit_with` and put the result in the action.

The module does no I/O itself. A `TurnLink` moves bytes: `net::NetConnection`
(TCP) implements it, `MemoryLink::pair()` gives an in-process pair for tests or
hot-seat/AI seats, and a WebSocket link for the web build only needs `send`,
`pump` and `is_closed`. Call `host.poll()` / `client.poll()` once per frame and
react to the returned `TurnEvent`s.
