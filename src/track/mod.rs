pub mod peaks;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopRegion {
    pub start: u64,
    pub end: u64,
}

pub struct Track {
    pub samples: Vec<f32>,   // interleaved
    pub sample_rate: u32,
    pub channels: u16,
}

impl Track {
    pub fn frame_count(&self) -> u64 {
        self.samples.len() as u64 / self.channels as u64
    }
}
