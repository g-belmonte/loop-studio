# Architecture

This document describes how Loop Studio is wired together. It is a living document — when a design choice changes, this file changes with it.

## Goals

1. **Real-time-safe audio**: the audio callback never allocates, never locks for an unbounded time, never decodes from disk.
2. **Responsive GUI**: the UI thread never blocks on decoding, file I/O, or DSP.
3. **Pluggable DSP**: the time-stretch / pitch-shift implementation is behind a trait so we can swap algorithms (in-house WSOLA → community phase-vocoder → Rubber Band/SoundTouch via FFI) without touching the engine or UI.
4. **Deterministic state**: session state (loop, speed, pitch, position) lives in one place and is the only source of truth.

## Threads

```
GUI (egui)  ──Command (crossbeam)──►  Engine worker  ──ringbuf──►  cpal callback
     ▲                                  (DSP runs here)
     └──── SharedState atomics ──────────────┘

App (GUI thread) ──spawns std::thread──► Load-time worker (symphonia + peaks)
                                              │
                                              ▼
                                  Arc<Track> sent back via crossbeam,
                                  then handed to engine via Command::LoadTrack
```

### GUI thread (`eframe`)
- Renders egui widgets every frame.
- Reads playback state (position, RMS, etc.) via lock-free atomics.
- Sends user intent (`LoadTrack`, `Play`, `Pause`, `Stop`, `Seek`, `SetLoop`, `SetSpeed`, `SetPitch`) to the engine over a `crossbeam-channel`.
- Owns no audio data directly — it asks the engine.

### Engine thread
- Owns the DSP stage and the producer half of the ring buffer.
- Pulls commands from the channel; reads source samples from the loaded `Track` (whole-file, decoded up front — see *Decode strategy* below); pushes processed samples into the ring buffer.
- Maintains the playback cursor and applies loop wrap-around at the source.

### Load-time worker (one-shot)
- A `std::thread` spawned by `App` for each file open.
- Decodes the file via `audio::decoder::decode_file`, then computes the GUI's `TrackPeaks` envelope (one min/max pair per `BUCKET_FRAMES` source frames).
- Sends `Arc<Track>` and `Arc<TrackPeaks>` back to the GUI thread; the GUI hands the track to the engine via `Command::LoadTrack` and keeps the peaks for the waveform widget.

> **Status:** engine thread is up (v0.1 step 2) — owns the cpal output stream, the ring producer, the cursor, and a `Box<dyn TimePitchProcessor>` (currently `ResampleSpeed`, with `Passthrough` as fallback when the resampler can't be constructed). Decoding and peak computation live in the load-time worker (v0.1 steps 1 and 3) rather than the engine. They could move into the engine later if streamed decode ever replaces whole-file decode.

### Audio callback (`cpal`)
- The only thread that touches the OS audio device.
- Pops samples from the ring buffer; writes to the output buffer.
- Underrun handling: fill the unfilled tail of the output buffer with zeros (silence). No flag yet — adding one when something needs to react.
- **No allocation, no syscalls, no `.lock()`** in this callback — ever.

## Data flow for one playback frame

1. **Input**: engine has read N source samples from the loaded `Arc<Track>` (post-loop-wrap or loop-stitch).
2. **Time-stretch**: the DSP stage consumes them at rate `1/speed` and emits time-stretched samples at the same pitch.
3. **Pitch-shift**: applied by resampling the time-stretched output to shift pitch (or by an integrated phase-vocoder that does both at once — see DSP section).
4. **Output**: pushed into the ring buffer for the audio callback.

The exact ordering depends on the DSP impl: phase-vocoder approaches do (2) and (3) in one transform; WSOLA-style impls do (2) then resample for (3).

## Module layout

```
src/
├── main.rs              # entry point, env_logger init, eframe::run_native
├── app.rs               # eframe::App impl — top-level UI state + update()
│
├── engine/
│   ├── mod.rs           # public Engine handle: spawn(), commands, state snapshot
│   ├── command.rs       # Command enum sent GUI -> engine
│   ├── state.rs         # SharedState: atomics read by GUI
│   └── worker.rs        # the engine thread loop
│
├── audio/
│   ├── mod.rs
│   ├── output.rs        # cpal stream setup, callback
│   ├── decoder.rs       # symphonia wrapper, returns interleaved f32 frames
│   └── ring.rs          # thin wrapper around `ringbuf` for our sample type
│
├── dsp/
│   ├── mod.rs           # trait TimePitchProcessor
│   ├── passthrough.rs   # baseline: no stretch, no shift (sanity check)
│   ├── resample.rs      # speed-only via rubato (pitch coupled — sanity check)
│   └── wsola.rs         # MVP impl: WSOLA time-stretch + resample for pitch
│
├── track/
│   ├── mod.rs           # Track: decoded samples, sample rate, channels
│   └── peaks.rs         # downsampled min/max for waveform rendering
│
├── session/
│   ├── mod.rs           # Session struct, save() / load() to JSON
│   └── schema.rs        # serde types (versioned)
│
└── ui/
    ├── mod.rs
    ├── transport.rs     # play/pause/seek/speed/pitch widgets
    ├── waveform.rs      # custom egui widget: peaks + playhead + loop region
    └── menu.rs          # file menu, open/save session
```

## Key types (sketch)

```rust
// engine/command.rs
pub enum Command {
    LoadTrack(Arc<Track>),     // engine takes ownership, resets cursor
    Play,
    Pause,
    Stop,                      // pause + reset cursor to 0
    Seek(u64),                 // source frame index
    SetLoop(Option<LoopRegion>),
    SetSpeed(f32),             // 0.25..=2.0
    SetPitch(f32),             // semitones, -12.0..=12.0
}

// track/mod.rs
pub struct LoopRegion { pub start: u64, pub end: u64 } // source-frame indices

// engine/state.rs — read by GUI, written by engine
pub struct SharedState {
    pub position:    AtomicU64,  // current source-frame index
    pub duration:    AtomicU64,  // total source frames
    pub playing:     AtomicBool,
    pub speed_bits:  AtomicU32,  // f32::to_bits()
    pub pitch_bits:  AtomicU32,  // f32::to_bits()
    pub loaded_id:   AtomicU64,  // bumped on each LoadTrack
}

// dsp/mod.rs
pub trait TimePitchProcessor: Send {
    fn set_speed(&mut self, speed: f32);
    fn set_pitch_semitones(&mut self, semitones: f32);
    /// Frames of input the DSP wants per process() call.
    fn input_frames_per_chunk(&self) -> usize;
    /// Upper bound on output frames per process() call across all settings.
    fn max_output_frames_per_chunk(&self) -> usize;
    fn process(&mut self, input: &[f32], output: &mut [f32], channels: usize) -> (usize, usize);
}
```

## DSP

Independent time-stretch and pitch-shift is the hardest part of this project. We have a tiered plan:

1. **Phase 1 — Passthrough** (`dsp/passthrough.rs`). ✅ Done. Wired the engine end-to-end with no DSP. Now serves as a fallback when the resampler can't be constructed.
2. **Phase 2 — Speed-coupled** (`dsp/resample.rs`). ✅ Done in v0.1 step 5. `rubato::SincFixedIn`, chunk = 1024 frames, `max_resample_ratio_relative = 4.0`. Speed slider works; pitch is coupled to speed (turntable).
3. **Phase 3 — WSOLA + resample** (`dsp/wsola.rs`). Implement WSOLA (Waveform-Similarity Overlap-Add) for time-stretch — ~200 lines, works in the time domain, low-latency. Cascade with `rubato` for pitch shift. This is the MVP target. **Subdivided into v0.1 steps 6a and 6b — see plan below.**
4. **Phase 4 — Phase vocoder** (optional). If WSOLA quality isn't enough on harmonic material, add an FFT-based phase vocoder behind the same trait.
5. **Phase 5 — FFI** (optional, license-permitting). Rubber Band or SoundTouch as opt-in features for top-tier quality.

The `TimePitchProcessor` trait keeps the engine ignorant of which phase we're in.

### Step 6 plan (next up — Phase 3, subdivided)

**Step 6a — WSOLA-based speed processor.**
- Implement WSOLA in `dsp/wsola.rs`: time-domain frame extraction, cross-correlation similarity search around the analysis position, Hann-windowed overlap-add. Typical parameters to start: frame size 2048, analysis hop 512, search radius ±256, 50 % overlap. One `Wsola` instance per channel.
- Wrap in a `WsolaSpeed` struct implementing `TimePitchProcessor`. `set_speed(s)` sets `stretch_factor = 1/s`; `set_pitch_semitones` is a no-op (still a placeholder).
- Replace `ResampleSpeed` with `WsolaSpeed` in `engine::worker::make_dsp`.
- Result: existing speed slider preserves pitch — the "vinyl" interim from step 5 is gone. No pitch slider yet.
- Risks: chunk-boundary continuity (engine calls `process()` in 1024-frame chunks; WSOLA's frame size is larger, so internal state must survive across calls); transient smearing on drums; phasiness on sustained tones. Validation is audible only.

**Step 6b — Pitch shift cascade + slider UI.**
- Build a composite DSP that owns a `Wsola` and a `rubato::SincFixedIn`, plumbed back-to-back: WSOLA stretches in time, the resampler shifts pitch by changing playback rate.
- Math (verified): `stretch_factor = 2^(pitch_semitones/12) / speed`; `resample_ratio = 2^(-pitch_semitones/12)`. Both setters recompute both stages. At `(speed=1, pitch=0)` both stages are identity; at `(speed=0.5, pitch=0)` only WSOLA stretches; at `(speed=1, pitch=+12)` WSOLA stretches 2× and the resampler resamples 0.5× (net duration unchanged, pitch up an octave).
- Wire `Command::SetPitch` end-to-end (it's already plumbed but no-op).
- Add a pitch slider to `ui::transport`: range ±12 semitones, "0 st" reset button, sends `Command::SetPitch`.
- Risks: ratio math correctness (test by ear at multiple `(speed, pitch)` combinations); whether `WSOLA::set_stretch_factor` needs ramping to avoid clicks when the user drags either slider — `rubato::set_resample_ratio(_, ramp=true)` already smooths the resampler side.

**Why this split (and not more)**: a "WSOLA without engine integration" sub-step would just be dead code that can't be validated until wired in — quality is only audible after integration. Step 6a is the minimum unit that's both meaningful and testable. Step 6b is mostly composition + UI; the algorithmic risk is contained in 6a.

**Detour clause**: if 6a's audible quality is unacceptable, jump to Phase 4 (FFT phase vocoder) before doing 6b. The trait shape doesn't need to change.

## Why a custom mixer instead of `rodio`

`rodio` models audio as composable `Source`s; that's elegant for game SFX but awkward when you need to (a) change effective playback rate live, (b) loop with sample-accurate boundaries, (c) keep position in *source* samples (not output samples) so the GUI can show where you are in the *track*. Owning the callback lets us treat the source position as the cursor of truth and project everything else from it.

## Session format (v0)

```json
{
  "version": 1,
  "track_path": "/home/me/music/song.flac",
  "track_sample_rate": 44100,
  "loop_region": { "start": 1234567, "end": 2345678 },
  "speed": 0.75,
  "pitch_semitones": -2.0,
  "last_position": 1500000
}
```

Versioned from day one so we can migrate without breaking saved sessions.

## Decisions

- **Decode strategy = whole-file** (decided v0.1 step 1). `audio::decoder::decode_file` reads the entire file into a `Track { samples: Vec<f32>, ... }` of interleaved f32. Cost: ~1.3 GB RAM for a 1-hour 44.1 kHz stereo track. Acceptable for a practice tool that mostly chews on 3–8 minute songs. Revisit if real-world use surfaces long-form pain (audiobooks, full concerts).
- **Output stream rate = track rate when supported, device default otherwise** (decided v0.1 step 2). `audio::output::open` first tries to build an F32 stream at the track's exact rate and channel count; if that fails it falls back to `default_output_config()` and logs a warning that playback rate will be wrong. Resampling for mismatched rates would naturally piggyback on the rubato stage that landed in step 5, but isn't implemented yet — the warning still fires when rates don't match. The stream is reopened on every `LoadTrack` whose rate or channel count differs from the current stream.

  We deliberately do **not** call `device.supported_output_configs()`. On Linux with pipewire-alsa it errors out during the probe ("device no longer available") even though `build_output_stream` against the same device works fine. Try-then-fall-back is more robust than enumerate-then-build on every backend we've encountered.
- **Ring buffer size = 16384 interleaved samples** (decided v0.1 step 2, raised in step 5). About 170 ms at 48 kHz stereo. The original 8192 was right when the DSP was passthrough but became a deadlock once `ResampleSpeed` arrived: with `max_resample_ratio_relative = 4.0` and `chunk = 1024`, the resampler's worst-case output is 4096 frames = 8192 stereo samples, so an 8192-sample ring could only ever be refilled when fully empty. Doubling the ring gives the engine room to refill while the callback is still draining. The engine wakes every 2 ms.
- **Channels must match track ↔ output** (decided v0.1 step 2). Mono-on-stereo upmixing and arbitrary downmixing are deferred. If the device has no config matching the track's channel count, `output::open` falls back to the default and logs a warning; the resulting layout mismatch will sound wrong. Address this if a real file in real use trips it.
- **A/B loop interaction = drag to define, click to seek** (decided v0.1 step 4). On the waveform widget, a plain click seeks; a click-and-drag defines the loop region from press to release (auto-ordered so `start < end`). `drag_stopped` wins over `clicked` when both could fire. A "Clear loop" button appears below the waveform when a loop is active. Considered and rejected: modal "Set A / Set B" buttons (more clicks, hidden state) and modifier-clicks (poor discoverability). Keyboard shortcuts (`[`/`]`) are explicit v0.2 scope.
- **Cursor stays inside the loop** (decided v0.1 step 4). When a loop is active and the cursor would otherwise sit outside it (after `SetLoop`, after `Seek`, or any future code path), the engine snaps it to `loop.start`. Implemented as `engine::worker::snap_into_loop`, called from the `SetLoop` and `Seek` command handlers and re-checked at the top of `produce()` as defence in depth. Rationale: the user's mental model when they create a loop is "this region, on repeat, starting now" — playing-in from outside the loop violates that. Side effect: clicking outside an active loop snaps the playhead to `loop.start` rather than where you clicked; clear the loop first if you want to seek out.
- **Chunk-aware engine ↔ DSP contract** (decided v0.1 step 5). The DSP defines a fixed `input_frames_per_chunk()`, an upper bound `max_output_frames_per_chunk()`, and an `expected_output_frames_per_chunk()` that reflects the *current* ratio. `produce()` only calls `dsp.process()` once `in_chunk` source frames are available *and* the ring has `expected_output * channels` vacancy. Scratch is sized to the worst case (`max_output * channels`) so a mid-process ratio change can't overflow it; the vacancy check uses the expected output so the engine isn't over-conservative when the resampler is at unity. Cursor advances by the consumed input count, not the produced output. Chunk = 1024 frames (~23 ms at 44.1 kHz) for both passthrough and the rubato resampler.
- **DSP recreated per `LoadTrack` when channel count changes** (decided v0.1 step 5). Rubato's `SincFixedIn` is constructed with a fixed channel count, so we can't reuse it across mono ↔ stereo loads. The engine builds a new `ResampleSpeed` (and resizes the scratch buffer) only when the channel count actually changes — same-channel reloads keep the existing DSP and the user's current speed setting. The current speed is also re-applied to the new DSP so the slider value carries over across track changes.
- **End-of-track tail is dropped; loop boundaries are stitched** (decided v0.1 step 5). When fewer than `in_chunk` source frames remain before the boundary:
  - **No loop active** → drop the last <23 ms, snap cursor to `total_frames`, the run loop's EOF check fires.
  - **Loop active and `loop_length >= in_chunk`** → assemble the chunk's input from `cursor..loop.end` plus the head of `loop.start..` into a pre-allocated `stitch_buf`; advance the cursor to `loop.start + head_frames`. Loop wraps stay gap-free and the DSP still gets a clean fixed-size chunk.
  - **Loop active and `loop_length < in_chunk`** → produce silence and bail. Sub-23-ms loops aren't supported in v0.1.

  Without stitching, loop wrapping was broken whenever `loop_length` wasn't an exact multiple of `in_chunk`: the cursor stuck `loop_length % in_chunk` frames before `loop.end` and the snap-back at the top of `produce()` never fired. Pre-step-5 code didn't have this problem because it pushed any size, but the chunk-aware DSP can't.
- **Loop state lives in `App`, not `SharedState`** (decided v0.1 step 4). The user creates loops in the UI, so `App` is the natural source of truth; `Command::SetLoop` is fire-and-forget to the engine. Adding loop atomics to `SharedState` would just create a second copy that's always one frame behind. Reconsider if anything other than the engine ever needs to *read* the engine's effective loop region.

## Open questions

- **Ring flush on seek**: ringbuf has no producer-side flush, so after a `Seek` the ~85 ms of audio already in the ring still plays before the new position is heard. Acceptable today but if it becomes annoying we'll need an epoch protocol (callback drops samples it sees as stale) or a stream-restart trick. For now, keep the ring small.
- **Seek-while-playing slider jitter**: the seek slider re-binds to the engine's `position` every frame, so dragging while playing produces visible micro-stepping (pointer says X, next frame engine says X+Δ). Plan: when the slider response reports `dragged()`, freeze the displayed value to the user's pointer until release. Cheap fix, deferred to v0.2.
- **Phase vocoder crate**: survey current ecosystem at the start of Phase 4. Candidates: `phase-vocoder`, `signalsmith-stretch` bindings (if they exist), or roll our own.
