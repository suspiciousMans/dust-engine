fn native host_cos(x: f64) -> f64;
fn native host_sin(x: f64) -> f64;
fn native host_random() -> f64;

struct State {
    time: f64,
    spawn_timer: f64
}

struct Spawn {
    x: f64, y: f64, z: f64,
    size: f64,
    r: f64, g: f64, b: f64
}

fn update_game(state: State, dt: f64, count: i64) -> (State, List<Spawn>) {
    let time = state.time + dt
    let spawn_timer = state.spawn_timer + dt
    let spawns = []

    if count < 120 && spawn_timer > 0.4 {
        let angle = host_random() * 6.2832
        let rad = 3.0 + host_random() * 5.0
        let x = host_cos(angle) * rad
        let z = host_sin(angle) * rad
        let y = 6.0 + host_random() * 4.0
        let size = 0.5 + host_random() * 0.8
        let r = host_random()
        let g = host_random()
        let b = host_random()
        let s = Spawn(x: x, y: y, z: z, size: size, r: r, g: g, b: b)
        let spawns = push(spawns, s)
    }

    let next = State(time: time, spawn_timer: if len(spawns) > 0 { 0.0 } else { spawn_timer })
    return (next, spawns)
}

fn describe_game(time: f64, count: i64) {
    print("Physics Sandbox | objects: ", count, " | time: ", time)
}
