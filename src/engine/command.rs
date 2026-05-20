use std::sync::Arc;

use crate::dsp::DspKind;
use crate::dsp::eq::EqSettings;
use crate::engine::metronome::MetronomeSettings;
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
    /// Replace the metronome state in one shot. Cheap (Copy), so the UI
    /// sends it on every relevant control change rather than splitting into
    /// per-field commands.
    SetMetronome(MetronomeSettings),
    /// Master output gain in dB. Applied post-DSP and post-metronome as the
    /// engine's final stage, with a per-chunk linear ramp to the new target so
    /// fast slider drags don't zipper. Not persisted in sessions — resets to
    /// 0 dB on every app launch.
    SetMasterVolume(f32),
    /// Replace the EQ state in one shot. Like SetMetronome, the UI sends the
    /// full settings struct on every relevant change rather than splitting
    /// into per-field commands. Applied between DSP and metronome on the
    /// worker; coefficient smoothing across the chunk is handled inside `Eq`.
    SetEq(EqSettings),
}
