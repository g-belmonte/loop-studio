pub mod command;
pub mod metronome;
pub mod speed_ramp;
pub mod state;
pub mod worker;

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use crossbeam_channel::Sender;

use crate::dsp::passthrough::Passthrough;
use crate::dsp::phase_vocoder::{PhaseVocoderPitchShift, PhaseVocoderSpeed};
use crate::dsp::wsola::{WsolaPitchShift, WsolaSpeed};
use crate::dsp::{DspKind, TimePitchProcessor};

pub use command::Command;
pub use state::SharedState;

/// Build a DSP for `channels` configured to `current_speed` / `current_pitch`.
/// The selected `DspKind` picks the family; within each family, we try the
/// full pitch-shift composite first (`*PitchShift`), fall back to the
/// speed-only adapter (`*Speed`) if rubato can't be constructed for this
/// channel count (speed slider still works, pitch slider becomes a no-op),
/// and ultimately fall back to `Passthrough` for degenerate channel counts.
///
/// Used by the engine worker on every `LoadTrack` / `SetDsp`, and by the
/// offline renderer (`crate::render`) to build a fresh DSP that matches the
/// engine's settings without disturbing it.
pub fn make_dsp(
    channels: usize,
    kind: DspKind,
    current_speed: f32,
    current_pitch: f32,
) -> Box<dyn TimePitchProcessor> {
    if channels == 0 {
        log::error!("track has 0 channels; using passthrough");
        return Box::new(Passthrough::new());
    }
    match kind {
        DspKind::Wsola => match WsolaPitchShift::new(channels) {
            Ok(mut p) => {
                p.set_speed(current_speed);
                p.set_pitch_semitones(current_pitch);
                Box::new(p)
            }
            Err(e) => {
                log::error!(
                    "WsolaPitchShift failed for {channels} ch: {e:#}; \
                     falling back to WsolaSpeed (no pitch shift)"
                );
                let mut w = WsolaSpeed::new(channels);
                w.set_speed(current_speed);
                Box::new(w)
            }
        },
        DspKind::PhaseVocoder => match PhaseVocoderPitchShift::new(channels) {
            Ok(mut p) => {
                p.set_speed(current_speed);
                p.set_pitch_semitones(current_pitch);
                Box::new(p)
            }
            Err(e) => {
                log::error!(
                    "PhaseVocoderPitchShift failed for {channels} ch: {e:#}; \
                     falling back to PhaseVocoderSpeed (no pitch shift)"
                );
                let mut w = PhaseVocoderSpeed::new(channels);
                w.set_speed(current_speed);
                Box::new(w)
            }
        },
    }
}

/// GUI-side handle to the audio engine. Cheap to clone the `Arc<SharedState>`
/// out of; commands are sent over a `crossbeam-channel`.
pub struct Engine {
    tx: Sender<Command>,
    state: Arc<SharedState>,
    _join: JoinHandle<()>,
}

impl Engine {
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let state = Arc::new(SharedState::default());
        let state_for_worker = state.clone();
        let join = thread::Builder::new()
            .name("loop-studio-engine".into())
            .spawn(move || worker::run(rx, state_for_worker))
            .context("spawning engine thread")?;
        Ok(Self {
            tx,
            state,
            _join: join,
        })
    }

    pub fn send(&self, cmd: Command) {
        if let Err(e) = self.tx.send(cmd) {
            log::warn!("engine receiver dropped: {e}");
        }
    }

    pub fn state(&self) -> &SharedState {
        &self.state
    }
}
