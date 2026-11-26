//! Built-in hi-fi rendering demo: HDR + 4x MSAA target, per-pixel PBR,
//! soft shadows, ACES tonemap, orbit camera over a small tabletop scene.
//!
//! `cargo run -p dust_engine --example hifi_showcase`
//! Left-drag orbits, wheel zooms, R toggles the retro target, Esc quits.
//! `DUST_SCREENSHOT=out.png` renders a few frames, saves one, and exits.

use dust_engine::app::{App, Context, Game};
use dust_engine::camera::OrbitCamera;
use dust_engine::glam::{Mat4, Quat, Vec3};
use dust_engine::glow::{self, HasContext};
use dust_engine::mesh::{primitives, GpuMesh};
use dust_engine::renderer::{
    patch_mesh_shaders, PostParams, Renderer, ShadowBounds, ShadowSettings, TargetOptions,
};
use dust_engine::sdl2::event::Event;
use dust_engine::sdl2::keyboard::Keycode;
use dust_engine::sdl2::mouse::MouseButton;
use dust_engine::shader::builtin::{PbrMaterial, PointLight, SceneLighting};
use dust_engine::shader::{ShaderVariantCache, PBR_MESH_FRAG, PBR_MESH_VERT, TONEMAP_COMPOSITE_FRAG};
use dust_engine::texture::{GpuTexture, TextureFilter};

struct Item {
    mesh: usize,
    model: Mat4,
    material: PbrMaterial,
}

struct Showcase {
    renderer: Option<Renderer>,
    shaders: Option<ShaderVariantCache>,
    meshes: Vec<GpuMesh>,
    white: Option<GpuTexture>,
    items: Vec<Item>,
    camera: OrbitCamera,
    lighting: SceneLighting,
    dragging: bool,
    hifi: bool,
    frame: u32,
    screenshot: Option<String>,
}

const CUBE: usize = 0;
const SPHERE: usize = 1;
const CYLINDER: usize = 2;

fn scene() -> Vec<Item> {
    let mut items = Vec::new();
    let t = |p: Vec3, s: Vec3| Mat4::from_scale_rotation_translation(s, Quat::IDENTITY, p);
    // table and board
    items.push(Item {
        mesh: CUBE,
        model: t(Vec3::new(0.0, -0.3, 0.0), Vec3::new(14.0, 0.6, 10.0)),
        material: PbrMaterial { roughness: 0.6, ..PbrMaterial::plastic([0.12, 0.07, 0.04]) },
    });
    items.push(Item {
        mesh: CUBE,
        model: t(Vec3::new(0.0, 0.05, 0.0), Vec3::new(10.0, 0.1, 7.0)),
        material: PbrMaterial { roughness: 0.85, ..PbrMaterial::plastic([0.03, 0.04, 0.08]) },
    });
    // star systems: discs with a glowing core
    let systems = [(-3.0, -1.5), (0.0, -2.0), (3.0, -1.0), (-2.0, 1.5), (1.5, 1.8)];
    for (x, z) in systems {
        items.push(Item {
            mesh: CYLINDER,
            model: t(Vec3::new(x, 0.13, z), Vec3::new(1.6, 0.06, 1.6)),
            material: PbrMaterial::metal([0.75, 0.6, 0.35], 0.35),
        });
        items.push(Item {
            mesh: SPHERE,
            model: t(Vec3::new(x, 0.35, z), Vec3::splat(0.3)),
            material: PbrMaterial { emissive: [2.5, 1.6, 0.6], ..PbrMaterial::plastic([1.0, 0.7, 0.3]) },
        });
    }
    // player pieces: ships (spheres+cylinders) in four colours
    let colors = [[0.7, 0.08, 0.06], [0.05, 0.25, 0.75], [0.85, 0.65, 0.05], [0.9, 0.9, 0.9]];
    for (i, c) in colors.iter().enumerate() {
        let (x, z) = systems[i];
        for k in 0..3 {
            let a = k as f32 * 2.1 + i as f32;
            let p = Vec3::new(x + a.cos() * 0.6, 0.3, z + a.sin() * 0.6);
            items.push(Item {
                mesh: CYLINDER,
                model: Mat4::from_scale_rotation_translation(
                    Vec3::new(0.18, 0.35, 0.18),
                    Quat::from_rotation_z(0.0),
                    p,
                ),
                material: PbrMaterial { roughness: 0.3, ..PbrMaterial::plastic(*c) },
            });
        }
        items.push(Item {
            mesh: CUBE,
            model: t(Vec3::new(x, 0.42, z + 0.0), Vec3::new(0.3, 0.5, 0.3)),
            material: PbrMaterial::metal(*c, 0.25),
        });
    }
    items
}

impl Showcase {
    fn new() -> Self {
        let mut camera = OrbitCamera::new(Vec3::ZERO, 12.0);
        camera.pitch = 0.85;
        camera.yaw = 0.35;
        camera.min_distance = 4.0;
        camera.max_distance = 30.0;
        let mut lighting = SceneLighting::default();
        lighting.point_lights.push(PointLight {
            position: [0.0, 1.5, 0.0],
            color: [1.0, 0.6, 0.3],
            intensity: 6.0,
            range: 8.0,
        });
        Self {
            renderer: None,
            shaders: None,
            meshes: Vec::new(),
            white: None,
            items: scene(),
            camera,
            lighting,
            dragging: false,
            hifi: true,
            frame: 0,
            screenshot: std::env::var("DUST_SCREENSHOT").ok(),
        }
    }

    fn apply_mode(&mut self, gl: &glow::Context) -> anyhow::Result<()> {
        let r = self.renderer.as_mut().unwrap();
        if self.hifi {
            r.set_resolution_scale(1.0);
            r.set_target_options(gl, TargetOptions::hifi())?;
        } else {
            r.set_resolution_scale(0.4);
            r.set_target_options(gl, TargetOptions::retro())?;
        }
        Ok(())
    }
}

impl Game for Showcase {
    fn init(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let gl = ctx.gl();
        let mut renderer = Renderer::new(gl, ctx.drawable_size(), 1.0, TONEMAP_COMPOSITE_FRAG)?;
        renderer.set_shadow_settings(gl, ShadowSettings::smooth());
        renderer.set_exposure(1.1);
        self.renderer = Some(renderer);
        self.apply_mode(gl)?;
        let (v, f) = patch_mesh_shaders(PBR_MESH_VERT, PBR_MESH_FRAG, "FragColor")?;
        self.shaders = Some(ShaderVariantCache::new(v, f));
        for m in [primitives::cube(), primitives::uv_sphere(32, 16), primitives::cylinder(32)] {
            self.meshes.push(GpuMesh::upload(gl, &m)?);
        }
        self.white = Some(GpuTexture::from_rgba8(gl, &[255; 4], 1, 1, TextureFilter::Bilinear)?);
        let t = self.renderer.as_ref().unwrap().scene_target();
        log::info!("scene target: hdr={} msaa={}", t.is_hdr(), t.is_multisampled());
        Ok(())
    }

    fn handle_event(&mut self, ctx: &mut Context, event: &Event) {
        match event {
            Event::KeyDown { keycode: Some(Keycode::Escape), .. } => ctx.should_quit = true,
            Event::KeyDown { keycode: Some(Keycode::R), .. } => {
                self.hifi = !self.hifi;
                if let Err(e) = self.apply_mode(ctx.gl()) {
                    log::error!("{e}");
                }
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, .. } => self.dragging = true,
            Event::MouseButtonUp { mouse_btn: MouseButton::Left, .. } => self.dragging = false,
            Event::MouseMotion { xrel, yrel, .. } if self.dragging => {
                self.camera.orbit(-*xrel as f32 * 0.005, *yrel as f32 * 0.005);
            }
            Event::MouseWheel { y, .. } => self.camera.zoom(-*y as f32),
            _ => {}
        }
    }

    fn update(&mut self, _ctx: &mut Context, _dt: f32) -> anyhow::Result<()> {
        Ok(())
    }

    fn render(&mut self, ctx: &mut Context) -> anyhow::Result<()> {
        let size = ctx.drawable_size();
        let gl = ctx.gl();
        let renderer = self.renderer.as_mut().unwrap();
        renderer.resize_if_needed(gl, size)?;
        let view = self.camera.view_matrix();
        let proj = self.camera.projection_matrix(ctx.aspect_ratio());
        let sun = Vec3::from(self.lighting.sun_dir).normalize();

        let bounds = ShadowBounds::Aabb { min: Vec3::new(-7.0, -1.0, -5.0), max: Vec3::new(7.0, 2.0, 5.0) };
        if renderer.begin_shadow_pass(gl, sun, &bounds) {
            for it in &self.items {
                renderer.shadow_draw(gl, &it.model, &self.meshes[it.mesh]);
            }
            renderer.end_shadow_pass(gl);
        }

        renderer.begin_scene(gl);
        unsafe {
            gl.clear_color(0.015, 0.015, 0.025, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.enable(glow::CULL_FACE);
            gl.cull_face(glow::BACK);
        }
        let program = self.shaders.as_mut().unwrap().get_or_compile(gl, 0)?;
        renderer.bind_shadows(gl, program);
        self.lighting.upload(gl, program, self.camera.position().to_array());
        unsafe {
            let loc = |n: &str| gl.get_uniform_location(program, n);
            gl.uniform_matrix_4_f32_slice(loc("uView").as_ref(), false, &view.to_cols_array());
            gl.uniform_matrix_4_f32_slice(loc("uProj").as_ref(), false, &proj.to_cols_array());
        }
        let white = self.white.as_ref().unwrap().handle;
        for it in &self.items {
            let mat = PbrMaterial { base_texture: Some(white), ..it.material };
            mat.upload(gl, program);
            unsafe {
                let loc = gl.get_uniform_location(program, "uModel");
                gl.uniform_matrix_4_f32_slice(loc.as_ref(), false, &it.model.to_cols_array());
            }
            self.meshes[it.mesh].draw(gl);
        }
        unsafe { gl.disable(glow::CULL_FACE) };
        renderer.present(gl, size, &PostParams::default());

        self.frame += 1;
        if let Some(path) = &self.screenshot {
            if self.frame == 3 {
                let (w, h) = size;
                let mut px = vec![0u8; (w * h * 4) as usize];
                unsafe {
                    gl.read_pixels(
                        0,
                        0,
                        w as i32,
                        h as i32,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelPackData::Slice(&mut px),
                    );
                }
                let row = (w * 4) as usize;
                let flipped: Vec<u8> = px.chunks(row).rev().flatten().copied().collect();
                dust_engine::texture::save_rgba8_png(std::path::Path::new(path), &flipped, w, h)?;
                log::info!("saved {path}");
                ctx.should_quit = true;
            }
        }
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    App::run("Dust hi-fi showcase", 1280, 720, Showcase::new())
}
