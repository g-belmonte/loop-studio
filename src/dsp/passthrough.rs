use crate::dsp::TimePitchProcessor;

/// Identity processor: input samples copied to output verbatim. Speed and
/// pitch are accepted but ignored. Used as a fallback when the resampler
/// fails to construct.
pub struct Passthrough;

impl Passthrough {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Passthrough {
    fn default() -> Self {
        Self::new()
    }
}

const PASSTHROUGH_CHUNK_FRAMES: usize = 1024;

impl TimePitchProcessor for Passthrough {
    fn set_speed(&mut self, _speed: f32) {}
    fn set_pitch_semitones(&mut self, _semitones: f32) {}

    fn input_frames_per_chunk(&self) -> usize {
        PASSTHROUGH_CHUNK_FRAMES
    }

    fn max_output_frames_per_chunk(&self) -> usize {
        PASSTHROUGH_CHUNK_FRAMES
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
    ) -> (usize, usize) {
        let in_frames = PASSTHROUGH_CHUNK_FRAMES;
        let in_samples = in_frames * channels;
        if input.len() < in_samples || output.len() < in_samples {
            return (0, 0);
        }
        output[..in_samples].copy_from_slice(&input[..in_samples]);
        (in_frames, in_frames)
    }
}
