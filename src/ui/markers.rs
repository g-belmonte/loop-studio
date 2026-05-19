use crate::engine::{Command, Engine};
use crate::track::Marker;
use crate::ui::transport;

/// Render the markers side list: one row per marker with an index, a
/// timestamp button that seeks to it, an editable label, and a delete `✕`.
///
/// The list is the source of truth for delete and label edits; jumping by
/// timestamp is duplicated by the `1`–`9` and Ctrl+←/→ shortcuts.
pub fn show(
    ui: &mut egui::Ui,
    markers: &mut Vec<Marker>,
    sample_rate: u32,
    engine: &Engine,
) {
    ui.horizontal(|ui| {
        ui.label(format!("Markers ({})", markers.len()));
        ui.label("·  press M to add at playhead");
    });

    if markers.is_empty() {
        return;
    }

    let mut to_delete: Option<usize> = None;
    for (i, m) in markers.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.label(format!("{:>2}.", i + 1));
            if ui
                .button(transport::format_time(m.frame, sample_rate))
                .on_hover_text("Jump to marker")
                .clicked()
            {
                engine.send(Command::Seek(m.frame));
            }
            ui.add(
                egui::TextEdit::singleline(&mut m.label)
                    .hint_text("label")
                    .desired_width(220.0),
            );
            if ui.button("X").on_hover_text("Delete marker").clicked() {
                to_delete = Some(i);
            }
        });
    }
    if let Some(i) = to_delete {
        markers.remove(i);
    }
}
