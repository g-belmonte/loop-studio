use crate::track::{LoopRegion, NamedLoop};
use crate::ui::transport;

/// Action returned by the loops panel that requires mutating state outside the
/// panel's `&mut` borrows (engine sends, `loop_region` updates). `App` applies
/// these after the central-panel closure returns.
#[derive(Debug, Clone, Copy)]
pub enum LoopsAction {
    None,
    /// Activate slot `index`: load it into `loop_region` and push to engine.
    Activate(usize),
    /// Remove slot `index`. If it was active, deactivate the loop region too.
    Delete(usize),
    /// Save the current `loop_region` as a new slot.
    SaveCurrent,
}

pub fn show(
    ui: &mut egui::Ui,
    loops: &mut [NamedLoop],
    active_loop: Option<usize>,
    current: Option<LoopRegion>,
    sample_rate: u32,
) -> LoopsAction {
    let mut action = LoopsAction::None;

    ui.horizontal(|ui| {
        ui.label(format!("Loops ({})", loops.len()));
        ui.label("·  Shift+1..9 to activate");
        let can_save = current.is_some() && loops.len() < 9;
        let tooltip = if loops.len() >= 9 {
            "Slot limit reached (9)"
        } else if current.is_none() {
            "No active loop region"
        } else {
            "Save current loop region as a new slot"
        };
        if ui
            .add_enabled(can_save, egui::Button::new("Save current loop"))
            .on_hover_text(tooltip)
            .clicked()
        {
            action = LoopsAction::SaveCurrent;
        }
    });

    if loops.is_empty() {
        return action;
    }

    for (i, l) in loops.iter_mut().enumerate() {
        let is_active = active_loop == Some(i);
        ui.horizontal(|ui| {
            let marker = if is_active { "●" } else { " " };
            ui.label(format!("{marker} {:>1}.", i + 1));
            let label = format!(
                "{} → {}",
                transport::format_time(l.start, sample_rate),
                transport::format_time(l.end, sample_rate),
            );
            if ui
                .selectable_label(is_active, label)
                .on_hover_text("Activate loop")
                .clicked()
                && !is_active
            {
                action = LoopsAction::Activate(i);
            }
            ui.add(
                egui::TextEdit::singleline(&mut l.label)
                    .hint_text("label")
                    .desired_width(180.0),
            );
            if ui.button("X").on_hover_text("Delete loop").clicked() {
                action = LoopsAction::Delete(i);
            }
        });
    }

    action
}
