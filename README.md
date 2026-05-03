# Loop Studio

A practice-focused music player for musicians who learn by ear. Open a track, mark a loop, slow it down, transpose it, and drill the passage until your fingers know it.

## What it does (target)

- Play common audio formats (MP3, FLAC, WAV, OGG, AAC, M4A).
- Display the waveform; click to seek.
- Set A/B loop points by clicking the waveform; loop seamlessly.
- Change playback **speed** independently of pitch (0.25× – 2.0×).
- Change **pitch** independently of speed (±12 semitones, fine cents later).
- Save and reload practice sessions (file path + loop + speed + pitch) as JSON.

## Status

Pre-MVP. The repo currently contains the architecture, a `Cargo.toml` with the chosen stack, and module skeletons. Nothing audible yet.

## Tech stack

| Concern        | Choice                              | Why |
|----------------|-------------------------------------|-----|
| GUI            | `egui` via `eframe`                 | Pure-Rust, immediate-mode, fast iteration, well-suited to tool UIs. |
| Audio output   | `cpal`                              | Low-level cross-platform output; we control the callback. |
| Decoding       | `symphonia`                         | Pure-Rust, broad format support, stream-friendly. |
| Resampling     | `rubato`                            | High-quality sample-rate conversion, used as a DSP building block. |
| Time/pitch DSP | TBD — pure-Rust phase vocoder/WSOLA | Pluggable; see [ARCHITECTURE.md](./ARCHITECTURE.md#dsp). |
| Ring buffer    | `ringbuf`                           | Lock-free SPSC, real-time-safe. |
| File dialog    | `rfd`                               | Native open/save dialogs. |
| Sessions       | `serde` + `serde_json`              | Simple, human-readable. |

See [ARCHITECTURE.md](./ARCHITECTURE.md) for thread layout, data flow, and module responsibilities.

## Roadmap

### v0.1 — MVP

- [ ] **Open file** via dialog or drag-drop; decode in background.
- [ ] **Transport**: play / pause / stop / seek.
- [ ] **Waveform view** with playhead and click-to-seek.
- [ ] **A/B loop**: click waveform to set start/end; seamless loop playback.
- [ ] **Speed slider** 0.25× – 2.0× (real-time).
- [ ] **Pitch slider** ±12 semitones (real-time, independent of speed).
- [ ] **Session save/load** (JSON: path, loop, speed, pitch, last position).

### v0.2 — Quality of life

- [ ] Keyboard shortcuts (space, arrows, `[`/`]` for loop points).
- [ ] Markers / cue points within a track.
- [ ] Per-track session auto-save.
- [ ] Zoom + scroll on waveform.
- [ ] Cents-level pitch nudge.

### v0.3 — Practice features

- [ ] **Speed ramping**: gradually increase loop speed each pass.
- [ ] **EQ / band isolation** (focus on bass or vocal range).
- [ ] **Click track / metronome** synced to a tap-set tempo.
- [ ] Export looped section as audio.

### Later

- Stem separation (offload to an external model).
- MIDI controller mapping.
- Cross-fade loops, "smart" loop boundary snapping to zero crossings or beats.

## Building & running

```sh
cargo run --release
```

Linux will pull system deps for ALSA / Wayland / X11 — install via your package manager if `cpal` or `eframe` complain at link time. On Arch:

```sh
sudo pacman -S alsa-lib libxkbcommon
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
