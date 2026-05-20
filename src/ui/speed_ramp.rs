use crate::engine::speed_ramp::{SpeedRampSettings, StepUnit};
use crate::engine::{Command, Engine};

/// Render the speed-ramp row: enable toggle, target speed, step unit selector,
/// step amount, and passes-per-step. Sends a single `Command::SetSpeedRamp` at
/// the end when anything changed.
pub fn show(ui: &mut egui::Ui, settings: &mut SpeedRampSettings, engine: &Engine) {
    let before = *settings;

    ui.horizontal(|ui| {
        ui.checkbox(&mut settings.enabled, "Speed ramp")
            .on_hover_text(
                "Gradually increase speed each loop pass. Has no effect when no loop is set.",
            );
        ui.add_enabled_ui(settings.enabled, |ui| {
            ui.label("→");
            ui.add(
                egui::Slider::new(&mut settings.target_speed, 0.25..=2.0)
                    .logarithmic(true)
                    .text("target ×"),
            );
            if ui.button("1×").clicked() {
                settings.target_speed = 1.0;
            }
        });
    });

    ui.add_enabled_ui(settings.enabled, |ui| {
        ui.horizontal(|ui| {
            ui.label("Step:");
            ui.add(
                egui::DragValue::new(&mut settings.step_amount)
                    .range(0.1..=50.0)
                    .speed(0.1),
            );
            ui.radio_value(&mut settings.step_unit, StepUnit::Percent, "%")
                .on_hover_text("Per-step delta as a percentage of 1× speed.");
            ui.radio_value(&mut settings.step_unit, StepUnit::Bpm, "BPM")
                .on_hover_text("Per-step delta in BPM, scaled by the metronome's source tempo.");
            ui.label("every");
            ui.add(
                egui::DragValue::new(&mut settings.passes_per_step)
                    .range(1..=32)
                    .speed(0.1),
            );
            ui.label("loop(s)");
        });
    });

    // Clamp the raw fields against the DragValue ranges so a typed-in
    // out-of-range value can't slip past the engine guard.
    settings.step_amount = settings.step_amount.clamp(0.1, 50.0);
    settings.passes_per_step = settings.passes_per_step.clamp(1, 32);
    settings.target_speed = settings.target_speed.clamp(0.25, 2.0);

    if *settings != before {
        engine.send(Command::SetSpeedRamp(*settings));
    }
}
