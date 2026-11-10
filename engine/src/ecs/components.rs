use std::path::PathBuf;
use std::sync::Arc;

use glam::{EulerRot, Mat4, Quat, Vec3};

use crate::camera::CameraShakeSpec;
use crate::level::{LevelTransition, MeshSource};
use crate::mesh::GpuMesh;
use crate::screen_effect::ScreenEffectSpec;
use crate::texture::GpuTexture;

/// Converts GUI-edited Euler degrees to the quaternion `Transform::rotation`
/// actually stores. Kept as one shared function (rather than each caller
/// rolling its own) so every Euler-authoring path — level objects, rig
/// parts — agrees on axis order.
pub fn euler_deg_to_quat(euler_deg: Vec3) -> Quat {
    Quat::from_euler(
        EulerRot::XYZ,
        euler_deg.x.to_radians(),
        euler_deg.y.to_radians(),
        euler_deg.z.to_radians(),
    )
}

#[derive(Clone, Copy, Debug)]
pub struct Transform {
    pub position: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Transform {
    pub fn from_position(position: Vec3) -> Self {
        Self {
            position,
            ..Default::default()
        }
    }

    pub fn matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.position)
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

/// Shared handles so one loaded mesh/texture can back many entities without
/// re-uploading GPU resources per instance.
#[derive(Clone)]
pub struct MeshRenderer {
    pub mesh: Arc<GpuMesh>,
    pub texture: Option<Arc<GpuTexture>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub fov_y_radians: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            fov_y_radians: 45f32.to_radians(),
            near: 0.1,
            far: 500.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LightKind {
    Directional { direction: Vec3 },
    Point { range: f32 },
}

#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub color: Vec3,
    pub intensity: f32,
    pub kind: LightKind,
    /// Mirrors `engine::level::LevelLight::is_static` — see
    /// `Sandbox::bake_static_lighting`.
    pub is_static: bool,
}

/// Marks an entity as a level-editor-placed object and carries enough
/// information to serialize it back into a `LevelObject`. `rotation_euler_deg`
/// is the authoritative editable rotation (the GUI edits this directly);
/// `Transform::rotation` is recomputed from it on every edit rather than
/// converting back and forth, which would drift the value over repeated edits.
#[derive(Clone, Debug)]
pub struct LevelObjectMeta {
    pub name: String,
    pub mesh_source: MeshSource,
    pub texture_path: Option<PathBuf>,
    pub rotation_euler_deg: Vec3,
    /// Path to the `.pss` script backing this object's `BehaviorSlot`, if
    /// any — kept here (mirroring `texture_path`) so the F2 panel and
    /// `build_level_from_ecs` can round-trip it without inspecting the
    /// `BehaviorSlot` component itself.
    pub script_path: Option<PathBuf>,
    /// Mirrors `LevelObject::level_transition` for the same reason as
    /// `script_path` — round-trippable by the F2 panel and
    /// `build_level_from_ecs` without inspecting other components.
    pub level_transition: Option<LevelTransition>,
    /// Mirrors `LevelObject::screen_effect` for the same reason as
    /// `level_transition` — round-trippable by the F2 panel and
    /// `build_level_from_ecs` without inspecting other components.
    pub screen_effect: Option<ScreenEffectSpec>,
    /// Mirrors `LevelObject::camera_shake` for the same reason as
    /// `screen_effect` — round-trippable by the F2 panel and
    /// `build_level_from_ecs` without inspecting other components.
    pub camera_shake: Option<CameraShakeSpec>,
}

/// Tags the player entity in Play mode. Plain tunable fields — this is the
/// obvious place to retune movement feel for your own game.
#[derive(Clone, Copy, Debug)]
pub struct PlayerController {
    pub move_speed: f32,
    pub jump_speed: f32,
    pub interact_radius: f32,
    pub interact_impulse: f32,
}

impl Default for PlayerController {
    fn default() -> Self {
        Self {
            move_speed: 4.0,
            jump_speed: 4.5,
            interact_radius: 2.0,
            interact_impulse: 6.0,
        }
    }
}

/// Describes the engine's own components to the editor inspector
/// (see `editor::inspector`). Add new inspectable components here.
pub fn register_engine_components(reg: &mut crate::editor::inspector::Registry) {
    use crate::editor::inspector::ComponentBuilder;

    reg.register(
        ComponentBuilder::new("Transform", Transform::default)
            .not_removable()
            .vec3("position", |t| t.position, |t, v| t.position = v)
            .tooltip("World-space position")
            .vec3(
                "rotation",
                |t| {
                    let (x, y, z) = t.rotation.to_euler(EulerRot::XYZ);
                    Vec3::new(x.to_degrees(), y.to_degrees(), z.to_degrees())
                },
                |t, v| t.rotation = euler_deg_to_quat(v),
            )
            .tooltip("Euler angles in degrees (XYZ)")
            .vec3("scale", |t| t.scale, |t, v| t.scale = v),
    );
    reg.register(
        ComponentBuilder::new("Camera", Camera::default)
            .f32("fov_y_degrees", |c| c.fov_y_radians.to_degrees(), |c, v| c.fov_y_radians = v.to_radians())
            .range(1.0, 179.0)
            .tooltip("Vertical field of view")
            .f32("near", |c| c.near, |c, v| c.near = v)
            .range(0.001, 100.0)
            .speed(0.01)
            .f32("far", |c| c.far, |c, v| c.far = v)
            .range(1.0, 100000.0)
            .speed(1.0),
    );
    reg.register(
        ComponentBuilder::new("Light", || Light {
            color: Vec3::ONE,
            intensity: 1.0,
            kind: LightKind::Point { range: 10.0 },
            is_static: false,
        })
        .color("color", |l| l.color.to_array(), |l, v| l.color = Vec3::from(v))
        .f32("intensity", |l| l.intensity, |l, v| l.intensity = v)
        .range(0.0, 100.0)
        .enumeration(
            "kind",
            &["Directional", "Point"],
            |l| matches!(l.kind, LightKind::Point { .. }) as usize,
            |l, i| {
                l.kind = if i == 0 {
                    LightKind::Directional { direction: Vec3::new(0.0, -1.0, -0.3).normalize() }
                } else {
                    LightKind::Point { range: 10.0 }
                }
            },
        )
        .tooltip("Switching kind resets direction/range to defaults")
        .f32(
            "range",
            |l| match l.kind {
                LightKind::Point { range } => range,
                _ => 0.0,
            },
            |l, v| {
                if let LightKind::Point { range } = &mut l.kind {
                    *range = v;
                }
            },
        )
        .range(0.0, 1000.0)
        .tooltip("Point lights only")
        .vec3(
            "direction",
            |l| match l.kind {
                LightKind::Directional { direction } => direction,
                _ => Vec3::ZERO,
            },
            |l, v| {
                if let LightKind::Directional { direction } = &mut l.kind {
                    *direction = v;
                }
            },
        )
        .tooltip("Directional lights only")
        .bool("is_static", |l| l.is_static, |l, v| l.is_static = v)
        .tooltip("Baked into static lighting"),
    );
    reg.register(
        ComponentBuilder::new("PlayerController", PlayerController::default)
            .f32("move_speed", |p| p.move_speed, |p, v| p.move_speed = v)
            .range(0.0, 50.0)
            .f32("jump_speed", |p| p.jump_speed, |p, v| p.jump_speed = v)
            .range(0.0, 50.0)
            .f32("interact_radius", |p| p.interact_radius, |p, v| p.interact_radius = v)
            .range(0.0, 20.0)
            .f32("interact_impulse", |p| p.interact_impulse, |p, v| p.interact_impulse = v)
            .range(0.0, 100.0),
    );
    reg.register_cloneable::<MeshRenderer>();
    reg.register_cloneable::<LevelObjectMeta>();
}
