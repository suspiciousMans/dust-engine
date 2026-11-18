use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use dust_engine::app::{App, Context, Game};
use dust_engine::camera::FirstPersonCamera;
use dust_engine::ecs::{Entity, MeshRenderer, Transform, World};
use dust_engine::editor::bounds::{Aabb, LocalBounds};
use dust_engine::editor::runtime::{EditorRuntime, NoHost};
use dust_engine::editor::shell::PerfSnapshot;
use dust_engine::editor::{Locked, Name, Registry};
use dust_engine::glow;
use dust_engine::glow::HasContext;
use dust_engine::glam::{Mat4, Quat, Vec3};
use dust_engine::mesh::{primitives, GpuMesh};
use dust_engine::particles::{ParticleEmitter, ParticleEmitterDef, step};
use dust_engine::physics::{step as physics_step, Collider, ColliderShape, PhysicsParams, RigidBody};
use dust_engine::renderer::{patch_mesh_shaders, Renderer, ShadowBounds, ShadowSettings, DEFAULT_FRAGMENT_SRC};
use dust_engine::sdl2::event::Event;
use dust_engine::sdl2::keyboard::Keycode;
use dust_engine::sdl2::mouse::MouseButton;
use dust_engine::shader::link_program;

mod natives;
pub use natives::__oxidized_natives;

include!(concat!(env!("OUT_DIR"), "/ox_generated.rs"));

use ox_modules::sandbox;
use ox_modules::sandbox::DynamicValue;

#[derive(Clone, Copy, Debug, Default)]
struct CubeColor([f32; 4]);

#[derive(Default)]
struct SpatialHash {
    cell_size: f32,
    cells: std::collections::HashMap<[i32; 3], Vec<(Entity, Vec3, Vec3, f32)>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tool {
    #[default]
    Place,
    Remove,
    Spray,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Place => "Place",
            Tool::Remove => "Remove",
            Tool::Spray => "Spray",
        }
    }

    fn cycle(self) -> Self {
        match self {
            Tool::Place => Tool::Remove,
            Tool::Remove => Tool::Spray,
            Tool::Spray => Tool::Place,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Material {
    Yellow,
    Red,
    Green,
    Blue,
    White,
}

impl Material {
    fn name(self) -> &'static str {
        match self {
            Material::Yellow => "Yellow",
            Material::Red => "Red",
            Material::Green => "Green",
            Material::Blue => "Blue",
            Material::White => "White",
        }
    }

    fn color(self) -> [f32; 4] {
        match self {
            Material::Yellow => [0.9, 0.9, 0.2, 1.0],
            Material::Red => [0.9, 0.2, 0.2, 1.0],
            Material::Green => [0.2, 0.9, 0.2, 1.0],
            Material::Blue => [0.2, 0.5, 0.95, 1.0],
            Material::White => [0.95, 0.95, 0.95, 1.0],
        }
    }

    fn cycle(self) -> Self {
        match self {
            Material::Yellow => Material::Red,
            Material::Red => Material::Green,
            Material::Green => Material::Blue,
            Material::Blue => Material::White,
            Material::White => Material::Yellow,
        }
    }
}

impl Default for Material {
    fn default() -> Self {
        Material::Yellow
    }
}

#[derive(Clone, Copy, Debug)]
struct CubeMaterial(Material);

impl CubeMaterial {
    fn bounce(self) -> f32 {
        match self.0 {
            Material::Yellow => 0.55,
            Material::Red => 0.7,
            Material::Green => 0.3,
            Material::Blue => 0.15,
            Material::White => 0.45,
        }
    }
}

impl SpatialHash {
    fn new(cell_size: f32) -> Self {
        Self {
            cell_size,
            cells: std::collections::HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.cells.clear();
    }

    fn key(&self, pos: Vec3) -> [i32; 3] {
        let s = self.cell_size;
        [
            (pos.x / s).floor() as i32,
            (pos.y / s).floor() as i32,
            (pos.z / s).floor() as i32,
        ]
    }

    fn insert(&mut self, entity: Entity, pos: Vec3, half_extents: Vec3, bounce: f32) {
        let min = pos - half_extents;
        let max = pos + half_extents;
        let i0 = self.key(min);
        let i1 = self.key(max);

        for ix in i0[0]..=i1[0] {
            for iy in i0[1]..=i1[1] {
                for iz in i0[2]..=i1[2] {
                    self.cells.entry([ix, iy, iz]).or_default().push((entity, pos, half_extents, bounce));
                }
            }
        }
    }

    fn query_point(&self, point: Vec3) -> impl Iterator<Item = (Vec3, Vec3, f32)> + '_ {
        let k = self.key(point);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();

        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let cell = [k[0] + dx, k[1] + dy, k[2] + dz];
                    if let Some(entries) = self.cells.get(&cell) {
                        for &(entity, pos, half, bounce) in entries {
                            if seen.insert(entity) {
                                out.push((pos, half, bounce));
                            }
                        }
                    }
                }
            }
        }

        out.into_iter()
    }
}

struct SandboxGame {
    world: World,
    player_position: Vec3,
    camera: FirstPersonCamera,
    renderer: Option<Renderer>,
    program: Option<glow::Program>,
    cube_mesh: Option<Arc<GpuMesh>>,
    plane_mesh: Option<Arc<GpuMesh>>,
    quad_mesh: Option<Arc<GpuMesh>>,
    liquid_emitter: Entity,
    spray_emitter: Entity,
    mouse_locked: bool,
    current_tool: Tool,
    current_material: Material,
    ox: OxSandbox,
    spatial_hash: SpatialHash,
    elapsed: f64,
    gravity_preset: usize,
    editor: Option<EditorRuntime>,
    /// `DUST_SHADOWS` (1 = smooth PCF, ps2 = low-res dithered): opt-in demo of
    /// the engine's directional shadow map. `None` = original rendering.
    shadows: Option<ShadowSettings>,
}

impl Game for SandboxGame {
    fn init(&mut self, ctx: &mut Context) -> Result<()> {
        let gl = ctx.gl();
        let drawable_size = ctx.drawable_size();
        let asset_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        // Renderer
        let post_src = std::fs::read_to_string(asset_root.join("assets/shaders/post_composite.frag"))
            .unwrap_or_else(|_| DEFAULT_FRAGMENT_SRC.to_string());
        self.renderer = Some(Renderer::new(gl, drawable_size, 0.5, &post_src)?);

        // Shaders
        let vert = std::fs::read_to_string(asset_root.join("assets/shaders/mesh.vert"))?;
        let frag = std::fs::read_to_string(asset_root.join("assets/shaders/mesh.frag"))?;
        self.shadows = shadow_settings_from_env();
        if let Some(settings) = self.shadows {
            // Opt-in: the engine patches shadow receiving into the game's own
            // mesh shaders in memory; the files on disk stay untouched.
            let (vert, frag) = patch_mesh_shaders(&vert, &frag, "FragColor")?;
            self.program = Some(link_program(gl, &vert, &frag)?);
            self.renderer.as_mut().unwrap().set_shadow_settings(gl, settings);
        } else {
            self.program = Some(link_program(gl, &vert, &frag)?);
        }

        // Meshes
        let cube_mesh = Arc::new(GpuMesh::upload(gl, &primitives::cube())?);
        let plane_mesh = Arc::new(GpuMesh::upload(gl, &primitives::plane())?);
        let quad_mesh = Arc::new(GpuMesh::upload(gl, &primitives::quad())?);
        self.cube_mesh = Some(cube_mesh);
        self.plane_mesh = Some(plane_mesh);
        self.quad_mesh = Some(quad_mesh);

        // Ground — thin slab at y=0.0, top surface at y=0.1
        let _ground_entity = self.world.spawn((
            Name("Ground".into()),
            Locked(true),
            // The plane mesh is unit-sized; without this the picker would use a unit cube.
            LocalBounds(Aabb::from_center_half(Vec3::ZERO, Vec3::new(1.0, 0.01, 1.0))),
            Transform {
                position: Vec3::new(0.0, 0.0, 0.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::new(20.0, 1.0, 20.0),
            },
            MeshRenderer {
                mesh: Arc::clone(self.plane_mesh.as_ref().unwrap()),
                texture: None,
            },
            Collider {
                shape: ColliderShape::Aabb { half_extents: Vec3::new(20.0, 0.1, 20.0) },
                is_trigger: false,
            },
        ));

        if self.shadows.is_some() {
            // Static demo cubes so the shadows have something to show.
            for (i, (x, y, z, h)) in [(-3.0, 0.6, 4.0, 0.5), (0.0, 1.4, 2.0, 0.5), (3.5, 0.6, 3.0, 0.5), (-1.5, 2.6, 5.0, 0.5), (1.5, 0.6, 7.0, 0.5)]
                .into_iter()
                .enumerate()
            {
                self.world.spawn((
                    Name(format!("Shadow demo cube {i}")),
                    Transform {
                        position: Vec3::new(x, y, z),
                        rotation: Quat::from_rotation_y(i as f32 * 0.5),
                        scale: Vec3::splat(h * 2.0),
                    },
                    MeshRenderer { mesh: Arc::clone(self.cube_mesh.as_ref().unwrap()), texture: None },
                    CubeColor([0.9, 0.35 + 0.1 * i as f32, 0.2, 1.0]),
                ));
            }
        }

        // Liquid emitter (main fountain)
        let liquid_emitter = self.world.spawn((
            Name("Fountain".into()),
            Transform::from_position(Vec3::new(0.0, 0.6, 0.0)),
            ParticleEmitter::new(ParticleEmitterDef {
                rate_per_sec: 150.0,
                lifetime_min: 3.0,
                lifetime_max: 6.0,
                speed_min: 1.0,
                speed_max: 2.8,
                spread_deg: 25.0,
                gravity_scale: 0.35,
                start_size: 0.14,
                end_size: 0.05,
                start_color: [0.15, 0.45, 0.95, 0.7],
                end_color: [0.35, 0.75, 1.0, 0.0],
                max_particles: 1200,
            }),
        ));

        // Spray emitter (player-held)
        let spray_emitter = self.world.spawn((
            Name("Spray".into()),
            Transform::from_position(Vec3::new(0.0, 3.0, 12.0)),
            ParticleEmitter::new(ParticleEmitterDef {
                rate_per_sec: 0.0,
                lifetime_min: 1.0,
                lifetime_max: 2.0,
                speed_min: 2.5,
                speed_max: 5.0,
                spread_deg: 15.0,
                gravity_scale: 0.3,
                start_size: 0.09,
                end_size: 0.025,
                start_color: [0.2, 0.55, 1.0, 0.8],
                end_color: [0.5, 0.85, 1.0, 0.0],
                max_particles: 800,
            }),
        ));

        // Camera & player
        self.player_position = Vec3::new(0.0, 0.5, 12.0);
        self.camera = FirstPersonCamera::default();
        self.camera.fov_y_radians = 60.0_f32.to_radians();
        self.camera.near = 0.1;
        self.camera.far = 200.0;

        self.liquid_emitter = liquid_emitter;
        self.spray_emitter = spray_emitter;
        self.mouse_locked = true;
        let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(true);

        // Oxidized initial state
        self.ox = OxSandbox::new();
        self.spatial_hash = SpatialHash::new(2.0);

        // Editor (F1). Components the snapshot must restore on Stop.
        let mut registry = Registry::with_defaults();
        registry.register_cloneable::<RigidBody>();
        registry.register_cloneable::<Collider>();
        registry.register_cloneable::<CubeColor>();
        registry.register_cloneable::<CubeMaterial>();
        let mut rt = EditorRuntime::new(ctx.gl_arc(), registry, Some(asset_root.join(".editor")), Vec::new())?;
        if std::env::var_os("DUST_EDITOR").is_some() {
            rt.open = true;
            self.mouse_locked = false;
            let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(false);
        }
        self.editor = Some(rt);

        Ok(())
    }

    fn handle_event(&mut self, ctx: &mut Context, event: &Event) {
        if let Some(rt) = self.editor.as_mut() {
            rt.handle_event(event);
            if matches!(event, Event::KeyDown { keycode: Some(Keycode::F1), repeat: false, .. }) {
                rt.open = !rt.open;
                self.mouse_locked = !rt.open;
                let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(self.mouse_locked);
                if !rt.open {
                    rt.save_layout();
                }
                return;
            }
            // With the editor open the UI owns input until the mouse is captured for play.
            let esc = matches!(event, Event::KeyDown { keycode: Some(Keycode::Escape), .. });
            if rt.open && !self.mouse_locked && !esc {
                return;
            }
        }
        match event {
            Event::KeyDown { keycode: Some(Keycode::Escape), .. } => {
                self.mouse_locked = !self.mouse_locked;
                let _ = ctx.platform.sdl.mouse().set_relative_mouse_mode(self.mouse_locked);
            }
            Event::KeyDown { keycode: Some(Keycode::Q), .. } => {
                ctx.should_quit = true;
            }
            Event::KeyDown { keycode: Some(Keycode::Num1), .. } => {
                self.current_tool = Tool::Place;
                log::info!("Tool: {}", self.current_tool.name());
            }
            Event::KeyDown { keycode: Some(Keycode::Num2), .. } => {
                self.current_tool = Tool::Remove;
                log::info!("Tool: {}", self.current_tool.name());
            }
            Event::KeyDown { keycode: Some(Keycode::Num3), .. } => {
                self.current_tool = Tool::Spray;
                log::info!("Tool: {}", self.current_tool.name());
            }
            Event::KeyDown { keycode: Some(Keycode::Tab), .. } => {
                self.current_tool = self.current_tool.cycle();
                log::info!("Tool: {}", self.current_tool.name());
            }
            Event::KeyDown { keycode: Some(Keycode::F), .. } => {
                if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.liquid_emitter) {
                    if emitter.def.rate_per_sec > 0.0 {
                        emitter.def.rate_per_sec = 0.0;
                        log::info!("Fountain: OFF");
                    } else {
                        emitter.def.rate_per_sec = 150.0;
                        log::info!("Fountain: ON (150/s)");
                    }
                }
            }
            Event::KeyDown { keycode: Some(Keycode::G), .. } => {
                let presets = [0.15, 0.35, 0.6];
                self.gravity_preset = (self.gravity_preset + 1) % presets.len();
                let g = presets[self.gravity_preset];
                if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.liquid_emitter) {
                    emitter.def.gravity_scale = g;
                }
                if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.spray_emitter) {
                    emitter.def.gravity_scale = g;
                }
                log::info!("Gravity: {:.2}", g);
            }
            Event::KeyDown { keycode: Some(Keycode::R), .. } => {
                if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.liquid_emitter) {
                    emitter.def.rate_per_sec = 150.0;
                    emitter.def.max_particles = 1200;
                    emitter.def.gravity_scale = 0.35;
                }
                if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.spray_emitter) {
                    emitter.def.gravity_scale = 0.3;
                    emitter.def.max_particles = 800;
                }
                self.gravity_preset = 1;
                log::info!("Reset: defaults restored");
            }
            Event::MouseWheel { y, .. } => {
                if *y != 0 {
                    let prev = self.current_material;
                    self.current_material = if *y > 0 {
                        self.current_material.cycle()
                    } else {
                        match self.current_material {
                            Material::Yellow => Material::White,
                            Material::Red => Material::Yellow,
                            Material::Green => Material::Red,
                            Material::Blue => Material::Green,
                            Material::White => Material::Blue,
                        }
                    };
                    if prev != self.current_material {
                        log::info!("Material: {}", self.current_material.name());
                    }
                }
            }
            _ => {}
        }
    }

    fn update(&mut self, ctx: &mut Context, dt: f32) -> Result<()> {
        let dt = dt.min(0.1);
        self.elapsed += dt as f64;

        // Frame time diagnostics
        if self.elapsed > 1.0 {
            log::info!("Frame dt: {:.1}ms", dt * 1000.0);
        }

        // Mouse look (inverted Y, only when locked)
        if self.mouse_locked {
            let (dx, dy) = ctx.input.mouse_delta();
            if dx != 0 || dy != 0 {
                self.camera.look(dx as f32 * 0.002, -dy as f32 * 0.002);
            }
        }

        let simulating = self.editor.as_ref().map_or(true, |e| e.is_simulating());
        let typing = self.editor.as_ref().is_some_and(|e| e.wants_keyboard());
        let keys_ok = simulating && !typing;
        let editor_open = self.editor.as_ref().is_some_and(|e| e.open);

        // Movement (projected to XZ plane so looking up/down doesn't change height)
        let forward = self.camera.forward();
        let right = self.camera.right();
        let speed = 5.0 * dt;
        let mut move_dir = forward;
        move_dir.y = 0.0;
        move_dir = move_dir.normalize_or_zero();
        let mut right_dir = right;
        right_dir.y = 0.0;
        right_dir = right_dir.normalize_or_zero();
        if keys_ok && ctx.input.is_key_down(Keycode::W) {
            self.player_position += move_dir * speed;
        }
        if keys_ok && ctx.input.is_key_down(Keycode::S) {
            self.player_position -= move_dir * speed;
        }
        if keys_ok && ctx.input.is_key_down(Keycode::A) {
            self.player_position -= right_dir * speed;
        }
        if keys_ok && ctx.input.is_key_down(Keycode::D) {
            self.player_position += right_dir * speed;
        }

        // Edit and Pause freeze the simulation (and the tools that act on it).
        if !simulating {
            return Ok(());
        }

        // Physics
        let params = PhysicsParams {
            gravity: 9.8,
            linear_damping: 0.15,
        };
        physics_step(&mut self.world, dt, &params);

        // Particles
        step(&mut self.world, dt);

        // Collect solid AABBs into spatial hash for collision,
        // and collect entities to despawn in the same pass
        self.spatial_hash.clear();
        let mut to_despawn = vec![];
        for (entity, (transform, collider)) in self.world.query::<(&Transform, &Collider)>().iter() {
            if let ColliderShape::Aabb { half_extents } = collider.shape {
                let bounce = self.world.get::<&CubeMaterial>(entity)
                    .map(|m| m.bounce())
                    .unwrap_or(0.55);
                self.spatial_hash.insert(entity, transform.position, half_extents, bounce);
            }
            if transform.position.y < -20.0 {
                to_despawn.push(entity);
            }
        }
        for entity in to_despawn {
            let _ = self.world.despawn(entity);
        }
        let player_feet = self.player_position;
        let player_radius = 0.4;
        let mut player_push = Vec3::ZERO;
        for (aabb_pos, aabb_half, _bounce) in self.spatial_hash.query_point(player_feet) {
            if let Some(push) = sphere_vs_aabb(player_feet, player_radius, aabb_pos, aabb_half) {
                player_push += push;
            }
        }
        self.player_position += player_push;

        // Particle collision with ground and voxels via spatial hash
        for (_entity, (_transform, emitter)) in self.world.query::<(&Transform, &mut ParticleEmitter)>().iter() {
            for particle in &mut emitter.particles {
                // Collect all contacts first, then resolve deepest penetration first
                let mut contacts = Vec::new();
                for (aabb_pos, aabb_half, bounce) in self.spatial_hash.query_point(particle.position) {
                    if let Some(push) = point_vs_aabb(particle.position, aabb_pos, aabb_half) {
                        let push_dir = push.normalize_or_zero();
                        let vel_into = particle.velocity.dot(push_dir);
                        if vel_into < 0.0 {
                            contacts.push((push, push_dir, vel_into, bounce));
                        }
                    }
                }

                // Resolve deepest penetration first to avoid jitter in corners
                contacts.sort_by(|a, b| b.0.length_squared().partial_cmp(&a.0.length_squared()).unwrap_or(std::cmp::Ordering::Equal));

                for (push, push_dir, vel_into, bounce) in contacts {
                    particle.position += push;
                    particle.velocity -= push_dir * vel_into * bounce;
                    // Clamp speed to prevent energy gain from glancing collisions
                    let speed = particle.velocity.length();
                    if speed > 6.0 {
                        particle.velocity = particle.velocity.normalize_or_zero() * 6.0;
                    }
                }
            }
        }

        // Move spray emitter with player
        if let Ok(mut transform) = self.world.get::<&mut Transform>(self.spray_emitter) {
            transform.position = self.eye_position();
        }

        // Mouse actions dispatch based on current tool
        if (!editor_open || self.mouse_locked) && ctx.input.button_just_pressed(MouseButton::Left) {
            match self.current_tool {
                Tool::Place => {
                    let forward = self.camera.forward();
                    let spawn_pos = self.player_position + forward * 5.0;
                    let ground_pos = Vec3::new(spawn_pos.x, 0.35, spawn_pos.z);

                    // Don't place if it would trap the player
                    if ground_pos.distance(self.player_position) > 1.0 {
                        self.world.spawn((
                            Name("Cube".into()),
                            Transform::from_position(ground_pos),
                            MeshRenderer {
                                mesh: Arc::clone(self.cube_mesh.as_ref().unwrap()),
                                texture: None,
                            },
                            Collider {
                                shape: ColliderShape::Aabb { half_extents: Vec3::splat(0.25) },
                                is_trigger: false,
                            },
                            CubeColor(self.current_material.color()),
                            CubeMaterial(self.current_material),
                        ));
                    }
                }
                Tool::Remove => {
                    let target = self.player_position + self.camera.forward() * 5.0;
                    if let Some(entity) = self.find_nearest_entity(target, 2.0) {
                        let _ = self.world.despawn(entity);
                    }
                }
                Tool::Spray => {}
            }
        }

        if (!editor_open || self.mouse_locked) && ctx.input.button_just_pressed(MouseButton::Right) {
            match self.current_tool {
                Tool::Spray => {
                    if let Ok(mut emitter) = self.world.get::<&mut ParticleEmitter>(self.spray_emitter) {
                        let origin = self.eye_position() + self.camera.forward() * 2.0;
                        let count_before = emitter.particles.len();
                        emitter.spawn_burst(origin, 40);

                        let new_count = emitter.particles.len();
                        if new_count > count_before {
                            let look_dir = self.camera.forward();
                            let spread = emitter.def.spread_deg.to_radians();
                            let rotation = if look_dir.dot(Vec3::Y).abs() > 0.999 {
                                Quat::from_rotation_y(std::f32::consts::PI)
                            } else {
                                Quat::from_rotation_arc(Vec3::Y, look_dir)
                            };

                            for (idx, particle) in emitter.particles[count_before..new_count].iter_mut().enumerate() {
                                let seed = (idx as f32) * 0.6180339887;
                                let theta = (seed * 12.9898).sin() * 43758.5453;
                                let frac = theta - theta.floor();
                                let angle = frac * std::f32::consts::TAU;
                                let phi = frac * spread;

                                let local_dir = Vec3::new(
                                    phi.sin() * angle.cos(),
                                    phi.cos(),
                                    phi.sin() * angle.sin(),
                                );
                                let direction = rotation * local_dir;
                                let speed = particle.velocity.length();
                                particle.velocity = direction * speed;
                            }
                        }
                    }
                }
                Tool::Place | Tool::Remove => {}
            }
        }

        // Oxidized update — count colored cubes/voxels (excludes ground/emitters)
        let count = self.world.query::<(&CubeColor,)>().iter().count() as i64;
        let spawns = self.ox.step(dt as f64, count);

        // Periodic stats print from oxidized
        if self.elapsed % 1.0 < dt as f64 {
            sandbox::describe_game(self.elapsed, count);
        }

        for spawn in spawns {
            // Safety: don't spawn inside the ground or inside the player
            if spawn.y < 0.1 {
                continue;
            }
            let spawn_pos = Vec3::new(spawn.x, spawn.y, spawn.z);
            if spawn_pos.distance(self.player_position) < 1.5 {
                continue;
            }

            self.world.spawn((
                Name("Spawned cube".into()),
                Transform::from_position(Vec3::new(spawn.x, spawn.y, spawn.z)),
                MeshRenderer {
                    mesh: Arc::clone(self.cube_mesh.as_ref().unwrap()),
                    texture: None,
                },
                Collider {
                    shape: ColliderShape::Aabb { half_extents: Vec3::splat(spawn.size * 0.5) },
                    is_trigger: false,
                },
                RigidBody::default(),
                CubeColor([spawn.r, spawn.g, spawn.b, 1.0]),
                CubeMaterial(Material::Yellow),
            ));
        }

        Ok(())
    }

    fn render(&mut self, ctx: &mut Context) -> Result<()> {
        if self.editor.as_ref().is_some_and(|e| e.open) {
            return self.render_editor(ctx);
        }
        let size = ctx.drawable_size();
        let view = self.camera.view_matrix(self.eye_position());
        let proj = self.camera.projection_matrix(ctx.aspect_ratio());
        self.draw_scene(ctx.gl(), size, view, proj)?;
        self.renderer.as_mut().unwrap().present(ctx.gl(), size, &Default::default());
        Ok(())
    }
}

impl SandboxGame {
    /// Draws the scene into the renderer's scene target at `size` pixels.
    fn draw_scene(&mut self, gl: &glow::Context, size: (u32, u32), view: Mat4, proj: Mat4) -> Result<()> {
        let renderer = self.renderer.as_mut().unwrap();
        renderer.resize_if_needed(gl, size)?;
        // Shadow pass (opt-in): depth of every coloured mesh from the sun,
        // fitted to the camera frustum. Skipped entirely when shadows are off.
        if renderer.shadows_active() {
            let bounds = ShadowBounds::camera(view, proj, renderer.shadow_settings());
            if renderer.begin_shadow_pass(gl, SHADOW_SUN, &bounds) {
                for (_e, (transform, mesh_renderer, _c)) in
                    self.world.query::<(&Transform, &MeshRenderer, &CubeColor)>().iter()
                {
                    renderer.shadow_draw(gl, &transform.matrix(), &mesh_renderer.mesh);
                }
                renderer.end_shadow_pass(gl);
            }
        }
        renderer.begin_scene(gl);

        unsafe {
            gl.clear_color(0.08, 0.08, 0.08, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.enable(glow::DEPTH_TEST);
            gl.enable(glow::CULL_FACE);
        }

        let program = self.program.unwrap();
        unsafe {
            gl.use_program(Some(program));

            let view_loc = gl.get_uniform_location(program, "uView");
            let proj_loc = gl.get_uniform_location(program, "uProj");
            let light_loc = gl.get_uniform_location(program, "uLightDir");
            let model_loc = gl.get_uniform_location(program, "uModel");
            let color_loc = gl.get_uniform_location(program, "uColor");

            gl.uniform_matrix_4_f32_slice(view_loc.as_ref(), false, &view.to_cols_array());
            gl.uniform_matrix_4_f32_slice(proj_loc.as_ref(), false, &proj.to_cols_array());
            if self.shadows.is_some() {
                // Same "toward the light" vector the shadow pass uses.
                gl.uniform_3_f32(light_loc.as_ref(), SHADOW_SUN.x, SHADOW_SUN.y, SHADOW_SUN.z);
                renderer.bind_shadows(gl, program);
            } else {
                gl.uniform_3_f32(light_loc.as_ref(), 0.5, -0.7, 0.5);
            }

            for (_entity, (transform, mesh_renderer, color)) in
                self.world.query::<(&Transform, &MeshRenderer, &CubeColor)>().iter()
            {
                gl.uniform_matrix_4_f32_slice(
                    model_loc.as_ref(),
                    false,
                    &transform.matrix().to_cols_array(),
                );
                gl.uniform_4_f32(color_loc.as_ref(), color.0[0], color.0[1], color.0[2], color.0[3]);
                mesh_renderer.mesh.draw(gl);
            }
            if self.shadows.is_some() {
                // The ground carries no CubeColor so the loop above skips it;
                // in the shadow demo draw it as a receiver.
                for (_e, (name, transform, mesh_renderer)) in
                    self.world.query::<(&Name, &Transform, &MeshRenderer)>().iter()
                {
                    if name.0 == "Ground" {
                        gl.uniform_matrix_4_f32_slice(model_loc.as_ref(), false, &transform.matrix().to_cols_array());
                        gl.uniform_4_f32(color_loc.as_ref(), 0.45, 0.62, 0.4, 1.0);
                        mesh_renderer.mesh.draw(gl);
                    }
                }
            }
        }

        // Particles
        let quad_mesh = self.quad_mesh.as_ref().unwrap().clone();
        let (_, billboard_rotation, _) = view.inverse().to_scale_rotation_translation();
        let mut particle_draws = Vec::new();
        for (_entity, (_transform, emitter)) in self.world.query::<(&Transform, &ParticleEmitter)>().iter() {
            for particle in &emitter.particles {
                let (size, color) = emitter.appearance(particle);
                let model = Mat4::from_scale_rotation_translation(Vec3::splat(size), billboard_rotation, particle.position);
                particle_draws.push((quad_mesh.clone(), model.to_cols_array(), color));
            }
        }
        renderer.draw_particles(gl, &view.to_cols_array(), &proj.to_cols_array(), particle_draws.into_iter());
        Ok(())
    }

    /// Scene into the editor's Viewport tab, then the docked editor on top.
    fn render_editor(&mut self, ctx: &mut Context) -> Result<()> {
        let gl = ctx.gl();
        let drawable = ctx.drawable_size();
        let Some(mut rt) = self.editor.take() else { return Ok(()) };
        let size = rt.ensure_target(gl, drawable)?;
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let simulating = rt.is_simulating();
        // Play shows the game camera, Edit/Pause the editor camera.
        let (view, proj) = if simulating {
            (self.camera.view_matrix(self.eye_position()), self.camera.projection_matrix(aspect))
        } else {
            rt.view_proj()
        };
        self.draw_scene(gl, size, view, proj)?;
        let renderer = self.renderer.as_mut().unwrap();
        if !simulating {
            renderer.draw_grid(gl, proj * view, 80.0);
        }
        renderer.present_to(gl, rt.target.framebuffer(), size, &Default::default());

        let dt = ctx.time.delta.max(1e-4);
        let perf = PerfSnapshot { fps: 1.0 / dt, frame_ms: dt * 1000.0, entities: self.world.len() as usize, draw_calls: 0 };
        rt.run_ui(gl, drawable, &mut self.world, &mut NoHost, perf);
        self.editor = Some(rt);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct SpawnDef {
    x: f32,
    y: f32,
    z: f32,
    size: f32,
    r: f32,
    g: f32,
    b: f32,
}

struct OxSandbox {
    state: DynamicValue,
}

impl OxSandbox {
    fn new() -> Self {
        Self {
            state: DynamicValue::Struct(
                "State".to_string(),
                vec![
                    ("time".to_string(), DynamicValue::Float(0.0)),
                    ("spawn_timer".to_string(), DynamicValue::Float(0.0)),
                ],
            ),
        }
    }

    fn step(&mut self, dt: f64, count: i64) -> Vec<SpawnDef> {
        let result = sandbox::update_game(self.state.clone(), dt, count);
        let mut spawns = Vec::new();

        match result {
            DynamicValue::Tuple(items) if items.len() == 2 => {
                self.state = items[0].clone();
                let spawn_list = &items[1];
                if let DynamicValue::List(list) = spawn_list {
                    for item in list.iter() {
                        if let DynamicValue::Struct(_, fields) = item {
                            spawns.push(SpawnDef {
                                x: dyn_f64(fields, "x") as f32,
                                y: dyn_f64(fields, "y") as f32,
                                z: dyn_f64(fields, "z") as f32,
                                size: dyn_f64(fields, "size") as f32,
                                r: dyn_f64(fields, "r") as f32,
                                g: dyn_f64(fields, "g") as f32,
                                b: dyn_f64(fields, "b") as f32,
                            });
                        }
                    }
                }
            }
            other => {
                log::warn!("unexpected oxidized return shape: {:?}", other);
            }
        }

        spawns
    }
}

fn dyn_f64(fields: &[(String, DynamicValue)], name: &str) -> f64 {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_f64())
        .unwrap_or_default()
}

impl Default for SandboxGame {
    fn default() -> Self {
        Self {
            world: World::new(),
            player_position: Vec3::new(0.0, 0.5, 12.0),
            camera: FirstPersonCamera::default(),
            renderer: None,
            program: None,
            cube_mesh: None,
            plane_mesh: None,
            quad_mesh: None,
            liquid_emitter: Entity::DANGLING,
            spray_emitter: Entity::DANGLING,
            mouse_locked: true,
            current_tool: Tool::default(),
            current_material: Material::default(),
            ox: OxSandbox::new(),
            spatial_hash: SpatialHash::new(2.0),
            elapsed: 0.0,
            gravity_preset: 1,
            editor: None,
            shadows: None,
        }
    }
}

impl SandboxGame {
    fn eye_position(&self) -> Vec3 {
        self.player_position + Vec3::new(0.0, 1.7, 0.0)
    }

    fn find_nearest_entity(&self, target: Vec3, max_dist: f32) -> Option<Entity> {
        let mut best = None;
        let mut best_dist = max_dist * max_dist;

        for (entity, (transform, _collider)) in self.world.query::<(&Transform, &Collider)>().iter() {
            let dist = transform.position.distance_squared(target);
            if dist < best_dist {
                best_dist = dist;
                best = Some(entity);
            }
        }

        best
    }
}

fn sphere_vs_aabb(sphere_pos: Vec3, radius: f32, aabb_pos: Vec3, aabb_half: Vec3) -> Option<Vec3> {
    let local = sphere_pos - aabb_pos;
    let closest_local = local.clamp(-aabb_half, aabb_half);
    let diff = local - closest_local;
    let distance = diff.length();
    if distance > 1e-5 {
        if distance < radius {
            let normal = diff / distance;
            Some(normal * (radius - distance))
        } else {
            None
        }
    } else {
        let penetration = aabb_half - local.abs();
        let (axis, depth) = if penetration.x < penetration.y && penetration.x < penetration.z {
            (Vec3::X, penetration.x)
        } else if penetration.y < penetration.z {
            (Vec3::Y, penetration.y)
        } else {
            (Vec3::Z, penetration.z)
        };
        let normal = axis * local.dot(axis).signum();
        Some(normal * (depth + radius))
    }
}

fn point_vs_aabb(point: Vec3, aabb_pos: Vec3, aabb_half: Vec3) -> Option<Vec3> {
    let local = point - aabb_pos;
    let penetration = aabb_half - local.abs();
    if penetration.x <= 0.0 || penetration.y <= 0.0 || penetration.z <= 0.0 {
        return None;
    }
    let (axis, depth) = if penetration.x < penetration.y && penetration.x < penetration.z {
        (Vec3::X, penetration.x)
    } else if penetration.y < penetration.z {
        (Vec3::Y, penetration.y)
    } else {
        (Vec3::Z, penetration.z)
    };
    let normal = axis * local.dot(axis).signum();
    Some(normal * (depth + 0.01))
}

/// Direction toward the sun in the shadow demo.
const SHADOW_SUN: Vec3 = Vec3::new(0.45, 0.75, 0.35);

/// `DUST_SHADOWS=1|smooth` -> PCF, `ps2` -> low-res dithered; unset/other -> off.
fn shadow_settings_from_env() -> Option<ShadowSettings> {
    match std::env::var("DUST_SHADOWS").ok()?.to_ascii_lowercase().as_str() {
        "1" | "true" | "smooth" => Some(ShadowSettings::smooth()),
        "ps2" => Some(ShadowSettings::ps2()),
        _ => None,
    }
}

fn main() -> Result<()> {
    env_logger::init();
    App::run("Physics Sandbox", 1280, 720, SandboxGame::default())
}
