use realfft::RealFftPlanner;

use crate::track::Track;

/// Per-track BPM-detection state held by `App`. Reset to `Idle` on every
/// track load. `Running` carries no payload — the worker thread sends its
/// result back over a channel `App` drains each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BpmStatus {
    Idle,
    Running,
    Done(f32),
    Failed,
}

/// FFT frame size for the spectral-flux onset envelope.
const FRAME: usize = 1024;
/// Hop between successive frames — 50 % overlap.
const HOP: usize = 512;

/// Tempo search range. 60–200 BPM covers ballads through fast rock/funk;
/// outside this band the octave-resolve below remaps detections back in.
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;
/// Centre and width of the perceptual prior applied to the autocorrelation
/// peak search. Centred slightly above 100 BPM where most popular music sits;
/// 60 BPM wide so half- and double-tempo candidates get noticeably attenuated
/// without locking the search to a narrow window.
const PRIOR_CENTRE_BPM: f32 = 120.0;
const PRIOR_SIGMA_BPM: f32 = 60.0;

/// Detect tempo over `[start_frame, end_frame)` of the track. Returns BPM
/// rounded to an integer, or `None` if the range is too short or no rhythmic
/// structure survives the autocorrelation prior.
///
/// Algorithm: mono-mix → windowed FFT → spectral-flux onset envelope →
/// half-wave-rectified, mean-subtracted → biased autocorrelation across the
/// `[MIN_BPM, MAX_BPM]` lag range → octave-resolve against a perceptual prior.
pub fn detect_bpm(track: &Track, start_frame: u64, end_frame: u64) -> Option<f32> {
    let sr = track.sample_rate as f32;
    if !sr.is_finite() || sr <= 0.0 {
        return None;
    }
    let channels = track.channels as usize;
    if channels == 0 {
        return None;
    }
    let total = track.samples.len() / channels;
    let start = (start_frame as usize).min(total);
    let end = (end_frame as usize).min(total);
    if end <= start + FRAME {
        return None;
    }

    let mono = mono_mix(&track.samples, channels, start, end);
    let envelope = onset_envelope(&mono)?;

    let onset_sr = sr / HOP as f32;
    autocorrelate_tempo(&envelope, onset_sr)
}

fn mono_mix(samples: &[f32], channels: usize, start: usize, end: usize) -> Vec<f32> {
    let mut mono = Vec::with_capacity(end - start);
    let scale = 1.0 / channels as f32;
    for i in start..end {
        let base = i * channels;
        let mut s = 0.0;
        for c in 0..channels {
            s += samples[base + c];
        }
        mono.push(s * scale);
    }
    mono
}

/// Spectral-flux onset envelope: per-frame sum of positive magnitude
/// differences across bins. Mean-subtracted and half-wave-rectified before
/// returning so the autocorrelation isn't dominated by the DC component.
fn onset_envelope(mono: &[f32]) -> Option<Vec<f32>> {
    let n_frames = mono.len().checked_sub(FRAME)? / HOP + 1;
    if n_frames < 8 {
        return None;
    }

    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME);
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();

    let window: Vec<f32> = (0..FRAME)
        .map(|n| {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / (FRAME - 1) as f32).cos()
        })
        .collect();

    let n_bins = FRAME / 2 + 1;
    let mut prev_mag = vec![0.0f32; n_bins];
    let mut flux = Vec::with_capacity(n_frames);

    for f in 0..n_frames {
        let start_idx = f * HOP;
        for n in 0..FRAME {
            input[n] = mono[start_idx + n] * window[n];
        }
        fft.process_with_scratch(&mut input, &mut spectrum, &mut scratch)
            .ok()?;
        let mut sum = 0.0f32;
        for k in 0..n_bins {
            let re = spectrum[k].re;
            let im = spectrum[k].im;
            let mag = (re * re + im * im).sqrt();
            if f > 0 {
                let d = mag - prev_mag[k];
                if d > 0.0 {
                    sum += d;
                }
            }
            prev_mag[k] = mag;
        }
        flux.push(sum);
    }

    // Drop the first (zero) frame and centre/rectify so the autocorrelation
    // sees a zero-mean, sparse-positive signal.
    flux.remove(0);
    if flux.is_empty() {
        return None;
    }
    let mean = flux.iter().sum::<f32>() / flux.len() as f32;
    for x in &mut flux {
        *x = (*x - mean).max(0.0);
    }
    Some(flux)
}

/// Score every integer lag in `[MIN_BPM, MAX_BPM]` by the autocorrelation of
/// the onset envelope at that lag, biased toward perceptually plausible
/// tempos. Then octave-resolve the winner against `0.5×` and `2×` candidates.
fn autocorrelate_tempo(envelope: &[f32], onset_sr: f32) -> Option<f32> {
    let min_lag = (60.0 * onset_sr / MAX_BPM).round() as usize;
    let max_lag = (60.0 * onset_sr / MIN_BPM).round() as usize;
    if min_lag == 0 || max_lag >= envelope.len() {
        return None;
    }

    let mut best_lag = 0usize;
    let mut best_score = f32::NEG_INFINITY;
    for lag in min_lag..=max_lag {
        let r = autocorr_at(envelope, lag);
        let bpm = 60.0 * onset_sr / lag as f32;
        let score = r * prior(bpm);
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }
    if best_score <= 0.0 || best_lag == 0 {
        return None;
    }

    let raw_bpm = 60.0 * onset_sr / best_lag as f32;
    let resolved = octave_resolve(envelope, onset_sr, raw_bpm);
    Some(resolved.round())
}

fn autocorr_at(envelope: &[f32], lag: usize) -> f32 {
    if lag >= envelope.len() {
        return 0.0;
    }
    let n = envelope.len() - lag;
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += envelope[i] * envelope[i + lag];
    }
    sum / n as f32
}

/// Compare the chosen BPM against its half and double; pick whichever has the
/// best raw autocorrelation × prior. Catches the common case where the raw
/// peak lands on a period-doubled grid (half-tempo) for a song that's
/// perceptually faster.
fn octave_resolve(envelope: &[f32], onset_sr: f32, bpm: f32) -> f32 {
    let candidates = [bpm * 0.5, bpm, bpm * 2.0];
    let mut best = bpm;
    let mut best_score = f32::NEG_INFINITY;
    for &cand in &candidates {
        if !(MIN_BPM..=MAX_BPM).contains(&cand) {
            continue;
        }
        let lag = (60.0 * onset_sr / cand).round() as usize;
        if lag == 0 || lag >= envelope.len() {
            continue;
        }
        let score = autocorr_at(envelope, lag) * prior(cand);
        if score > best_score {
            best_score = score;
            best = cand;
        }
    }
    best
}

fn prior(bpm: f32) -> f32 {
    let d = (bpm - PRIOR_CENTRE_BPM) / PRIOR_SIGMA_BPM;
    (-0.5 * d * d).exp()
}
