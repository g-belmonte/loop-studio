use anyhow::{Context, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::dsp::TimePitchProcessor;

/// Frames of input the resampler consumes per `process()` call. Matches the
/// pre-step-5 engine tick size; ~23 ms at 44.1 kHz.
const CHUNK_FRAMES: usize = 1024;

/// Multiplicative bound on how far the resample ratio can wander from the
/// initial 1.0. With max_relative = 4.0 the ratio range is [0.25, 4.0],
/// which covers playback speeds [0.25×, 4×]. UI caps at 2× — the headroom
/// is just there so we don't have to recreate the resampler when the slider
/// is dragged hard.
const MAX_RATIO_RELATIVE: f64 = 4.0;

/// Resampler-based "speed" processor. Speed is coupled to pitch — slowing
/// down lowers pitch, speeding up raises it (turntable behaviour). Step 6
/// (WSOLA) decouples them.
pub struct ResampleSpeed {
    resampler: SincFixedIn<f32>,
    channels: usize,
    /// Reusable planar buffers for de/re-interleaving. Sized at construction.
    input_planar: Vec<Vec<f32>>,
    output_planar: Vec<Vec<f32>>,
    max_output_frames: usize,
}

impl ResampleSpeed {
    pub fn new(channels: usize) -> Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler =
            SincFixedIn::<f32>::new(1.0, MAX_RATIO_RELATIVE, params, CHUNK_FRAMES, channels)
                .context("creating SincFixedIn")?;

        let max_output_frames = resampler.output_frames_max();
        let input_planar = vec![vec![0.0_f32; CHUNK_FRAMES]; channels];
        let output_planar = vec![vec![0.0_f32; max_output_frames]; channels];

        Ok(Self {
            resampler,
            channels,
            input_planar,
            output_planar,
            max_output_frames,
        })
    }
}

impl TimePitchProcessor for ResampleSpeed {
    fn set_speed(&mut self, speed: f32) {
        // Engine ratio = output_rate / input_rate = 1/speed.
        // 2× playback → consume 2 input frames per 1 output frame → ratio 0.5.
        let speed = speed.clamp(0.25, 4.0) as f64;
        let ratio = 1.0 / speed;
        // ramp = true smooths the ratio change so the slider doesn't click.
        if let Err(e) = self.resampler.set_resample_ratio(ratio, true) {
            log::warn!("resampler.set_resample_ratio({ratio}) failed: {e}");
        }
    }

    fn set_pitch_semitones(&mut self, _semitones: f32) {
        // Independent pitch shift is step 6 (WSOLA + resample).
    }

    fn input_frames_per_chunk(&self) -> usize {
        CHUNK_FRAMES
    }

    fn max_output_frames_per_chunk(&self) -> usize {
        self.max_output_frames
    }

    fn expected_output_frames_per_chunk(&self) -> usize {
        self.resampler.output_frames_next()
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
    ) -> (usize, usize) {
        if channels != self.channels {
            log::warn!(
                "ResampleSpeed: channel count changed from {} to {channels}; skipping",
                self.channels
            );
            return (0, 0);
        }
        let needed_in_samples = CHUNK_FRAMES * channels;
        if input.len() < needed_in_samples {
            return (0, 0);
        }
        if output.len() < self.max_output_frames * channels {
            return (0, 0);
        }

        // Deinterleave into planar input.
        for ch in 0..channels {
            let dst = &mut self.input_planar[ch][..CHUNK_FRAMES];
            for i in 0..CHUNK_FRAMES {
                dst[i] = input[i * channels + ch];
            }
        }

        let (_in_used_per_ch, out_frames) = match self.resampler.process_into_buffer(
            &self.input_planar,
            &mut self.output_planar,
            None,
        ) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("rubato process error: {e}");
                return (0, 0);
            }
        };

        // Re-interleave planar output.
        for ch in 0..channels {
            let src = &self.output_planar[ch][..out_frames];
            for i in 0..out_frames {
                output[i * channels + ch] = src[i];
            }
        }

        (CHUNK_FRAMES, out_frames)
    }
}
