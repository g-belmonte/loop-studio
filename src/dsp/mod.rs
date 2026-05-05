pub mod passthrough;
pub mod wsola;

/// Time-stretch and/or pitch-shift on interleaved f32 samples.
///
/// The engine drives chunk-based DSP: it waits until it has at least
/// `input_frames_per_chunk()` source frames available *and* enough ring-buffer
/// vacancy for `max_output_frames_per_chunk()` output frames, then calls
/// `process()` exactly once.
pub trait TimePitchProcessor: Send {
    fn set_speed(&mut self, speed: f32);
    fn set_pitch_semitones(&mut self, semitones: f32);

    /// Frames of input the DSP wants per `process()` call. The engine will
    /// not call `process()` until this many source frames are available.
    fn input_frames_per_chunk(&self) -> usize;

    /// Upper bound on frames the DSP could emit from one `process()` call,
    /// across all current settings. Stable for the lifetime of the DSP — the
    /// engine sizes its scratch output buffer to this value.
    fn max_output_frames_per_chunk(&self) -> usize;

    /// Frames the *next* `process()` call will emit given the current settings.
    /// The engine uses this for the ring-vacancy check so it doesn't have to
    /// be conservative when the actual output is much smaller than the worst
    /// case (e.g. a resampler at speed 1.0 outputs `chunk_size`, not the full
    /// `chunk_size × max_ratio`). Default implementation returns the max,
    /// which is always safe but coarse.
    fn expected_output_frames_per_chunk(&self) -> usize {
        self.max_output_frames_per_chunk()
    }

    /// Run one chunk. `input` must contain at least
    /// `input_frames_per_chunk() * channels` interleaved samples; `output`
    /// must have room for at least `max_output_frames_per_chunk() * channels`.
    /// Returns `(input_frames_consumed, output_frames_written)`.
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
    ) -> (usize, usize);
}
