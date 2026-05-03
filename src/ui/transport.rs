use std::sync::atomic::Ordering;

use crate::engine::{Command, Engine};

/// Render play/pause/stop + a seek slider. Returns true if the user is
/// actively dragging the seek slider, so the caller can decide whether to
/// keep repainting at high frequency.
pub fn show(ui: &mut egui::Ui, engine: &Engine, sample_rate: u32, total_frames: u64) -> bool {
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
