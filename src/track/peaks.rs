use crate::track::Track;

/// Source frames aggregated into one (min, max) pair.
///
/// 1024 frames ≈ 23 ms at 44.1 kHz. For a 4-minute track that yields ~10 300
/// buckets — small enough to keep in RAM trivially, dense enough that the
/// rendering aggregation per pixel never looks chunky.
pub const BUCKET_FRAMES: u32 = 1024;

/// Downsampled min/max envelope of a track, mono-mixed across channels.
///
/// `min[i]` / `max[i]` are the lowest and highest sample values seen across
/// all channels in the i-th `BUCKET_FRAMES`-frame window. Parallel arrays
/// are slightly more cache-friendly than `Vec<(f32, f32)>` when rendering
/// scans both halves linearly.
pub struct TrackPeaks {
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

impl TrackPeaks {
    pub fn compute(track: &Track) -> Self {
        let total_frames = track.frame_count() as usize;
        let channels = track.channels as usize;
        let bucket = BUCKET_FRAMES as usize;
        let n_buckets = total_frames.div_ceil(bucket);

        let mut min = Vec::with_capacity(n_buckets);
        let mut max = Vec::with_capacity(n_buckets);

        for b in 0..n_buckets {
            let start = b * bucket * channels;
            let end = (((b + 1) * bucket).min(total_frames)) * channels;
            let chunk = &track.samples[start..end];
            let mut lo = 0.0_f32;
            let mut hi = 0.0_f32;
            for &s in chunk {
                if s < lo {
                    lo = s;
                }
                if s > hi {
                    hi = s;
                }
            }
            min.push(lo);
            max.push(hi);
        }

        Self { min, max }
    }

    pub fn len(&self) -> usize {
        self.min.len()
    }
}
