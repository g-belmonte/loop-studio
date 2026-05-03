use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::track::LoopRegion;
use crate::track::peaks::TrackPeaks;

const LOOP_BORDER_COLOR: Color32 = Color32::from_rgb(255, 165, 0);

/// Action returned by the waveform widget for the caller (App) to dispatch.
#[derive(Debug, Clone, Copy)]
pub enum WaveformAction {
    None,
    Seek(u64),
    SetLoop(LoopRegion),
}

/// Render the waveform with playhead and an optional loop region.
///
/// Interaction:
/// - **Click**: seek to that frame.
/// - **Click-and-drag**: define a new loop region (auto-ordered start < end).
pub fn show(
    ui: &mut egui::Ui,
    peaks: &TrackPeaks,
    position_frame: u64,
    total_frames: u64,
    loop_region: Option<LoopRegion>,
    height: f32,
) -> WaveformAction {
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    let bg = ui.visuals().extreme_bg_color;
    let wave_color = ui.visuals().widgets.active.fg_stroke.color;
    let playhead_color = ui.visuals().selection.bg_fill;
    let loop_fill = Color32::from_rgba_premultiplied(255, 165, 0, 40);
    let loop_preview_fill = Color32::from_rgba_premultiplied(255, 165, 0, 80);
    let loop_border = Stroke::new(1.0, LOOP_BORDER_COLOR);

    painter.rect_filled(rect, 2.0, bg);

    // Waveform bars.
    let width_px = rect.width().round() as usize;
    let n_buckets = peaks.len();
    if width_px > 0 && n_buckets > 0 {
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.5;
        let stroke = Stroke::new(1.0, wave_color);

        for px in 0..width_px {
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
            let y_top = mid_y - hi.clamp(-1.0, 1.0) * half_h;
            let y_bot = mid_y - lo.clamp(-1.0, 1.0) * half_h;
            let (y_top, y_bot) = if (y_bot - y_top).abs() < 1.0 {
                (mid_y - 0.5, mid_y + 0.5)
            } else {
                (y_top, y_bot)
            };
            painter.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_bot)], stroke);
        }
    }

    // Committed loop region.
    if let Some(r) = loop_region {
        if total_frames > 0 {
            let x0 = frame_to_x(r.start, rect, total_frames);
            let x1 = frame_to_x(r.end, rect, total_frames);
            let region_rect = Rect::from_x_y_ranges(x0..=x1, rect.y_range());
            painter.rect_filled(region_rect, 0.0, loop_fill);
            painter.line_segment(
                [Pos2::new(x0, rect.top()), Pos2::new(x0, rect.bottom())],
                loop_border,
            );
            painter.line_segment(
                [Pos2::new(x1, rect.top()), Pos2::new(x1, rect.bottom())],
                loop_border,
            );
        }
    }

    // Drag-to-define-loop. The drag's start frame is stashed in egui memory so
    // the widget remains stateless from the caller's point of view.
    let drag_id = ui.make_persistent_id("waveform_drag_start");
    let mut drag_start: Option<u64> = ui.data(|d| d.get_temp(drag_id));

    if response.drag_started() {
        if let Some(p) = response.interact_pointer_pos() {
            let f = pixel_x_to_frame(p.x, rect, total_frames);
            ui.data_mut(|d| d.insert_temp(drag_id, f));
            drag_start = Some(f);
        }
    }

    if response.dragged() && total_frames > 0 {
        if let (Some(start), Some(p)) = (drag_start, response.interact_pointer_pos()) {
            let cur = pixel_x_to_frame(p.x, rect, total_frames);
            let (lo, hi) = if start <= cur { (start, cur) } else { (cur, start) };
            if hi > lo {
                let x0 = frame_to_x(lo, rect, total_frames);
                let x1 = frame_to_x(hi, rect, total_frames);
                painter.rect_filled(
                    Rect::from_x_y_ranges(x0..=x1, rect.y_range()),
                    0.0,
                    loop_preview_fill,
                );
            }
        }
    }

    // Playhead — drawn last so it sits on top of the loop overlay.
    if total_frames > 0 {
        let t = (position_frame as f64 / total_frames as f64).clamp(0.0, 1.0) as f32;
        let x = rect.left() + t * rect.width();
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, playhead_color),
        );
    }

    // Resolve actions: drag_stopped wins over clicked when both could fire.
    if response.drag_stopped() {
        let start_opt = ui.data_mut(|d| d.remove_temp::<u64>(drag_id));
        if let (Some(start), Some(p)) = (start_opt, response.interact_pointer_pos()) {
            let end = pixel_x_to_frame(p.x, rect, total_frames);
            let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
            if hi > lo {
                return WaveformAction::SetLoop(LoopRegion { start: lo, end: hi });
            }
        }
    }

    if response.clicked() && total_frames > 0 {
        if let Some(p) = response.interact_pointer_pos() {
            return WaveformAction::Seek(pixel_x_to_frame(p.x, rect, total_frames));
        }
    }

    WaveformAction::None
}

fn pixel_x_to_frame(x: f32, rect: Rect, total_frames: u64) -> u64 {
    if rect.width() <= 0.0 || total_frames == 0 {
        return 0;
    }
    let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    (t as f64 * total_frames as f64) as u64
}

fn frame_to_x(frame: u64, rect: Rect, total_frames: u64) -> f32 {
    if total_frames == 0 {
        return rect.left();
    }
    let t = (frame as f64 / total_frames as f64).clamp(0.0, 1.0) as f32;
    rect.left() + t * rect.width()
}
