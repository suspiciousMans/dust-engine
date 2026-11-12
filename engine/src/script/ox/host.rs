//! Glue a game needs to run `.ox` behavior scripts: one [`OxScripts`] per game
//! owns the runtime, builds scripts on first use (cached on disk, ~3-6 s cold,
//! instant warm), hands out [`Behavior`]s for entities, feeds input in and
//! drains script commands out, and surfaces background hot-reload results.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hecs::{Entity, World};

use super::api::{InputSnapshot, OxCommand, OxServices};
use super::runtime::OxRuntime;
use crate::behavior::Behavior;
use crate::ecs::Transform;
use crate::input::Input;

pub struct OxScripts {
    rt: OxRuntime,
    asset_root: PathBuf,
}

impl OxScripts {
    /// Scripts live in `<asset_root>/scripts/*.ox`; build output goes to
    /// `<asset_root>/.ox-build`. Fails if `oxidized`/`rustc` can't be found.
    /// Starts a (non-recursive) file watcher on the scripts dir when it exists.
    pub fn new(asset_root: &Path) -> Result<Self, String> {
        let scripts = asset_root.join("scripts");
        let mut rt = OxRuntime::new(&scripts, asset_root.join(".ox-build"))?;
        if scripts.is_dir() {
            if let Err(e) = rt.start_watching() {
                log::warn!("script hot reload unavailable: {e}");
            }
        }
        Ok(Self { rt, asset_root: asset_root.to_path_buf() })
    }

    /// True for paths this system handles (`*.ox`).
    pub fn is_script_path(path: &Path) -> bool {
        path.extension().map_or(false, |e| e == "ox")
    }

    /// A behavior for `entity` running the script at `relative` (relative to
    /// the asset root, e.g. `scripts/bob_demo.ox`). Builds it first if no build
    /// is loaded yet. On a build error the behavior is still returned (it
    /// no-ops until a fixed save hot-swaps in) together with the error lines.
    pub fn behavior(&mut self, relative: &Path, entity: Entity) -> (Box<dyn Behavior>, Vec<String>) {
        let file = self.asset_root.join(relative);
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("script").to_string();
        let mut errors = Vec::new();
        if !self.rt.is_loaded(&stem) {
            if let Err(d) = self.rt.reload(&file) {
                errors = d.to_strings();
            }
        }
        let mut b = self.rt.behavior(&stem);
        b.entity = entity.to_bits().get();
        (Box::new(b), errors)
    }

    /// Call once per frame before dispatching hooks.
    pub fn begin_frame(&self, input: &Input) {
        self.rt.services().set_input(snapshot(input));
    }

    /// Finished background rebuilds, as console lines (`ok` or diagnostics).
    pub fn poll(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for r in self.rt.poll() {
            match r.result {
                Ok(()) => lines.push(format!("script '{}' reloaded", r.script)),
                Err(d) => lines.extend(d.to_strings().into_iter().map(|l| format!("script error: {l}"))),
            }
        }
        lines
    }

    pub fn services(&self) -> Arc<OxServices> {
        self.rt.services()
    }

    /// Applies the commands scripts queued during the frame that only need the
    /// `World` (`set_rot`, `set_scale`). Returns the rest (`spawn`/`despawn`)
    /// for the game to handle.
    pub fn apply_commands(&self, world: &mut World) -> Vec<OxCommand> {
        apply_basic_commands(world, self.rt.services().drain_commands())
    }

    pub fn runtime(&self) -> &OxRuntime {
        &self.rt
    }
}

/// `set_rot`/`set_scale` onto `Transform`s (rotation is XYZ Euler radians, matching
/// `euler_deg_to_quat`); everything else is returned.
pub fn apply_basic_commands(world: &mut World, commands: Vec<OxCommand>) -> Vec<OxCommand> {
    let mut rest = Vec::new();
    for c in commands {
        match c {
            OxCommand::SetRotation { entity, rotation } => {
                if let Some(e) = Entity::from_bits(entity) {
                    if let Ok(mut t) = world.get::<&mut Transform>(e) {
                        t.rotation = glam::Quat::from_euler(glam::EulerRot::XYZ, rotation[0], rotation[1], rotation[2]);
                    }
                }
            }
            OxCommand::SetScale { entity, scale } => {
                if let Some(e) = Entity::from_bits(entity) {
                    if let Ok(mut t) = world.get::<&mut Transform>(e) {
                        t.scale = glam::Vec3::from(scale);
                    }
                }
            }
            other => rest.push(other),
        }
    }
    rest
}

/// The input a script sees: held/just-pressed key names (SDL names, e.g.
/// `"Space"`, `"W"`) and `move_x`/`move_y` axes from WASD + the left stick.
pub fn snapshot(input: &Input) -> InputSnapshot {
    use sdl2::keyboard::Keycode as K;
    let mut s = InputSnapshot::default();
    s.down.extend(input.keys_down().map(|k| k.name()));
    s.pressed.extend(input.keys_just_pressed().map(|k| k.name()));
    let held = |k: K| input.is_key_down(k) as i32 as f32;
    let (sx, sy) = input.left_stick();
    s.axes.insert("move_x".into(), (held(K::D) - held(K::A) + sx).clamp(-1.0, 1.0));
    s.axes.insert("move_y".into(), (held(K::W) - held(K::S) - sy).clamp(-1.0, 1.0));
    s
}
