/// What a script (or a native Rust `Behavior`) can do to the entity it's
/// attached to, without needing direct access to the ECS `World` or
/// `AudioContext` — the embedder builds a concrete implementation of this
/// per dispatch, scoped to just that one entity.
pub trait ScriptApi {
    fn log(&mut self, message: &str);
    fn position(&self) -> (f32, f32, f32);
    fn set_position(&mut self, x: f32, y: f32, z: f32);
    fn move_by(&mut self, dx: f32, dy: f32, dz: f32);
    fn play_tone(&mut self, frequency_hz: f32, duration_secs: f32);
    /// Plays a sound file once, fire-and-forget. `path` is relative to the
    /// game's asset root, same convention as script/texture paths.
    fn play_sfx(&mut self, path: &str);
    /// Seconds since the game started — handy for scripts driving their own
    /// motion (`sin(time() * speed)`) without tracking a local phase.
    fn elapsed(&self) -> f32;
    /// Sets (or creates) a named HUD progress bar, `fraction` clamped to
    /// `0.0..=1.0` — e.g. a health or stamina bar.
    fn set_hud_bar(&mut self, name: &str, fraction: f32);
    /// Shows a transient HUD message that fades after `seconds`.
    fn show_toast(&mut self, message: &str, seconds: f32);
    /// Fades a full-screen color tint in over `fade_in_secs`, holds it for
    /// `hold_secs`, then fades it back out over `fade_out_secs` — composited
    /// into the frame alongside the posterize/dither post effect. `strength`
    /// is the peak tint strength, `0.0..=1.0`.
    fn screen_flash(
        &mut self,
        r: f32,
        g: f32,
        b: f32,
        strength: f32,
        fade_in_secs: f32,
        hold_secs: f32,
        fade_out_secs: f32,
    );
    /// Jostles the camera's eye position for `duration_secs`, decaying
    /// linearly from `intensity` (world units) back to zero.
    fn camera_shake(&mut self, intensity: f32, duration_secs: f32);
}

/// Programmable per-entity behavior. The exact same trait is implemented
/// either by `script::ox::OxBehavior` (running a compiled Oxidized `.ox`
/// script) or by a plain Rust type (see `NativeBobBehavior` below) — the game dispatches
/// through `Behavior` uniformly and never needs to know which kind backed a
/// given entity. This is what "use Rust the same way the scripting language
/// would" means in practice: prototype as a script, port to Rust later, and
/// nothing about how it's wired into the game has to change.
pub trait Behavior: Send + Sync {
    fn on_ready(&mut self, api: &mut dyn ScriptApi) {
        let _ = api;
    }
    fn on_update(&mut self, api: &mut dyn ScriptApi, dt: f32) {
        let _ = (api, dt);
    }
    fn on_interact(&mut self, api: &mut dyn ScriptApi) {
        let _ = api;
    }
    fn on_trigger_enter(&mut self, api: &mut dyn ScriptApi, other_name: &str) {
        let _ = (api, other_name);
    }
    fn on_trigger_exit(&mut self, api: &mut dyn ScriptApi, other_name: &str) {
        let _ = (api, other_name);
    }
}

/// The ECS component: wraps whichever `Behavior` an entity has, script or
/// native, behind one boxed trait object.
pub struct BehaviorSlot(pub Box<dyn Behavior>);

/// Native-Rust parity example: the identical `Behavior` trait, no script
/// involved. Sine-bobs vertically and logs+tones on interact —
/// deliberately mirroring what `bob_demo.ox` does as a script, so
/// interacting with both demo cubes side by side makes the parity concrete.
pub struct NativeBobBehavior {
    phase: f32,
    amplitude: f32,
    period_secs: f32,
}

impl NativeBobBehavior {
    pub fn new(amplitude: f32, period_secs: f32) -> Self {
        Self { phase: 0.0, amplitude, period_secs }
    }
}

impl Behavior for NativeBobBehavior {
    fn on_update(&mut self, api: &mut dyn ScriptApi, dt: f32) {
        // `move_by`, not `set_position`: `ScriptApi` has no notion of a
        // "base" position, only the entity's current one, so motion is
        // expressed as the delta between this frame's and last frame's
        // point on the sine wave rather than an absolute offset.
        let previous_phase = self.phase;
        self.phase += dt;
        let wave = |p: f32| (p / self.period_secs * std::f32::consts::TAU).sin();
        api.move_by(0.0, self.amplitude * (wave(self.phase) - wave(previous_phase)), 0.0);
    }

    fn on_interact(&mut self, api: &mut dyn ScriptApi) {
        api.log("Native behavior cube says hi!");
        api.play_tone(750.0, 0.1);
    }
}
