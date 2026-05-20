use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError};
use ringbuf::traits::{Observer, Producer};

use crate::audio::output::{self, ActiveOutput};
use crate::dsp::eq::Eq;
use crate::dsp::passthrough::Passthrough;
use crate::dsp::phase_vocoder::{PhaseVocoderPitchShift, PhaseVocoderSpeed};
use crate::dsp::wsola::{WsolaPitchShift, WsolaSpeed};
use crate::dsp::{DspKind, TimePitchProcessor};
use crate::engine::Command;
use crate::engine::metronome::Metronome;
use crate::engine::speed_ramp::{SpeedRampSettings, step_in_speed_units};
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
    let mut current_kind: DspKind = DspKind::default();
    let mut metronome = Metronome::new();
    // EQ runs between dsp.process() and metronome.mix_segment(). Recreated
    // alongside the DSP when channel count or sample rate changes (per-channel
    // biquad state, and coefficient formulae depend on sr). Until then it
    // holds a default bypass instance so process_in_place is a no-op.
    let mut eq = Eq::new(0, 0);
    // Master output gain (linear). `current` tracks what the last chunk ended
    // at; `target` is what the UI most recently requested. produce() ramps
    // current → target linearly across one chunk to avoid zipper noise.
    let mut master_current_gain: f32 = 1.0;
    let mut master_target_gain: f32 = 1.0;
    // Speed-ramp state. Settings carry across loads (UI source of truth);
    // `passes_since_step` is the local counter the worker keeps to know when
    // to fire a step. Reset on a rising edge of `enabled` and on Stop.
    let mut speed_ramp = SpeedRampSettings::default();
    let mut passes_since_step: u32 = 0;
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
            &mut current_kind,
            &mut metronome,
            &mut eq,
            &mut master_target_gain,
            &mut speed_ramp,
            &mut passes_since_step,
            &mut scratch,
            &state,
        );
        if disconnected {
            break;
        }

        if playing
            && let (Some(t), Some(out)) = (&track, &mut output)
        {
            produce(
                t,
                &mut cursor,
                loop_region,
                out,
                &mut *dsp,
                &mut metronome,
                &mut eq,
                &mut master_current_gain,
                master_target_gain,
                &speed_ramp,
                &mut passes_since_step,
                &mut scratch,
                &mut stitch_buf,
                &state,
            );

            if loop_region.is_none() && cursor >= t.frame_count() {
                playing = false;
                state.playing.store(false, Ordering::Relaxed);
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
    current_kind: &mut DspKind,
    metronome: &mut Metronome,
    eq: &mut Eq,
    master_target_gain: &mut f32,
    speed_ramp: &mut SpeedRampSettings,
    passes_since_step: &mut u32,
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
                current_kind,
                metronome,
                eq,
                master_target_gain,
                speed_ramp,
                passes_since_step,
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
    current_kind: &mut DspKind,
    metronome: &mut Metronome,
    eq: &mut Eq,
    master_target_gain: &mut f32,
    speed_ramp: &mut SpeedRampSettings,
    passes_since_step: &mut u32,
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

            // Recreate DSP if channel count changed (per-channel-count state in
            // both WSOLA buffers and the rubato resampler). Carry the current
            // speed and pitch so the sliders stay meaningful across loads.
            if *current_channels != Some(new_track.channels) {
                let carried_speed = f32::from_bits(state.speed_bits.load(Ordering::Relaxed));
                let carried_pitch = f32::from_bits(state.pitch_bits.load(Ordering::Relaxed));
                *dsp = make_dsp(
                    new_track.channels as usize,
                    *current_kind,
                    carried_speed,
                    carried_pitch,
                );
                *current_channels = Some(new_track.channels);
                let needed = dsp.max_output_frames_per_chunk() * new_track.channels as usize;
                if scratch.len() < needed {
                    scratch.resize(needed, 0.0);
                }
            }

            metronome.set_sample_rate(new_track.sample_rate);
            // Loop is cleared on load — anchor reverts to track start.
            metronome.set_anchor(0);
            metronome.reset_voice();

            // EQ holds per-channel biquad state and computes coefficients from
            // the sample rate; recreate it on every LoadTrack rather than
            // gating on channel-count change so a same-channel-count reload at
            // a different sample rate (e.g. 44.1 → 48 kHz) doesn't keep stale
            // coefficients. Settings are re-pushed by the App via SetEq.
            *eq = Eq::new(new_track.channels as usize, new_track.sample_rate);

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
            metronome.reset_voice();
            state.playing.store(false, Ordering::Relaxed);
        }
        Command::Stop => {
            *playing = false;
            *cursor = 0;
            metronome.reset_voice();
            state.playing.store(false, Ordering::Relaxed);
            state.position.store(0, Ordering::Relaxed);
        }
        Command::Seek(pos) => {
            if let Some(t) = track.as_ref() {
                let target = pos.min(t.frame_count());
                *cursor = snap_into_loop(target, *loop_region);
                metronome.reset_voice();
                state.position.store(*cursor, Ordering::Relaxed);
            }
        }
        Command::SetLoop(region) => {
            *loop_region = region;
            metronome.set_anchor(region.map(|r| r.start).unwrap_or(0));
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
        Command::SetDsp(kind) => {
            // Always remember the requested kind — applied lazily on the next
            // LoadTrack if no track is loaded yet, or rebuilt now if one is.
            if *current_kind != kind {
                *current_kind = kind;
                if let Some(channels) = *current_channels {
                    let carried_speed =
                        f32::from_bits(state.speed_bits.load(Ordering::Relaxed));
                    let carried_pitch =
                        f32::from_bits(state.pitch_bits.load(Ordering::Relaxed));
                    *dsp = make_dsp(
                        channels as usize,
                        kind,
                        carried_speed,
                        carried_pitch,
                    );
                    let needed = dsp.max_output_frames_per_chunk() * channels as usize;
                    if scratch.len() < needed {
                        scratch.resize(needed, 0.0);
                    }
                }
            }
        }
        Command::SetMetronome(settings) => {
            metronome.set_settings(settings);
        }
        Command::SetEq(settings) => {
            eq.set_settings(settings);
        }
        Command::SetSpeedRamp(new) => {
            // Rising edge on `enabled` resets the per-step counter so the
            // first bump lands `passes_per_step` wraps after the user turns it
            // on — not immediately, which would feel jarring. Falling edge
            // also resets so re-enabling later starts a fresh cycle.
            if new.enabled != speed_ramp.enabled {
                *passes_since_step = 0;
            }
            *speed_ramp = new;
        }
        Command::SetMasterVolume(db) => {
            // Clamp + convert to linear here so produce() never has to think
            // about dB. Floor at -60 dB (~0.001×) since the slider stops there;
            // a non-finite input falls back to unity rather than poisoning the
            // ramp with NaN.
            let g = if db.is_finite() {
                10f32.powf(db.max(-60.0) / 20.0)
            } else {
                1.0
            };
            *master_target_gain = g;
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
    metronome: &mut Metronome,
    eq: &mut Eq,
    master_current_gain: &mut f32,
    master_target_gain: f32,
    speed_ramp: &SpeedRampSettings,
    passes_since_step: &mut u32,
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
    if let Some(l) = loop_region
        && (*cursor >= l.end || *cursor < l.start)
    {
        *cursor = l.start;
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
    let cursor_before = *cursor;
    // Set inside the stitch arm below; consumed after push_slice to drive the
    // speed-ramp step counter. The wrap happens *at the end* of this chunk
    // (cursor crosses loop.end into loop.start), so we bump the counter then,
    // not on the first chunk that lands inside the loop.
    let mut wrapped = false;

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

    // `stitch_tail_frames` is `Some(n)` only when we built `input_slice` from
    // two source ranges: `[cursor_before, cursor_before + n)` followed by
    // `[loop.start, loop.start + (in_chunk - n))`. The metronome needs this
    // to schedule beats correctly across the wrap.
    let (input_slice, new_cursor, stitch_tail_frames): (&[f32], u64, Option<usize>) =
        if frames_avail >= in_chunk {
            let start = (*cursor as usize) * channels;
            (
                &track.samples[start..start + in_samples],
                *cursor + in_chunk as u64,
                None,
            )
        } else if let Some(l) = loop_region {
            let loop_length = (l.end - l.start) as usize;
            if loop_length < in_chunk {
                // Loop region shorter than one DSP chunk (~23 ms at 44.1 kHz).
                // Not supported in v0.1; produce silence and bail.
                return;
            }
            wrapped = true;
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
            (
                &stitch_buf[..in_samples],
                l.start + head_frames as u64,
                Some(tail_frames),
            )
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
    let out_slice = &mut scratch[..out_samples];

    // EQ runs over the DSP output before the metronome mixes in. Placing it
    // pre-metronome keeps the click pristine even when a "solo" band has
    // filtered the music down to a sliver — the metronome stays audible.
    eq.process_in_place(out_slice);

    // Mix metronome into the just-produced output. For stitched chunks we
    // split the output proportionally to the source-frame split so each beat
    // is scheduled against the correct source range. The proportional split
    // is approximate (the DSP doesn't expose per-sample source correspondence)
    // but with chunks ~23 ms the worst-case timing slip is below human
    // onset precision.
    if let (Some(tail_frames), Some(l)) = (stitch_tail_frames, loop_region) {
        let tail_out = ((tail_frames as f64 * out_written as f64) / in_chunk as f64) as usize;
        let tail_out = tail_out.min(out_written);
        let head_out = out_written - tail_out;
        let split = tail_out * channels;
        let (tail_slice, head_slice) = out_slice.split_at_mut(split);
        metronome.mix_segment(tail_slice, channels, cursor_before, tail_frames as u64);
        if head_out > 0 {
            metronome.mix_segment(
                head_slice,
                channels,
                l.start,
                (in_chunk - tail_frames) as u64,
            );
        }
    } else {
        metronome.mix_segment(out_slice, channels, cursor_before, in_chunk as u64);
    }

    apply_master_gain(out_slice, channels, master_current_gain, master_target_gain);

    output.producer.push_slice(out_slice);

    *cursor = new_cursor;
    state.position.store(*cursor, Ordering::Relaxed);

    if wrapped {
        advance_speed_ramp(speed_ramp, passes_since_step, dsp, metronome, state);
    }
}

/// Called once per loop wrap. If a ramp is configured and we've reached the
/// next step, nudge the speed toward target by one step (clamped) and update
/// `state.speed_bits` so the UI slider follows. Ramps in BPM units are
/// translated via the metronome's current BPM; if the user hasn't dialed a
/// meaningful BPM, the step resolves to zero and we no-op.
fn advance_speed_ramp(
    speed_ramp: &SpeedRampSettings,
    passes_since_step: &mut u32,
    dsp: &mut dyn TimePitchProcessor,
    metronome: &Metronome,
    state: &SharedState,
) {
    if !speed_ramp.enabled {
        return;
    }
    *passes_since_step = passes_since_step.saturating_add(1);
    let needed = speed_ramp.passes_per_step.max(1);
    if *passes_since_step < needed {
        return;
    }
    *passes_since_step = 0;

    let current = f32::from_bits(state.speed_bits.load(Ordering::Relaxed));
    let target = speed_ramp.target_speed;
    if !current.is_finite() || !target.is_finite() {
        return;
    }
    let diff = target - current;
    if diff.abs() < 1e-6 {
        return;
    }
    let delta = step_in_speed_units(speed_ramp, metronome.bpm());
    if delta <= 0.0 {
        return;
    }
    let signed = if diff > 0.0 { delta } else { -delta };
    // Clamp at target so we don't overshoot on the last step.
    let next = if signed > 0.0 {
        (current + signed).min(target)
    } else {
        (current + signed).max(target)
    };
    dsp.set_speed(next);
    state.speed_bits.store(next.to_bits(), Ordering::Relaxed);
}

/// Build the DSP for a track. The selected `DspKind` picks the family;
/// within each family, we try the full pitch-shift composite first
/// (`*PitchShift`), fall back to the speed-only adapter (`*Speed`) if
/// rubato can't be constructed for this channel count (speed slider still
/// works, pitch slider becomes a no-op), and ultimately fall back to
/// `Passthrough` for degenerate channel counts.
fn make_dsp(
    channels: usize,
    kind: DspKind,
    current_speed: f32,
    current_pitch: f32,
) -> Box<dyn TimePitchProcessor> {
    if channels == 0 {
        log::error!("track has 0 channels; using passthrough");
        return Box::new(Passthrough::new());
    }
    match kind {
        DspKind::Wsola => match WsolaPitchShift::new(channels) {
            Ok(mut p) => {
                p.set_speed(current_speed);
                p.set_pitch_semitones(current_pitch);
                Box::new(p)
            }
            Err(e) => {
                log::error!(
                    "WsolaPitchShift failed for {channels} ch: {e:#}; \
                     falling back to WsolaSpeed (no pitch shift)"
                );
                let mut w = WsolaSpeed::new(channels);
                w.set_speed(current_speed);
                Box::new(w)
            }
        },
        DspKind::PhaseVocoder => match PhaseVocoderPitchShift::new(channels) {
            Ok(mut p) => {
                p.set_speed(current_speed);
                p.set_pitch_semitones(current_pitch);
                Box::new(p)
            }
            Err(e) => {
                log::error!(
                    "PhaseVocoderPitchShift failed for {channels} ch: {e:#}; \
                     falling back to PhaseVocoderSpeed (no pitch shift)"
                );
                let mut w = PhaseVocoderSpeed::new(channels);
                w.set_speed(current_speed);
                Box::new(w)
            }
        },
    }
}

/// Apply the master output gain in place. When current and target match (the
/// steady state once the slider settles) we skip the multiplication entirely
/// at unity, or apply a constant scalar otherwise; when they differ we
/// linearly ramp current → target across the segment to mask zipper noise on
/// fast slider drags. `current` is updated to `target` on exit so the next
/// chunk starts where this one ended.
fn apply_master_gain(out: &mut [f32], channels: usize, current: &mut f32, target: f32) {
    let frames = out.len() / channels;
    if frames == 0 {
        return;
    }
    let same = (*current - target).abs() < 1e-7;
    if same {
        if (target - 1.0).abs() < 1e-7 {
            return;
        }
        for s in out.iter_mut() {
            *s *= target;
        }
        return;
    }
    let step = (target - *current) / frames as f32;
    let mut g = *current;
    for f in 0..frames {
        let base = f * channels;
        for s in &mut out[base..base + channels] {
            *s *= g;
        }
        g += step;
    }
    *current = target;
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
