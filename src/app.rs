use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::CreationContext;

use crate::analysis::bpm::{self, BpmStatus};
use crate::audio::decoder;
use crate::dsp::DspKind;
use crate::dsp::eq::EqSettings;
use crate::engine::metronome::MetronomeSettings;
use crate::engine::speed_ramp::SpeedRampSettings;
use crate::engine::{Command, Engine};
use crate::session::{Session, auto as autosession};
use crate::track::peaks::TrackPeaks;
use crate::track::{LoopRegion, Marker, NamedLoop, Track};
use crate::ui::loops::LoopsAction;
use crate::ui::metronome::{MetronomeAction, TapTempo};
use crate::ui::shortcuts::LoopEndpoint;
use crate::ui::waveform::{WaveformAction, WaveformView};
use crate::ui::{
    eq as eq_ui, loops as loops_ui, markers as marker_list, menu, metronome as metronome_ui,
    shortcuts, speed_ramp as speed_ramp_ui, transport, waveform,
};

/// Debounce interval for per-track auto-save. A loaded track is checked once
/// per interval; the file is rewritten only when the serialised state actually
/// changed since the last write (so a paused track or static settings don't
/// keep churning the disk).
const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);

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

/// Result of a one-shot BPM-detection worker. The path lets `App` discard
/// stale results that landed after the user moved on to a different track.
struct BpmResult {
    path: PathBuf,
    bpm: Option<f32>,
}

/// State to apply *after* an in-flight decode finishes, when the user opened
/// a session rather than a bare audio file. Held on the GUI thread; consumed
/// by `drain_decode_results` once the matching `LoadResult` lands.
struct PendingRestore {
    path: PathBuf,
    loops: Vec<NamedLoop>,
    active_loop: Option<usize>,
    speed: f32,
    pitch_semitones: f32,
    last_position: u64,
    dsp_kind: DspKind,
    markers: Vec<Marker>,
    metronome: MetronomeSettings,
    eq: EqSettings,
    speed_ramp: SpeedRampSettings,
}

pub struct App {
    status: LoadStatus,
    load_tx: Sender<LoadResult>,
    load_rx: Receiver<LoadResult>,
    engine: Option<Engine>,
    /// Engine-facing loop region — what the worker is actually looping. May
    /// reflect a saved loop (when `active_loop` is `Some`) or a one-off
    /// region the user hasn't saved yet.
    loop_region: Option<LoopRegion>,
    /// Saved loop slots (UI source of truth). Slot N (1-indexed) =
    /// `loops[N-1]`, addressed by `Shift+Num1..Num9`. Reset on track load;
    /// restored from `PendingRestore` on session load.
    loops: Vec<NamedLoop>,
    /// Index into `loops` of the slot currently driving `loop_region`. `None`
    /// when no slot is selected (either no active loop or an unsaved region).
    /// Edits to `loop_region` while `active_loop` is `Some` auto-sync the
    /// matching slot so dragging endpoints on a saved loop tweaks it in place.
    active_loop: Option<usize>,
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
    /// Pitch split (UI source of truth). The engine sees only the combined
    /// `coarse + cents / 100.0` semitones via `Command::SetPitch`; holding
    /// the two halves here keeps the sliders independent across edits and
    /// across the "0 st" / "0 ct" reset buttons.
    pitch_coarse: i32,
    pitch_cents: i32,
    /// Master output gain in dB (UI source of truth). Default 0 dB (unity).
    /// Not persisted in sessions — resets on every app launch.
    master_volume_db: f32,
    /// Metronome settings (UI source of truth — engine sees a copy via
    /// `Command::SetMetronome` on every change). Persisted in sessions.
    metronome: MetronomeSettings,
    /// EQ settings (UI source of truth — engine gets a copy via
    /// `Command::SetEq` on every change). Persisted in sessions (v5+).
    eq: EqSettings,
    /// Speed-ramp settings (UI source of truth — engine gets a copy via
    /// `Command::SetSpeedRamp`). Persisted in sessions (v6+). The engine
    /// owns the per-step counter; the UI never reads it back.
    speed_ramp: SpeedRampSettings,
    /// Tap-tempo accumulator. Purely GUI; doesn't survive across runs.
    tap_tempo: TapTempo,
    /// Visible window into the waveform. Reset to full-track on every load
    /// (not persisted in sessions — it's a viewport, not session state).
    view: WaveformView,
    /// When true, the view pages forward to keep the playhead in frame during
    /// playback. Toggled from the UI; persists across track loads.
    follow_playhead: bool,
    /// Last time the auto-saver fired its periodic tick. Compared against
    /// `AUTOSAVE_INTERVAL` to debounce writes during playback.
    last_autosave_at: Option<Instant>,
    /// Serialised JSON of the last successfully written autosession. Used as
    /// a cheap dirty-check: if `build_session()` re-serialises to the same
    /// bytes, the on-disk file is already up to date and we skip the write.
    last_autosave_json: Option<String>,
    /// BPM detection state for the currently loaded track. Reset to `Idle`
    /// on every load; never persisted.
    bpm_status: BpmStatus,
    bpm_tx: Sender<BpmResult>,
    bpm_rx: Receiver<BpmResult>,
}

impl App {
    pub fn new(_cc: &CreationContext<'_>) -> Self {
        let (load_tx, load_rx) = unbounded();
        let (bpm_tx, bpm_rx) = unbounded();
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
            loops: Vec::new(),
            active_loop: None,
            dsp_kind: DspKind::default(),
            pending_restore: None,
            session_error: None,
            pending_loop: None,
            markers: Vec::new(),
            pitch_coarse: 0,
            pitch_cents: 0,
            master_volume_db: 0.0,
            metronome: MetronomeSettings::default(),
            eq: EqSettings::default(),
            speed_ramp: SpeedRampSettings::default(),
            tap_tempo: TapTempo::new(),
            view: WaveformView::full(0),
            follow_playhead: true,
            last_autosave_at: None,
            last_autosave_json: None,
            bpm_status: BpmStatus::Idle,
            bpm_tx,
            bpm_rx,
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = menu::pick_audio_file() else {
            return;
        };
        // Auto-restore: if we've seen this track before, queue its saved
        // state so `drain_decode_results` applies it once decode lands.
        // An explicit Load Session... already populates `pending_restore`,
        // so we don't overwrite it here.
        if self.pending_restore.is_none()
            && let Some(session) = autosession::load_for(&path)
        {
            self.pending_restore = Some(PendingRestore {
                path: path.clone(),
                loops: session.loops,
                active_loop: session.active_loop,
                speed: session.speed,
                pitch_semitones: session.pitch_semitones,
                last_position: session.last_position,
                dsp_kind: session.dsp_kind,
                markers: session.markers,
                metronome: session.metronome,
                eq: session.eq,
                speed_ramp: session.speed_ramp,
            });
        }
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

    /// Drain BPM-detection results from the worker channel. Stale results
    /// (path mismatch — the user switched tracks while detection was running)
    /// are dropped on the floor; only a result whose path matches the current
    /// `Loaded` track updates `bpm_status`.
    fn drain_bpm_results(&mut self) {
        while let Ok(BpmResult { path, bpm }) = self.bpm_rx.try_recv() {
            let matches = matches!(&self.status, LoadStatus::Loaded { path: cur, .. } if cur == &path);
            if !matches {
                continue;
            }
            self.bpm_status = match bpm {
                Some(b) => BpmStatus::Done(b),
                None => BpmStatus::Failed,
            };
        }
    }

    /// Kick off a BPM-detection worker for the currently loaded track over the
    /// active loop region (or the whole track if no loop is set). No-op when
    /// no track is loaded or a detection is already running.
    fn spawn_bpm_detect(&mut self) {
        if matches!(self.bpm_status, BpmStatus::Running) {
            return;
        }
        let LoadStatus::Loaded { path, track, .. } = &self.status else {
            return;
        };
        let path = path.clone();
        let track = track.clone();
        let (start, end) = match self.loop_region {
            Some(r) => (r.start, r.end),
            None => (0, track.frame_count()),
        };
        self.bpm_status = BpmStatus::Running;
        let tx = self.bpm_tx.clone();
        thread::spawn(move || {
            let bpm = bpm::detect_bpm(&track, start, end);
            let _ = tx.send(BpmResult { path, bpm });
        });
    }

    fn drain_decode_results(&mut self) {
        while let Ok(LoadResult { path, result }) = self.load_rx.try_recv() {
            // Flush the outgoing track's state before we hand the engine a
            // new track and reset our UI fields. `autosave_flush` reads the
            // engine state for whichever track is *currently* loaded — at
            // this point that's still the old one.
            self.autosave_flush();
            // The new track starts fresh as far as the dirty-check goes.
            // Setting `last_autosave_at` to now (rather than None) delays the
            // first save by AUTOSAVE_INTERVAL so the engine has time to apply
            // the queued LoadTrack / SetSpeed / SetPitch / Seek / SetLoop
            // commands before we sample its state.
            self.last_autosave_json = None;
            self.last_autosave_at = Some(Instant::now());
            // Any [/] press from a previous track has no meaning here.
            self.pending_loop = None;
            // Detection isn't persisted — every new track starts unanalysed.
            // A late result for the old track is ignored in `drain_bpm_results`.
            self.bpm_status = BpmStatus::Idle;
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

                    // Default: clear loops + active region. Overridden below
                    // if a pending session restore matches this path.
                    let mut new_loop = None;
                    self.loops.clear();
                    self.active_loop = None;
                    self.view = WaveformView::full(track.frame_count());

                    if let Some(engine) = &self.engine {
                        engine.send(Command::LoadTrack(track.clone()));
                    }

                    // If the pending restore is for a different path, take() still
                    // consumes it — that's the discard.
                    if let Some(pending) = self.pending_restore.take()
                        && pending.path == path
                    {
                        let total = track.frame_count();
                        self.loops = clamp_loops(pending.loops, total);
                        self.active_loop = clamp_active_loop(pending.active_loop, &self.loops);
                        new_loop = self.active_loop.map(|i| self.loops[i].region());
                        let last_pos = pending.last_position.min(total);
                        self.dsp_kind = pending.dsp_kind;
                        self.markers = clamp_markers(pending.markers, total);
                        self.metronome = pending.metronome;
                        self.eq = pending.eq;
                        self.speed_ramp = pending.speed_ramp;
                        (self.pitch_coarse, self.pitch_cents) =
                            split_pitch(pending.pitch_semitones);
                        if let Some(engine) = &self.engine {
                            // SetDsp first so the rebuilt processor
                            // receives the new speed/pitch directly.
                            // SetMetronome before SetLoop so the engine's
                            // metronome settings are in place when SetLoop
                            // updates the anchor (and not before LoadTrack,
                            // which resets the anchor on its own).
                            // SetSpeedRamp lands before SetLoop too so the
                            // engine's first wrap on this track is counted
                            // against the restored ramp config.
                            engine.send(Command::SetDsp(pending.dsp_kind));
                            engine.send(Command::SetSpeed(pending.speed));
                            engine.send(Command::SetPitch(pending.pitch_semitones));
                            engine.send(Command::SetMetronome(pending.metronome));
                            engine.send(Command::SetSpeedRamp(pending.speed_ramp));
                            engine.send(Command::SetLoop(new_loop));
                            engine.send(Command::Seek(last_pos));
                        }
                    }

                    // The worker recreates its EQ on every LoadTrack
                    // (per-channel biquad state + sr-dependent coefficients),
                    // which resets it to bypass. Re-push App's EQ state — the
                    // restore branch above updated self.eq if a session was
                    // applied; otherwise this carries the user's current
                    // EQ across the load, matching how speed/pitch carry via
                    // SharedState and how the metronome carries via its own
                    // persistent worker state.
                    if let Some(engine) = &self.engine {
                        engine.send(Command::SetEq(self.eq));
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
    }

    /// Snapshot the current loaded track + engine state into a `Session`.
    /// Returns `None` when there's nothing to save (no track loaded, or the
    /// engine failed to start). Shared by manual save and auto-save.
    fn build_session(&self) -> Option<(PathBuf, Session)> {
        let LoadStatus::Loaded {
            path: track_path,
            track,
            ..
        } = &self.status
        else {
            return None;
        };
        let engine = self.engine.as_ref()?;
        let state = engine.state();
        let session = Session {
            version: Session::CURRENT_VERSION,
            track_path: track_path.clone(),
            track_sample_rate: track.sample_rate,
            loops: self.loops.clone(),
            active_loop: self.active_loop,
            speed: f32::from_bits(state.speed_bits.load(Ordering::Relaxed)),
            pitch_semitones: f32::from_bits(state.pitch_bits.load(Ordering::Relaxed)),
            last_position: state.position.load(Ordering::Relaxed),
            dsp_kind: self.dsp_kind,
            markers: self.markers.clone(),
            metronome: self.metronome,
            eq: self.eq,
            speed_ramp: self.speed_ramp,
        };
        Some((track_path.clone(), session))
    }

    fn save_session(&mut self) {
        let Some((track_path, session)) = self.build_session() else {
            return;
        };

        let default_name = track_path
            .file_stem()
            .map(|s| format!("{}.session.json", s.to_string_lossy()))
            .unwrap_or_else(|| "session.json".into());
        let default_dir = track_path.parent();

        let Some(save_path) = menu::pick_session_save(&default_name, default_dir) else {
            return;
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

    /// Periodic auto-save tick. Called every frame; bails out fast when
    /// the debounce hasn't elapsed or nothing has changed.
    fn autosave_tick(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_autosave_at
            && now.duration_since(prev) < AUTOSAVE_INTERVAL
        {
            return;
        }
        self.last_autosave_at = Some(now);
        self.autosave_flush();
    }

    /// Write the current state to the per-track autosession file if it
    /// differs from the last write. Safe to call when no track is loaded
    /// (it just no-ops).
    fn autosave_flush(&mut self) {
        let Some((track_path, session)) = self.build_session() else {
            return;
        };
        let json = match serde_json::to_string_pretty(&session) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("autosession serialise failed: {e:#}");
                return;
            }
        };
        if self.last_autosave_json.as_deref() == Some(json.as_str()) {
            return;
        }
        match autosession::save_for(&track_path, &session) {
            Ok(()) => {
                self.last_autosave_json = Some(json);
            }
            Err(e) => {
                log::warn!("autosession write failed: {e:#}");
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
            loops: session.loops,
            active_loop: session.active_loop,
            speed: session.speed,
            pitch_semitones: session.pitch_semitones,
            last_position: session.last_position,
            dsp_kind: session.dsp_kind,
            markers: session.markers,
            metronome: session.metronome,
            eq: session.eq,
            speed_ramp: session.speed_ramp,
        });
        self.spawn_decode(session.track_path, ctx.clone());
    }
}

/// Clamp markers from a session to the loaded track. Drops any whose frame
/// is past end-of-track, sorts by frame, and dedupes exact-frame collisions
/// (keeping the first label).
/// Decompose total semitones into the (coarse st, cents) pair the UI shows.
/// Round-to-nearest keeps cents in `[-50, 50]` for any total in `[-12, 12]`;
/// used once at session load and never again (the UI state is sticky after).
fn split_pitch(total_semitones: f32) -> (i32, i32) {
    let total = if total_semitones.is_finite() {
        total_semitones
    } else {
        0.0
    };
    let coarse = (total.round() as i32).clamp(-12, 12);
    let cents = (((total - coarse as f32) * 100.0).round() as i32).clamp(-50, 50);
    (coarse, cents)
}

fn clamp_markers(mut markers: Vec<Marker>, total_frames: u64) -> Vec<Marker> {
    markers.retain(|m| m.frame < total_frames);
    markers.sort_by_key(|m| m.frame);
    markers.dedup_by_key(|m| m.frame);
    markers
}

/// Clamp saved loops to the loaded track. Each loop is end-clamped to
/// `total_frames`; any loop that starts past EOF or collapses to start ≥ end
/// after clamping is dropped. Order is preserved so slot indices the user
/// memorised stay stable across reopens (when nothing was clamped away).
fn clamp_loops(mut loops: Vec<NamedLoop>, total_frames: u64) -> Vec<NamedLoop> {
    loops.retain_mut(|l| {
        if l.start >= total_frames {
            return false;
        }
        l.end = l.end.min(total_frames);
        l.start < l.end
    });
    loops
}

/// Drop a restored `active_loop` index if it's out of range for the
/// post-clamp `loops` vector.
fn clamp_active_loop(active: Option<usize>, loops: &[NamedLoop]) -> Option<usize> {
    active.filter(|&i| i < loops.len())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_decode_results();
        self.drain_bpm_results();
        self.autosave_tick();
        // Surfaced from inside the central panel's closure (which holds a
        // mutable borrow of `self`); acted on after the borrow ends.
        let mut metronome_action = MetronomeAction::None;

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
                &mut self.loops,
                &mut self.active_loop,
                &mut self.markers,
                &mut self.view,
                &mut self.metronome,
                &mut self.tap_tempo,
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
                        let total = track.frame_count();
                        if self.follow_playhead {
                            waveform::follow_playhead(&mut self.view, total, position);
                        }
                        let action = waveform::show(
                            ui,
                            peaks,
                            position,
                            total,
                            self.loop_region,
                            &self.markers,
                            &mut self.view,
                            160.0,
                        );
                        match action {
                            WaveformAction::None => {}
                            WaveformAction::Seek(frame) => {
                                engine.send(Command::Seek(frame));
                            }
                            WaveformAction::SetLoop(region) => {
                                self.loop_region = Some(region);
                                // Defining a region detaches from any saved
                                // slot — the slot is unchanged, and the user
                                // is now on a fresh unsaved region they can
                                // re-Save explicitly.
                                self.active_loop = None;
                                engine.send(Command::SetLoop(Some(region)));
                            }
                        }

                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.follow_playhead, "Follow playhead");
                            let zoomed = self.view.len < total;
                            if ui
                                .add_enabled(zoomed, egui::Button::new("Reset zoom"))
                                .clicked()
                            {
                                self.view = WaveformView::full(total);
                            }
                            ui.label(format!(
                                "View: {} → {}",
                                transport::format_time(self.view.start, track.sample_rate),
                                transport::format_time(
                                    self.view.end().min(total),
                                    track.sample_rate
                                ),
                            ));
                        });

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
                                    self.active_loop = None;
                                    engine.send(Command::SetLoop(None));
                                }
                            });
                        }

                        ui.add_space(8.0);
                        let transport_result = transport::show(
                            ui,
                            engine,
                            &mut self.dsp_kind,
                            &mut self.pitch_coarse,
                            &mut self.pitch_cents,
                            &mut self.master_volume_db,
                            track.sample_rate,
                            track.frame_count(),
                        );
                        // Manual speed override disables an in-progress ramp:
                        // the user's intent wins. Push the disabled state to
                        // the engine so it stops counting wraps too.
                        if transport_result.speed_user_changed && self.speed_ramp.enabled {
                            self.speed_ramp.enabled = false;
                            engine.send(Command::SetSpeedRamp(self.speed_ramp));
                        }

                        ui.add_space(8.0);
                        speed_ramp_ui::show(ui, &mut self.speed_ramp, engine);

                        ui.add_space(8.0);
                        ui.separator();
                        metronome_action = metronome_ui::show(
                            ui,
                            &mut self.metronome,
                            &mut self.tap_tempo,
                            engine,
                            &self.bpm_status,
                        );

                        ui.add_space(8.0);
                        ui.separator();
                        eq_ui::show(ui, &mut self.eq, engine);

                        ui.add_space(8.0);
                        ui.separator();
                        let loops_action = loops_ui::show(
                            ui,
                            &mut self.loops,
                            self.active_loop,
                            self.loop_region,
                            track.sample_rate,
                        );
                        match loops_action {
                            LoopsAction::None => {}
                            LoopsAction::Activate(i) => {
                                if let Some(l) = self.loops.get(i) {
                                    let region = l.region();
                                    self.loop_region = Some(region);
                                    self.active_loop = Some(i);
                                    self.pending_loop = None;
                                    engine.send(Command::SetLoop(Some(region)));
                                }
                            }
                            LoopsAction::Delete(i) => {
                                if i < self.loops.len() {
                                    self.loops.remove(i);
                                    // Maintain active_loop pointing at the
                                    // same slot conceptually: drop it if the
                                    // deleted row was active, shift it down if
                                    // a slot before the active one disappeared.
                                    self.active_loop = match self.active_loop {
                                        Some(a) if a == i => None,
                                        Some(a) if a > i => Some(a - 1),
                                        other => other,
                                    };
                                    if self.active_loop.is_none() {
                                        self.loop_region = None;
                                        engine.send(Command::SetLoop(None));
                                    }
                                }
                            }
                            LoopsAction::SaveCurrent => {
                                if let Some(r) = self.loop_region
                                    && self.loops.len() < 9
                                {
                                    self.loops.push(NamedLoop {
                                        start: r.start,
                                        end: r.end,
                                        label: String::new(),
                                    });
                                    self.active_loop = Some(self.loops.len() - 1);
                                }
                            }
                        }

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

        if metronome_action == MetronomeAction::DetectBpm {
            self.spawn_bpm_detect();
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Final autosave on clean shutdown so unsaved state between
        // `AUTOSAVE_INTERVAL` ticks isn't lost.
        self.autosave_flush();
    }
}
