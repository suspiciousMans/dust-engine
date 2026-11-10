# Converted from bob_demo.pss by `oxc_scripts convert`. Review before use.
# Top-level `let` state now lives in engine-held per-entity vars (get_var/set_var).

// Demo script attached to "Script Demo Cube" in the default level.
// Bobs up and down and reacts to interact — deliberately the same math as
// `NativeBobBehavior` in engine/src/behavior.rs (the "Native Behavior Cube"
// standing next to this one), to make the point that a script and a
// compiled Rust type can drive identical behavior through the same hooks.





fn on_ready() {
    log("Script Demo Cube ready!")
}

fn on_update(dt: Float) {
    let previous_phase = get_var("phase", 0.0)
    set_var("phase", get_var("phase", 0.0) + dt)
    // 6.283185307 is tau (2*pi) — this language has no named constants yet.
    let wave_now = sin(get_var("phase", 0.0) / get_var("period_secs", 2.0) * tau())
    let wave_prev = sin(previous_phase / get_var("period_secs", 2.0) * tau())
    move_by(0.0, get_var("amplitude", 0.4) * (wave_now - wave_prev), 0.0)
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
