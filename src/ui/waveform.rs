use egui::{Pos2, Sense, Stroke, Vec2};

use crate::track::peaks::TrackPeaks;

/// Render the waveform with playhead. Returns `Some(frame)` if the user
/// clicked-to-seek; the caller is responsible for dispatching the seek.
pub fn show(
    ui: &mut egui::Ui,
    peaks: &TrackPeaks,
    position_frame: u64,
    total_frames: u64,
    height: f32,
) -> Option<u64> {
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter_at(rect);

    let bg = ui.visuals().extreme_bg_color;
    let wave_color = ui.visuals().widgets.active.fg_stroke.color;
    let playhead_color = ui.visuals().selection.bg_fill;

    painter.rect_filled(rect, 2.0, bg);

    let width_px = rect.width().round() as usize;
    let n_buckets = peaks.len();

    if width_px > 0 && n_buckets > 0 {
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.5;
        let stroke = Stroke::new(1.0, wave_color);

        for px in 0..width_px {
            // Map this pixel to a bucket range — guarantee at least one bucket
            // per pixel even when the widget is wider than `n_buckets`.
            let i0 = (px * n_buckets) / width_px;
            let i1 = (((px + 1) * n_buckets) / width_px)
                .max(i0 + 1)
                .min(n_buckets);

            let mut lo = 0.0_f32;
            let mut hi = 0.0_f32;
            for i in i0..i1 {
                if peaks.min[i] < lo {
                    lo = peaks.min[i];
                }
                if peaks.max[i] > hi {
                    hi = peaks.max[i];
                }
            }

            let x = rect.left() + px as f32 + 0.5;
            // Sample range is roughly [-1, 1]; flip y because screen y grows down.
            let y_top = mid_y - hi.clamp(-1.0, 1.0) * half_h;
            let y_bot = mid_y - lo.clamp(-1.0, 1.0) * half_h;

            // Always draw at least a 1px tick at center for true silence so
            // the user sees a continuous waveform footprint.
            let (y_top, y_bot) = if (y_bot - y_top).abs() < 1.0 {
                (mid_y - 0.5, mid_y + 0.5)
            } else {
                (y_top, y_bot)
            };
            painter.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_bot)], stroke);
        }
    }

    if total_frames > 0 {
        let t = (position_frame as f64 / total_frames as f64).clamp(0.0, 1.0) as f32;
        let x = rect.left() + t * rect.width();
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, playhead_color),
        );
    }

    if response.clicked() && total_frames > 0 {
        if let Some(pointer) = response.interact_pointer_pos() {
            let t = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            return Some((t as f64 * total_frames as f64) as u64);
        }
    }

    None
}
