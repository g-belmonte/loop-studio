use crate::dsp::TimePitchProcessor;

/// Identity processor: input bytes copied to output verbatim. Speed and pitch
/// settings are accepted but ignored. Used to validate the audio path before
/// any real DSP lands.
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

impl TimePitchProcessor for Passthrough {
    fn set_speed(&mut self, _speed: f32) {}
    fn set_pitch_semitones(&mut self, _semitones: f32) {}

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize) {
        let n = input.len().min(output.len());
        output[..n].copy_from_slice(&input[..n]);
        (n, n)
    }
}
