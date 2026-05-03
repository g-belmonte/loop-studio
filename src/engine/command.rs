use std::path::PathBuf;

use crate::track::LoopRegion;

#[derive(Debug, Clone)]
pub enum Command {
    Load(PathBuf),
    Play,
    Pause,
    Seek(u64),
    SetLoop(Option<LoopRegion>),
    SetSpeed(f32),
    SetPitch(f32),
}
