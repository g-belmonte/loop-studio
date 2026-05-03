pub mod passthrough;
pub mod resample;
pub mod wsola;

pub trait TimePitchProcessor: Send {
    fn set_speed(&mut self, speed: f32);
    fn set_pitch_semitones(&mut self, semitones: f32);
    /// Process input samples to output. Returns (input_consumed, output_written).
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize);
}
