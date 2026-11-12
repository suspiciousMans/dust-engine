// Scripted smoke test for the docked editor (debug builds only).
//
// The docked editor is the game's only editor: F4, F2 and F1 each open it
// (F4 closes it again). Presses F4 (open), Play (F3), Stop (F3), F4 (close), F2
// (open), F4 (close), F1 (open), F4 (close). The harness injects these as
// in-process SDL events; nothing touches the desktop's input. Check the log for:
//   "docked editor opened" x3, "docked editor closed" x3 and
//   "entered play mode" ... "exited play mode, restored edit-time state"
// Run with:
//   cargo run -p mushroom_man -- --smoke-test scripts/smoke/docked_editor.ox

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

fn tap(key: String) {
    press(key)
    release(key)
}

fn tick(frame: Int, t: Float) {
    let phase = get_var("phase", 0.0)
    if phase == 0.0 {
        if seconds_in_phase(t) > 0.5 {
            tap("F4")
            enter_phase(1.0, t)
        }
    } else if phase == 1.0 {
        if seconds_in_phase(t) > 1.0 {
            tap("F3")
            enter_phase(2.0, t)
        }
    } else if phase == 2.0 {
        if seconds_in_phase(t) > 1.5 {
            tap("F3")
            enter_phase(3.0, t)
        }
    } else if phase == 3.0 {
        if seconds_in_phase(t) > 1.0 {
            tap("F4")
            enter_phase(4.0, t)
        }
    } else if phase == 4.0 {
        if seconds_in_phase(t) > 0.5 {
            tap("F2")
            enter_phase(5.0, t)
        }
    } else if phase == 5.0 {
        if seconds_in_phase(t) > 0.7 {
            tap("F4")
            enter_phase(6.0, t)
        }
    } else if phase == 6.0 {
        if seconds_in_phase(t) > 0.5 {
            tap("F1")
            enter_phase(7.0, t)
        }
    } else if phase == 7.0 {
        if seconds_in_phase(t) > 0.7 {
            tap("F4")
            enter_phase(8.0, t)
        }
    } else if phase == 8.0 {
        if seconds_in_phase(t) > 0.5 {
            log("docked editor smoke test passed")
            finish(true)
        }
    }
}
