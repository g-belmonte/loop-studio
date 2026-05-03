# Architecture

This document describes how Loop Studio is wired together. It is a living document — when a design choice changes, this file changes with it.

## Goals

1. **Real-time-safe audio**: the audio callback never allocates, never locks for an unbounded time, never decodes from disk.
2. **Responsive GUI**: the UI thread never blocks on decoding, file I/O, or DSP.
3. **Pluggable DSP**: the time-stretch / pitch-shift implementation is behind a trait so we can swap algorithms (in-house WSOLA → community phase-vocoder → Rubber Band/SoundTouch via FFI) without touching the engine or UI.
4. **Deterministic state**: session state (loop, speed, pitch, position) lives in one place and is the only source of truth.

## Threads

```
┌──────────────┐    commands     ┌─────────────┐  decoded   ┌──────────┐ samples ┌────────────┐
│  GUI thread  │ ───────────────▶│  Engine     │──frames───▶│   DSP    │────────▶│   Audio    │
│  (egui)      │   (crossbeam)   │  thread     │            │  stage   │   ring  │  callback  │
│              │◀───── state ────│             │            │ (stretch │  buffer │   (cpal)   │
└──────────────┘   (Arc<Atomic>) │             │            │  + pitch)│         └────────────┘
                                 └─────────────┘            └──────────┘
                                       │
                                       ▼
                                 ┌─────────────┐
                                 │ Symphonia   │
                                 │  decoder    │
                                 └─────────────┘
```

### GUI thread (`eframe`)
- Renders egui widgets every frame.
- Reads playback state (position, RMS, etc.) via lock-free atomics.
- Sends user intent (`Load`, `Play`, `Pause`, `Seek`, `SetLoop`, `SetSpeed`, `SetPitch`) to the engine over a `crossbeam-channel`.
- Owns no audio data directly — it asks the engine.

### Engine thread
- Owns the DSP stage and the producer half of the ring buffer.
- Pulls commands from the channel; reads source samples from the loaded `Track` (whole-file, decoded up front — see *Decode strategy* below); pushes processed samples into the ring buffer.
- Maintains the playback cursor and applies loop wrap-around at the source.
- Pre-computes a downsampled **peaks array** when a track loads (one min/max pair per N source frames) and shares it with the GUI via `Arc<TrackPeaks>`.

> **Status:** the engine thread does not exist yet. v0.1 step 1 (file open) decodes on a one-shot `std::thread` spawned from the GUI and stores the resulting `Track` in `App`. Ownership moves to the engine in step 2 when the cpal output path lands.

### Audio callback (`cpal`)
- The only thread that touches the OS audio device.
- Pops samples from the ring buffer; writes to the output buffer.
- Underrun handling: emit silence + set an atomic flag the engine watches.
- **No allocation, no syscalls, no `.lock()`** in this callback — ever.

## Data flow for one playback frame

1. **Input**: engine has read N source samples from `symphonia` (post-loop-wrap).
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
    Load(PathBuf),
    Play,
    Pause,
    Seek(u64),                 // sample index in source
    SetLoop(Option<LoopRegion>),
    SetSpeed(f32),             // 0.25..=2.0
    SetPitch(f32),             // semitones, -12.0..=12.0
}

// track/mod.rs
pub struct LoopRegion { pub start: u64, pub end: u64 } // source-sample indices

// engine/state.rs — read by GUI, written by engine
pub struct SharedState {
    pub position:   AtomicU64,  // current source-sample index
    pub duration:   AtomicU64,
    pub playing:    AtomicBool,
    pub speed:      AtomicU32,  // f32 bits
    pub pitch:      AtomicU32,
    pub loaded_id:  AtomicU64,  // bump when a new track is ready
}

// dsp/mod.rs
pub trait TimePitchProcessor: Send {
    fn set_speed(&mut self, speed: f32);
    fn set_pitch_semitones(&mut self, semitones: f32);
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize);
    // returns (input consumed, output written)
}
```

## DSP

Independent time-stretch and pitch-shift is the hardest part of this project. We have a tiered plan:

1. **Phase 1 — Passthrough** (`dsp/passthrough.rs`). Wire the engine end-to-end with no DSP. Confirms decode → ring buffer → callback works.
2. **Phase 2 — Speed-coupled** (`dsp/resample.rs`). Use `rubato` to resample on the fly: changes speed *and* pitch together. Validates the resampler integration and the variable-rate consumer pattern.
3. **Phase 3 — WSOLA + resample** (`dsp/wsola.rs`). Implement WSOLA (Waveform-Similarity Overlap-Add) for time-stretch — ~200 lines, works in the time domain, low-latency. Cascade with `rubato` for pitch shift. This is the MVP target.
4. **Phase 4 — Phase vocoder** (optional). If WSOLA quality isn't enough on harmonic material, add an FFT-based phase vocoder behind the same trait.
5. **Phase 5 — FFI** (optional, license-permitting). Rubber Band or SoundTouch as opt-in features for top-tier quality.

The `TimePitchProcessor` trait keeps the engine ignorant of which phase we're in.

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

## Open questions

- **Phase vocoder crate**: survey current ecosystem at the start of Phase 4. Candidates: `phase-vocoder`, `signalsmith-stretch` bindings (if they exist), or roll our own.
- **Backpressure**: how big is the ring buffer? Start with 200 ms at 48 kHz stereo; tune once we observe underruns.
