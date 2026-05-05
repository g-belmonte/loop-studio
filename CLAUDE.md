# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Read this first

`ARCHITECTURE.md` is the source of truth for the threading model, the engine ↔ DSP contract, and every non-obvious design decision (each one carries a "decided v0.1 step N" tag and the reasoning behind it). When you change behaviour that contradicts something in there — update both the code and the doc in the same change.

`README.md` tracks the roadmap as a checkbox list (v0.1 is closed; new work lands in v0.2). Tick the box in the same change that implements the item.

## Next planned work

v0.1 is closed and the first v0.2 thread (step 6c — WSOLA quality pass) is in: AMDF similarity search over a sum-of-channels mono mix, unity-gain OLA, and ramped stretch transitions. Audible verification of 6c is still owed — exercise pitch-up on solo voice or sustained tones, drag both sliders during playback to confirm the clicks are gone. If quality isn't there, the detour clause stands: jump to Phase 4 (FFT phase vocoder).

What's queued for the rest of v0.2:

- Keyboard shortcuts (space, arrows, `[`/`]` for loop points).
- Markers / cue points.
- Per-track session auto-save.
- Waveform zoom + scroll.
- Cents-level pitch nudge.

Ask the user which thread they want before starting — the order isn't determined.

## Commands

```sh
cargo run                                # debug build, native window
cargo run --release                      # use this for any audio-quality assessment
cargo check --message-format short       # fastest feedback loop while iterating
```

There are no tests yet (`cargo test` does nothing useful). UI behaviour is verified by running the app — say so explicitly when you can't run it from your session.

## Linux runtime requirement

cpal goes through ALSA. On a PipeWire desktop, the ALSA `default` device only works if `pipewire-alsa` is installed (it ships `/usr/share/alsa/alsa.conf.d/99-pipewire-default.conf` to redirect ALSA → PipeWire). Without it you'll see `snd_pcm_open failed: No such file or directory` and the engine can't open a stream. On Arch:

```sh
sudo pacman -S pipewire-alsa alsa-lib libxkbcommon
```

We deliberately do **not** call `device.supported_output_configs()` — pipewire-alsa errors out during that probe even though `build_output_stream` against the same device works fine. `audio::output::open` tries the desired config first and falls back to `default_output_config()` on failure. Don't reintroduce enumeration.

## Big-picture architecture

Three live threads + one one-shot per file load:

```
GUI (egui)  ──Command (crossbeam)──►  Engine worker  ──ringbuf──►  cpal callback
     ▲                                       │
     └──── SharedState atomics ──────────────┘

App (GUI thread) ──spawns std::thread──► Load-time worker (decode + peaks)
                                              │
                                              ▼
                                  Arc<Track> sent back via crossbeam,
                                  then handed to engine via Command::LoadTrack
```

- **GUI thread** (`src/app.rs`, `src/ui/`): renders egui, owns the loop region (UI source of truth), reads playback position from `SharedState` atomics, sends `Command`s to the engine.
- **Engine worker** (`src/engine/worker.rs`): owns the cpal `Stream`, the ring producer, the playback cursor, and a `Box<dyn TimePitchProcessor>`. Drains commands every tick (2 ms), then produces one DSP chunk if both source and ring permit.
- **cpal callback**: pops samples from the ring, fills underruns with zeros. **No allocation, no syscalls, no `.lock()`** — ever.
- **Load-time worker**: a one-shot `std::thread` per file open. Decodes the file (whole-file into RAM) and computes the waveform peaks, then sends both back to `App` over a crossbeam channel.

## DSP staging

`TimePitchProcessor` (in `src/dsp/mod.rs`) abstracts time-stretch + pitch-shift. The engine is chunk-aware: it queries `input_frames_per_chunk()`, `max_output_frames_per_chunk()` (for scratch sizing), and `expected_output_frames_per_chunk()` (for ring-vacancy checking — returning the max here would deadlock when the worst-case output far exceeds the steady-state output). Implementations live in `src/dsp/`:

- `passthrough.rs` — identity, ultimate fallback (degenerate channel count).
- `wsola.rs` — `Wsola` (streaming WSOLA, frame 2048 / hop 512 / search ±256 / Hann, AMDF similarity search over a sum-of-channels mono mix, unity-gain OLA via window/COLA-sum compensation, ramped stretch via `target_stretch`/`STRETCH_RAMP_PER_STEP` — see step 6c), plus two `TimePitchProcessor` adapters: `WsolaSpeed` (speed-only, used as fallback when the rubato resampler in the composite can't be constructed) and `WsolaPitchShift` (the active path — composite that cascades `Wsola` with `rubato::SincFixedIn` chunk = 1024, `max_resample_ratio_relative = 4.0`). `WsolaPitchShift::recompute` enforces the cascade math: `stretch = 2^(p/12) / speed` for WSOLA, `ratio = 1 / 2^(p/12)` for rubato, net composite ratio = `1/speed`.

  `Wsola::effective_stretch()` (the larger of current and target) is what both adapters report through `expected_output_frames_per_chunk` so a ramp-down doesn't under-reserve ring vacancy and silently drop samples in `push_slice`.

When you change the DSP plan, edit the "DSP" section of `ARCHITECTURE.md` so the staged phases stay accurate.

## Engine invariants worth knowing

- **Cursor stays inside the loop**. Whenever a loop is active and the cursor would land outside it (after `SetLoop`, `Seek`, or any other path), `snap_into_loop` pulls it to `loop.start`. `produce()` re-checks every tick as defence in depth.
- **Loop wraps are stitched, not skipped**. When fewer than `in_chunk` source frames remain before `loop.end`, `produce()` assembles the chunk from `cursor..loop.end` plus the head of `loop.start..` into `stitch_buf` so the DSP still gets a clean fixed-size input. Don't change `produce()` to drop the loop tail — the resulting silence at every wrap is bad for a practice tool.
- **DSP is recreated on `LoadTrack` only when channel count changes** (rubato is per-channel-count). Same-channel reloads keep the DSP and the user's current speed setting.
- **End-of-track tail (<23 ms) is dropped** with no loop active; cursor snaps to `total_frames` so the EOF check in the run loop fires.
- **Sub-chunk loops are not supported** (loop_length < in_chunk → silence). Documented limitation; don't try to fix in v0.1.

## Conventions specific to this codebase

- Decoded `Track`s are wrapped in `Arc<Track>` — both the GUI (for waveform metadata) and the engine (for sample reads) hold copies cheaply.
- Loop region lives in `App`, not `SharedState`. The user creates loops in the UI, so App is the source of truth; `Command::SetLoop` is fire-and-forget.
- `ui::transport::format_time(frames, sample_rate)` is the shared time-formatting helper — use it; don't duplicate.
- Time values in atomics are stored as `f32::to_bits()` in `AtomicU32` (`SharedState::speed_bits`, `pitch_bits`).
