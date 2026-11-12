// Smoke test for the docked editor, the game's only editor: F4, F2 and F1 each
// open it (F4 also closes it), then Play (F3), hold W with the mouse captured,
// Stop (F3, scene restored via apply_level), close it. Debug builds only (the
// editor does not exist in release).
//   cargo run -p walkaround_demo -- --smoke-test scripts/smoke/docked_editor.ox
// The script cannot read game state, so "opened" / "closed" are checked from the
// log: run with RUST_LOG=info and expect, in order, the "docked editor opened"
// and "docked editor closed" lines noted next to each phase below.

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
            log("pressing F4 (expect: docked editor opened)")
            tap("F4")
            enter_phase(1.0, t)
        }
    } else if phase == 1.0 {
        if seconds_in_phase(t) > 0.7 {
            log("pressing F4 (expect: docked editor closed)")
            tap("F4")
            enter_phase(2.0, t)
        }
    } else if phase == 2.0 {
        if seconds_in_phase(t) > 0.5 {
            log("pressing F2 (expect: docked editor opened)")
            tap("F2")
            enter_phase(3.0, t)
        }
    } else if phase == 3.0 {
        if seconds_in_phase(t) > 0.7 {
            // F2 on an open editor is a no-op, F4 closes it.
            log("pressing F2 again (expect: no change)")
            tap("F2")
            log("pressing F4 (expect: docked editor closed)")
            tap("F4")
            enter_phase(4.0, t)
        }
    } else if phase == 4.0 {
        if seconds_in_phase(t) > 0.5 {
            log("pressing F1 (expect: docked editor opened)")
            tap("F1")
            enter_phase(5.0, t)
        }
    } else if phase == 5.0 {
        // Let the dock lay out and render a few frames in Edit mode.
        if seconds_in_phase(t) > 1.0 {
            tap("F3")
            enter_phase(6.0, t)
        }
    } else if phase == 6.0 {
        if seconds_in_phase(t) > 0.5 {
            press("W")
            enter_phase(7.0, t)
        }
    } else if phase == 7.0 {
        if seconds_in_phase(t) > 1.0 {
            release("W")
            tap("F3")
            enter_phase(8.0, t)
        }
    } else if phase == 8.0 {
        // Edit mode again; give the restored scene a few frames, then close the dock.
        if seconds_in_phase(t) > 1.0 {
            log("pressing F4 (expect: docked editor closed)")
            tap("F4")
            enter_phase(9.0, t)
        }
    } else if phase == 9.0 {
        if seconds_in_phase(t) > 0.5 {
            log("docked editor smoke test passed")
            finish(true)
        }
    }
}
