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

> **Status:** engine thread is up (v0.1 step 2) — owns the cpal output stream, the ring producer, the cursor, and a `Box<dyn TimePitchProcessor>` (currently `WsolaPitchShift` after step 6b; falls back to `WsolaSpeed` if rubato construction fails, then `Passthrough` for degenerate channel counts). Decoding and peak computation live in the load-time worker (v0.1 steps 1 and 3) rather than the engine. They could move into the engine later if streamed decode ever replaces whole-file decode.

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
│   ├── passthrough.rs   # baseline: no stretch, no shift (fallback)
│   └── wsola.rs         # MVP impl: WSOLA time-stretch + rubato cascade for pitch
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

1. **Phase 1 — Passthrough** (`dsp/passthrough.rs`). ✅ Done. Wired the engine end-to-end with no DSP. Now serves as a fallback when DSP construction is degenerate (e.g. zero-channel track).
2. **Phase 2 — Speed-coupled** (was `dsp/resample.rs`). ✅ Done in v0.1 step 5; **removed in step 6a** when `WsolaSpeed` replaced it. Used `rubato::SincFixedIn` with chunk = 1024 and `max_resample_ratio_relative = 4.0`. Speed slider worked but pitch was coupled to speed (turntable). Step 6b reintroduced a `rubato::SincFixedIn` *inside the WSOLA composite* (`WsolaPitchShift`) — same crate, no longer standalone.
3. **Phase 3 — WSOLA + resample** (`dsp/wsola.rs`). Time-domain WSOLA for time-stretch — pure-Rust, low-latency. Cascade with `rubato` for pitch shift. This is the MVP target. **Subdivided into v0.1 steps 6a and 6b — see plan below.** ✅ 6a done (`WsolaSpeed`); ✅ 6b done — `WsolaPitchShift` composite drives the engine, and the pitch slider is live in `ui::transport`.
4. **Phase 4 — Phase vocoder** (optional). If WSOLA quality isn't enough on harmonic material, add an FFT-based phase vocoder behind the same trait.
5. **Phase 5 — FFI** (optional, license-permitting). Rubber Band or SoundTouch as opt-in features for top-tier quality.

The `TimePitchProcessor` trait keeps the engine ignorant of which phase we're in.

### Step 6 plan (Phase 3, subdivided)

**Step 6a — WSOLA-based speed processor.** ✅ Done.
- Implemented WSOLA in `dsp/wsola.rs`: time-domain frame extraction, cross-correlation similarity search around the analysis position, Hann-windowed overlap-add. Parameters: frame size 2048, synthesis hop 512 (75 % overlap → COLA with Hann), search radius ±256. One shared `Wsola` per track that handles all channels in lock-step (single δ across channels keeps the stereo image coherent).
- Wrapped in a `WsolaSpeed` struct implementing `TimePitchProcessor`. `set_speed(s)` sets `stretch = 1/s`; `set_pitch_semitones` is still a no-op placeholder.
- `WsolaSpeed` replaced `ResampleSpeed` in `engine::worker::make_dsp`. WSOLA construction is infallible, so the fallback path (Passthrough) is now only used for degenerate channel counts.
- Result: speed slider preserves pitch — the "vinyl" interim from step 5 is gone. No pitch slider yet.
- Chunk-boundary continuity: the engine still calls `process()` with `input_frames_per_chunk() = 1024`; `Wsola` ingests those into per-channel growing buffers and runs as many synthesis steps as the buffer permits, queuing output per-channel for drainage. State (synth_tail, nat_ref, analysis_pos, in_buf) survives across calls. `analysis_pos` is trimmed from the front of `in_buf` once it crosses 16384 frames.
- Output sizing: `max_output_frames_per_chunk = 4096 + 2·SYNTHESIS_HOP = 5120` (worst case at stretch = 4 plus one hop of slack); `expected_output_frames_per_chunk = ⌈ENGINE_CHUNK · stretch⌉` to give the engine a useful ring-vacancy estimate.
- Risks (still open until 6b): transient smearing on drums; phasiness on sustained tones — audible-only verification. If quality is unacceptable, take the detour clause below.

**Step 6b — Pitch shift cascade + slider UI.** ✅ Done.
- Composite `WsolaPitchShift` owns a `Wsola` and a `rubato::SincFixedIn`, plumbed back-to-back: WSOLA stretches in time, the resampler shifts pitch by changing playback rate. Engine builds it via `make_dsp`; falls back to `WsolaSpeed` if rubato construction fails (speed slider still works, pitch slider would no-op), and to `Passthrough` for degenerate channel counts.
- Math (in `WsolaPitchShift::recompute`): `pitch_factor = 2^(p/12)`; `stretch = pitch_factor / speed` → WSOLA; `resample_ratio = 1 / pitch_factor` → rubato. Net composite ratio = `1/speed` regardless of pitch. Both `set_speed` and `set_pitch_semitones` recompute both stages. Verified corners: `(speed=1, pitch=0)` → stretch = 1, ratio = 1 (identity); `(speed=0.5, pitch=0)` → stretch = 2, ratio = 1 (only WSOLA stretches); `(speed=1, pitch=+12)` → stretch = 2, ratio = 0.5 (WSOLA 2×, rubato 0.5× — net duration unchanged, pitch up an octave).
- Resampler config: `SincFixedIn`, chunk = 1024 frames, `max_resample_ratio_relative = 4.0` (initial ratio = 1.0 → runtime range [0.25, 4.0]; ±12 semitones only needs [0.5, 2.0], headroom for future widening). `set_resample_ratio(_, ramp=true)` smooths the resampler side; WSOLA stretch is currently set without ramping.
- Pipeline per `process()`: feed engine chunk to WSOLA → synthesise up to `ENGINE_CHUNK × stretch` frames → drain WSOLA output (planar) into `wsola_drain` → run resampler in 1024-frame chunks while `wsola_drain` has enough → append resampler output to `out_queue` → drain `out_queue` interleaved into the engine's scratch. `wsola_drain` and `out_queue` carry leftover state across calls (variable-rate I/O on both sides of the cascade requires it).
- WSOLA `MAX_STRETCH` raised from 4 to 8 to accommodate `(speed=0.25, pitch=+12) → stretch = 8`. The composite reports the same `max_output_frames_per_chunk` as `WsolaSpeed` because the *net* ratio never exceeds 4 (composite_ratio = 1/speed_min); WSOLA can transiently produce more inside the pipeline, but those frames are consumed by the resampler before crossing the engine boundary.
- Pitch slider in `ui::transport`: linear range ±12 semitones (perceptually equal steps), "0 st" reset button, sends `Command::SetPitch`. Paired with the existing logarithmic speed slider; both follow the same shape (slider + reset).
- Risks (still open until audible verification): ratio math correctness across multiple `(speed, pitch)` combinations; whether WSOLA's stretch change needs ramping to avoid clicks during slider drags (`rubato::set_resample_ratio(_, ramp=true)` already smooths the resampler side). Detour clause: if quality is unacceptable on harmonic material, jump to Phase 4 (FFT phase vocoder) — the trait shape doesn't need to change.

### Step 6c plan — WSOLA quality pass (queued)

Early audible testing of step 6b surfaced two issues we deferred for a focused follow-up:
- Pitch-shift quality is asymmetric — pitching down is mostly OK, pitching up is noticeably distorted on harmonic content.
- Audible clicks appear when sliders are at non-default settings, especially during drags.

Three targeted fixes, each independently testable:

1. **Similarity search → AMDF.** Replace the raw cross-correlation in `Wsola::step` with AMDF (`Σ |nat_ref[i] − in_buf[start+i]|`, minimised over δ). Raw correlation is biased toward high-energy regions of the input and picks poor frame alignments on tonal content; AMDF normalises against the local envelope without the cost of full normalised cross-correlation. Also worth considering: search over a sum-of-channels mono mix instead of channel 0 only, so stereo content with diverging channels doesn't mis-steer the search.
2. **OLA gain compensation.** Symmetric Hann at hop = N/4 has a COLA sum ≈ 1.75, so the OLA output is ~5 dB hot and clips on loud sources. Divide the precomputed `window` by the COLA constant in `Wsola::new` so identity (stretch = 1) is unity-gain. Cheap, definitively correct, and explains why clipping might appear at any non-passthrough setting.
3. **Stretch ramping.** Currently `Wsola::set_stretch` swaps `analysis_hop` instantly while `rubato::set_resample_ratio(_, ramp=true)` already smooths the resampler side. The asymmetry produces phase glitches when the user drags a slider. Fix: store both `current_analysis_hop` and `target_analysis_hop`, and step `current` toward `target` by a bounded delta per synthesis step.

Risks: each fix is local and reversible; AMDF is the highest-impact and the most likely to also subtly change perceived character (some material may sound different, not strictly better). Validation is audible only — exercise the pitch-up direction on solo voice or sustained tones, and drag both sliders during playback to listen for clicks.

Detour clause unchanged: if 6c isn't enough on harmonic material, the trait shape supports a drop-in replacement with Phase 4 (FFT phase vocoder).

v0.1's session save/load shipped before 6c — 6c is a quality polish on existing features, not a blocker for shipping v0.1.

## Why a custom mixer instead of `rodio`

`rodio` models audio as composable `Source`s; that's elegant for game SFX but awkward when you need to (a) change effective playback rate live, (b) loop with sample-accurate boundaries, (c) keep position in *source* samples (not output samples) so the GUI can show where you are in the *track*. Owning the callback lets us treat the source position as the cursor of truth and project everything else from it.

## Session format (v1)

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
- **Output stream rate = track rate when supported, device default otherwise** (decided v0.1 step 2). `audio::output::open` first tries to build an F32 stream at the track's exact rate and channel count; if that fails it falls back to `default_output_config()` and logs a warning that playback rate will be wrong. Resampling for mismatched rates could piggyback on the rubato resampler now living inside `WsolaPitchShift`, but isn't implemented yet — the warning still fires when rates don't match. The stream is reopened on every `LoadTrack` whose rate or channel count differs from the current stream.

  We deliberately do **not** call `device.supported_output_configs()`. On Linux with pipewire-alsa it errors out during the probe ("device no longer available") even though `build_output_stream` against the same device works fine. Try-then-fall-back is more robust than enumerate-then-build on every backend we've encountered.
- **Ring buffer size = 16384 interleaved samples** (decided v0.1 step 2, raised in step 5). About 170 ms at 48 kHz stereo. The original 8192 was right when the DSP was passthrough but became a deadlock once `ResampleSpeed` arrived: with `max_resample_ratio_relative = 4.0` and `chunk = 1024`, the resampler's worst-case output is 4096 frames = 8192 stereo samples, so an 8192-sample ring could only ever be refilled when fully empty. Doubling the ring gives the engine room to refill while the callback is still draining. The engine wakes every 2 ms.
- **Channels must match track ↔ output** (decided v0.1 step 2). Mono-on-stereo upmixing and arbitrary downmixing are deferred. If the device has no config matching the track's channel count, `output::open` falls back to the default and logs a warning; the resulting layout mismatch will sound wrong. Address this if a real file in real use trips it.
- **A/B loop interaction = drag to define, click to seek** (decided v0.1 step 4). On the waveform widget, a plain click seeks; a click-and-drag defines the loop region from press to release (auto-ordered so `start < end`). `drag_stopped` wins over `clicked` when both could fire. A "Clear loop" button appears below the waveform when a loop is active. Considered and rejected: modal "Set A / Set B" buttons (more clicks, hidden state) and modifier-clicks (poor discoverability). Keyboard shortcuts (`[`/`]`) are explicit v0.2 scope.
- **Cursor stays inside the loop** (decided v0.1 step 4). When a loop is active and the cursor would otherwise sit outside it (after `SetLoop`, after `Seek`, or any future code path), the engine snaps it to `loop.start`. Implemented as `engine::worker::snap_into_loop`, called from the `SetLoop` and `Seek` command handlers and re-checked at the top of `produce()` as defence in depth. Rationale: the user's mental model when they create a loop is "this region, on repeat, starting now" — playing-in from outside the loop violates that. Side effect: clicking outside an active loop snaps the playhead to `loop.start` rather than where you clicked; clear the loop first if you want to seek out.
- **Chunk-aware engine ↔ DSP contract** (decided v0.1 step 5). The DSP defines a fixed `input_frames_per_chunk()`, an upper bound `max_output_frames_per_chunk()`, and an `expected_output_frames_per_chunk()` that reflects the *current* ratio. `produce()` only calls `dsp.process()` once `in_chunk` source frames are available *and* the ring has `expected_output * channels` vacancy. Scratch is sized to the worst case (`max_output * channels`) so a mid-process ratio change can't overflow it; the vacancy check uses the expected output so the engine isn't over-conservative when the DSP is producing close to 1:1. Cursor advances by `in_chunk` regardless of what the DSP returned for `_in_used`. Chunk = 1024 frames (~23 ms at 44.1 kHz) for `Passthrough`, `WsolaSpeed`, and `WsolaPitchShift`.
- **DSP recreated per `LoadTrack` when channel count changes** (decided v0.1 step 5, rationale carried into step 6a/6b). The DSP holds per-channel state — `WsolaSpeed` allocates per-channel input buffers, OLA tails, and natural-progression references; `WsolaPitchShift` adds a `rubato::SincFixedIn` whose channel count is also fixed at construction plus per-channel `wsola_drain` and `out_queue` buffers. Either way, mono ↔ stereo reloads can't reuse the existing instance. The engine builds a new DSP (and resizes the scratch buffer) only when the channel count actually changes — same-channel reloads keep the existing DSP and the user's current speed and pitch settings. Both speed and pitch are re-applied to the new DSP so the slider values carry over across track changes.
- **End-of-track tail is dropped; loop boundaries are stitched** (decided v0.1 step 5). When fewer than `in_chunk` source frames remain before the boundary:
  - **No loop active** → drop the last <23 ms, snap cursor to `total_frames`, the run loop's EOF check fires.
  - **Loop active and `loop_length >= in_chunk`** → assemble the chunk's input from `cursor..loop.end` plus the head of `loop.start..` into a pre-allocated `stitch_buf`; advance the cursor to `loop.start + head_frames`. Loop wraps stay gap-free and the DSP still gets a clean fixed-size chunk.
  - **Loop active and `loop_length < in_chunk`** → produce silence and bail. Sub-23-ms loops aren't supported in v0.1.

  Without stitching, loop wrapping was broken whenever `loop_length` wasn't an exact multiple of `in_chunk`: the cursor stuck `loop_length % in_chunk` frames before `loop.end` and the snap-back at the top of `produce()` never fired. Pre-step-5 code didn't have this problem because it pushed any size, but the chunk-aware DSP can't.
- **Loop state lives in `App`, not `SharedState`** (decided v0.1 step 4). The user creates loops in the UI, so `App` is the natural source of truth; `Command::SetLoop` is fire-and-forget to the engine. Adding loop atomics to `SharedState` would just create a second copy that's always one frame behind. Reconsider if anything other than the engine ever needs to *read* the engine's effective loop region.
- **WSOLA streams via per-channel growing input buffers** (decided v0.1 step 6a). The engine hands `Wsola` 1024 input frames per `process()` call, but the WSOLA frame size is 2048 + ±256 search radius. We accumulate input into per-channel `Vec<f32>` buffers and run synthesis steps until the buffer can't support the next step; output is queued per channel and drained on each call. State that survives across calls: `analysis_pos` (frame index in `in_buf`), `synth_tail` (OLA accumulator), `nat_ref` (cross-correlation target for the next step). `in_buf` is drained from the front once `analysis_pos` exceeds 16 384 frames so it doesn't grow unbounded. Considered and rejected: re-running the WSOLA loop synchronously per call without buffering — would force a smaller frame size and degrade quality.
- **Single δ across channels** (decided v0.1 step 6a). The cross-correlation similarity search runs on channel 0 only and the chosen δ is applied to *all* channels' frame extraction. Per-channel δ would smear stereo image (left and right shifting independently in time). The cost is that mono content on one channel can mis-steer the search on a track where the channels diverge, but for typical music this is acceptable.
- **WSOLA → resampler cascade for independent pitch** (decided v0.1 step 6b). WSOLA does the time-stretch and rubato handles the duration-preserving pitch shift, with `stretch = 2^(p/12) / speed` and `resample_ratio = 2^(-p/12)`. Considered and rejected: a single integrated phase vocoder (Phase 4) — adds an FFT dependency and we want to validate WSOLA quality first; doing pitch shift inside WSOLA via interpolated frame extraction — couples the algorithms in a way that's harder to swap out for Phase 4 later. The cascade keeps each stage understandable and lets us replace either independently.
- **DSP fallback chain** (decided v0.1 step 6b). `make_dsp` tries `WsolaPitchShift` (full speed + pitch) → `WsolaSpeed` (speed only, pitch slider no-ops) → `Passthrough` (degenerate channel count). Only the rubato construction can realistically fail; if it does, dropping to `WsolaSpeed` keeps the speed slider working — better than degrading all the way to passthrough. The user sees a log warning, not a silent feature loss.
- **Session restore is deferred until decode lands** (decided v0.1 step 7). `App::load_session` parses the JSON, stores a `PendingRestore` (track path + loop / speed / pitch / last position), and triggers the existing async decode. `drain_decode_results` consumes the `PendingRestore` once the matching `LoadResult` arrives, sending `LoadTrack` followed by `SetSpeed`, `SetPitch`, `SetLoop`, and `Seek` (in that order so the engine's "cursor stays inside the loop" invariant resolves correctly). The pending state is keyed by the track path, so a different file landing first discards it; a decode failure also drops the pending state to avoid replaying it on a later open. Considered and rejected: blocking the GUI on decode (kills responsiveness) and pre-decoding into a hidden cache (extra complexity for no UX gain).
- **Lenient session loading at the boundary** (decided v0.1 step 7). Saved sessions are validated only where the data could actually be wrong: the loop region is clamped to `[0, track_frame_count)` and dropped if degenerate (`start ≥ end` after clamping); `last_position` is clamped to track length; speed and pitch are passed through to the engine, which clamps them itself. The `version` field is checked strictly — any value other than `Session::CURRENT_VERSION = 1` errors out. A missing track file surfaces through the existing decode-failure path (`LoadStatus::Failed`) and clears the pending restore. JSON parse errors and write failures bubble up to a `session_error` line in the UI.

## Open questions

- **Ring flush on seek**: ringbuf has no producer-side flush, so after a `Seek` the ~85 ms of audio already in the ring still plays before the new position is heard. Acceptable today but if it becomes annoying we'll need an epoch protocol (callback drops samples it sees as stale) or a stream-restart trick. For now, keep the ring small.
- **Seek-while-playing slider jitter**: the seek slider re-binds to the engine's `position` every frame, so dragging while playing produces visible micro-stepping (pointer says X, next frame engine says X+Δ). Plan: when the slider response reports `dragged()`, freeze the displayed value to the user's pointer until release. Cheap fix, deferred to v0.2.
- **Phase vocoder crate**: survey current ecosystem at the start of Phase 4. Candidates: `phase-vocoder`, `signalsmith-stretch` bindings (if they exist), or roll our own.
