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
        ((state >> 16) as f64) / 65536.0
    }
}
