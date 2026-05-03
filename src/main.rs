mod app;
mod audio;
mod dsp;
mod engine;
mod session;
mod track;
mod ui;

use anyhow::Result;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 600.0])
            .with_title("Loop Studio"),
        ..Default::default()
    };

    eframe::run_native(
        "Loop Studio",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
