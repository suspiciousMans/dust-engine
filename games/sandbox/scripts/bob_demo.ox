# Demo script attached to "Script Demo Cube" in the default level.
# Bobs up and down and reacts to interact — deliberately the same math as
# `NativeBobBehavior` in engine/src/behavior.rs (the "Native Behavior Cube"
# standing next to this one), to make the point that a script and a
# compiled Rust type can drive identical behavior through the same hooks.
#
# Ported from bob_demo.pss. Script functions are stateless, so the phase
# accumulator lives in an engine-held per-entity variable (get_var/set_var);
# amplitude and period are tunables (designer-overridable, same defaults).

fn on_ready() {
    log("Script Demo Cube ready!")
}

fn on_update(dt: Float) {
    let amplitude = tunable("amplitude", 0.4)
    let period_secs = tunable("period_secs", 2.0)
    let previous_phase = get_var("phase", 0.0)
    let phase = previous_phase + dt
    set_var("phase", phase)
    let wave_now = sin(phase / period_secs * tau())
    let wave_prev = sin(previous_phase / period_secs * tau())
    move_by(0.0, amplitude * (wave_now - wave_prev), 0.0)
    // Demonstrates a script driving the HUD: a "Wobble" bar that tracks
    // how close the cube is to the top of its bob cycle.
    hud_bar("Wobble", wave_now * 0.5 + 0.5)
}

fn on_interact() {
    log("Scripted cube says hi!")
    play_tone(500.0, 0.1)
    // Demonstrates a script playing a real decoded sample file, side by
    // side with the procedural tone above.
    play_sound("sfx/demo_blip.wav")
}
