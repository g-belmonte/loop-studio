use std::sync::Arc;

use crate::dsp::DspKind;
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
    /// Switch DSP family. Tears down the current processor and rebuilds for
    /// the loaded track's channel count, carrying current speed/pitch across.
    /// No-op if no track is loaded yet — the kind is remembered and applied
    /// on the next `LoadTrack`.
    SetDsp(DspKind),
}
