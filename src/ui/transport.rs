use std::sync::atomic::Ordering;

use crate::dsp::DspKind;
use crate::engine::{Command, Engine};

/// Render play/pause/stop + a seek slider. Returns true if the user is
/// actively dragging the seek slider, so the caller can decide whether to
/// keep repainting at high frequency.
///
/// `dsp_kind` is the App's chosen stretch engine (UI source of truth, like
/// the loop region). The selector edits it in place and sends `SetDsp` on
/// change.
///
/// `pitch_coarse` and `pitch_cents` are likewise UI-owned — the engine only
/// ever sees the combined `coarse + cents / 100.0` total via
/// `Command::SetPitch`. Keeping the split here means the two sliders are
/// independent: dragging cents to ±50 won't bump the semitone, and the "0 st"
/// reset doesn't wipe the user's cents offset (and vice versa).
pub fn show(
    ui: &mut egui::Ui,
    engine: &Engine,
    dsp_kind: &mut DspKind,
    pitch_coarse: &mut i32,
    pitch_cents: &mut i32,
    master_volume_db: &mut f32,
    sample_rate: u32,
    total_frames: u64,
) -> bool {
    let state = engine.state();
    let playing = state.playing.load(Ordering::Relaxed);
    let position = state.position.load(Ordering::Relaxed).min(total_frames);

    ui.horizontal(|ui| {
        let label = if playing { "⏸ Pause" } else { "▶ Play" };
        if ui.button(label).clicked() {
            engine.send(if playing { Command::Pause } else { Command::Play });
        }
        if ui.button("⏹ Stop").clicked() {
            engine.send(Command::Stop);
        }
        ui.label(format!(
            "{} / {}",
            format_time(position, sample_rate),
            format_time(total_frames, sample_rate),
        ));
    });

    // Seek slider. While being dragged, we repeatedly send Seek; the engine
    // applies the latest one. The displayed value follows the engine when
    // idle and the pointer when dragged — egui handles this naturally because
    // we re-bind `pos` from `position` every frame.
    let mut pos = position;
    let response = ui.add(
        egui::Slider::new(&mut pos, 0..=total_frames.max(1))
            .show_value(false)
            .text("seek"),
    );
    let dragging = response.dragged();
    if response.changed() {
        engine.send(Command::Seek(pos));
    }

    // Master output volume in dB. -60 floor (slightly below audible at unity
    // headroom) up to +6 dB headroom; the slider is linear in dB-space because
    // dB is already a log scale. Per-sample ramping lives in the worker, so
    // we just fire SetMasterVolume on change and let the engine smooth it.
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::Slider::new(master_volume_db, -60.0..=6.0)
                .suffix(" dB")
                .text("master"),
        );
        if r.changed() {
            engine.send(Command::SetMasterVolume(*master_volume_db));
        }
        if ui.button("0 dB").clicked() {
            *master_volume_db = 0.0;
            engine.send(Command::SetMasterVolume(0.0));
        }
    });

    // Speed control. Logarithmic so 0.5× and 2× sit equidistant from 1×.
    let mut speed = f32::from_bits(state.speed_bits.load(Ordering::Relaxed));
    if !speed.is_finite() || speed <= 0.0 {
        speed = 1.0;
    }
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::Slider::new(&mut speed, 0.25..=2.0)
                .logarithmic(true)
                .text("speed ×"),
        );
        if r.changed() {
            engine.send(Command::SetSpeed(speed));
        }
        if ui.button("1×").clicked() {
            engine.send(Command::SetSpeed(1.0));
        }
    });

    // Pitch control split into coarse (integer semitones) and fine (cents).
    // The two values live in App so the sliders don't bleed into each other
    // through a round-trip via the shared-state total; on any change we send
    // the combined value to the engine.
    let send_combined = |coarse: i32, cents: i32| {
        engine.send(Command::SetPitch(coarse as f32 + cents as f32 / 100.0));
    };
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::Slider::new(pitch_coarse, -12..=12)
                .suffix(" st")
                .text("pitch"),
        );
        if r.changed() {
            send_combined(*pitch_coarse, *pitch_cents);
        }
        if ui.button("0 st").clicked() {
            *pitch_coarse = 0;
            send_combined(*pitch_coarse, *pitch_cents);
        }
    });
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::Slider::new(pitch_cents, -50..=50)
                .suffix(" ct")
                .text("fine"),
        );
        if r.changed() {
            send_combined(*pitch_coarse, *pitch_cents);
        }
        if ui.button("0 ct").clicked() {
            *pitch_cents = 0;
            send_combined(*pitch_coarse, *pitch_cents);
        }
    });

    // Stretch-engine selector. Switching during playback rebuilds the DSP
    // mid-stream — current speed/pitch carry across.
    ui.horizontal(|ui| {
        ui.label("Stretch engine:");
        let prev = *dsp_kind;
        ui.radio_value(dsp_kind, DspKind::Wsola, "WSOLA")
            .on_hover_text("Cleaner transients (drums, plucks).");
        ui.radio_value(dsp_kind, DspKind::PhaseVocoder, "Phase vocoder")
            .on_hover_text("Cleaner sustained tones (vocals, strings).");
        if *dsp_kind != prev {
            engine.send(Command::SetDsp(*dsp_kind));
        }
    });

    dragging
}

pub fn format_time(frames: u64, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return "0:00.000".into();
    }
    let secs = frames as f64 / sample_rate as f64;
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let ms = ((secs - total as f64) * 1000.0).round() as u32;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}
