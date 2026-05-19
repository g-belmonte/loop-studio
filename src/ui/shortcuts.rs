use std::sync::atomic::Ordering;

use egui::{Key, Modifiers};

use crate::engine::{Command, Engine};
use crate::track::LoopRegion;

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

        // Arrows: Shift = 1 s, no modifier = 5 s. Check Shift first so the
        // unmodified consume_key doesn't swallow shifted presses.
        for &(key, sign) in &[(Key::ArrowLeft, -1i64), (Key::ArrowRight, 1i64)] {
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
    });
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
