// Phase 3 (MVP target): WSOLA time-stretch + rubato resample for pitch shift.
// Step 6a: speed-only WSOLA preserves pitch (`WsolaSpeed`).
// Step 6b: cascade WSOLA with a rubato resampler for independent pitch shift
// (`WsolaPitchShift`).
// Step 6c: AMDF similarity search over a sum-of-channels mono mix; window
// divided by the steady-state OLA sum for unity-gain identity; `stretch`
// ramps toward `target_stretch` per synthesis step instead of snapping.

use anyhow::{Context, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::dsp::TimePitchProcessor;

/// Analysis frame size. Longer = more pitch fidelity, more transient smear.
const FRAME_SIZE: usize = 2048;
/// Synthesis hop. With Hann + hop = N/4 we get COLA (constant overlap-add).
const SYNTHESIS_HOP: usize = 512;
/// ± frames around the nominal analysis position when searching for the
/// best-matching frame (similarity criterion: AMDF against the
/// natural-progression reference from the previous step).
const SEARCH_RADIUS: usize = 256;
/// Overlap region between adjacent synthesis frames.
const OVERLAP: usize = FRAME_SIZE - SYNTHESIS_HOP;

/// Engine chunk: input frames per `process()` call.
const ENGINE_CHUNK_FRAMES: usize = 1024;

/// Stretch range. stretch = output_duration / input_duration.
/// Bounded by what either adapter requires; the composite needs up to 8
/// (speed = 0.25× combined with pitch = +12 st → stretch = 2 / 0.25 = 8).
const MIN_STRETCH: f32 = 0.25;
const MAX_STRETCH: f32 = 8.0;

/// Maximum change in `stretch` per synthesis step (step 6c, fix #3). Smooths
/// slider drags / reset jumps so `analysis_hop` doesn't snap, which causes
/// audible clicks at ratio boundaries. With ~22 synth steps/sec at 44.1 kHz,
/// 0.1 means a 1× → 4× extreme reset converges in ~30 steps ≈ 350 ms — fast
/// enough to feel responsive, slow enough to be glitch-free.
const STRETCH_RAMP_PER_STEP: f32 = 0.1;

/// Upper bound on output frames per `process()` call for `WsolaSpeed` (which
/// clamps stretch ≤ 4 — speed slider range). Worst case at stretch = 4:
/// analysis_hop = SYNTHESIS_HOP / 4 = 128, so 1024 input frames support up
/// to 8 synthesis steps × 512 = 4096 output frames; add one hop of slack.
/// `WsolaPitchShift` reports the same value because its *net* ratio is 1/speed
/// (pitch is duration-neutral); WSOLA can transiently produce more during the
/// composite's pipeline, but those frames are immediately consumed by the
/// resampler, never crossing the engine boundary.
const ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK: usize = 4096 + SYNTHESIS_HOP * 2;

/// Trim threshold: drop the front of `in_buf` once `analysis_pos` has crossed
/// this. Keeps memory bounded without re-shifting on every step.
const TRIM_AT: usize = 1 << 14;
const TRIM_KEEP_BEFORE: usize = SEARCH_RADIUS + FRAME_SIZE;

/// Streaming WSOLA processor. Drives one analysis-synthesis pipeline shared
/// across `channels` interleaved channels — a single δ per analysis step keeps
/// stereo image coherent.
pub struct Wsola {
    channels: usize,

    /// Per-channel growing input buffer. Trimmed periodically.
    in_buf: Vec<Vec<f32>>,
    /// Frame index in `in_buf` where the next analysis step's nominal centre lives.
    analysis_pos: usize,

    /// OLA accumulator tail (length OVERLAP per channel). Holds the part of
    /// the running output that hasn't been emitted yet because future frames
    /// may still add to it.
    synth_tail: Vec<Vec<f32>>,
    /// Pre-allocated swap buffer for the next-step tail (avoids per-step alloc).
    new_tail_scratch: Vec<Vec<f32>>,

    /// Natural-progression reference: the un-windowed tail of the previously
    /// extracted frame, offset by SYNTHESIS_HOP. Used as the target signal in
    /// the AMDF search for the next step's best δ (see step 6c).
    nat_ref: Vec<Vec<f32>>,
    /// Whether `nat_ref` has been populated (false until the first step runs).
    nat_ref_valid: bool,

    /// Per-channel queue of synthesized output samples not yet drained.
    out_queue: Vec<Vec<f32>>,

    /// Pre-computed Hann window of length FRAME_SIZE, divided by the COLA sum
    /// so identity (stretch = 1) is unity-gain. Without the compensation, OLA
    /// at hop = N/4 with a symmetric Hann produces output a few dB hot — see
    /// step 6c, fix #2.
    window: Vec<f32>,
    /// Per-channel scratch for the extracted (un-windowed and then windowed) frame.
    extract: Vec<Vec<f32>>,

    /// Mono-mixed natural-progression reference (length OVERLAP). Reused as
    /// the AMDF target each step so the search isn't channel-0-biased on
    /// stereo content with diverging channels (step 6c, fix #1).
    nat_ref_mono: Vec<f32>,
    /// Mono-mixed search region of `in_buf` (length 2·SEARCH_RADIUS + OVERLAP).
    /// Precomputed once per step so the inner δ loop runs over a single
    /// channel-summed signal.
    in_mono: Vec<f32>,

    /// Where `set_stretch` writes; the actual `stretch` ramps toward this
    /// over multiple synthesis steps (step 6c, fix #3).
    target_stretch: f32,
    /// Currently-active stretch, ∈ [MIN_STRETCH, MAX_STRETCH]. Lags
    /// `target_stretch` during transitions.
    stretch: f32,
    /// analysis_hop = max(1, round(SYNTHESIS_HOP / stretch)).
    analysis_hop: usize,
}

impl Wsola {
    pub fn new(channels: usize) -> Self {
        let mut window: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| {
                0.5 - 0.5
                    * ((2.0 * std::f32::consts::PI * i as f32) / (FRAME_SIZE as f32 - 1.0)).cos()
            })
            .collect();

        // Step 6c, fix #2: divide the window by the steady-state OLA sum so
        // identity (stretch = 1) is unity-gain. Computed against `window` (not
        // hard-coded) so the constant stays tied to the actual (window shape,
        // hop) rather than a magic number — for symmetric Hann at hop = N/4
        // it lands at ~2.0; this stays correct if either is changed.
        // We evaluate at p = FRAME_SIZE — past the boundary frames at p < N
        // where contributions are partial.
        let cola_sum: f32 = {
            let p = FRAME_SIZE;
            let m_max = p / SYNTHESIS_HOP;
            let m_min = (p + 1).saturating_sub(FRAME_SIZE).div_ceil(SYNTHESIS_HOP);
            let mut s = 0.0_f32;
            for m in m_min..=m_max {
                let offset = p - m * SYNTHESIS_HOP;
                if offset < FRAME_SIZE {
                    s += window[offset];
                }
            }
            s
        };
        for w in window.iter_mut() {
            *w /= cola_sum;
        }

        Self {
            channels,
            in_buf: vec![Vec::with_capacity(FRAME_SIZE * 4); channels],
            analysis_pos: 0,
            synth_tail: vec![vec![0.0; OVERLAP]; channels],
            new_tail_scratch: vec![vec![0.0; OVERLAP]; channels],
            nat_ref: vec![vec![0.0; OVERLAP]; channels],
            nat_ref_valid: false,
            out_queue: vec![Vec::with_capacity(ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK); channels],
            window,
            extract: vec![vec![0.0; FRAME_SIZE]; channels],
            nat_ref_mono: vec![0.0; OVERLAP],
            in_mono: vec![0.0; 2 * SEARCH_RADIUS + OVERLAP],
            target_stretch: 1.0,
            stretch: 1.0,
            analysis_hop: SYNTHESIS_HOP,
        }
    }

    /// Set the *target* stretch. The active `stretch` ramps toward it on each
    /// synthesis step — see `advance_stretch_ramp`. Setting `target_stretch`
    /// has no immediate effect on the next-emitted frame; it only changes
    /// where future frames are headed.
    pub fn set_stretch(&mut self, stretch: f32) {
        self.target_stretch = stretch.clamp(MIN_STRETCH, MAX_STRETCH);
    }

    /// Most-recently-requested stretch.
    pub fn target_stretch(&self) -> f32 {
        self.target_stretch
    }

    /// Larger of `stretch` and `target_stretch` — the conservative upper
    /// bound on output rate the engine should plan ring vacancy against
    /// during a ramp-down (otherwise the ring's `push_slice` silently drops
    /// the excess).
    pub fn effective_stretch(&self) -> f32 {
        self.stretch.max(self.target_stretch)
    }

    /// Step `stretch` toward `target_stretch` by at most `STRETCH_RAMP_PER_STEP`.
    /// Called at the top of every `step()` so the ramp lives in synthesis-step
    /// time rather than wall-clock time. Snaps to target once we're within
    /// half a step to avoid drifting forever from FP rounding.
    fn advance_stretch_ramp(&mut self) {
        let diff = self.target_stretch - self.stretch;
        if diff.abs() <= STRETCH_RAMP_PER_STEP * 0.5 {
            self.stretch = self.target_stretch;
        } else {
            self.stretch += diff.signum() * STRETCH_RAMP_PER_STEP;
        }
        self.analysis_hop = ((SYNTHESIS_HOP as f32 / self.stretch).round() as usize).max(1);
    }

    /// Append `frames` interleaved input frames to the per-channel buffer.
    pub fn ingest(&mut self, input: &[f32], frames: usize) {
        for ch in 0..self.channels {
            let buf = &mut self.in_buf[ch];
            buf.reserve(frames);
            for i in 0..frames {
                buf.push(input[i * self.channels + ch]);
            }
        }
    }

    /// Frames currently waiting in the output queue (per channel — all
    /// channels stay in lock-step).
    pub fn output_available(&self) -> usize {
        self.out_queue[0].len()
    }

    /// Drain up to `max_out` frames into the interleaved `output` buffer.
    /// Returns frames written.
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

    /// Drain up to `max_out` frames into per-channel destination buffers
    /// (appending to `dst[ch]`). Used by the composite to feed the resampler
    /// without round-tripping through interleaved layout.
    pub fn drain_output_planar(&mut self, dst: &mut [Vec<f32>], max_out: usize) -> usize {
        let n = self.out_queue[0].len().min(max_out);
        for ch in 0..self.channels {
            dst[ch].extend_from_slice(&self.out_queue[ch][..n]);
            self.out_queue[ch].drain(..n);
        }
        n
    }

    /// Run synthesis steps until either the input runs out or `out_queue` has
    /// at least `target_frames` queued.
    pub fn synthesize_up_to(&mut self, target_frames: usize) {
        while self.out_queue[0].len() < target_frames {
            if !self.step() {
                return;
            }
        }
    }

    /// Reset all internal state. Use when a discontinuity invalidates the
    /// accumulator (e.g., track change).
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        for ch in 0..self.channels {
            self.in_buf[ch].clear();
            self.out_queue[ch].clear();
            self.synth_tail[ch].iter_mut().for_each(|s| *s = 0.0);
        }
        self.analysis_pos = 0;
        self.nat_ref_valid = false;
    }

    /// Run one WSOLA analysis-synthesis step. Returns true on success, false
    /// if not enough input is buffered yet.
    fn step(&mut self) -> bool {
        // Step 6c, fix #3: ramp the active stretch toward target before this
        // step's analysis_hop is used. Putting it here (vs. in set_stretch)
        // means the ramp rate is governed by synthesis-step rate, so it stays
        // proportional to the audio timeline regardless of slider event rate.
        self.advance_stretch_ramp();

        // We may search up to +SEARCH_RADIUS from analysis_pos and read
        // FRAME_SIZE samples from there — bound the read window to in_buf.
        let needed_end = self.analysis_pos + SEARCH_RADIUS + FRAME_SIZE;
        if self.in_buf[0].len() < needed_end {
            return false;
        }

        // 1. Find best δ ∈ [-Δ, +Δ]. Step 6c, fix #1: AMDF (sum of absolute
        //    differences) over a sum-of-channels mono mix, minimised. Raw
        //    cross-correlation was biased toward high-energy regions and
        //    picked poor frame alignments on tonal content; AMDF normalises
        //    against the local envelope without the cost of a full
        //    normalised cross-correlation. Mono mix (rather than channel 0)
        //    keeps stereo content with diverging channels from mis-steering
        //    the search. Single δ per step → stereo image stays coherent.
        //    On the first step there's no reference, so δ = 0.
        let delta: isize = if !self.nat_ref_valid {
            0
        } else {
            let center = self.analysis_pos as isize;
            // Don't search before the start of the buffer.
            let lo = -(self.analysis_pos.min(SEARCH_RADIUS) as isize);
            let hi = SEARCH_RADIUS as isize;

            // Build the mono mixes once per step — the inner δ loop then
            // works on a single channel-summed signal, avoiding a per-d
            // re-sum across channels. Scale (1/channels) is omitted because
            // it's a constant factor across all δ and doesn't affect argmin.
            for i in 0..OVERLAP {
                let mut s = 0.0_f32;
                for ch in 0..self.channels {
                    s += self.nat_ref[ch][i];
                }
                self.nat_ref_mono[i] = s;
            }
            let region_start = (center + lo) as usize;
            let region_len = (hi - lo) as usize + OVERLAP;
            for i in 0..region_len {
                let mut s = 0.0_f32;
                for ch in 0..self.channels {
                    s += self.in_buf[ch][region_start + i];
                }
                self.in_mono[i] = s;
            }

            let mut best_d: isize = 0;
            let mut best_score = f32::INFINITY;
            for d in lo..=hi {
                let start = (d - lo) as usize;
                let mut score = 0.0_f32;
                for i in 0..OVERLAP {
                    score += (self.nat_ref_mono[i] - self.in_mono[start + i]).abs();
                }
                if score < best_score {
                    best_score = score;
                    best_d = d;
                }
            }
            best_d
        };

        // 2. Extract frame at (analysis_pos + δ), un-windowed, per channel.
        let frame_start = (self.analysis_pos as isize + delta) as usize;
        for ch in 0..self.channels {
            let in_ch = &self.in_buf[ch];
            let dst = &mut self.extract[ch];
            dst[..FRAME_SIZE].copy_from_slice(&in_ch[frame_start..frame_start + FRAME_SIZE]);
        }

        // 3. Save the un-windowed natural-progression reference for the NEXT
        //    step: the "what would naturally come next" signal is the part of
        //    this frame at offset SYNTHESIS_HOP (length OVERLAP).
        for ch in 0..self.channels {
            self.nat_ref[ch]
                .copy_from_slice(&self.extract[ch][SYNTHESIS_HOP..SYNTHESIS_HOP + OVERLAP]);
        }
        self.nat_ref_valid = true;

        // 4. Apply Hann window to the extracted frame in place.
        for ch in 0..self.channels {
            let frame = &mut self.extract[ch];
            for i in 0..FRAME_SIZE {
                frame[i] *= self.window[i];
            }
        }

        // 5. Overlap-add into the running accumulator.
        //    Accumulator state is `synth_tail` (length OVERLAP) — the part of
        //    the running output that future frames can still contribute to.
        //    Layout: accumulator[0..FRAME_SIZE] = (synth_tail | 0) + windowed_frame.
        //    Emit accumulator[0..SYNTHESIS_HOP], save accumulator[SYNTHESIS_HOP..]
        //    as the new synth_tail.
        for ch in 0..self.channels {
            let frame = &self.extract[ch];
            let tail = &mut self.synth_tail[ch];
            let queue = &mut self.out_queue[ch];

            // Emit the front: SYNTHESIS_HOP samples that won't see any more
            // contributions (by definition of synthesis hop = N/4 with Hann).
            queue.reserve(SYNTHESIS_HOP);
            for i in 0..SYNTHESIS_HOP {
                queue.push(tail[i] + frame[i]);
            }

            // Compose the new tail (length OVERLAP) into the swap buffer:
            //   for i in [SYNTHESIS_HOP, FRAME_SIZE):
            //     accumulator[i] = (i < OVERLAP ? tail[i] : 0) + frame[i]
            let nt = &mut self.new_tail_scratch[ch];
            for ip in 0..OVERLAP {
                let i = ip + SYNTHESIS_HOP;
                let from_tail = if i < OVERLAP { tail[i] } else { 0.0 };
                nt[ip] = from_tail + frame[i];
            }
            std::mem::swap(tail, nt);
        }

        // 6. Advance analysis position by the nominal hop (δ shifts where we
        //    *read*, not where we step).
        self.analysis_pos += self.analysis_hop;

        // 7. Trim the in_buf prefix when it grows large. We still might read
        //    up to SEARCH_RADIUS frames before analysis_pos, so leave a margin.
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

/// `TimePitchProcessor` adapter: speed-only WSOLA. Pitch is preserved.
/// Step 6b will compose this with a `rubato` resampler for independent pitch.
pub struct WsolaSpeed {
    inner: Wsola,
}

impl WsolaSpeed {
    pub fn new(channels: usize) -> Self {
        Self {
            inner: Wsola::new(channels),
        }
    }
}

impl TimePitchProcessor for WsolaSpeed {
    fn set_speed(&mut self, speed: f32) {
        let s = speed.clamp(0.25, 4.0);
        // stretch = 1 / speed.
        self.inner.set_stretch(1.0 / s);
    }

    fn set_pitch_semitones(&mut self, _semitones: f32) {
        // Independent pitch shift is step 6b.
    }

    fn input_frames_per_chunk(&self) -> usize {
        ENGINE_CHUNK_FRAMES
    }

    fn max_output_frames_per_chunk(&self) -> usize {
        ADAPTER_MAX_OUTPUT_FRAMES_PER_CHUNK
    }

    fn expected_output_frames_per_chunk(&self) -> usize {
        // Steady-state: per ENGINE_CHUNK input we emit ENGINE_CHUNK × stretch.
        // Use the larger of current and target so a ramp-down (current > target)
        // doesn't under-reserve ring vacancy and silently drop samples.
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
                "WsolaSpeed: channel count changed from {} to {channels}; skipping",
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

// ───────────────────────── WsolaPitchShift (step 6b) ─────────────────────────
//
// Composite: WSOLA stretches in time, rubato resamples to shift pitch.
// Cascaded back-to-back so that the *net* duration is governed by speed alone
// (pitch is duration-neutral).
//
// Math:
//     pitch_factor   = 2^(pitch_semitones / 12)
//     stretch_factor = pitch_factor / speed   (handed to WSOLA)
//     resample_ratio = 1 / pitch_factor       (handed to rubato; shrinks the
//                                              WSOLA output by pitch_factor)
//     net_ratio      = stretch_factor × resample_ratio = 1 / speed
//
// Sanity-check corners:
//     (speed=1, pitch=0)   → stretch = 1, ratio = 1   (identity through both)
//     (speed=0.5, pitch=0) → stretch = 2, ratio = 1   (only WSOLA stretches)
//     (speed=1, pitch=+12) → stretch = 2, ratio = 0.5 (WSOLA 2×, rubato 0.5×;
//                                                     net duration unchanged,
//                                                     pitch up an octave)

/// Resampler chunk size — frames the resampler consumes per call. We feed it
/// from `wsola_drain` once that buffer has at least this many frames.
const RESAMPLE_CHUNK_FRAMES: usize = 1024;

/// Multiplicative bound on rubato's runtime ratio. Initial ratio = 1.0; with
/// `relative = 4.0` the runtime range is [0.25, 4.0]. ±12 semitones only needs
/// [0.5, 2.0]; the headroom keeps us from rebuilding the resampler if the
/// pitch range is widened later.
const RESAMPLE_MAX_RATIO_RELATIVE: f64 = 4.0;

/// `TimePitchProcessor` adapter: WSOLA + rubato cascade. Independent speed
/// and pitch.
pub struct WsolaPitchShift {
    wsola: Wsola,
    resampler: SincFixedIn<f32>,
    channels: usize,

    /// Per-channel WSOLA → resampler buffer. Grows as WSOLA produces output;
    /// drained `RESAMPLE_CHUNK_FRAMES` at a time when the resampler runs.
    wsola_drain: Vec<Vec<f32>>,
    /// Per-channel resampler output scratch, sized to `output_frames_max()`.
    resampler_out: Vec<Vec<f32>>,
    /// Per-channel composite output queue, drained interleaved at the end of
    /// each `process()` call.
    out_queue: Vec<Vec<f32>>,

    /// Current speed (clamped to [0.25, 2.0] — UI range).
    speed: f32,
    /// Current pitch in semitones (clamped to [-12.0, 12.0]).
    pitch_semitones: f32,
}

impl WsolaPitchShift {
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
        .context("creating SincFixedIn for WsolaPitchShift")?;

        let max_resampler_out = resampler.output_frames_max();
        let resampler_out = vec![vec![0.0; max_resampler_out]; channels];

        Ok(Self {
            wsola: Wsola::new(channels),
            resampler,
            channels,
            wsola_drain: vec![Vec::with_capacity(RESAMPLE_CHUNK_FRAMES * 2); channels],
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
        self.wsola.set_stretch(stretch);
        if let Err(e) = self.resampler.set_resample_ratio(ratio, true) {
            log::warn!("resampler.set_resample_ratio({ratio}): {e}");
        }
    }
}

impl TimePitchProcessor for WsolaPitchShift {
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
        // Steady-state net composite ratio is 1/speed (pitch is duration-neutral).
        // During a WSOLA ramp-down, however, current_stretch > target_stretch
        // while rubato has already ramped to the new ratio, so the composite
        // can transiently exceed 1/speed. Compute the worst-case ratio from the
        // larger WSOLA stretch × current resample ratio.
        let pitch_factor = 2.0_f32.powf(self.pitch_semitones / 12.0);
        let resample_ratio = 1.0 / pitch_factor;
        let est = (ENGINE_CHUNK_FRAMES as f32 * self.wsola.effective_stretch() * resample_ratio)
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
                "WsolaPitchShift: channel count changed from {} to {channels}; skipping",
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

        // 1. Feed WSOLA, run synthesis until it has produced ~one chunk's
        //    worth of stretched output (steady-state per-call expectation).
        //    We aim for `target_stretch` rather than the currently-active
        //    stretch — the latter lags during a ramp, and using it as the
        //    synth target would let the resampler starve mid-transition.
        self.wsola.ingest(input, ENGINE_CHUNK_FRAMES);
        let stretch_target =
            (ENGINE_CHUNK_FRAMES as f32 * self.wsola.target_stretch()).ceil() as usize;
        self.wsola.synthesize_up_to(stretch_target);

        // 2. Pull all WSOLA output into wsola_drain (per-channel, planar).
        let avail = self.wsola.output_available();
        if avail > 0 {
            self.wsola.drain_output_planar(&mut self.wsola_drain, avail);
        }

        // 3. Run the resampler in fixed-size chunks for as long as wsola_drain
        //    has enough input. Output goes onto `out_queue`. We bail early if
        //    `out_queue` already has enough to fill `max_out` — the rest stays
        //    in wsola_drain for the next call.
        while self.wsola_drain[0].len() >= RESAMPLE_CHUNK_FRAMES
            && self.out_queue[0].len() < max_out
        {
            let (in_used, out_frames) = match self.resampler.process_into_buffer(
                &self.wsola_drain,
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
                self.wsola_drain[ch].drain(..in_used);
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
