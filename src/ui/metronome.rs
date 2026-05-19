use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::engine::metronome::{MAX_BPM, MIN_BPM, MetronomeSettings};
use crate::engine::{Command, Engine};

/// Rolling tap-tempo state. Lives in `App`; entirely GUI-side.
///
/// Each tap records `Instant::now()` and drops any stored taps older than
/// `STALE_AFTER`, so a fresh series doesn't blend with the previous one.
/// BPM is the mean of the kept inter-tap intervals (a median over very few
/// samples is jittery — a mean across the last few taps is steadier and what
/// a player actually wants when they tap to a passage).
pub struct TapTempo {
    taps: VecDeque<Instant>,
}

const STALE_AFTER: Duration = Duration::from_secs(2);
/// Keep the most recent N taps. Four taps gives three intervals — enough to
/// smooth one shaky tap, few enough to feel responsive to a tempo change.
const MAX_TAPS: usize = 4;

impl TapTempo {
    pub fn new() -> Self {
        Self {
            taps: VecDeque::new(),
        }
    }

    /// Record a tap. Returns `Some(bpm)` when the tap history is long enough
    /// to derive one (i.e. at least two taps within `STALE_AFTER` of each
    /// other), `None` otherwise. The returned BPM is clamped to
    /// `[MIN_BPM, MAX_BPM]` so a fat-fingered double-tap can't drive the
    /// metronome past where it can play.
    pub fn tap(&mut self) -> Option<f32> {
        let now = Instant::now();
        while let Some(&front) = self.taps.front() {
            if now.duration_since(front) > STALE_AFTER {
                self.taps.pop_front();
            } else {
                break;
            }
        }
        self.taps.push_back(now);
        while self.taps.len() > MAX_TAPS {
            self.taps.pop_front();
        }
        if self.taps.len() < 2 {
            return None;
        }
        let mut sum = 0.0;
        let mut n = 0;
        for w in self.taps.iter().collect::<Vec<_>>().windows(2) {
            sum += w[1].duration_since(*w[0]).as_secs_f32();
            n += 1;
        }
        let mean = sum / n as f32;
        if mean <= 0.0 {
            return None;
        }
        Some((60.0 / mean).clamp(MIN_BPM, MAX_BPM))
    }
}

/// Render the metronome row: enable toggle, BPM entry, Tap button, accent
/// toggle, beats-per-measure spinner, and volume slider. Anything that
/// changes the engine-relevant state results in a single
/// `Command::SetMetronome(settings)` send at the end.
pub fn show(
    ui: &mut egui::Ui,
    settings: &mut MetronomeSettings,
    tap: &mut TapTempo,
    engine: &Engine,
) {
    let before = *settings;

    ui.horizontal(|ui| {
        ui.checkbox(&mut settings.enabled, "Metronome");
        ui.add_enabled_ui(settings.enabled, |ui| {
            ui.add(
                egui::DragValue::new(&mut settings.bpm)
                    .range(MIN_BPM..=MAX_BPM)
                    .speed(0.5)
                    .suffix(" BPM"),
            );
            if ui
                .button("Tap")
                .on_hover_text("Tap (or press T) — averages the last 4 taps")
                .clicked()
                && let Some(bpm) = tap.tap()
            {
                settings.bpm = bpm;
            }
            ui.checkbox(&mut settings.accent, "Accent");
            ui.add_enabled_ui(settings.accent, |ui| {
                ui.add(
                    egui::DragValue::new(&mut settings.beats_per_measure)
                        .range(1..=16)
                        .speed(0.1),
                );
                ui.label("beats/measure");
            });
        });
    });

    ui.add_enabled_ui(settings.enabled, |ui| {
        ui.add(
            egui::Slider::new(&mut settings.volume_db, -30.0..=6.0)
                .suffix(" dB")
                .text("metronome vol"),
        );
    });

    // BPM is clamped on tap; clamp here too so a manually-typed out-of-range
    // value doesn't sneak past the engine's MIN_BPM guard.
    settings.bpm = settings.bpm.clamp(MIN_BPM, MAX_BPM);

    if *settings != before {
        engine.send(Command::SetMetronome(*settings));
    }
}
