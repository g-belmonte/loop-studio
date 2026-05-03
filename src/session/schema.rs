use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::track::LoopRegion;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub track_path: PathBuf,
    pub track_sample_rate: u32,
    pub loop_region: Option<LoopRegion>,
    pub speed: f32,
    pub pitch_semitones: f32,
    pub last_position: u64,
}
