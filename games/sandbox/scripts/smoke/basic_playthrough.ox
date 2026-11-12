// Scripted smoke test, driven by engine::app::App::run_scripted /
// engine::script::ox::SmokeDriver (not the in-game Behavior/ScriptApi
// system bob_demo.ox uses — this script's `tick(frame, t)` is called once per
// real frame by the smoke-test harness itself, and its "standard library"
// is press/release/assert_true/assert_eq/assert_eq_str/finish/log/time and
// get_var/set_var instead; see docs/SCRIPTING-OXIDIZED.md §8).
//
// Enters Play mode, walks forward for a couple of seconds, then finishes
// successfully — proving the harness can drive a real window end to end
// (input injection, a live game loop, timed phases) without the game
// hanging or crashing. Run with:
//   cargo run -p sandbox -- --smoke-test scripts/smoke/basic_playthrough.ox

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
        // Give Play mode a moment to spin up before moving.
        if seconds_in_phase(t) > 0.3 {
            press("W")
            enter_phase(2.0, t)
        }
    } else if phase == 2.0 {
        if seconds_in_phase(t) > 2.0 {
            release("W")
            enter_phase(3.0, t)
        }
    } else if phase == 3.0 {
        assert_true(t > 2.0, "expected in-game time to have advanced past 2 seconds")
        log("basic playthrough smoke test passed")
        finish(true)
    }
}
