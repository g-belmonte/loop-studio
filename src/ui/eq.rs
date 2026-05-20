use crate::dsp::eq::{BAND_FREQS_HZ, EqSettings, NUM_BANDS};
use crate::engine::{Command, Engine};

const BAND_LABELS: [&str; NUM_BANDS] = ["Low", "Lo-mid", "Mid", "Hi-mid", "High"];
const GAIN_RANGE_DB: std::ops::RangeInclusive<f32> = -24.0..=24.0;

/// Render the EQ panel: enable toggle + a column per band (label / gain
/// slider / solo button / "0 dB" reset). Solo is mutually exclusive — clicking
/// one band's Solo unsets the others.
pub fn show(ui: &mut egui::Ui, settings: &mut EqSettings, engine: &Engine) {
    let before = *settings;

    ui.horizontal(|ui| {
        ui.checkbox(&mut settings.enabled, "EQ");
        ui.add_enabled_ui(settings.enabled, |ui| {
            if ui
                .button("Flat")
                .on_hover_text("Reset all bands to 0 dB and clear solo")
                .clicked()
            {
                for b in &mut settings.bands {
                    b.gain_db = 0.0;
                    b.solo = false;
                }
            }
        });
    });

    ui.add_enabled_ui(settings.enabled, |ui| {
        ui.horizontal(|ui| {
            for i in 0..NUM_BANDS {
                ui.vertical(|ui| {
                    ui.label(BAND_LABELS[i]);
                    ui.small(format_freq(BAND_FREQS_HZ[i]));
                    // Vertical slider so five bands fit comfortably across the
                    // window without each column ballooning.
                    ui.add(
                        egui::Slider::new(&mut settings.bands[i].gain_db, GAIN_RANGE_DB.clone())
                            .vertical()
                            .suffix(" dB")
                            .show_value(true),
                    );
                    if ui
                        .small_button("0 dB")
                        .on_hover_text("Reset this band")
                        .clicked()
                    {
                        settings.bands[i].gain_db = 0.0;
                    }
                    let mut solo = settings.bands[i].solo;
                    if ui
                        .selectable_label(solo, "Solo")
                        .on_hover_text("Isolate this band; mutes the others")
                        .clicked()
                    {
                        solo = !solo;
                        // Mutual exclusion: clear other solos when this one
                        // turns on. When it turns off, just clear this one.
                        for (j, b) in settings.bands.iter_mut().enumerate() {
                            b.solo = if j == i { solo } else { false };
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });
    });

    // Clamp every band gain so a fat-finger DragValue paste can't exceed range.
    for b in &mut settings.bands {
        b.gain_db = b.gain_db.clamp(*GAIN_RANGE_DB.start(), *GAIN_RANGE_DB.end());
    }

    if *settings != before {
        engine.send(Command::SetEq(*settings));
    }
}

fn format_freq(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{:.1} kHz", hz / 1000.0)
    } else {
        format!("{} Hz", hz as u32)
    }
}
