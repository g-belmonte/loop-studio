use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::dsp::DspKind;
use crate::dsp::eq::EqSettings;
use crate::engine::metronome::MetronomeSettings;
use crate::engine::speed_ramp::SpeedRampSettings;
use crate::track::{LoopRegion, Marker};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub track_path: PathBuf,
    pub track_sample_rate: u32,
    pub loop_region: Option<LoopRegion>,
    pub speed: f32,
    pub pitch_semitones: f32,
    pub last_position: u64,
    pub dsp_kind: DspKind,
    pub markers: Vec<Marker>,
    pub metronome: MetronomeSettings,
    pub eq: EqSettings,
    pub speed_ramp: SpeedRampSettings,
}

impl Session {
    pub const CURRENT_VERSION: u32 = 6;

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serialising session")?;
        std::fs::write(path, json)
            .with_context(|| format!("writing session to {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading session from {}", path.display()))?;
        let session: Session =
            serde_json::from_slice(&bytes).context("parsing session JSON")?;
        if session.version != Self::CURRENT_VERSION {
            bail!(
                "unsupported session version {} (expected {})",
                session.version,
                Self::CURRENT_VERSION
            );
        }
        Ok(session)
    }
}
