//! Offline loop renderer. Renders the active loop region through a fresh
//! DSP (matching the engine's current settings) and writes the result to a
//! WAV file. Runs on a `std::thread` spawned by `App::spawn_render`; the
//! engine and cpal callback are untouched.
//!
//! Robustness model:
//!
//! - Writes to `<path>.part` and `fs::rename`s to the final path only after
//!   the WAV header is finalised. A killed process (app closed mid-render)
//!   never leaves a half-valid `.wav` at the chosen path.
//! - A shared `Arc<AtomicBool>` cancel flag is polled once per chunk. On
//!   cancel/error the worker drops the writer and `remove_file`s the `.part`
//!   so cleanup is best-effort even without an `on_exit` hook getting a
//!   chance to run.
//!
//! Signal chain mirrors the engine for the staged-in features: pitch + speed
//! via the DSP, EQ baked in, optional metronome. Master volume is
//! deliberately excluded — it's a monitoring control, not part of the
//! artistic signal. Speed ramp is frozen at `current_speed` at request time
//! and does not advance during the render.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;

use crate::dsp::eq::{Eq, EqSettings};
use crate::dsp::{DspKind, TimePitchProcessor};
use crate::engine::make_dsp;
use crate::engine::metronome::{Metronome, MetronomeSettings};
use crate::track::{LoopRegion, Track};

/// Sample format for the exported WAV. 16-bit PCM is the universal default;
/// 32-bit float preserves the DSP output bit-perfectly for users who want to
/// re-process the export downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Pcm16,
    F32,
}

impl ExportFormat {
    fn spec(self, channels: u16, sample_rate: u32) -> hound::WavSpec {
        let (bits, sample_format) = match self {
            ExportFormat::Pcm16 => (16, hound::SampleFormat::Int),
            ExportFormat::F32 => (32, hound::SampleFormat::Float),
        };
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format,
        }
    }
}

/// Snapshot of everything the renderer needs. Built on the GUI thread when
/// the user clicks Export, then moved to the worker — by design no shared
/// state with the live engine beyond `Arc<Track>` (read-only).
pub struct RenderRequest {
    pub track: Arc<Track>,
    pub loop_region: LoopRegion,
    pub dsp_kind: DspKind,
    pub speed: f32,
    pub pitch_semitones: f32,
    pub eq: EqSettings,
    pub include_metronome: bool,
    pub metronome: MetronomeSettings,
    pub out_path: PathBuf,
    pub format: ExportFormat,
    /// Tags the result so a late delivery for a stale request (e.g. user
    /// already moved on to a different export) can be discarded by the App.
    pub job_id: u64,
}

/// Lifecycle events sent back over a crossbeam channel. `Progress` carries a
/// fraction in `[0.0, 1.0]`; `Done` carries the final output path so the UI
/// can show "Exported to …" with the actual file (which may differ from the
/// user's chosen name if a future version sanitises it).
#[derive(Debug, Clone)]
pub enum RenderProgress {
    Started { job_id: u64 },
    Progress { job_id: u64, fraction: f32 },
    Done { job_id: u64, out_path: PathBuf },
    Failed { job_id: u64, error: String },
    Cancelled { job_id: u64 },
}

/// Spawn the render worker. The handle is returned for `on_exit` to join
/// briefly after setting `cancel`; the App holds it in `Option` so a finished
/// job can be `take()`n and dropped.
pub fn spawn(
    req: RenderRequest,
    tx: Sender<RenderProgress>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loop-studio-render".into())
        .spawn(move || run(req, tx, cancel))
        .expect("spawning render thread")
}

fn run(req: RenderRequest, tx: Sender<RenderProgress>, cancel: Arc<AtomicBool>) {
    let job_id = req.job_id;
    let _ = tx.send(RenderProgress::Started { job_id });

    let out_path = req.out_path.clone();
    let part_path = part_path_for(&out_path);

    match render_to_part(&req, &tx, &cancel, &part_path) {
        Ok(RenderOutcome::Done) => match std::fs::rename(&part_path, &out_path) {
            Ok(()) => {
                let _ = tx.send(RenderProgress::Done { job_id, out_path });
            }
            Err(e) => {
                // The .part file exists but we couldn't rename it. Leave the
                // .part on disk (the user might want to recover it manually);
                // report the error.
                let _ = tx.send(RenderProgress::Failed {
                    job_id,
                    error: format!("rename failed: {e}"),
                });
            }
        },
        Ok(RenderOutcome::Cancelled) => {
            let _ = std::fs::remove_file(&part_path);
            let _ = tx.send(RenderProgress::Cancelled { job_id });
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part_path);
            let _ = tx.send(RenderProgress::Failed {
                job_id,
                error: format!("{e:#}"),
            });
        }
    }
}

enum RenderOutcome {
    Done,
    Cancelled,
}

/// Render through DSP → EQ → (optional) metronome and stream samples into the
/// WAV writer at `part_path`. Two passes: the first warms the DSP state and is
/// discarded; the second is captured to disk. Output length per pass equals
/// `loop_length / speed` source frames, computed exactly via the input chunk
/// counter (not estimated up front, so a non-stationary speed ramp could be
/// re-added later without changing this code).
fn render_to_part(
    req: &RenderRequest,
    tx: &Sender<RenderProgress>,
    cancel: &AtomicBool,
    part_path: &std::path::Path,
) -> Result<RenderOutcome> {
    let track = &req.track;
    let channels = track.channels as usize;
    if channels == 0 {
        return Err(anyhow!("track has 0 channels"));
    }
    let loop_length = req
        .loop_region
        .end
        .checked_sub(req.loop_region.start)
        .ok_or_else(|| anyhow!("loop end before start"))?;
    if loop_length == 0 {
        return Err(anyhow!("loop is empty"));
    }

    // Build a fresh DSP instance — never touch the engine's. The composite's
    // input chunk size is what we use to walk the loop; pitch and speed are
    // applied via the same setters the engine uses on every command.
    let mut dsp = make_dsp(channels, req.dsp_kind, req.speed, req.pitch_semitones);
    let in_chunk = dsp.input_frames_per_chunk();
    if (loop_length as usize) < in_chunk {
        return Err(anyhow!(
            "loop is shorter than one DSP chunk ({} < {} frames) — same limitation as live playback",
            loop_length,
            in_chunk
        ));
    }
    // Worst-case output sizing: `max_output_frames_per_chunk` covers any ratio
    // the DSP could currently emit. Renderer is single-shot per chunk so we
    // don't need the cheaper `expected_*` value the live engine uses.
    let out_max = dsp.max_output_frames_per_chunk();

    // EQ runs over DSP output, same position as the engine. We build a fresh
    // instance for the same reason: per-channel biquad state and
    // sr-dependent coefficients can't be borrowed from the live engine
    // without disturbing playback.
    let mut eq = Eq::new(channels, track.sample_rate);
    eq.set_settings(req.eq);

    // Metronome (if enabled) is built fresh too, anchored at loop.start so
    // beat 1 lands at the start of the captured pass — matching how the
    // engine anchors when a loop is active.
    let mut metronome = Metronome::new();
    metronome.set_sample_rate(track.sample_rate);
    metronome.set_anchor(req.loop_region.start);
    if req.include_metronome {
        metronome.set_settings(req.metronome);
    }

    let mut input_buf: Vec<f32> = vec![0.0; in_chunk * channels];
    let mut scratch: Vec<f32> = vec![0.0; out_max * channels];

    // Expected total source frames per pass: one full loop_length. We feed
    // chunks of `in_chunk` source frames starting at loop.start; when we'd
    // cross loop.end we stitch from loop.start (same shape as the engine's
    // produce()). Each pass stops once we've fed `loop_length` source
    // frames.
    let spec = req.format.spec(track.channels, track.sample_rate);
    let writer = hound::WavWriter::create(part_path, spec)
        .with_context(|| format!("creating WAV writer at {}", part_path.display()))?;
    let mut writer = writer;

    // Priming pass: produce one loop's worth of output and throw it away.
    // The DSP, EQ, and metronome states carry over to the captured pass.
    let _ = out_max; // sizing of `scratch`; render_one_pass reads through it
    let priming_outcome = render_one_pass(
        track,
        req.loop_region,
        &mut *dsp,
        &mut eq,
        &mut metronome,
        in_chunk,
        channels,
        &mut input_buf,
        &mut scratch,
        |_out| Ok(()), // discard
        cancel,
        tx,
        req.job_id,
        0.0,
        0.5,
    )?;
    if let RenderOutcome::Cancelled = priming_outcome {
        drop(writer); // best-effort flush, then we delete the .part
        return Ok(RenderOutcome::Cancelled);
    }

    // Capture pass: write samples to the WAV. The output-frame stream from
    // the DSP is `loop_length / speed` frames long, matching what one pass of
    // live playback would emit.
    let capture_outcome = render_one_pass(
        track,
        req.loop_region,
        &mut *dsp,
        &mut eq,
        &mut metronome,
        in_chunk,
        channels,
        &mut input_buf,
        &mut scratch,
        |out| write_samples(&mut writer, out, req.format),
        cancel,
        tx,
        req.job_id,
        0.5,
        1.0,
    )?;
    if let RenderOutcome::Cancelled = capture_outcome {
        // Drop writer first so the .part is closed before remove_file.
        drop(writer);
        return Ok(RenderOutcome::Cancelled);
    }

    writer.finalize().context("finalising WAV writer")?;
    Ok(RenderOutcome::Done)
}

/// One pass through the loop: feed `loop_length` source frames into the DSP
/// (stitching across the loop boundary on the last sub-chunk), run EQ, mix
/// the metronome, and hand the output to `sink`. Returns Cancelled if the
/// cancel flag fires between chunks.
///
/// `progress_lo`/`progress_hi` bound the fraction this pass reports — the
/// caller uses [0, 0.5] for priming and [0.5, 1.0] for capture so the bar
/// reaches 100 % at end of render.
#[allow(clippy::too_many_arguments)]
fn render_one_pass(
    track: &Track,
    region: LoopRegion,
    dsp: &mut dyn TimePitchProcessor,
    eq: &mut Eq,
    metronome: &mut Metronome,
    in_chunk: usize,
    channels: usize,
    input_buf: &mut [f32],
    scratch: &mut [f32],
    mut sink: impl FnMut(&[f32]) -> Result<()>,
    cancel: &AtomicBool,
    tx: &Sender<RenderProgress>,
    job_id: u64,
    progress_lo: f32,
    progress_hi: f32,
) -> Result<RenderOutcome> {
    let loop_length = region.end - region.start;
    let mut cursor = region.start;
    let mut consumed: u64 = 0;
    let mut last_reported_progress = -1.0_f32;

    while consumed < loop_length {
        if cancel.load(Ordering::Relaxed) {
            return Ok(RenderOutcome::Cancelled);
        }

        let cursor_before = cursor;
        let remaining = loop_length - consumed;
        let this_chunk = (remaining as usize).min(in_chunk);

        // Cap chunk at loop.end on the source side, then stitch from loop.start
        // for any remainder up to in_chunk. The DSP always sees exactly
        // in_chunk frames regardless.
        let frames_to_loop_end = (region.end - cursor) as usize;
        let tail = frames_to_loop_end.min(this_chunk).min(in_chunk);
        let head = in_chunk - tail;
        let cur_samples = (cursor as usize) * channels;
        let start_samples = (region.start as usize) * channels;
        input_buf[..tail * channels]
            .copy_from_slice(&track.samples[cur_samples..cur_samples + tail * channels]);
        if head > 0 {
            input_buf[tail * channels..in_chunk * channels].copy_from_slice(
                &track.samples[start_samples..start_samples + head * channels],
            );
        }
        // Advance cursor: on a normal step this is `cursor += in_chunk`; on
        // the boundary step it wraps to `loop.start + head`.
        cursor = if head > 0 {
            region.start + head as u64
        } else {
            cursor + in_chunk as u64
        };

        let (_in_used, out_written) = dsp.process(input_buf, scratch, channels);
        let out_samples = out_written * channels;
        if out_samples == 0 {
            // Possible during the first few chunks of WSOLA/PV while internal
            // buffers fill; harmless — just advance and ask for more.
            consumed = (consumed + this_chunk as u64).min(loop_length);
            continue;
        }
        let out_slice = &mut scratch[..out_samples];

        eq.process_in_place(out_slice);
        // Metronome mixing mirrors the engine: pre-master, source-frame
        // anchored. We pass the full chunk's source range (`in_chunk`
        // frames starting at `cursor_before`) rather than splitting at the
        // stitch boundary — the worst-case timing slip is < 23 ms and only
        // affects the beat that falls inside the wrap chunk.
        metronome.mix_segment(out_slice, channels, cursor_before, in_chunk as u64);

        sink(out_slice)?;

        // The progress fraction is driven by source frames consumed, not
        // output frames written — input frame consumption is monotonic and
        // exact regardless of speed.
        consumed = (consumed + this_chunk as u64).min(loop_length);
        let pass_fraction = consumed as f32 / loop_length as f32;
        let overall = progress_lo + (progress_hi - progress_lo) * pass_fraction;
        // Throttle to ~1 % steps so the channel isn't flooded; the UI repaint
        // budget doesn't care about more.
        if overall - last_reported_progress > 0.01 {
            last_reported_progress = overall;
            let _ = tx.send(RenderProgress::Progress {
                job_id,
                fraction: overall,
            });
        }
    }
    // Ensure the final 100 %-of-this-pass tick is sent even after throttling.
    let _ = tx.send(RenderProgress::Progress {
        job_id,
        fraction: progress_hi,
    });
    Ok(RenderOutcome::Done)
}

fn write_samples<W: std::io::Write + std::io::Seek>(
    writer: &mut hound::WavWriter<std::io::BufWriter<W>>,
    samples: &[f32],
    format: ExportFormat,
) -> Result<()> {
    match format {
        ExportFormat::F32 => {
            for &s in samples {
                writer
                    .write_sample(s)
                    .context("writing f32 sample to WAV")?;
            }
        }
        ExportFormat::Pcm16 => {
            // Truncating conversion. Clip to avoid wrap; no dither — known
            // limitation, documented in ARCHITECTURE.md.
            for &s in samples {
                let clipped = s.clamp(-1.0, 1.0);
                let v = (clipped * 32767.0) as i16;
                writer.write_sample(v).context("writing i16 sample to WAV")?;
            }
        }
    }
    Ok(())
}

/// Sibling path for the in-flight write. `foo.wav` → `foo.wav.part`.
/// Same parent dir keeps `fs::rename` atomic (no cross-filesystem move).
fn part_path_for(out_path: &std::path::Path) -> PathBuf {
    let mut s = out_path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}
