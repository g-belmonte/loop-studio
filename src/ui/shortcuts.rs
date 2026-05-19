use std::sync::atomic::Ordering;

use egui::{Key, Modifiers};

use crate::engine::{Command, Engine};
use crate::track::{LoopRegion, Marker};

/// One half of an in-progress `[`/`]` loop definition. While
/// `pending` holds a value, the user has pressed one endpoint key and
/// the loop is not yet active; pressing the complementary key materialises
/// the loop region.
#[derive(Debug, Clone, Copy)]
pub enum LoopEndpoint {
    Start(u64),
    End(u64),
}

/// Drive the global keyboard shortcuts. Only call when a track is loaded.
///
/// Uses `consume_key` so the seek slider doesn't also receive arrow keys
/// when it happens to have focus. Bails out when egui reports a text input
/// wants keyboard input (we have no text inputs today but this stays
/// correct if one is added).
pub fn handle(
    ctx: &egui::Context,
    engine: &Engine,
    track_sample_rate: u32,
    total_frames: u64,
    loop_region: &mut Option<LoopRegion>,
    pending_loop: &mut Option<LoopEndpoint>,
    markers: &mut Vec<Marker>,
) {
    if ctx.wants_keyboard_input() {
        return;
    }

    let state = engine.state();
    let position = state.position.load(Ordering::Relaxed).min(total_frames);
    let playing = state.playing.load(Ordering::Relaxed);
    let sr = track_sample_rate as u64;

    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::Space) {
            engine.send(if playing { Command::Pause } else { Command::Play });
        }

        // Arrows: Ctrl = step between markers, Shift = 1 s seek,
        // no modifier = 5 s seek. Check Ctrl/Shift before plain so the
        // unmodified consume_key doesn't swallow modified presses.
        for &(key, sign) in &[(Key::ArrowLeft, -1i64), (Key::ArrowRight, 1i64)] {
            if i.consume_key(Modifiers::COMMAND, key) {
                if let Some(target) =
                    step_marker(markers, position, sign > 0, track_sample_rate)
                {
                    engine.send(Command::Seek(target));
                }
                continue;
            }
            let small = i.consume_key(Modifiers::SHIFT, key);
            let large = !small && i.consume_key(Modifiers::NONE, key);
            if small || large {
                let secs = if small { 1 } else { 5 };
                let delta = (sr as i64).saturating_mul(secs) * sign;
                let target = (position as i64 + delta).clamp(0, total_frames as i64) as u64;
                engine.send(Command::Seek(target));
            }
        }

        if i.consume_key(Modifiers::NONE, Key::Home) {
            let target = loop_region.map(|r| r.start).unwrap_or(0);
            engine.send(Command::Seek(target));
        }
        if i.consume_key(Modifiers::NONE, Key::End) {
            let target = loop_region
                .map(|r| r.end.saturating_sub(1))
                .unwrap_or(total_frames);
            engine.send(Command::Seek(target));
        }

        if i.consume_key(Modifiers::NONE, Key::Escape) {
            if loop_region.is_some() {
                engine.send(Command::SetLoop(None));
                *loop_region = None;
            }
            *pending_loop = None;
        }

        if i.consume_key(Modifiers::NONE, Key::OpenBracket) {
            apply_endpoint(LoopEndpoint::Start(position), loop_region, pending_loop, engine);
        }
        if i.consume_key(Modifiers::NONE, Key::CloseBracket) {
            apply_endpoint(LoopEndpoint::End(position), loop_region, pending_loop, engine);
        }

        // M: drop a marker at the current playhead. Duplicate-frame presses are
        // a no-op so a held key doesn't pile up identical entries.
        if i.consume_key(Modifiers::NONE, Key::M) {
            add_marker(markers, position);
        }

        // 1..9: jump to the Nth marker (1-indexed). Beyond 9 markers the
        // extras have no shortcut — use Ctrl+arrows or the side list.
        const NUMS: [Key; 9] = [
            Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5,
            Key::Num6, Key::Num7, Key::Num8, Key::Num9,
        ];
        for (idx, key) in NUMS.iter().enumerate() {
            if i.consume_key(Modifiers::NONE, *key) {
                if let Some(m) = markers.get(idx) {
                    engine.send(Command::Seek(m.frame));
                }
            }
        }
    });
}

/// Insert a marker at `frame`, keeping `markers` sorted by frame. Silently
/// skips if a marker already exists at the same frame.
fn add_marker(markers: &mut Vec<Marker>, frame: u64) {
    match markers.binary_search_by_key(&frame, |m| m.frame) {
        Ok(_) => {}
        Err(pos) => markers.insert(
            pos,
            Marker {
                frame,
                label: String::new(),
            },
        ),
    }
}

/// Return the frame of the next/previous marker relative to `position`.
///
/// Forward: smallest frame strictly greater than `position`.
///
/// Backward: largest frame strictly less than an "anchor" position. The anchor
/// is `position` itself, *unless* a marker sits within `MARKER_BACK_TOL_SECS`
/// just behind the playhead — in that case we assume playback has drifted past
/// a marker we just jumped to and use that marker's frame as the anchor, so
/// repeated Ctrl+← walks back through the marker list instead of yanking back
/// to the same marker every time.
fn step_marker(
    markers: &[Marker],
    position: u64,
    forward: bool,
    sample_rate: u32,
) -> Option<u64> {
    if forward {
        return markers.iter().find(|m| m.frame > position).map(|m| m.frame);
    }
    const MARKER_BACK_TOL_SECS: f32 = 0.5;
    let tolerance = (sample_rate as f32 * MARKER_BACK_TOL_SECS) as u64;
    let anchor = markers
        .iter()
        .rev()
        .find(|m| m.frame < position && position - m.frame <= tolerance)
        .map(|m| m.frame)
        .unwrap_or(position);
    markers
        .iter()
        .rev()
        .find(|m| m.frame < anchor)
        .map(|m| m.frame)
}

/// State machine for `[` / `]` presses.
///
/// - If a loop is already active, update the matching edge in place.
/// - Otherwise, if the complementary endpoint is pending, materialise a loop
///   (auto-ordered so `start < end`).
/// - Otherwise, stash this endpoint as pending.
fn apply_endpoint(
    new: LoopEndpoint,
    loop_region: &mut Option<LoopRegion>,
    pending_loop: &mut Option<LoopEndpoint>,
    engine: &Engine,
) {
    if let Some(active) = *loop_region {
        let (a, b) = match new {
            LoopEndpoint::Start(s) => (s, active.end),
            LoopEndpoint::End(e) => (active.start, e),
        };
        commit(a, b, loop_region, pending_loop, engine);
        return;
    }

    match (*pending_loop, new) {
        (Some(LoopEndpoint::End(e)), LoopEndpoint::Start(s))
        | (Some(LoopEndpoint::Start(s)), LoopEndpoint::End(e)) => {
            commit(s, e, loop_region, pending_loop, engine);
        }
        _ => {
            *pending_loop = Some(new);
        }
    }
}

fn commit(
    a: u64,
    b: u64,
    loop_region: &mut Option<LoopRegion>,
    pending_loop: &mut Option<LoopEndpoint>,
    engine: &Engine,
) {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    if hi > lo {
        let region = LoopRegion { start: lo, end: hi };
        *loop_region = Some(region);
        *pending_loop = None;
        engine.send(Command::SetLoop(Some(region)));
    }
}
