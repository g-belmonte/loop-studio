use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use eframe::CreationContext;

use crate::audio::decoder;
use crate::engine::{Command, Engine};
use crate::track::Track;
use crate::ui::{menu, transport};

enum LoadStatus {
    Idle,
    Loading(PathBuf),
    Loaded { path: PathBuf, track: Arc<Track> },
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
    engine: Option<Engine>,
}

impl App {
    pub fn new(_cc: &CreationContext<'_>) -> Self {
        let (load_tx, load_rx) = unbounded();
        let engine = match Engine::spawn() {
            Ok(e) => Some(e),
            Err(e) => {
                log::error!("failed to spawn audio engine: {e:#}");
                None
            }
        };
        Self {
            status: LoadStatus::Idle,
            load_tx,
            load_rx,
            engine,
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
                            let track = Arc::new(track);
                            if let Some(engine) = &self.engine {
                                engine.send(Command::LoadTrack(track.clone()));
                            }
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

        // Repaint while decoding (spinner) or while a track is loaded (so the
        // playhead/seek slider update during playback).
        match self.status {
            LoadStatus::Loading(_) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            LoadStatus::Loaded { .. } => {
                ctx.request_repaint_after(Duration::from_millis(33));
            }
            _ => {}
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
                    ui.label(format!(
                        "{} Hz · {} ch · {} frames",
                        track.sample_rate,
                        track.channels,
                        track.frame_count()
                    ));
                    ui.add_space(8.0);
                    if let Some(engine) = &self.engine {
                        transport::show(ui, engine, track.sample_rate, track.frame_count());
                    } else {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            "Audio engine failed to start — check the log.",
                        );
                    }
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
