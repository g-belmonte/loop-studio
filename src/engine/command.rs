use std::sync::Arc;

use crate::track::{LoopRegion, Track};

#[derive(Debug, Clone)]
pub enum Command {
    /// Hand a fully-decoded track to the engine. The engine takes ownership of
    /// the playback cursor and resets it to frame 0.
    LoadTrack(Arc<Track>),
    Play,
    Pause,
    /// Pause and reset the cursor to frame 0.
    Stop,
    /// Seek to a source frame index (clamped to track length).
    Seek(u64),
    SetLoop(Option<LoopRegion>),
    SetSpeed(f32),
    SetPitch(f32),
}
