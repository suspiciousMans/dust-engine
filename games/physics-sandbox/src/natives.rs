pub mod __oxidized_natives {
    pub fn host_cos(x: f64) -> f64 {
        x.cos()
    }

    pub fn host_sin(x: f64) -> f64 {
        x.sin()
    }

    pub fn host_random() -> f64 {
        use std::sync::atomic::{AtomicU64, Ordering};

        static RNG: AtomicU64 = AtomicU64::new(1);
        let state = RNG.load(Ordering::Relaxed).wrapping_mul(1103515245).wrapping_add(12345);
        RNG.store(state, Ordering::Relaxed);
        // Top 31 bits of the 64-bit state, scaled into [0, 1). (The old
        // `(state >> 16) / 65536` returned values up to ~4e9, so spawned
        // cubes were billions of units wide and the spatial hash tried to
        // insert them into billions of cells, eating all RAM.)
        ((state >> 33) as f64) / ((1u64 << 31) as f64)
    }
}
