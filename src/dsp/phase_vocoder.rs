// Phase 4 (v0.2 step 8a): FFT-based time-stretch via phase vocoder, cascaded
// with a rubato resampler for pitch shift. Mirrors `wsola.rs`'s shape — same
// trait adapters, same composite math, same stretch-ramp discipline — so the
// engine is ignorant of which DSP family is active and the user can A/B them
// per track via `DspKind`.
//
// Algorithm (per synthesis step, per channel):
//   1. Window the analysis frame, FFT it.
//   2. For each bin k: extract magnitude and phase. Subtract the nominal
//      phase advance ω_k · H_a, wrap to (-π, π], add back to ω_k to get the
//      bin's *true* instantaneous frequency ω_true.
//   3. Accumulate output phase: out_phase[k] += ω_true · H_s.
//   4. Build the synthesis spectrum from |X[k]| · exp(i · out_phase[k]); IFFT.
//   5. Window the output frame again (analysis × synthesis windowing) and
//      OLA at hop H_s into the per-channel output queue.
//
// Step 8b will refine: transient-detect-and-passthrough (skip phase
// advancement on percussive frames so drum hits keep their snap) and
// Laroche–Dolson phase locking (lock non-peak bin phases to nearby spectral
// peaks so harmonics stay coherent across frames). Both are local to this
// file; neither touches the trait or the engine.

use std::sync::Arc;

use anyhow::{Context, Result};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::dsp::TimePitchProcessor;

/// Analysis frame size. Same as WSOLA so swapping kinds doesn't change the
/// fundamental time/frequency trade-off.
const FRAME_SIZE: usize = 2048;
/// Synthesis hop. With Hann + hop = N/4 we get COLA (constant overlap-add).
const SYNTHESIS_HOP: usize = 512;
const OVERLAP: usize = FRAME_SIZE - SYNTHESIS_HOP;
/// Real-input FFT bin count: N/2 + 1.
const NUM_BINS: usize = FRAME_SIZE / 2 + 1;

/// Engine chunk: input frames per `process()` call. Same as WSOLA.
const ENGINE_CHUNK_FRAMES: usize = 1024;

/// Stretch range. Composite needs up to 8 (speed = 0.25 × pitch = +12 →
/// stretch = 2 / 0.25 = 8).
const MIN_STRETCH: f32 = 0.25;
const MAX_STRETCH: f32 = 8.0;

/// Maximum change in `stretch` per synthesis step. PV is more sensitive to
/// `H_a` jumps than WSOLA — the phase advance formula is keyed to `H_a`, so
/// a sudden jump bends every bin's `ω_true` for one frame and introduces an
/// audible click. Ramping is essential here, not just nice-to-have.
const STRETCH_RAMP_PER_STEP: f32 = 0.1;

/// Upper bound on output frames per `process()` call for `PhaseVocoderSpeed`
/// (which clamps stretch ≤ 4 — speed slider range). Same shape as WSOLA's
/// `ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK`. The composite reports the same value
/// because its *net* ratio is `1/speed` (pitch is duration-neutral); PV can
/// transiently produce more inside the pipeline, but those frames are
/// consumed by the resampler before crossing the engine boundary.
const ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK: usize = 4096 + SYNTHESIS_HOP * 2;

const TRIM_AT: usize = 1 << 14;
const TRIM_KEEP_BEFORE: usize = FRAME_SIZE;

const TWO_PI: f32 = std::f32::consts::TAU;
const PI: f32 = std::f32::consts::PI;

/// Streaming phase-vocoder time-stretch processor. Per-channel state
/// (`prev_phase`, `out_phase`, OLA tail, queues) but shared timing
/// (`analysis_pos`, `analysis_hop`, `stretch`) so all channels advance in
/// lock-step — preserves stereo decorrelation while keeping a single timeline.
pub struct PhaseVocoder {
    channels: usize,

    /// Per-channel growing input buffer. Trimmed periodically.
    in_buf: Vec<Vec<f32>>,
    /// Frame index in `in_buf` where the next analysis step's frame starts.
    analysis_pos: usize,

    /// OLA accumulator tail (length OVERLAP per channel).
    synth_tail: Vec<Vec<f32>>,
    new_tail_scratch: Vec<Vec<f32>>,

    /// Per-channel queue of synthesised output samples not yet drained.
    out_queue: Vec<Vec<f32>>,

    /// Hann window scaled by `1/sqrt(Σ w² at hop)` so that analysis ×
    /// synthesis windowing OLA's to unity at stretch = 1.
    window: Vec<f32>,

    /// Per-channel scratch for the windowed analysis frame (also acts as FFT
    /// input — realfft mutates it in place).
    extract: Vec<Vec<f32>>,
    /// Per-channel analysis spectrum (FFT output).
    spectrum: Vec<Vec<Complex<f32>>>,
    /// Synthesis spectrum (built from magnitude + accumulated phase). Reused
    /// across channels — built fresh per channel before each IFFT.
    synth_spectrum: Vec<Complex<f32>>,
    /// Per-channel previous-frame analysis phase (for `Δφ` per bin).
    prev_phase: Vec<Vec<f32>>,
    /// Per-channel running synthesis phase, incremented by `ω_true · H_s`.
    out_phase: Vec<Vec<f32>>,
    /// Time-domain IFFT output. Reused across channels.
    ifft_out: Vec<f32>,

    /// Real-input forward FFT plan (Send + Sync via realfft's bounds).
    fft: Arc<dyn RealToComplex<f32>>,
    /// Real-input inverse FFT plan.
    ifft: Arc<dyn ComplexToReal<f32>>,
    /// Pre-allocated FFT/IFFT scratch (real-time-safe — no per-step alloc).
    fft_scratch: Vec<Complex<f32>>,
    ifft_scratch: Vec<Complex<f32>>,

    /// Where `set_stretch` writes; `stretch` ramps toward this on each step.
    target_stretch: f32,
    stretch: f32,
    /// analysis_hop = max(1, round(SYNTHESIS_HOP / stretch)).
    analysis_hop: usize,

    /// Whether `prev_phase` has been populated (false until the first step).
    /// On the first step we set `prev_phase = phase` and skip the unwrap so
    /// `out_phase` doesn't take a wild jump from the (zero-initialised) prior.
    prev_phase_valid: bool,
}

impl PhaseVocoder {
    pub fn new(channels: usize) -> Self {
        let mut window: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| 0.5 - 0.5 * ((TWO_PI * i as f32) / (FRAME_SIZE as f32 - 1.0)).cos())
            .collect();

        // PV uses the window twice (analysis side and synthesis side), so the
        // OLA constant we want is `Σ w² at hop SYNTHESIS_HOP`. Dividing the
        // window by `sqrt(cola_sum)` makes both passes contribute a factor of
        // `1/sqrt(cola_sum)`, so `Σ w_norm² = 1` in steady state → unity gain
        // through identity (stretch = 1, ratio = 1 in the composite).
        let cola_sum: f32 = {
            let p = FRAME_SIZE;
            let m_max = p / SYNTHESIS_HOP;
            let m_min = (p + 1).saturating_sub(FRAME_SIZE).div_ceil(SYNTHESIS_HOP);
            let mut s = 0.0_f32;
            for m in m_min..=m_max {
                let off = p - m * SYNTHESIS_HOP;
                if off < FRAME_SIZE {
                    let w = window[off];
                    s += w * w;
                }
            }
            s
        };
        let scale = cola_sum.sqrt().max(1e-12);
        for w in window.iter_mut() {
            *w /= scale;
        }

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FRAME_SIZE);
        let ifft = planner.plan_fft_inverse(FRAME_SIZE);
        let fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_scratch_len()];
        let ifft_scratch = vec![Complex::new(0.0, 0.0); ifft.get_scratch_len()];

        Self {
            channels,
            in_buf: vec![Vec::with_capacity(FRAME_SIZE * 4); channels],
            analysis_pos: 0,
            synth_tail: vec![vec![0.0; OVERLAP]; channels],
            new_tail_scratch: vec![vec![0.0; OVERLAP]; channels],
            out_queue: vec![Vec::with_capacity(ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK); channels],
            window,
            extract: vec![vec![0.0; FRAME_SIZE]; channels],
            spectrum: vec![vec![Complex::new(0.0, 0.0); NUM_BINS]; channels],
            synth_spectrum: vec![Complex::new(0.0, 0.0); NUM_BINS],
            prev_phase: vec![vec![0.0; NUM_BINS]; channels],
            out_phase: vec![vec![0.0; NUM_BINS]; channels],
            ifft_out: vec![0.0; FRAME_SIZE],
            fft,
            ifft,
            fft_scratch,
            ifft_scratch,
            target_stretch: 1.0,
            stretch: 1.0,
            analysis_hop: SYNTHESIS_HOP,
            prev_phase_valid: false,
        }
    }

    /// Set the *target* stretch. The active `stretch` ramps toward it on
    /// each synthesis step.
    pub fn set_stretch(&mut self, stretch: f32) {
        self.target_stretch = stretch.clamp(MIN_STRETCH, MAX_STRETCH);
    }

    pub fn target_stretch(&self) -> f32 {
        self.target_stretch
    }

    /// Conservative upper bound on output rate during a ramp — see
    /// `wsola::Wsola::effective_stretch` for the rationale.
    pub fn effective_stretch(&self) -> f32 {
        self.stretch.max(self.target_stretch)
    }

    fn advance_stretch_ramp(&mut self) {
        let diff = self.target_stretch - self.stretch;
        if diff.abs() <= STRETCH_RAMP_PER_STEP * 0.5 {
            self.stretch = self.target_stretch;
        } else {
            self.stretch += diff.signum() * STRETCH_RAMP_PER_STEP;
        }
        self.analysis_hop = ((SYNTHESIS_HOP as f32 / self.stretch).round() as usize).max(1);
    }

    pub fn ingest(&mut self, input: &[f32], frames: usize) {
        for ch in 0..self.channels {
            let buf = &mut self.in_buf[ch];
            buf.reserve(frames);
            for i in 0..frames {
                buf.push(input[i * self.channels + ch]);
            }
        }
    }

    pub fn output_available(&self) -> usize {
        self.out_queue[0].len()
    }

    pub fn drain_output(&mut self, output: &mut [f32], max_out: usize) -> usize {
        let n = self.out_queue[0].len().min(max_out);
        for i in 0..n {
            for ch in 0..self.channels {
                output[i * self.channels + ch] = self.out_queue[ch][i];
            }
        }
        for ch in 0..self.channels {
            self.out_queue[ch].drain(..n);
        }
        n
    }

    pub fn drain_output_planar(&mut self, dst: &mut [Vec<f32>], max_out: usize) -> usize {
        let n = self.out_queue[0].len().min(max_out);
        for ch in 0..self.channels {
            dst[ch].extend_from_slice(&self.out_queue[ch][..n]);
            self.out_queue[ch].drain(..n);
        }
        n
    }

    pub fn synthesize_up_to(&mut self, target_frames: usize) {
        while self.out_queue[0].len() < target_frames {
            if !self.step() {
                return;
            }
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        for ch in 0..self.channels {
            self.in_buf[ch].clear();
            self.out_queue[ch].clear();
            self.synth_tail[ch].iter_mut().for_each(|s| *s = 0.0);
            self.prev_phase[ch].iter_mut().for_each(|p| *p = 0.0);
            self.out_phase[ch].iter_mut().for_each(|p| *p = 0.0);
        }
        self.analysis_pos = 0;
        self.prev_phase_valid = false;
    }

    /// Run one PV analysis-synthesis step. Returns true on success, false if
    /// not enough input is buffered yet.
    fn step(&mut self) -> bool {
        self.advance_stretch_ramp();

        let needed_end = self.analysis_pos + FRAME_SIZE;
        if self.in_buf[0].len() < needed_end {
            return false;
        }

        let h_a = self.analysis_hop as f32;
        let h_s = SYNTHESIS_HOP as f32;
        let inv_n = 1.0 / FRAME_SIZE as f32;

        for ch in 0..self.channels {
            // 1. Window the analysis frame into `extract[ch]`.
            {
                let in_ch = &self.in_buf[ch];
                let dst = &mut self.extract[ch];
                for i in 0..FRAME_SIZE {
                    dst[i] = in_ch[self.analysis_pos + i] * self.window[i];
                }
            }

            // 2. Forward FFT (realfft mutates `extract[ch]` in place — it's
            //    overwritten next step, so that's fine).
            if let Err(e) = self.fft.process_with_scratch(
                &mut self.extract[ch],
                &mut self.spectrum[ch],
                &mut self.fft_scratch,
            ) {
                log::warn!("realfft forward error: {e}");
                return false;
            }

            // 3. Phase advance per bin.
            for k in 0..NUM_BINS {
                let bin = self.spectrum[ch][k];
                let mag = (bin.re * bin.re + bin.im * bin.im).sqrt();
                let phase = bin.im.atan2(bin.re);

                if !self.prev_phase_valid {
                    // First step: no prior phase to diff against. Anchor
                    // out_phase to the analysis phase so the first IFFT
                    // matches the input frame's actual phase relationships.
                    self.out_phase[ch][k] = phase;
                } else {
                    let omega_k = TWO_PI * k as f32 / FRAME_SIZE as f32;
                    let expected = omega_k * h_a;
                    let mut dphase = phase - self.prev_phase[ch][k] - expected;
                    // Wrap to (-π, π].
                    dphase = (dphase + PI).rem_euclid(TWO_PI) - PI;
                    let true_omega = omega_k + dphase / h_a;
                    self.out_phase[ch][k] += true_omega * h_s;
                }
                self.prev_phase[ch][k] = phase;

                // 4. Build synthesis spectrum.
                let p = self.out_phase[ch][k];
                self.synth_spectrum[k] = Complex::new(mag * p.cos(), mag * p.sin());
            }

            // realfft requires imag(0) and imag(N/2) == 0 for a real result.
            // Take the real part (drop any phase noise we accumulated there).
            self.synth_spectrum[0].im = 0.0;
            self.synth_spectrum[NUM_BINS - 1].im = 0.0;

            // 5. Inverse FFT.
            if let Err(e) = self.ifft.process_with_scratch(
                &mut self.synth_spectrum,
                &mut self.ifft_out,
                &mut self.ifft_scratch,
            ) {
                log::warn!("realfft inverse error: {e}");
                return false;
            }

            // 6. Apply synthesis window and normalise (realfft is unnormalised:
            //    forward × inverse round-trip scale = N).
            for i in 0..FRAME_SIZE {
                self.ifft_out[i] = self.ifft_out[i] * inv_n * self.window[i];
            }

            // 7. OLA into synth_tail; emit SYNTHESIS_HOP frames.
            let tail = &mut self.synth_tail[ch];
            let queue = &mut self.out_queue[ch];
            queue.reserve(SYNTHESIS_HOP);
            for i in 0..SYNTHESIS_HOP {
                queue.push(tail[i] + self.ifft_out[i]);
            }
            let nt = &mut self.new_tail_scratch[ch];
            for ip in 0..OVERLAP {
                let i = ip + SYNTHESIS_HOP;
                let from_tail = if i < OVERLAP { tail[i] } else { 0.0 };
                nt[ip] = from_tail + self.ifft_out[i];
            }
            std::mem::swap(tail, nt);
        }

        self.prev_phase_valid = true;
        self.analysis_pos += self.analysis_hop;

        if self.analysis_pos > TRIM_AT {
            let drop = self.analysis_pos - TRIM_KEEP_BEFORE;
            for ch in 0..self.channels {
                self.in_buf[ch].drain(..drop);
            }
            self.analysis_pos -= drop;
        }

        true
    }
}

/// `TimePitchProcessor` adapter: speed-only PV. Pitch is preserved (no
/// resampler). Used as the fallback when rubato can't be constructed for the
/// channel count, mirroring `wsola::WsolaSpeed`'s role.
pub struct PhaseVocoderSpeed {
    inner: PhaseVocoder,
}

impl PhaseVocoderSpeed {
    pub fn new(channels: usize) -> Self {
        Self {
            inner: PhaseVocoder::new(channels),
        }
    }
}

impl TimePitchProcessor for PhaseVocoderSpeed {
    fn set_speed(&mut self, speed: f32) {
        let s = speed.clamp(0.25, 4.0);
        self.inner.set_stretch(1.0 / s);
    }

    fn set_pitch_semitones(&mut self, _semitones: f32) {
        // No pitch shift in the speed-only adapter (matches WsolaSpeed).
    }

    fn input_frames_per_chunk(&self) -> usize {
        ENGINE_CHUNK_FRAMES
    }

    fn max_output_frames_per_chunk(&self) -> usize {
        ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK
    }

    fn expected_output_frames_per_chunk(&self) -> usize {
        let est = (ENGINE_CHUNK_FRAMES as f32 * self.inner.effective_stretch()).ceil() as usize;
        est.clamp(1, ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK)
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
    ) -> (usize, usize) {
        if channels != self.inner.channels {
            log::warn!(
                "PhaseVocoderSpeed: channel count changed from {} to {channels}; skipping",
                self.inner.channels
            );
            return (0, 0);
        }
        let needed_in_samples = ENGINE_CHUNK_FRAMES * channels;
        if input.len() < needed_in_samples {
            return (0, 0);
        }
        let max_out = ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK.min(output.len() / channels);
        if max_out == 0 {
            return (0, 0);
        }

        self.inner.ingest(input, ENGINE_CHUNK_FRAMES);
        let target = self.expected_output_frames_per_chunk().min(max_out);
        self.inner.synthesize_up_to(target);
        let written = self.inner.drain_output(output, max_out);
        (ENGINE_CHUNK_FRAMES, written)
    }
}

// ──────────────────────── PhaseVocoderPitchShift ─────────────────────────────
//
// Composite: PV stretches in time, rubato resamples to shift pitch. Same
// math as `wsola::WsolaPitchShift`:
//     pitch_factor   = 2^(pitch_semitones / 12)
//     stretch_factor = pitch_factor / speed   (handed to PV)
//     resample_ratio = 1 / pitch_factor       (handed to rubato)
//     net_ratio      = 1 / speed
//
// Doing pitch in the spectral domain is a Phase 5 / future-iteration concern;
// the cascade keeps the structure parallel to WSOLA so the comparison is
// apples-to-apples.

const RESAMPLE_CHUNK_FRAMES: usize = 1024;
const RESAMPLE_MAX_RATIO_RELATIVE: f64 = 4.0;

pub struct PhaseVocoderPitchShift {
    pv: PhaseVocoder,
    resampler: SincFixedIn<f32>,
    channels: usize,

    pv_drain: Vec<Vec<f32>>,
    resampler_out: Vec<Vec<f32>>,
    out_queue: Vec<Vec<f32>>,

    speed: f32,
    pitch_semitones: f32,
}

impl PhaseVocoderPitchShift {
    pub fn new(channels: usize) -> Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(
            1.0,
            RESAMPLE_MAX_RATIO_RELATIVE,
            params,
            RESAMPLE_CHUNK_FRAMES,
            channels,
        )
        .context("creating SincFixedIn for PhaseVocoderPitchShift")?;

        let max_resampler_out = resampler.output_frames_max();
        let resampler_out = vec![vec![0.0; max_resampler_out]; channels];

        Ok(Self {
            pv: PhaseVocoder::new(channels),
            resampler,
            channels,
            pv_drain: vec![Vec::with_capacity(RESAMPLE_CHUNK_FRAMES * 2); channels],
            resampler_out,
            out_queue: vec![Vec::with_capacity(ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK); channels],
            speed: 1.0,
            pitch_semitones: 0.0,
        })
    }

    fn recompute(&mut self) {
        let pitch_factor = 2.0_f32.powf(self.pitch_semitones / 12.0);
        let stretch = pitch_factor / self.speed;
        let ratio = 1.0 / pitch_factor as f64;
        self.pv.set_stretch(stretch);
        if let Err(e) = self.resampler.set_resample_ratio(ratio, true) {
            log::warn!("resampler.set_resample_ratio({ratio}): {e}");
        }
    }
}

impl TimePitchProcessor for PhaseVocoderPitchShift {
    fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.25, 2.0);
        self.recompute();
    }

    fn set_pitch_semitones(&mut self, semitones: f32) {
        self.pitch_semitones = semitones.clamp(-12.0, 12.0);
        self.recompute();
    }

    fn input_frames_per_chunk(&self) -> usize {
        ENGINE_CHUNK_FRAMES
    }

    fn max_output_frames_per_chunk(&self) -> usize {
        ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK
    }

    fn expected_output_frames_per_chunk(&self) -> usize {
        let pitch_factor = 2.0_f32.powf(self.pitch_semitones / 12.0);
        let resample_ratio = 1.0 / pitch_factor;
        let est = (ENGINE_CHUNK_FRAMES as f32 * self.pv.effective_stretch() * resample_ratio)
            .ceil() as usize;
        est.clamp(1, ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK)
    }

    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
    ) -> (usize, usize) {
        if channels != self.channels {
            log::warn!(
                "PhaseVocoderPitchShift: channel count changed from {} to {channels}; skipping",
                self.channels
            );
            return (0, 0);
        }
        let needed_in_samples = ENGINE_CHUNK_FRAMES * channels;
        if input.len() < needed_in_samples {
            return (0, 0);
        }
        let max_out = ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK.min(output.len() / channels);
        if max_out == 0 {
            return (0, 0);
        }

        // 1. Feed PV; aim for `target_stretch × ENGINE_CHUNK` frames so the
        //    resampler doesn't starve mid-ramp (active stretch lags target).
        self.pv.ingest(input, ENGINE_CHUNK_FRAMES);
        let stretch_target =
            (ENGINE_CHUNK_FRAMES as f32 * self.pv.target_stretch()).ceil() as usize;
        self.pv.synthesize_up_to(stretch_target);

        // 2. Drain PV output into the resampler input buffers (planar).
        let avail = self.pv.output_available();
        if avail > 0 {
            self.pv.drain_output_planar(&mut self.pv_drain, avail);
        }

        // 3. Run the resampler in fixed-size chunks while pv_drain has enough
        //    input. Bail early once out_queue is full enough to fill max_out.
        while self.pv_drain[0].len() >= RESAMPLE_CHUNK_FRAMES
            && self.out_queue[0].len() < max_out
        {
            let (in_used, out_frames) = match self.resampler.process_into_buffer(
                &self.pv_drain,
                &mut self.resampler_out,
                None,
            ) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("rubato process error: {e}");
                    break;
                }
            };
            for ch in 0..channels {
                self.pv_drain[ch].drain(..in_used);
                self.out_queue[ch].extend_from_slice(&self.resampler_out[ch][..out_frames]);
            }
        }

        // 4. Drain interleaved output into the engine's scratch.
        let n = self.out_queue[0].len().min(max_out);
        for i in 0..n {
            for ch in 0..channels {
                output[i * channels + ch] = self.out_queue[ch][i];
            }
        }
        for ch in 0..channels {
            self.out_queue[ch].drain(..n);
        }

        (ENGINE_CHUNK_FRAMES, n)
    }
}
