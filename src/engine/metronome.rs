use serde::{Deserialize, Serialize};

/// User-tweakable metronome state. Lives in `App` (UI source of truth) and is
/// pushed to the engine via `Command::SetMetronome` on every change. Persisted
/// in the session schema (v4+).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct MetronomeSettings {
    pub enabled: bool,
    pub bpm: f32,
    pub accent: bool,
    pub beats_per_measure: u32,
    pub volume_db: f32,
}

impl Default for MetronomeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bpm: 120.0,
            accent: true,
            beats_per_measure: 4,
            volume_db: -6.0,
        }
    }
}

/// One short click sound: 40 ms windowed sine, exponential 8 ms decay.
const CLICK_DURATION_SECS: f32 = 0.040;
const CLICK_DECAY_TAU_SECS: f32 = 0.008;
const CLICK_FREQ_NORMAL_HZ: f32 = 1000.0;
const CLICK_FREQ_ACCENT_HZ: f32 = 1500.0;
/// BPM safety floor / ceiling. Below 20 BPM intervals exceed 3 s (effectively
/// off); above 400 we'd start tripping the per-frame == 0 guard at extreme
/// sample rates.
pub const MIN_BPM: f32 = 20.0;
pub const MAX_BPM: f32 = 400.0;

/// Source-frame-anchored beat scheduler + mixer. Owned by the engine worker.
///
/// Beats fire at `anchor + k * (sr * 60 / bpm)` source frames; the anchor is
/// `loop.start` when a loop is active, else 0. On each produced output chunk
/// the worker calls [`mix_segment`] with the source range the chunk consumed
/// and the just-written output samples; we map beat frames to output frames
/// linearly within the chunk (an approximation, but with chunks ~23 ms this is
/// well below perceptual onset precision) and mix a click voice in place.
pub struct Metronome {
    settings: MetronomeSettings,
    sample_rate: u32,
    anchor_frame: u64,
    click_normal: Vec<f32>,
    click_accent: Vec<f32>,
    /// Currently-sounding click, or None when silent. New beats override the
    /// in-flight voice — at musical tempos beats are >>40 ms apart so layering
    /// would just over-complicate this.
    voice: Option<Voice>,
}

struct Voice {
    accent: bool,
    /// Sample offset within the active click buffer.
    pos: usize,
}

impl Metronome {
    pub fn new() -> Self {
        Self {
            settings: MetronomeSettings::default(),
            sample_rate: 0,
            anchor_frame: 0,
            click_normal: Vec::new(),
            click_accent: Vec::new(),
            voice: None,
        }
    }

    /// Rebuild click buffers if the output sample rate changed. Called from
    /// the worker on `LoadTrack`.
    pub fn set_sample_rate(&mut self, sr: u32) {
        if self.sample_rate == sr {
            return;
        }
        self.sample_rate = sr;
        self.click_normal = render_click(sr, CLICK_FREQ_NORMAL_HZ);
        self.click_accent = render_click(sr, CLICK_FREQ_ACCENT_HZ);
        self.voice = None;
    }

    /// Current source BPM (the user-dialed tempo of the underlying track).
    /// Read by the speed-ramp logic to translate a BPM-unit step into a delta
    /// on the speed multiplier.
    pub fn bpm(&self) -> f32 {
        self.settings.bpm
    }

    pub fn set_settings(&mut self, new: MetronomeSettings) {
        let was_enabled = self.settings.enabled;
        self.settings = new;
        if was_enabled && !new.enabled {
            self.voice = None;
        }
    }

    /// Set the source-frame at which beat 0 (the downbeat) lands. The worker
    /// updates this on `LoadTrack` (→ 0) and on `SetLoop` (→ `loop.start`).
    pub fn set_anchor(&mut self, anchor: u64) {
        self.anchor_frame = anchor;
    }

    /// Cut off the in-flight click. Called on Seek / Stop / Pause so playback
    /// transitions don't leave a click decaying.
    pub fn reset_voice(&mut self) {
        self.voice = None;
    }

    /// Mix click samples into a contiguous output segment in place.
    ///
    /// `source_start` / `source_consumed` describe the source-frame range
    /// the DSP just consumed to produce these output samples. For stitched
    /// loop-wrap chunks the worker calls us twice, once per source range.
    pub fn mix_segment(
        &mut self,
        out_buf: &mut [f32],
        channels: usize,
        source_start: u64,
        source_consumed: u64,
    ) {
        if !self.settings.enabled || self.sample_rate == 0 || channels == 0 {
            return;
        }
        let bpm = self.settings.bpm;
        if !bpm.is_finite() || bpm < MIN_BPM {
            return;
        }
        let out_frames = out_buf.len() / channels;
        if out_frames == 0 || source_consumed == 0 {
            return;
        }
        let frames_per_beat = (self.sample_rate as f64 * 60.0 / bpm as f64) as u64;
        if frames_per_beat == 0 {
            return;
        }

        let amp = 10f32.powf(self.settings.volume_db / 20.0);
        let source_end = source_start.saturating_add(source_consumed);

        // First beat with frame >= source_start. If the source range is below
        // the anchor (shouldn't happen with our snap-into-loop invariant, but
        // be defensive) we start at the anchor itself.
        let mut beat_frame = if source_start <= self.anchor_frame {
            self.anchor_frame
        } else {
            let offset = source_start - self.anchor_frame;
            let k = offset.div_ceil(frames_per_beat);
            self.anchor_frame + k * frames_per_beat
        };

        let mut out_cursor: usize = 0;
        while beat_frame < source_end {
            let src_offset = beat_frame - source_start;
            let out_offset = ((src_offset as f64 * out_frames as f64)
                / source_consumed as f64) as usize;
            let out_offset = out_offset.min(out_frames);

            self.mix_voice(out_buf, channels, out_cursor, out_offset, amp);

            let beat_idx = (beat_frame - self.anchor_frame) / frames_per_beat;
            let accent = self.settings.accent
                && self.settings.beats_per_measure > 0
                && beat_idx.is_multiple_of(self.settings.beats_per_measure as u64);
            self.voice = Some(Voice { accent, pos: 0 });

            out_cursor = out_offset;
            beat_frame = beat_frame.saturating_add(frames_per_beat);
        }

        self.mix_voice(out_buf, channels, out_cursor, out_frames, amp);
    }

    fn mix_voice(
        &mut self,
        out_buf: &mut [f32],
        channels: usize,
        from_frame: usize,
        to_frame: usize,
        amp: f32,
    ) {
        if from_frame >= to_frame {
            return;
        }
        let Some(v) = self.voice.as_mut() else {
            return;
        };
        let buf = if v.accent {
            &self.click_accent
        } else {
            &self.click_normal
        };
        let mut frame = from_frame;
        while frame < to_frame && v.pos < buf.len() {
            let s = buf[v.pos] * amp;
            let base = frame * channels;
            for c in 0..channels {
                out_buf[base + c] += s;
            }
            v.pos += 1;
            frame += 1;
        }
        if v.pos >= buf.len() {
            self.voice = None;
        }
    }
}

fn render_click(sample_rate: u32, freq_hz: f32) -> Vec<f32> {
    let duration = (sample_rate as f32 * CLICK_DURATION_SECS) as usize;
    let tau = sample_rate as f32 * CLICK_DECAY_TAU_SECS;
    let omega = 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
    (0..duration)
        .map(|n| {
            let env = (-(n as f32) / tau).exp();
            (omega * n as f32).sin() * env
        })
        .collect()
}
