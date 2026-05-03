use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

/// State written by the engine, read by the GUI. Lock-free.
pub struct SharedState {
    pub position: AtomicU64,
    pub duration: AtomicU64,
    pub playing: AtomicBool,
    pub speed_bits: AtomicU32,
    pub pitch_bits: AtomicU32,
    pub loaded_id: AtomicU64,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            position: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            speed_bits: AtomicU32::new(1.0_f32.to_bits()),
            pitch_bits: AtomicU32::new(0.0_f32.to_bits()),
            loaded_id: AtomicU64::new(0),
        }
    }
}
