use serde::{Deserialize, Serialize};

/// User-tweakable speed-ramp state. Lives in `App` (UI source of truth) and is
/// pushed to the engine via `Command::SetSpeedRamp` on every change. Persisted
/// in the session schema (v6+).
///
/// Ramping bumps the speed once every `passes_per_step` loop wraps, by
/// `step_amount` in either percentage-of-1× or BPM units, in the direction of
/// `target_speed`, snapping to target when the next step would overshoot. The
/// engine no-ops the bump when speed is already at target. Has no effect when
/// no loop is active (no wraps to count). Bumping the slider manually clears
/// `enabled` from the UI side — see [`crate::ui::transport`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct SpeedRampSettings {
    pub enabled: bool,
    /// Where the ramp should land. Ramp direction is `target_speed - speed`.
    pub target_speed: f32,
    pub step_unit: StepUnit,
    /// Magnitude of one step in the chosen unit. Always positive; the worker
    /// picks the sign from `target_speed - speed`.
    pub step_amount: f32,
    /// Loops between bumps. Clamped to ≥ 1 at the boundary.
    pub passes_per_step: u32,
}

/// Unit of one ramp step. Percent is a fixed delta on the speed multiplier
/// (5% → speed += 0.05 per step). Bpm converts via the metronome's BPM
/// (5 BPM with source = 120 BPM → speed += 5/120 ≈ 0.0417 per step), so
/// switching to BPM mode requires the user to have a meaningful BPM dialed
/// into the metronome row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepUnit {
    Percent,
    Bpm,
}

impl Default for SpeedRampSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            target_speed: 1.0,
            step_unit: StepUnit::Percent,
            step_amount: 5.0,
            passes_per_step: 2,
        }
    }
}

/// Resolve one step into a delta on the speed multiplier, given the metronome's
/// current source BPM (used only for [`StepUnit::Bpm`]). Returns 0.0 when the
/// inputs don't make sense (non-finite, zero BPM, etc.) so the caller treats
/// it as a no-op rather than poisoning the ramp.
pub fn step_in_speed_units(settings: &SpeedRampSettings, source_bpm: f32) -> f32 {
    if !settings.step_amount.is_finite() || settings.step_amount <= 0.0 {
        return 0.0;
    }
    match settings.step_unit {
        StepUnit::Percent => settings.step_amount / 100.0,
        StepUnit::Bpm => {
            if !source_bpm.is_finite() || source_bpm <= 0.0 {
                return 0.0;
            }
            settings.step_amount / source_bpm
        }
    }
}
