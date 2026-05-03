use eframe::CreationContext;

pub struct App {
    // Engine handle, shared state, UI state will live here.
    // Kept empty for now so the project compiles end-to-end.
}

impl App {
    pub fn new(_cc: &CreationContext<'_>) -> Self {
        Self {}
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Loop Studio");
            ui.label("Pre-MVP scaffold. See ARCHITECTURE.md.");
        });
    }
}
