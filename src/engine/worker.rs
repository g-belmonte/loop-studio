use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError};
use ringbuf::traits::{Observer, Producer};

use crate::audio::output::{self, ActiveOutput};
use crate::dsp::TimePitchProcessor;
use crate::dsp::passthrough::Passthrough;
use crate::dsp::resample::ResampleSpeed;
use crate::engine::Command;
use crate::engine::state::SharedState;
use crate::track::{LoopRegion, Track};

/// How often the worker wakes to refill the ring when nothing else triggers it.
/// Short enough that the ring never drains (8192 samples ≈ 85 ms at 48 kHz);
/// long enough to avoid burning a core.
const TICK: Duration = Duration::from_millis(2);

pub fn run(rx: Receiver<Command>, state: Arc<SharedState>) {
    let mut track: Option<Arc<Track>> = None;
    let mut cursor: u64 = 0;
    let mut playing = false;
    let mut loop_region: Option<LoopRegion> = None;
    let mut output: Option<ActiveOutput> = None;
    let mut dsp: Box<dyn TimePitchProcessor> = Box::new(Passthrough::new());
    let mut current_channels: Option<u16> = None;
    let mut scratch: Vec<f32> = Vec::new();
    // Stitching buffer for chunks that straddle a loop boundary. Lazily sized
    // on first wrap; cost is one allocation per playback session.
    let mut stitch_buf: Vec<f32> = Vec::new();

    loop {
        let disconnected = drain_commands(
            &rx,
            &mut track,
            &mut cursor,
            &mut playing,
            &mut loop_region,
            &mut output,
            &mut dsp,
            &mut current_channels,
            &mut scratch,
            &state,
        );
        if disconnected {
            break;
        }

        if playing {
            if let (Some(t), Some(out)) = (&track, &mut output) {
                produce(
                    t,
                    &mut cursor,
                    loop_region,
                    out,
                    &mut *dsp,
                    &mut scratch,
                    &mut stitch_buf,
                    &state,
                );

                if loop_region.is_none() && cursor >= t.frame_count() {
                    playing = false;
                    state.playing.store(false, Ordering::Relaxed);
                }
            }
        }

        std::thread::sleep(TICK);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_commands(
    rx: &Receiver<Command>,
    track: &mut Option<Arc<Track>>,
    cursor: &mut u64,
    playing: &mut bool,
    loop_region: &mut Option<LoopRegion>,
    output: &mut Option<ActiveOutput>,
    dsp: &mut Box<dyn TimePitchProcessor>,
    current_channels: &mut Option<u16>,
    scratch: &mut Vec<f32>,
    state: &SharedState,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(cmd) => apply(
                cmd,
                track,
                cursor,
                playing,
                loop_region,
                output,
                dsp,
                current_channels,
                scratch,
                state,
            ),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply(
    cmd: Command,
    track: &mut Option<Arc<Track>>,
    cursor: &mut u64,
    playing: &mut bool,
    loop_region: &mut Option<LoopRegion>,
    output: &mut Option<ActiveOutput>,
    dsp: &mut Box<dyn TimePitchProcessor>,
    current_channels: &mut Option<u16>,
    scratch: &mut Vec<f32>,
    state: &SharedState,
) {
    match cmd {
        Command::LoadTrack(new_track) => {
            // Reopen output if rate or channel count changed.
            let needs_reopen = match output.as_ref() {
                None => true,
                Some(out) => {
                    out.sample_rate != new_track.sample_rate
                        || out.channels != new_track.channels
                }
            };
            if needs_reopen {
                *output = None; // drop old stream first
                match output::open(new_track.sample_rate, new_track.channels) {
                    Ok(o) => *output = Some(o),
                    Err(e) => log::error!("failed to open output: {e:#}"),
                }
            }

            // Recreate DSP if channel count changed (rubato is per-channel-count).
            // Carry the current speed setting over so the slider stays meaningful.
            if *current_channels != Some(new_track.channels) {
                let carried_speed = f32::from_bits(state.speed_bits.load(Ordering::Relaxed));
                *dsp = make_dsp(new_track.channels as usize, carried_speed);
                *current_channels = Some(new_track.channels);
                let needed = dsp.max_output_frames_per_chunk() * new_track.channels as usize;
                if scratch.len() < needed {
                    scratch.resize(needed, 0.0);
                }
            }

            *cursor = 0;
            *playing = false;
            *loop_region = None;
            state.duration
                .store(new_track.frame_count(), Ordering::Relaxed);
            state.position.store(0, Ordering::Relaxed);
            state.playing.store(false, Ordering::Relaxed);
            state.loaded_id.fetch_add(1, Ordering::Relaxed);
            *track = Some(new_track);
        }
        Command::Play => {
            if track.is_some() && output.is_some() {
                *playing = true;
                state.playing.store(true, Ordering::Relaxed);
            }
        }
        Command::Pause => {
            *playing = false;
            state.playing.store(false, Ordering::Relaxed);
        }
        Command::Stop => {
            *playing = false;
            *cursor = 0;
            state.playing.store(false, Ordering::Relaxed);
            state.position.store(0, Ordering::Relaxed);
        }
        Command::Seek(pos) => {
            if let Some(t) = track.as_ref() {
                let target = pos.min(t.frame_count());
                *cursor = snap_into_loop(target, *loop_region);
                state.position.store(*cursor, Ordering::Relaxed);
            }
        }
        Command::SetLoop(region) => {
            *loop_region = region;
            let snapped = snap_into_loop(*cursor, *loop_region);
            if snapped != *cursor {
                *cursor = snapped;
                state.position.store(*cursor, Ordering::Relaxed);
            }
        }
        Command::SetSpeed(speed) => {
            dsp.set_speed(speed);
            state.speed_bits.store(speed.to_bits(), Ordering::Relaxed);
        }
        Command::SetPitch(semitones) => {
            dsp.set_pitch_semitones(semitones);
            state
                .pitch_bits
                .store(semitones.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Run one DSP chunk if both source and ring permit. The DSP defines the
/// chunk size and the upper bound on output; we only call `process()` once
/// both bounds are satisfied.
#[allow(clippy::too_many_arguments)]
fn produce(
    track: &Track,
    cursor: &mut u64,
    loop_region: Option<LoopRegion>,
    output: &mut ActiveOutput,
    dsp: &mut dyn TimePitchProcessor,
    scratch: &mut Vec<f32>,
    stitch_buf: &mut Vec<f32>,
    state: &SharedState,
) {
    let channels = track.channels as usize;
    let total_frames = track.frame_count();
    let in_chunk = dsp.input_frames_per_chunk();
    let out_max = dsp.max_output_frames_per_chunk();

    // Defence in depth: keep the cursor inside the loop on every tick. The
    // command handlers above already snap on Seek/SetLoop, but this catches
    // anything that slips through (e.g. a future code path that mutates the
    // cursor without going through a Command).
    if let Some(l) = loop_region {
        if *cursor >= l.end || *cursor < l.start {
            *cursor = l.start;
        }
    }

    if *cursor >= total_frames {
        return;
    }

    // Sizing: scratch covers the worst-case output (so a mid-process ratio
    // change can't overflow); vacancy uses the expected output for the next
    // call so we don't sit idle when the resampler is at unity.
    let scratch_size = out_max * channels;
    if scratch.len() < scratch_size {
        scratch.resize(scratch_size, 0.0);
    }
    let needed_vacancy = dsp.expected_output_frames_per_chunk() * channels;
    if output.producer.vacant_len() < needed_vacancy {
        return;
    }

    let in_samples = in_chunk * channels;

    // Decide where the chunk's input comes from. Three cases:
    //   1) Plain slice: enough source before the loop end (or end of track).
    //   2) Cross-boundary stitch: <chunk frames left in loop; assemble input
    //      from `cursor..loop.end` + `loop.start..(loop.start + leftover)`.
    //      Keeps loop wraps gap-free.
    //   3) End of track without a loop: drop the tail and stop.
    let boundary = match loop_region {
        Some(l) if *cursor < l.end => l.end,
        _ => total_frames,
    };
    let frames_avail = (boundary - *cursor) as usize;

    let (input_slice, new_cursor): (&[f32], u64) = if frames_avail >= in_chunk {
        let start = (*cursor as usize) * channels;
        (&track.samples[start..start + in_samples], *cursor + in_chunk as u64)
    } else if let Some(l) = loop_region {
        let loop_length = (l.end - l.start) as usize;
        if loop_length < in_chunk {
            // Loop region shorter than one DSP chunk (~23 ms at 44.1 kHz).
            // Not supported in v0.1; produce silence and bail.
            return;
        }
        if stitch_buf.len() < in_samples {
            stitch_buf.resize(in_samples, 0.0);
        }
        let cur_samples = (*cursor as usize) * channels;
        let loop_start_samples = (l.start as usize) * channels;
        let tail_frames = frames_avail;
        let head_frames = in_chunk - tail_frames;
        let tail_samples = tail_frames * channels;
        let head_samples = head_frames * channels;
        stitch_buf[..tail_samples]
            .copy_from_slice(&track.samples[cur_samples..cur_samples + tail_samples]);
        stitch_buf[tail_samples..in_samples].copy_from_slice(
            &track.samples[loop_start_samples..loop_start_samples + head_samples],
        );
        (&stitch_buf[..in_samples], l.start + head_frames as u64)
    } else {
        // End of track without a loop. Drop the <chunk-sized tail and snap
        // the cursor so the run loop's EOF check fires.
        *cursor = total_frames;
        state.position.store(*cursor, Ordering::Relaxed);
        return;
    };

    let (_in_used, out_written) =
        dsp.process(input_slice, &mut scratch[..scratch_size], channels);

    let out_samples = out_written * channels;
    output.producer.push_slice(&scratch[..out_samples]);

    *cursor = new_cursor;
    state.position.store(*cursor, Ordering::Relaxed);
}

/// Build the DSP for a track. Falls back to passthrough (no speed control)
/// if the resampler can't be constructed for the given channel count.
fn make_dsp(channels: usize, current_speed: f32) -> Box<dyn TimePitchProcessor> {
    match ResampleSpeed::new(channels) {
        Ok(mut r) => {
            r.set_speed(current_speed);
            Box::new(r)
        }
        Err(e) => {
            log::error!(
                "resampler failed for {channels} ch: {e:#}; using passthrough (no speed control)"
            );
            Box::new(Passthrough::new())
        }
    }
}

/// Snap a cursor frame into a loop region. Cursors below `start` or at/past
/// `end` are pulled to `start` (the only consistent landing spot for the
/// "stay inside the loop" invariant). Returns the input unchanged if no loop
/// is set or the region is degenerate.
fn snap_into_loop(cursor: u64, loop_region: Option<LoopRegion>) -> u64 {
    match loop_region {
        Some(l) if l.end > l.start && (cursor < l.start || cursor >= l.end) => l.start,
        _ => cursor,
    }
}
