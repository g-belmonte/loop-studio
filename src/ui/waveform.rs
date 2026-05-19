use egui::{Color32, PointerButton, Pos2, Rect, Sense, Stroke, Vec2};

use crate::track::peaks::{BUCKET_FRAMES, TrackPeaks};
use crate::track::{LoopRegion, Marker};

const LOOP_BORDER_COLOR: Color32 = Color32::from_rgb(255, 165, 0);
const MARKER_COLOR: Color32 = Color32::from_rgb(120, 220, 130);

/// Minimum visible window in source frames. Roughly 1.5 ms at 44.1 kHz —
/// past this point per-pixel peak buckets become smaller than a sample, so
/// further zoom would just stretch the same bucket across many pixels.
pub const MIN_VIEW_LEN: u64 = 64;

/// Visible window into the track, in source-frame indices.
///
/// `start` is the leftmost frame and `len` is the number of frames spanned by
/// the widget rect. The pair is the only thing that converts between pixels
/// and frames now; `total_frames` is used purely as a clamp bound.
#[derive(Debug, Clone, Copy)]
pub struct WaveformView {
    pub start: u64,
    pub len: u64,
}

impl WaveformView {
    pub fn full(total_frames: u64) -> Self {
        Self {
            start: 0,
            len: total_frames.max(MIN_VIEW_LEN),
        }
    }

    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// Action returned by the waveform widget for the caller (App) to dispatch.
#[derive(Debug, Clone, Copy)]
pub enum WaveformAction {
    None,
    Seek(u64),
    SetLoop(LoopRegion),
}

/// Render the waveform with playhead, loop region, markers, and view window.
///
/// Interaction:
/// - **Left click**: seek to that frame.
/// - **Left click-and-drag**: define a new loop region (auto-ordered).
/// - **Right click-and-drag**: pan the visible window.
/// - **Ctrl+scroll / pinch**: zoom anchored at the cursor.
/// - **Shift+scroll**: pan the visible window.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    peaks: &TrackPeaks,
    position_frame: u64,
    total_frames: u64,
    loop_region: Option<LoopRegion>,
    markers: &[Marker],
    view: &mut WaveformView,
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

    handle_zoom_pan(ui, &response, rect, view, total_frames);

    // Waveform bars over the visible window only.
    let width_px = rect.width().round() as usize;
    let n_buckets = peaks.len();
    if width_px > 0 && n_buckets > 0 && view.len > 0 {
        let mid_y = rect.center().y;
        let half_h = rect.height() * 0.5;
        let stroke = Stroke::new(1.0, wave_color);
        let bucket_frames = BUCKET_FRAMES as f64;
        let view_start_bucket = view.start as f64 / bucket_frames;
        let view_len_buckets = view.len as f64 / bucket_frames;

        for px in 0..width_px {
            // Bucket range that covers this pixel column. Using f64 mapping so
            // sub-bucket pixels still pick up exactly one bucket.
            let b0 = view_start_bucket + (px as f64 / width_px as f64) * view_len_buckets;
            let b1 = view_start_bucket + ((px + 1) as f64 / width_px as f64) * view_len_buckets;
            let i0 = (b0 as usize).min(n_buckets);
            let i1 = ((b1.ceil() as usize).max(i0 + 1)).min(n_buckets);
            if i0 >= n_buckets {
                break;
            }

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

    // Committed loop region — clipped to the visible window.
    if let Some(r) = loop_region
        && total_frames > 0
        && r.end > view.start
        && r.start < view.end()
    {
        let x0 = frame_to_x(r.start, rect, view);
        let x1 = frame_to_x(r.end, rect, view);
        let region_rect = Rect::from_x_y_ranges(x0..=x1, rect.y_range());
        painter.rect_filled(region_rect, 0.0, loop_fill);
        if r.start >= view.start {
            painter.line_segment(
                [Pos2::new(x0, rect.top()), Pos2::new(x0, rect.bottom())],
                loop_border,
            );
        }
        if r.end <= view.end() {
            painter.line_segment(
                [Pos2::new(x1, rect.top()), Pos2::new(x1, rect.bottom())],
                loop_border,
            );
        }
    }

    // Drag-to-define-loop (left button only). The drag's start frame is stashed
    // in egui memory so the widget remains stateless from the caller's POV.
    let drag_id = ui.make_persistent_id("waveform_drag_start");
    let mut drag_start: Option<u64> = ui.data(|d| d.get_temp(drag_id));

    if response.drag_started_by(PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
    {
        let f = pixel_x_to_frame(p.x, rect, view, total_frames);
        ui.data_mut(|d| d.insert_temp(drag_id, f));
        drag_start = Some(f);
    }

    if response.dragged_by(PointerButton::Primary)
        && total_frames > 0
        && let (Some(start), Some(p)) = (drag_start, response.interact_pointer_pos())
    {
        let cur = pixel_x_to_frame(p.x, rect, view, total_frames);
        let (lo, hi) = if start <= cur { (start, cur) } else { (cur, start) };
        if hi > lo {
            let x0 = frame_to_x(lo, rect, view);
            let x1 = frame_to_x(hi, rect, view);
            painter.rect_filled(
                Rect::from_x_y_ranges(x0..=x1, rect.y_range()),
                0.0,
                loop_preview_fill,
            );
        }
    }

    // Markers — drawn before the playhead so it stays on top when they coincide.
    if total_frames > 0 {
        let stroke = Stroke::new(1.0, MARKER_COLOR);
        for m in markers {
            if m.frame >= total_frames || m.frame < view.start || m.frame > view.end() {
                continue;
            }
            let x = frame_to_x(m.frame, rect, view);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                stroke,
            );
        }
    }

    // Playhead — drawn last so it sits on top of the loop overlay. Hidden when
    // outside the visible window.
    if total_frames > 0
        && position_frame >= view.start
        && position_frame <= view.end()
    {
        let x = frame_to_x(position_frame, rect, view);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, playhead_color),
        );
    }

    // Resolve actions: drag_stopped wins over clicked when both could fire.
    if response.drag_stopped_by(PointerButton::Primary) {
        let start_opt = ui.data_mut(|d| d.remove_temp::<u64>(drag_id));
        if let (Some(start), Some(p)) = (start_opt, response.interact_pointer_pos()) {
            let end = pixel_x_to_frame(p.x, rect, view, total_frames);
            let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
            if hi > lo {
                return WaveformAction::SetLoop(LoopRegion { start: lo, end: hi });
            }
        }
    }

    if response.clicked()
        && total_frames > 0
        && let Some(p) = response.interact_pointer_pos()
    {
        return WaveformAction::Seek(pixel_x_to_frame(p.x, rect, view, total_frames));
    }

    WaveformAction::None
}

/// Apply ctrl+wheel / pinch zoom (anchored at the pointer) and shift+wheel /
/// right-drag pan (in pixels → frames via the current view scale).
fn handle_zoom_pan(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: Rect,
    view: &mut WaveformView,
    total_frames: u64,
) {
    // Right-button drag → pan. drag_delta() reports the motion for whichever
    // button is currently dragging; we only consume it when secondary is the
    // active button so the left-button loop-define drag stays untouched.
    if response.dragged_by(PointerButton::Secondary) {
        let dx = response.drag_delta().x;
        if dx != 0.0 && rect.width() > 0.0 {
            pan_by_pixels(view, total_frames, -dx, rect.width());
        }
    }

    if !response.hovered() {
        return;
    }

    let pointer_x = ui.input(|i| i.pointer.hover_pos()).map(|p| p.x);
    let (mods, scroll, zoom) =
        ui.input(|i| (i.modifiers, i.smooth_scroll_delta, i.zoom_delta()));

    // Zoom: egui folds ctrl+wheel and trackpad pinch into `zoom_delta()`
    // automatically, so we only need to consume it once here. Anchor at the
    // pointer so the user can drill into a spot they're hovering on.
    if (zoom - 1.0).abs() > f32::EPSILON {
        let anchor_t = pointer_anchor_t(pointer_x, rect);
        zoom_by(view, total_frames, zoom, anchor_t);
    }

    // Pan: shift+wheel. Most backends put horizontal scroll on `scroll.x` and
    // shift-modified vertical scroll on `scroll.y`; fold them so either lands.
    if mods.shift && (scroll.x != 0.0 || scroll.y != 0.0) {
        let delta_px = scroll.x - scroll.y;
        pan_by_pixels(view, total_frames, -delta_px, rect.width());
    }
}

fn pointer_anchor_t(pointer_x: Option<f32>, rect: Rect) -> f32 {
    pointer_x
        .map(|x| ((x - rect.left()) / rect.width()).clamp(0.0, 1.0))
        .unwrap_or(0.5)
}

fn pan_by_pixels(view: &mut WaveformView, total_frames: u64, dx: f32, width: f32) {
    if width <= 0.0 {
        return;
    }
    let frames_per_px = view.len as f64 / width as f64;
    let delta = (dx as f64 * frames_per_px).round() as i64;
    let max_start = total_frames.max(view.len).saturating_sub(view.len) as i64;
    let new_start = (view.start as i64 + delta).clamp(0, max_start) as u64;
    view.start = new_start;
}

fn zoom_by(view: &mut WaveformView, total_frames: u64, factor: f32, anchor_t: f32) {
    if !factor.is_finite() || factor <= 0.0 {
        return;
    }
    let total = total_frames.max(MIN_VIEW_LEN);
    let anchor_frame = view.start as f64 + anchor_t as f64 * view.len as f64;
    let new_len = ((view.len as f64 / factor as f64).round() as u64)
        .clamp(MIN_VIEW_LEN, total);
    let new_start = (anchor_frame - anchor_t as f64 * new_len as f64)
        .clamp(0.0, (total - new_len) as f64) as u64;
    view.start = new_start;
    view.len = new_len;
}

/// Zoom anchored at the playhead. Called from the global keyboard shortcuts.
pub fn zoom_at_playhead(
    view: &mut WaveformView,
    total_frames: u64,
    factor: f32,
    playhead: u64,
) {
    let anchor_t = if view.len > 0 {
        ((playhead.saturating_sub(view.start)) as f64 / view.len as f64).clamp(0.0, 1.0) as f32
    } else {
        0.5
    };
    zoom_by(view, total_frames, factor, anchor_t);
}

/// Page the view so the playhead lands near the left edge. Used by the
/// follow-playhead toggle when the playhead drifts outside the visible window.
pub fn follow_playhead(view: &mut WaveformView, total_frames: u64, position: u64) {
    if total_frames == 0 || view.len == 0 {
        return;
    }
    if position >= view.start && position <= view.end() {
        return;
    }
    let max_start = total_frames.saturating_sub(view.len);
    view.start = position.min(max_start);
}

fn pixel_x_to_frame(x: f32, rect: Rect, view: &WaveformView, total_frames: u64) -> u64 {
    if rect.width() <= 0.0 || total_frames == 0 || view.len == 0 {
        return 0;
    }
    let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
    let f = view.start as f64 + t * view.len as f64;
    (f as u64).min(total_frames)
}

fn frame_to_x(frame: u64, rect: Rect, view: &WaveformView) -> f32 {
    if view.len == 0 {
        return rect.left();
    }
    let t = ((frame as f64 - view.start as f64) / view.len as f64).clamp(0.0, 1.0) as f32;
    rect.left() + t * rect.width()
}
