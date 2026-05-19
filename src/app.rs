use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use eframe::CreationContext;

use crate::audio::decoder;
use crate::dsp::DspKind;
use crate::engine::{Command, Engine};
use crate::session::Session;
use crate::track::peaks::TrackPeaks;
use crate::track::{LoopRegion, Marker, Track};
use crate::ui::shortcuts::LoopEndpoint;
use crate::ui::waveform::WaveformAction;
use crate::ui::{markers as marker_list, menu, shortcuts, transport, waveform};

enum LoadStatus {
    Idle,
    Loading(PathBuf),
    Loaded {
        path: PathBuf,
        track: Arc<Track>,
        peaks: Arc<TrackPeaks>,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

struct LoadResult {
    path: PathBuf,
    result: anyhow::Result<(Arc<Track>, Arc<TrackPeaks>)>,
}

/// State to apply *after* an in-flight decode finishes, when the user opened
/// a session rather than a bare audio file. Held on the GUI thread; consumed
/// by `drain_decode_results` once the matching `LoadResult` lands.
struct PendingRestore {
    path: PathBuf,
    loop_region: Option<LoopRegion>,
    speed: f32,
    pitch_semitones: f32,
    last_position: u64,
    dsp_kind: DspKind,
    markers: Vec<Marker>,
}

pub struct App {
    status: LoadStatus,
    load_tx: Sender<LoadResult>,
    load_rx: Receiver<LoadResult>,
    engine: Option<Engine>,
    loop_region: Option<LoopRegion>,
    /// Currently-selected DSP family (UI source of truth, like `loop_region`).
    /// Survives across track loads; sent to the engine via `Command::SetDsp`
    /// on user change or session restore.
    dsp_kind: DspKind,
    pending_restore: Option<PendingRestore>,
    session_error: Option<String>,
    /// Half-defined loop from a `[` or `]` press waiting for its partner.
    /// Reset whenever a track loads, the loop is cleared via Esc, or the
    /// loop is committed. See `ui::shortcuts`.
    pending_loop: Option<LoopEndpoint>,
    /// Navigation markers for the loaded track, kept sorted by `frame`.
    /// UI source of truth — the engine never reads markers. Reset on track
    /// load; restored from `PendingRestore` on session load.
    markers: Vec<Marker>,
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
            loop_region: None,
            dsp_kind: DspKind::default(),
            pending_restore: None,
            session_error: None,
            pending_loop: None,
            markers: Vec::new(),
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
            let result = decoder::decode_file(&path).map(|track| {
                let peaks = Arc::new(TrackPeaks::compute(&track));
                (Arc::new(track), peaks)
            });
            let _ = tx.send(LoadResult { path, result });
            ctx.request_repaint();
        });
    }

    fn drain_decode_results(&mut self) {
        loop {
            match self.load_rx.try_recv() {
                Ok(LoadResult { path, result }) => {
                    // Any [/] press from a previous track has no meaning here.
                    self.pending_loop = None;
                    // Clear markers by default; the session-restore branch below
                    // repopulates them if the matching pending restore lands.
                    self.markers.clear();
                    self.status = match result {
                        Ok((track, peaks)) => {
                            log::info!(
                                "loaded {}: {} Hz, {} ch, {} frames, {} peak buckets",
                                path.display(),
                                track.sample_rate,
                                track.channels,
                                track.frame_count(),
                                peaks.len(),
                            );

                            // Default: clear the loop region. Overridden below
                            // if a pending session restore matches this path.
                            let mut new_loop = None;

                            if let Some(engine) = &self.engine {
                                engine.send(Command::LoadTrack(track.clone()));
                            }

                            if let Some(pending) = self.pending_restore.take() {
                                if pending.path == path {
                                    let total = track.frame_count();
                                    new_loop = clamp_loop(pending.loop_region, total);
                                    let last_pos = pending.last_position.min(total);
                                    self.dsp_kind = pending.dsp_kind;
                                    self.markers = clamp_markers(pending.markers, total);
                                    if let Some(engine) = &self.engine {
                                        // SetDsp first so the rebuilt processor
                                        // receives the new speed/pitch directly.
                                        engine.send(Command::SetDsp(pending.dsp_kind));
                                        engine.send(Command::SetSpeed(pending.speed));
                                        engine.send(Command::SetPitch(
                                            pending.pitch_semitones,
                                        ));
                                        engine.send(Command::SetLoop(new_loop));
                                        engine.send(Command::Seek(last_pos));
                                    }
                                }
                                // else: a different file landed first — discard.
                            }

                            self.loop_region = new_loop;
                            LoadStatus::Loaded { path, track, peaks }
                        }
                        Err(e) => {
                            log::warn!("failed to load {}: {e:#}", path.display());
                            // A pending session restore for this path won't ever
                            // resolve — drop it so a later open doesn't pick it up.
                            if matches!(&self.pending_restore, Some(p) if p.path == path) {
                                self.pending_restore = None;
                            }
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

    fn save_session(&mut self) {
        let LoadStatus::Loaded {
            path: track_path,
            track,
            ..
        } = &self.status
        else {
            return;
        };
        let Some(engine) = &self.engine else { return };

        let default_name = track_path
            .file_stem()
            .map(|s| format!("{}.session.json", s.to_string_lossy()))
            .unwrap_or_else(|| "session.json".into());
        let default_dir = track_path.parent();

        let Some(save_path) = menu::pick_session_save(&default_name, default_dir) else {
            return;
        };

        let state = engine.state();
        let session = Session {
            version: Session::CURRENT_VERSION,
            track_path: track_path.clone(),
            track_sample_rate: track.sample_rate,
            loop_region: self.loop_region,
            speed: f32::from_bits(state.speed_bits.load(Ordering::Relaxed)),
            pitch_semitones: f32::from_bits(state.pitch_bits.load(Ordering::Relaxed)),
            last_position: state.position.load(Ordering::Relaxed),
            dsp_kind: self.dsp_kind,
            markers: self.markers.clone(),
        };

        match session.save(&save_path) {
            Ok(()) => {
                log::info!("saved session to {}", save_path.display());
                self.session_error = None;
            }
            Err(e) => {
                log::warn!("failed to save session: {e:#}");
                self.session_error = Some(format!("Save failed: {e:#}"));
            }
        }
    }

    fn load_session(&mut self, ctx: &egui::Context) {
        let Some(json_path) = menu::pick_session_open() else {
            return;
        };

        let session = match Session::load(&json_path) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("failed to load session from {}: {e:#}", json_path.display());
                self.session_error = Some(format!("Load failed: {e:#}"));
                return;
            }
        };

        self.session_error = None;
        self.pending_restore = Some(PendingRestore {
            path: session.track_path.clone(),
            loop_region: session.loop_region,
            speed: session.speed,
            pitch_semitones: session.pitch_semitones,
            last_position: session.last_position,
            dsp_kind: session.dsp_kind,
            markers: session.markers,
        });
        self.spawn_decode(session.track_path, ctx.clone());
    }
}

/// Clamp markers from a session to the loaded track. Drops any whose frame
/// is past end-of-track, sorts by frame, and dedupes exact-frame collisions
/// (keeping the first label).
fn clamp_markers(mut markers: Vec<Marker>, total_frames: u64) -> Vec<Marker> {
    markers.retain(|m| m.frame < total_frames);
    markers.sort_by_key(|m| m.frame);
    markers.dedup_by_key(|m| m.frame);
    markers
}

/// Clamp a saved loop region to the loaded track. Drops the loop if the
/// region is degenerate (start ≥ end after clamp) or starts past end-of-track.
fn clamp_loop(region: Option<LoopRegion>, total_frames: u64) -> Option<LoopRegion> {
    let r = region?;
    if r.start >= total_frames {
        return None;
    }
    let end = r.end.min(total_frames);
    if r.start >= end {
        return None;
    }
    Some(LoopRegion { start: r.start, end })
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

        // Global keyboard shortcuts. Runs before widget drawing so
        // `consume_key` keeps arrow keys away from the seek slider when both
        // could react to the same press.
        if let (Some(engine), LoadStatus::Loaded { track, .. }) = (&self.engine, &self.status) {
            let sr = track.sample_rate;
            let total = track.frame_count();
            shortcuts::handle(
                ctx,
                engine,
                sr,
                total,
                &mut self.loop_region,
                &mut self.pending_loop,
                &mut self.markers,
            );
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open…").clicked() {
                        ui.close_menu();
                        self.open_dialog(ctx);
                    }
                    ui.separator();
                    let track_loaded = matches!(self.status, LoadStatus::Loaded { .. });
                    if ui
                        .add_enabled(track_loaded, egui::Button::new("Save Session…"))
                        .clicked()
                    {
                        ui.close_menu();
                        self.save_session();
                    }
                    if ui.button("Load Session…").clicked() {
                        ui.close_menu();
                        self.load_session(ctx);
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Loop Studio");
            ui.separator();

            if let Some(err) = &self.session_error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
                ui.add_space(4.0);
            }

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
                LoadStatus::Loaded { path, track, peaks } => {
                    ui.label(format!("File: {}", path.display()));
                    ui.label(format!(
                        "{} Hz · {} ch · {} frames",
                        track.sample_rate,
                        track.channels,
                        track.frame_count()
                    ));
                    ui.add_space(8.0);

                    if let Some(engine) = &self.engine {
                        let position = engine.state().position.load(Ordering::Relaxed);
                        let action = waveform::show(
                            ui,
                            peaks,
                            position,
                            track.frame_count(),
                            self.loop_region,
                            &self.markers,
                            160.0,
                        );
                        match action {
                            WaveformAction::None => {}
                            WaveformAction::Seek(frame) => {
                                engine.send(Command::Seek(frame));
                            }
                            WaveformAction::SetLoop(region) => {
                                self.loop_region = Some(region);
                                engine.send(Command::SetLoop(Some(region)));
                            }
                        }

                        if let Some(r) = self.loop_region {
                            ui.horizontal(|ui| {
                                ui.label(format!(
                                    "Loop: {} → {}  ({})",
                                    transport::format_time(r.start, track.sample_rate),
                                    transport::format_time(r.end, track.sample_rate),
                                    transport::format_time(
                                        r.end.saturating_sub(r.start),
                                        track.sample_rate
                                    ),
                                ));
                                if ui.button("Clear loop").clicked() {
                                    self.loop_region = None;
                                    self.pending_loop = None;
                                    engine.send(Command::SetLoop(None));
                                }
                            });
                        }

                        ui.add_space(8.0);
                        transport::show(
                            ui,
                            engine,
                            &mut self.dsp_kind,
                            track.sample_rate,
                            track.frame_count(),
                        );

                        ui.add_space(8.0);
                        ui.separator();
                        marker_list::show(ui, &mut self.markers, track.sample_rate, engine);
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
