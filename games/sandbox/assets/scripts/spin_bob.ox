# Spin + bob demo behavior (replaces scripts/bob_demo.pss).
# Hooks: on_ready / on_update(dt) / on_interact / on_trigger_enter(other) / on_trigger_exit(other)
# State lives in the engine (get_var/set_var) because script functions are stateless.
# Tunables (tunable("name", default)) can be overridden per instance by the level/editor.

fn on_ready() {
    set_var("base_y", pos(1))
    log("spin_bob ready at y={pos(1)}")
}

fn on_update(dt: Float) {
    let speed = tunable("spin_speed", 1.5)
    let amplitude = tunable("bob_amplitude", 0.4)
    let period = tunable("bob_period", 2.0)

    if key_down("Space") == 1 {
        speed = speed * 3.0
    }

    let t = get_var("t", 0.0) + dt
    set_var("t", t)

    set_rot(rot(0), rot(1) + speed * dt, rot(2))

    let wave = sin(t / period * tau())
    set_pos(pos(0), get_var("base_y", 0.0) + amplitude * wave, pos(2))
    hud_bar("Wobble", wave * 0.5 + 0.5)
}

fn on_interact() {
    log("spin_bob says hi!")
    play_tone(500.0, 0.1)
    play_sound("sfx/demo_blip.wav")
}

fn on_trigger_enter(other: String) {
    log("trigger enter: {other}")
    let id = spawn("pickup_spark", pos(0), pos(1) + 1.0, pos(2))
    set_var("spark", id as Float)
}

fn on_trigger_exit(other: String) {
    log("trigger exit: {other}")
    despawn(get_var("spark", 0.0) as Int)
}
