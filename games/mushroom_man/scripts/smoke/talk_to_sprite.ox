// Scripted smoke test, driven by engine::app::App::run_scripted /
// engine::script::ox::SmokeDriver (not the in-game Behavior/ScriptApi
// system bob_demo.ox uses — this script's `tick(frame, t)` is called once per
// real frame by the smoke-test harness itself, and its "standard library"
// is press/release/assert_true/assert_eq/assert_eq_str/finish/log/time and
// get_var/set_var instead; see docs/SCRIPTING-OXIDIZED.md §8).
//
// Enters Play mode, walks toward roughly where the Cave Sprite stands in
// the "entrance" level, presses E to attempt a dialogue interaction, then
// finishes successfully — proving the harness can drive a real window
// through a multi-step playthrough (movement + interact) without the game
// hanging or crashing. Run with:
//   cargo run -p mushroom_man -- --smoke-test scripts/smoke/talk_to_sprite.ox

# Phase bookkeeping. Script functions are stateless, so the current phase and
# the time it started live host-side (get_var/set_var).
fn enter_phase(next: Float, t: Float) {
    set_var("phase", next)
    set_var("phase_started_at", t)
    log("entering phase " + str(next))
}

fn seconds_in_phase(t: Float) -> Float {
    return t - get_var("phase_started_at", 0.0)
}

fn tick(frame: Int, t: Float) {
    let phase = get_var("phase", 0.0)
    if phase == 0.0 {
        // Toggle from Edit mode into Play mode.
        press("F3")
        release("F3")
        enter_phase(1.0, t)
    } else if phase == 1.0 {
        // Give Play mode a moment to spin up, then walk (and turn slightly
        // right) toward the Cave Sprite's rough position near the entrance.
        if seconds_in_phase(t) > 0.3 {
            press("W")
            press("D")
            enter_phase(2.0, t)
        }
    } else if phase == 2.0 {
        if seconds_in_phase(t) > 1.5 {
            release("W")
            release("D")
            enter_phase(3.0, t)
        }
    } else if phase == 3.0 {
        // Attempt to start a dialogue with whatever's in front of us.
        press("E")
        release("E")
        enter_phase(4.0, t)
    } else if phase == 4.0 {
        if seconds_in_phase(t) > 0.5 {
            enter_phase(5.0, t)
        }
    } else if phase == 5.0 {
        assert_true(t > 2.0, "expected in-game time to have advanced past 2 seconds")
        log("talk to sprite smoke test passed")
        finish(true)
    }
}
