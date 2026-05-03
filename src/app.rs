use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use eframe::CreationContext;

use crate::audio::decoder;
use crate::track::Track;
use crate::ui::menu;

enum LoadStatus {
    Idle,
    Loading(PathBuf),
    Loaded { path: PathBuf, track: Track },
    Failed { path: PathBuf, error: String },
}

struct LoadResult {
    path: PathBuf,
    result: anyhow::Result<Track>,
}

pub struct App {
    status: LoadStatus,
    load_tx: Sender<LoadResult>,
    load_rx: Receiver<LoadResult>,
}

impl App {
    pub fn new(_cc: &CreationContext<'_>) -> Self {
        let (load_tx, load_rx) = unbounded();
        Self {
            status: LoadStatus::Idle,
            load_tx,
            load_rx,
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = menu::pick_audio_file() else {
            return;
        };
        self.spawn_decode(path, ctx.clone());
    }

    fn spawn_decode(&mut self, path: PathBuf, ctx: egui::Context) {
        self.status = LoadStatus::Loading(path.clone());
        let tx = self.load_tx.clone();
        thread::spawn(move || {
            let result = decoder::decode_file(&path);
            let _ = tx.send(LoadResult { path, result });
            ctx.request_repaint();
        });
    }

    fn drain_decode_results(&mut self) {
        loop {
            match self.load_rx.try_recv() {
                Ok(LoadResult { path, result }) => {
                    self.status = match result {
                        Ok(track) => {
                            log::info!(
                                "loaded {}: {} Hz, {} ch, {} frames",
                                path.display(),
                                track.sample_rate,
                                track.channels,
                                track.frame_count()
                            );
                            LoadStatus::Loaded { path, track }
                        }
                        Err(e) => {
                            log::warn!("failed to load {}: {e:#}", path.display());
                            LoadStatus::Failed {
                                path,
                                error: format!("{e:#}"),
                            }
                        }
                    };
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_decode_results();

        // Keep repainting while a decode is in flight so the spinner animates.
        if matches!(self.status, LoadStatus::Loading(_)) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open…").clicked() {
                        ui.close_menu();
                        self.open_dialog(ctx);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Loop Studio");
            ui.separator();

            match &self.status {
                LoadStatus::Idle => {
                    ui.label("No file loaded. Use File → Open… to pick an audio file.");
                }
                LoadStatus::Loading(path) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Decoding {}…", path.display()));
                    });
                }
                LoadStatus::Loaded { path, track } => {
                    ui.label(format!("File: {}", path.display()));
                    ui.label(format!("Sample rate: {} Hz", track.sample_rate));
                    ui.label(format!("Channels: {}", track.channels));
                    let frames = track.frame_count();
                    let secs = frames as f64 / track.sample_rate as f64;
                    ui.label(format!(
                        "Duration: {} ({} frames)",
                        format_duration(secs),
                        frames
                    ));
                }
                LoadStatus::Failed { path, error } => {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Failed to load file");
                    ui.label(format!("File: {}", path.display()));
                    ui.label(error);
                }
            }
        });
    }
}

fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let ms = ((secs - total as f64) * 1000.0).round() as u32;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}
