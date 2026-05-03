use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError};
use ringbuf::traits::{Observer, Producer};

use crate::audio::output::{self, ActiveOutput};
use crate::dsp::TimePitchProcessor;
use crate::dsp::passthrough::Passthrough;
use crate::engine::Command;
use crate::engine::state::SharedState;
use crate::track::{LoopRegion, Track};

/// How often the worker wakes to refill the ring when nothing else triggers it.
/// Short enough that the ring never drains (8192 samples ≈ 85 ms at 48 kHz);
/// long enough to avoid burning a core.
const TICK: Duration = Duration::from_millis(2);

/// Maximum frames the worker pushes per iteration. Bounds latency between
/// commands arriving and being acted on (we drain the channel between batches).
const MAX_FRAMES_PER_TICK: usize = 1024;

pub fn run(rx: Receiver<Command>, state: Arc<SharedState>) {
    let mut track: Option<Arc<Track>> = None;
    let mut cursor: u64 = 0;
    let mut playing = false;
    let mut loop_region: Option<LoopRegion> = None;
    let mut output: Option<ActiveOutput> = None;
    let mut dsp: Box<dyn TimePitchProcessor> = Box::new(Passthrough::new());

    loop {
        // Drain pending commands first — keeps response time tight.
        let disconnected = drain_commands(
            &rx,
            &mut track,
            &mut cursor,
            &mut playing,
            &mut loop_region,
            &mut output,
            &mut *dsp,
            &state,
        );
        if disconnected {
            break;
        }

        if playing {
            if let (Some(t), Some(out)) = (&track, &mut output) {
                produce(t, &mut cursor, loop_region, out, &state);

                // End-of-track stop (only when not looping).
                if loop_region.is_none() && cursor >= t.frame_count() {
                    playing = false;
                    state.playing.store(false, Ordering::Relaxed);
                }
            }
        }

        std::thread::sleep(TICK);
    }
}

fn drain_commands(
    rx: &Receiver<Command>,
    track: &mut Option<Arc<Track>>,
    cursor: &mut u64,
    playing: &mut bool,
    loop_region: &mut Option<LoopRegion>,
    output: &mut Option<ActiveOutput>,
    dsp: &mut dyn TimePitchProcessor,
    state: &SharedState,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(cmd) => apply(cmd, track, cursor, playing, loop_region, output, dsp, state),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

fn apply(
    cmd: Command,
    track: &mut Option<Arc<Track>>,
    cursor: &mut u64,
    playing: &mut bool,
    loop_region: &mut Option<LoopRegion>,
    output: &mut Option<ActiveOutput>,
    dsp: &mut dyn TimePitchProcessor,
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
                    Err(e) => {
                        log::error!("failed to open output: {e:#}");
                    }
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
                *cursor = pos.min(t.frame_count());
                state.position.store(*cursor, Ordering::Relaxed);
            }
        }
        Command::SetLoop(region) => {
            *loop_region = region;
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

/// Push as much source audio as fits in the ring this tick.
///
/// Passthrough-shaped: source frames map 1:1 to output frames. Real DSP will
/// be inserted here once the trait does meaningful work.
fn produce(
    track: &Track,
    cursor: &mut u64,
    loop_region: Option<LoopRegion>,
    output: &mut ActiveOutput,
    state: &SharedState,
) {
    let channels = track.channels as usize;
    let total_frames = track.frame_count();

    // Loop wrap if cursor is at or past loop end (covers seek-into-no-mans-land).
    if let Some(l) = loop_region {
        if *cursor >= l.end {
            *cursor = l.start;
        }
    }

    if *cursor >= total_frames {
        return;
    }

    // How many frames the ring can accept right now.
    let vacant_samples = output.producer.vacant_len();
    if vacant_samples < channels {
        return;
    }
    let max_frames_by_ring = vacant_samples / channels;

    // How many frames before we hit a boundary (loop end or end of track).
    let boundary = match loop_region {
        Some(l) if *cursor < l.end => l.end,
        _ => total_frames,
    };
    let max_frames_by_source = (boundary - *cursor) as usize;

    let frames = max_frames_by_ring
        .min(max_frames_by_source)
        .min(MAX_FRAMES_PER_TICK);
    if frames == 0 {
        return;
    }

    let start_sample = (*cursor as usize) * channels;
    let end_sample = start_sample + frames * channels;
    let pushed_samples = output
        .producer
        .push_slice(&track.samples[start_sample..end_sample]);

    let pushed_frames = (pushed_samples / channels) as u64;
    *cursor += pushed_frames;
    state.position.store(*cursor, Ordering::Relaxed);
}
