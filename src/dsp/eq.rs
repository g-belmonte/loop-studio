//! 5-band EQ with per-band solo isolation, sitting between the time/pitch
//! DSP and the metronome on the engine worker thread.
//!
//! Two modes share one filter graph:
//!
//! - **Normal**: 5 biquads cascaded per channel — low-shelf @ 200 Hz,
//!   peak/bell @ 500 / 1 k / 2.5 k Hz, high-shelf @ 4 kHz. Per-band gain only;
//!   centre frequencies and Q are fixed for now.
//! - **Solo**: when any band's `solo` flag is set, the cascade is replaced by a
//!   single isolation filter for that band (LPF for the low shelf, BPF for the
//!   peaks, HPF for the high shelf). The UI enforces at most one solo at a
//!   time; if more than one slip through, the lowest-index band wins.
//!
//! Zipper noise on slider drags is masked by linearly interpolating biquad
//! coefficients across each chunk from the previous chunk's end state to the
//! new target. Switching between Normal and Solo (or enabling/disabling the
//! EQ) is a structural change — `active` filter count differs — so the
//! interpolation can't be meaningful; in that case we snap to the new state
//! and accept a ~one-chunk artefact (~23 ms).

use serde::{Deserialize, Serialize};

pub const NUM_BANDS: usize = 5;

/// Centre / corner frequencies (Hz) for the five bands, in cascade order.
pub const BAND_FREQS_HZ: [f32; NUM_BANDS] = [200.0, 500.0, 1000.0, 2500.0, 4000.0];

/// Q for the three peak bands. 1.0 is moderately wide — still surgical enough
/// that boosting a single band sounds like a band, not a tone burst.
const PEAK_Q: f32 = 1.0;
/// Shelf "slope" Q. `1/√2` is the textbook Butterworth-shape shelf.
const SHELF_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// Q for LPF/HPF in solo mode (low / high shelf soloed).
const SOLO_LPHP_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// BPF Q for solo'd peak bands — broader skirts than a textbook BPF so the
/// soloed band sounds like a region, not a whistle.
const SOLO_BPF_Q: f32 = 0.7;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BandSettings {
    pub gain_db: f32,
    pub solo: bool,
}

impl Default for BandSettings {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            solo: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct EqSettings {
    pub enabled: bool,
    pub bands: [BandSettings; NUM_BANDS],
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bands: [BandSettings::default(); NUM_BANDS],
        }
    }
}

/// Normalised biquad coefficients (a0 divided out).
#[derive(Clone, Copy, Default, Debug)]
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoeffs {
    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    fn low_shelf(sr: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) + (a - 1.0) * cos_w + two_sqrt_a_alpha;
        Self::normalised(
            a * ((a + 1.0) - (a - 1.0) * cos_w + two_sqrt_a_alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w),
            a * ((a + 1.0) - (a - 1.0) * cos_w - two_sqrt_a_alpha),
            a0,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos_w),
            (a + 1.0) + (a - 1.0) * cos_w - two_sqrt_a_alpha,
        )
    }

    fn peak(sr: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        Self::normalised(
            1.0 + alpha * a,
            -2.0 * cos_w,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos_w,
            1.0 - alpha / a,
        )
    }

    fn high_shelf(sr: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) - (a - 1.0) * cos_w + two_sqrt_a_alpha;
        Self::normalised(
            a * ((a + 1.0) + (a - 1.0) * cos_w + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w),
            a * ((a + 1.0) + (a - 1.0) * cos_w - two_sqrt_a_alpha),
            a0,
            2.0 * ((a - 1.0) - (a + 1.0) * cos_w),
            (a + 1.0) - (a - 1.0) * cos_w - two_sqrt_a_alpha,
        )
    }

    fn lpf(sr: f32, f0: f32, q: f32) -> Self {
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        let one_minus_cos = 1.0 - cos_w;
        Self::normalised(
            one_minus_cos * 0.5,
            one_minus_cos,
            one_minus_cos * 0.5,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn hpf(sr: f32, f0: f32, q: f32) -> Self {
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        let one_plus_cos = 1.0 + cos_w;
        Self::normalised(
            one_plus_cos * 0.5,
            -one_plus_cos,
            one_plus_cos * 0.5,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn bpf(sr: f32, f0: f32, q: f32) -> Self {
        let (cos_w, sin_w) = omega(sr, f0);
        let alpha = sin_w / (2.0 * q);
        Self::normalised(
            alpha,
            0.0,
            -alpha,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn normalised(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        let inv = 1.0 / a0;
        Self {
            b0: b0 * inv,
            b1: b1 * inv,
            b2: b2 * inv,
            a1: a1 * inv,
            a2: a2 * inv,
        }
    }
}

fn omega(sr: f32, f0: f32) -> (f32, f32) {
    let w0 = (2.0 * std::f32::consts::PI * f0 / sr).clamp(1e-6, std::f32::consts::PI - 1e-6);
    (w0.cos(), w0.sin())
}

/// Direct-form-I biquad state. DF-I's state is past inputs/outputs rather than
/// internal transformed quantities, which behaves more gracefully when
/// coefficients change between samples (DF-II-transposed state is in a
/// coefficient-dependent basis, so changing coefficients while it's non-zero
/// produces brief transients).
#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    #[inline]
    fn process(&mut self, x: f32, c: &BiquadCoeffs) -> f32 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

pub struct Eq {
    channels: usize,
    sample_rate: f32,
    settings: EqSettings,
    /// `[channel][slot]` biquad state. NUM_BANDS slots; in solo mode only
    /// slot 0 is used (states 1..NUM_BANDS keep running but on identity coeffs,
    /// so the filter chain length stays structurally fixed once `active` is
    /// stable for a given mode).
    states: Vec<[BiquadState; NUM_BANDS]>,
    /// Coefficients at the *start* of the next chunk. After each chunk this
    /// is overwritten with the just-applied target so the next chunk's ramp
    /// continues smoothly.
    coeffs: [BiquadCoeffs; NUM_BANDS],
    /// Number of cascaded biquads currently in use: 0 (bypass), 1 (solo), or
    /// NUM_BANDS (normal). Used to detect structural changes that can't be
    /// smoothed.
    active: usize,
}

impl Eq {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let states = (0..channels.max(1))
            .map(|_| [BiquadState::default(); NUM_BANDS])
            .collect();
        Self {
            channels,
            sample_rate: sample_rate as f32,
            settings: EqSettings::default(),
            states,
            coeffs: [BiquadCoeffs::identity(); NUM_BANDS],
            active: 0,
        }
    }

    pub fn set_settings(&mut self, settings: EqSettings) {
        self.settings = settings;
    }

    fn compute_target(&self) -> ([BiquadCoeffs; NUM_BANDS], usize) {
        let mut c = [BiquadCoeffs::identity(); NUM_BANDS];
        if !self.settings.enabled {
            return (c, 0);
        }
        if let Some(i) = self.settings.bands.iter().position(|b| b.solo) {
            let f0 = BAND_FREQS_HZ[i];
            c[0] = if i == 0 {
                BiquadCoeffs::lpf(self.sample_rate, f0, SOLO_LPHP_Q)
            } else if i == NUM_BANDS - 1 {
                BiquadCoeffs::hpf(self.sample_rate, f0, SOLO_LPHP_Q)
            } else {
                BiquadCoeffs::bpf(self.sample_rate, f0, SOLO_BPF_Q)
            };
            return (c, 1);
        }
        c[0] = BiquadCoeffs::low_shelf(
            self.sample_rate,
            BAND_FREQS_HZ[0],
            SHELF_Q,
            self.settings.bands[0].gain_db,
        );
        c[1] = BiquadCoeffs::peak(
            self.sample_rate,
            BAND_FREQS_HZ[1],
            PEAK_Q,
            self.settings.bands[1].gain_db,
        );
        c[2] = BiquadCoeffs::peak(
            self.sample_rate,
            BAND_FREQS_HZ[2],
            PEAK_Q,
            self.settings.bands[2].gain_db,
        );
        c[3] = BiquadCoeffs::peak(
            self.sample_rate,
            BAND_FREQS_HZ[3],
            PEAK_Q,
            self.settings.bands[3].gain_db,
        );
        c[4] = BiquadCoeffs::high_shelf(
            self.sample_rate,
            BAND_FREQS_HZ[4],
            SHELF_Q,
            self.settings.bands[4].gain_db,
        );
        (c, NUM_BANDS)
    }

    /// Run the EQ over interleaved samples in place. `buf.len()` must be a
    /// multiple of `channels`; otherwise the trailing partial frame is ignored.
    pub fn process_in_place(&mut self, buf: &mut [f32]) {
        if self.channels == 0 {
            return;
        }
        let frames = buf.len() / self.channels;
        if frames == 0 {
            return;
        }
        let (target, target_active) = self.compute_target();

        // Both bypass — nothing to do, and don't churn state.
        if self.active == 0 && target_active == 0 {
            return;
        }

        // Structural change (mode toggle or enable/disable): can't smoothly
        // ramp a 5-band cascade to a 1-band isolation filter. Snap to target.
        if target_active != self.active {
            self.coeffs = target;
            self.active = target_active;
            self.process_chunk_constant(buf, frames);
            return;
        }

        // Smooth case: linearly ramp `coeffs` → `target` across the chunk.
        // When they're already equal (steady state), the ramp degenerates to a
        // constant pass — short-circuit so we don't pay the per-sample lerp.
        if coeffs_equal(&self.coeffs, &target, self.active) {
            self.process_chunk_constant(buf, frames);
        } else {
            self.process_chunk_ramped(buf, frames, &target);
            self.coeffs = target;
        }
    }

    fn process_chunk_constant(&mut self, buf: &mut [f32], frames: usize) {
        let active = self.active;
        let channels = self.channels;
        for f in 0..frames {
            for ch in 0..channels {
                let idx = f * channels + ch;
                let mut s = buf[idx];
                for slot in 0..active {
                    s = self.states[ch][slot].process(s, &self.coeffs[slot]);
                }
                buf[idx] = s;
            }
        }
    }

    fn process_chunk_ramped(
        &mut self,
        buf: &mut [f32],
        frames: usize,
        target: &[BiquadCoeffs; NUM_BANDS],
    ) {
        let active = self.active;
        let channels = self.channels;
        let inv = 1.0 / frames as f32;
        for f in 0..frames {
            let t = f as f32 * inv;
            // Interpolate once per frame; reused across channels.
            let mut frame_coeffs = [BiquadCoeffs::default(); NUM_BANDS];
            for slot in 0..active {
                let c0 = &self.coeffs[slot];
                let c1 = &target[slot];
                frame_coeffs[slot] = BiquadCoeffs {
                    b0: c0.b0 + (c1.b0 - c0.b0) * t,
                    b1: c0.b1 + (c1.b1 - c0.b1) * t,
                    b2: c0.b2 + (c1.b2 - c0.b2) * t,
                    a1: c0.a1 + (c1.a1 - c0.a1) * t,
                    a2: c0.a2 + (c1.a2 - c0.a2) * t,
                };
            }
            for ch in 0..channels {
                let idx = f * channels + ch;
                let mut s = buf[idx];
                for (state, coeffs) in self.states[ch]
                    .iter_mut()
                    .zip(frame_coeffs.iter())
                    .take(active)
                {
                    s = state.process(s, coeffs);
                }
                buf[idx] = s;
            }
        }
    }
}

fn coeffs_equal(a: &[BiquadCoeffs; NUM_BANDS], b: &[BiquadCoeffs; NUM_BANDS], n: usize) -> bool {
    const EPS: f32 = 1e-9;
    for i in 0..n {
        if (a[i].b0 - b[i].b0).abs() > EPS
            || (a[i].b1 - b[i].b1).abs() > EPS
            || (a[i].b2 - b[i].b2).abs() > EPS
            || (a[i].a1 - b[i].a1).abs() > EPS
            || (a[i].a2 - b[i].a2).abs() > EPS
        {
            return false;
        }
    }
    true
}
