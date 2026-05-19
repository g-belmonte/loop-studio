# Loop Studio

A practice-focused music player for musicians who learn by ear. Open a track, mark a loop, slow it down, transpose it, and drill the passage until your fingers know it.

## What it does (target)

- Play common audio formats (MP3, FLAC, WAV, OGG, AAC, M4A).
- Display the waveform; click to seek.
- Set A/B loop points by clicking the waveform; loop seamlessly.
- Change playback **speed** independently of pitch (0.25× – 2.0×).
- Change **pitch** independently of speed (±12 semitones coarse, ±50 cents fine).
- Save and reload practice sessions (file path + loop + speed + pitch) as JSON.

## Status

v0.1 (MVP) shipped, v0.2 closed: every checkbox in both lists is implemented. Next work moves to v0.3 (practice features).

## Tech stack

| Concern        | Choice                              | Why |
|----------------|-------------------------------------|-----|
| GUI            | `egui` via `eframe`                 | Pure-Rust, immediate-mode, fast iteration, well-suited to tool UIs. |
| Audio output   | `cpal`                              | Low-level cross-platform output; we control the callback. |
| Decoding       | `symphonia`                         | Pure-Rust, broad format support, stream-friendly. |
| Resampling     | `rubato`                            | High-quality sample-rate conversion, used as a DSP building block. |
| Time/pitch DSP | In-house WSOLA + `rubato` (PV next)  | WSOLA stretches in time, `rubato` shifts pitch; behind a trait so an FFT-based phase vocoder can ship alongside (step 8a) and the user picks per track. See [ARCHITECTURE.md](./ARCHITECTURE.md#dsp). |
| Ring buffer    | `ringbuf`                           | Lock-free SPSC, real-time-safe. |
| File dialog    | `rfd`                               | Native open/save dialogs. |
| Sessions       | `serde` + `serde_json`              | Simple, human-readable. |

See [ARCHITECTURE.md](./ARCHITECTURE.md) for thread layout, data flow, and module responsibilities.

## Roadmap

### v0.1 — MVP

- [x] **Open file** via dialog; decode in background.
- [x] **Transport**: play / pause / stop / seek.
- [x] **Waveform view** with playhead and click-to-seek.
- [x] **A/B loop**: click waveform to set start/end; seamless loop playback.
- [x] **Speed slider** 0.25× – 2.0× (real-time, pitch-preserving via WSOLA after step 6a).
- [x] **Pitch slider** ±12 semitones (real-time, independent of speed).
- [x] **Session save/load** (JSON: path, loop, speed, pitch, last position).

### v0.2 — Quality of life

- [x] **Keyboard shortcuts** (space, arrows, `[`/`]`, Esc, Home/End).
- [x] **Markers / cue points** (M to add, `1`–`9` / Ctrl+←/→ to navigate, editable labels).
- [x] **Per-track session auto-save** (debounced writes under `$XDG_DATA_HOME/loop-studio/autosessions/`, restored on Open).
- [x] **Waveform zoom + scroll** (Ctrl+wheel / pinch / `+`–`-` to zoom, Shift+wheel / right-drag to pan, follow-playhead toggle).
- [x] **Cents-level pitch nudge** (±50 ct fine slider alongside the ±12 st coarse slider).
- [x] **WSOLA quality pass**: AMDF similarity search, OLA gain compensation, stretch ramping.
- [x] **Phase vocoder time-stretch** with WSOLA / PV runtime selector (PV cleaner on sustained tones, WSOLA cleaner on transients — pick per track).
- [x] **PV refinements**: Laroche–Dolson phase locking and transient-detect-and-passthrough.

See [ARCHITECTURE.md](./ARCHITECTURE.md#decisions) for the design notes behind each item.

### v0.3 — Practice features

- [ ] **Speed ramping**: gradually increase loop speed each pass.
- [ ] **EQ / band isolation** (focus on bass or vocal range).
- [x] **Click track / metronome** synced to a tap-set tempo (source-frame anchored at the loop start, optional accent + beats/measure, BPM via tap / `T` key / number entry, mixed post-DSP into the engine output).
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
