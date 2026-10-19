//! The egui GUI: library, categories, playback controls and full settings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Align, Color32, Layout, RichText};

use crate::audio::engine::{
    Engine, EngineEvent, EngineMsg, PlayRequest, BASS_MAX_DB, BASS_MIN_DB, PRELOAD_MAX_SECS,
};
use crate::audio::{mic, scan};
use crate::config::{hotkey, Config, Route, RouteOverride, Sort, Sound};
use crate::i18n::{tr, K, Lang};
use crate::ipc::{IpcToUi, PlayEntry, Snapshot};
use crate::niri;
use crate::portal::{PortalEvent, PortalHandle};

// Palette: graphite surfaces, one violet accent, red only for stopping.
const BG: Color32 = Color32::from_rgb(0x0e, 0x0f, 0x14);
const SURFACE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1f);
const SURFACE2: Color32 = Color32::from_rgb(0x1d, 0x20, 0x2b);
const HOVER_BG: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x26);
const PLAYING_BG: Color32 = Color32::from_rgb(0x1f, 0x1b, 0x36);
const BORDER: Color32 = Color32::from_rgb(0x26, 0x29, 0x36);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe9, 0xf0);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x8f, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x8f, 0x7a, 0xff);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x3d, 0x35, 0x66);
const OK: Color32 = Color32::from_rgb(0x46, 0xc3, 0x7b);
const DANGER: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
const STOP_IDLE: Color32 = Color32::from_rgb(0x4a, 0x26, 0x2c);
const ROW_H: f32 = 56.0;
const SIDE_W: f32 = 252.0;
const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "ogg", "oga", "opus", "flac", "m4a", "m4b", "aac", "aiff", "caf", "wv",
];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Events from background tasks (scans, pactl work, niri insert).
pub enum UiBgEvent {
    Scan(scan::ScanResult),
    MicState { ready: bool, passthrough: bool, source: bool },
    NiriResult(Result<String, String>),
    AddPaths(Vec<String>),
    /// Is anything capturing from the virtual mic?
    MicCapture(bool),
}

/// Sender for background results that also wakes the UI: egui only runs
/// `update` on input or repaint, so without this results sat unseen.
#[derive(Clone)]
pub struct BgSender {
    tx: Sender<UiBgEvent>,
    ctx: egui::Context,
}

impl BgSender {
    fn send(&self, ev: UiBgEvent) -> Result<(), mpsc::SendError<UiBgEvent>> {
        let r = self.tx.send(ev);
        self.ctx.request_repaint();
        r
    }
}

#[derive(Clone, PartialEq)]
enum SideSelection {
    All,
    Category(String),
    Recent,
}

#[derive(Clone, Copy, PartialEq)]
enum Capture {
    Sound(u64),
    StopAll,
}

#[derive(Clone)]
struct EditSound {
    id: u64,
    name: String,
    category: String,
    new_category: String,
    hotkey: Option<String>,
    volume: f32,
    speed: f32,
    route: RouteOverride,
    loop_: bool,
    normalize: bool,
    stop_others: Option<bool>,
    path: String,
}

impl EditSound {
    fn from(s: &Sound) -> Self {
        EditSound {
            id: s.id,
            name: s.name.clone(),
            category: if s.category.is_empty() {
                "General".into()
            } else {
                s.category.clone()
            },
            new_category: String::new(),
            hotkey: s.hotkey.clone(),
            volume: s.volume,
            speed: s.speed,
            route: s.route,
            loop_: s.loop_,
            normalize: s.normalize,
            stop_others: s.stop_others,
            path: s.path.clone(),
        }
    }
}

#[derive(Clone, PartialEq)]
enum PendingDelete {
    Sound(u64),
    Category(String),
}

pub struct AstraApp {
    ctx: egui::Context,
    lang: Lang,
    cfg: Config,
    cfg_path: PathBuf,
    dirty: bool,

    engine: Engine,
    bg_tx: BgSender,
    bg_rx: Receiver<UiBgEvent>,
    ipc_rx: Receiver<IpcToUi>,
    ipc_snapshot: Snapshot,
    portal: Option<PortalHandle>,
    portal_rx: Option<Receiver<PortalEvent>>,
    portal_status: (String, bool),

    playing: HashMap<u64, f32>,
    mic_ready: bool,
    source_ready: bool,
    passthrough: bool,
    mic_hint_shown: bool,

    search: String,
    side: SideSelection,
    selected: Option<u64>,
    sort: Sort,

    settings_open: bool,
    devices_cache: Option<Vec<(String, String)>>,
    sources_cache: Option<Vec<(String, String)>>,
    niri_status: Option<(String, bool)>,

    edit: Option<EditSound>,
    capture: Option<Capture>,
    pending_delete: Option<PendingDelete>,
    category_dialog: Option<(Option<String>, String)>, // (old name, buffer)
    status: (String, bool),
    status_at: std::time::Instant,
    /// Shared with the IPC thread (`astra hear`).
    speakers_muted: Arc<std::sync::atomic::AtomicBool>,
}

impl AstraApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        apply_theme(ctx);

        let cfg = Config::load();
        let lang = cfg.settings.lang.unwrap_or_else(Lang::from_env);
        let cfg_path = Config::config_path();

        let engine = Engine::start(cfg.settings.master_volume, cfg.settings.speaker_sink.clone());

        let (bg_raw, bg_rx) = mpsc::channel::<UiBgEvent>();
        let bg_tx = BgSender { tx: bg_raw, ctx: ctx.clone() };
        let (ipc_tx, ipc_rx) = mpsc::channel::<IpcToUi>();
        let snapshot: Snapshot = Arc::new(Mutex::new(Vec::new()));

        let speakers_muted = Arc::new(std::sync::atomic::AtomicBool::new(cfg.settings.mute_speakers));
        let _ = engine.tx.send(EngineMsg::SetSpeakersMuted(cfg.settings.mute_speakers));
        let _ = engine.tx.send(EngineMsg::SetBass(cfg.settings.bass_db));
        let _ = engine.tx.send(EngineMsg::SetLocalMonitor(
            cfg.settings.monitor_locally && !cfg.settings.mute_speakers,
        ));
        crate::ipc::spawn_server(
            engine.tx.clone(),
            ipc_tx,
            snapshot.clone(),
            speakers_muted.clone(),
            ctx.clone(),
        );

        let mut app = AstraApp {
            ctx: ctx.clone(),
            lang,
            cfg,
            cfg_path,
            dirty: false,
            engine,
            bg_tx: bg_tx.clone(),
            bg_rx,
            ipc_rx,
            ipc_snapshot: snapshot,
            portal: None,
            portal_rx: None,
            portal_status: (String::new(), false),
            playing: HashMap::new(),
            mic_ready: false,
            source_ready: false,
            passthrough: false,
            mic_hint_shown: false,
            search: String::new(),
            side: SideSelection::All,
            selected: None,
            sort: Sort::Manual,
            settings_open: false,
            devices_cache: None,
            sources_cache: None,
            niri_status: None,
            edit: None,
            capture: None,
            pending_delete: None,
            category_dialog: None,
            status: (String::new(), true),
            status_at: std::time::Instant::now(),
            speakers_muted,
        };
        app.sort = app.cfg.settings.sort;

        // Bring up the virtual microphone in the background.
        {
            let bg = bg_tx.clone();
            let settings_pass = app.cfg.settings.passthrough;
            let src = app.cfg.settings.passthrough_source.clone();
            let mon = app.cfg.settings.monitor_locally && !app.cfg.settings.mute_speakers;
            std::thread::spawn(move || {
                let ready = mic::ensure_sink().is_ok();
                let source = mic::ensure_source().is_ok();
                let pass = if ready && settings_pass {
                    mic::set_passthrough(true, src.as_deref()).unwrap_or(false)
                } else {
                    false
                };
                if ready && mon {
                    let _ = mic::set_local_monitor(true, None);
                }
                let _ = bg.send(UiBgEvent::MicState {
                    ready,
                    passthrough: pass,
                    source,
                });
            });
        }

        app.refresh_snapshot();
        app.rebind_portal();
        for s in &app.cfg.sounds {
            app.preload(s);
        }
        app
    }

    fn t(&self, k: K) -> &'static str {
        tr(self.lang, k)
    }

    fn set_status(&mut self, msg: impl Into<String>, ok: bool) {
        self.status = (msg.into(), ok);
        self.status_at = std::time::Instant::now();
    }

    fn save(&mut self) {
        self.cfg.settings.sort = self.sort;
        if let Err(e) = self.cfg.save() {
            self.set_status(format!("save: {e}"), false);
            return;
        }
        self.dirty = false;
        self.refresh_snapshot();
    }

    /// Publish play entries for the IPC server.
    fn refresh_snapshot(&mut self) {
        let norm = self.cfg.settings.normalize;
        let def_route = self.cfg.settings.default_route;
        let def_stop = self.cfg.settings.stop_others;
        let entries: Vec<PlayEntry> = self
            .cfg
            .sounds
            .iter()
            .map(|s| PlayEntry {
                id: s.id,
                name: s.name.clone(),
                hotkey: s.hotkey.clone(),
                path: s.path.clone(),
                volume: s.volume,
                gain: s.norm_gain(norm),
                route: s.route.resolve(def_route),
                loop_: s.loop_,
                stop_others: s.stop_others.unwrap_or(def_stop),
                speed: s.speed,
            })
            .collect();
        *self.ipc_snapshot.lock().unwrap() = entries;
    }

    fn make_request(&self, s: &Sound) -> PlayRequest {
        PlayRequest {
            id: s.id,
            path: s.path.clone(),
            volume: s.volume,
            gain: s.norm_gain(self.cfg.settings.normalize),
            route: s.route.resolve(self.cfg.settings.default_route),
            loop_: s.loop_,
            stop_others: s
                .stop_others
                .unwrap_or(self.cfg.settings.stop_others),
            speed: s.speed,
        }
    }

    /// Decode short sounds ahead of time so the first press is instant.
    fn preload(&self, s: &Sound) {
        if s.duration_secs > 0.0 && s.duration_secs <= PRELOAD_MAX_SECS {
            self.engine.preload(&s.path);
        }
    }

    fn play(&mut self, id: u64) {
        if let Some(s) = self.cfg.sound(id).cloned() {
            if !Path::new(&s.path).exists() {
                self.set_status(format!("{}: {}", self.t(K::Missing), s.path), false);
                return;
            }
            self.engine.play(self.make_request(&s));
        }
    }

    fn toggle(&mut self, id: u64) {
        if self.playing.contains_key(&id) {
            self.engine.stop(id);
        } else {
            self.play(id);
        }
    }

    fn on_started(&mut self, id: u64) {
        self.playing.insert(id, 0.0);
        let route = self
            .cfg
            .sound(id)
            .map(|s| s.route.resolve(self.cfg.settings.default_route));
        if let Some(s) = self.cfg.sound_mut(id) {
            s.play_count += 1;
            s.last_played_ms = Some(now_ms());
        }
        self.cfg.recent.retain(|&x| x != id);
        self.cfg.recent.insert(0, id);
        self.cfg.recent.truncate(12);
        self.dirty = true;
        // If the sound goes only to the virtual mic and nothing is capturing
        // it, the user hears nothing — nudge once per session.
        if route == Some(Route::Mic) && !self.mic_hint_shown {
            self.mic_hint_shown = true;
            let bg = self.bg_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(600));
                let _ = bg.send(UiBgEvent::MicCapture(crate::audio::mic::has_capture()));
            });
        }
    }

    // ---------------------------------------------------------------- events

    fn handle_engine_events(&mut self) {
        while let Ok(ev) = self.engine.rx.try_recv() {
            match ev {
                EngineEvent::Started(id) => self.on_started(id),
                EngineEvent::Progress(id, pos) => {
                    self.playing.insert(id, pos);
                }
                EngineEvent::Finished(id) => {
                    self.playing.remove(&id);
                }
                EngineEvent::Error(id, msg) => {
                    self.playing.remove(&id);
                    self.set_status(format!("sound {id}: {msg}"), false);
                }
                EngineEvent::Status(m) => self.set_status(m, false),
            }
        }
    }

    fn handle_bg_events(&mut self) {
        while let Ok(ev) = self.bg_rx.try_recv() {
            match ev {
                UiBgEvent::Scan(res) => {
                    if let Some(s) = self.cfg.sound_mut(res.id) {
                        s.duration_secs = res.duration_secs;
                        s.peak = res.peak;
                    }
                    if let Some(s) = self.cfg.sound(res.id) {
                        self.preload(s);
                    }
                    self.dirty = true;
                }
                UiBgEvent::MicState { ready, passthrough, source } => {
                    self.mic_ready = ready;
                    self.source_ready = source;
                    self.passthrough = passthrough;
                    if !ready {
                        self.set_status(self.t(K::ErrNoPactl), false);
                    }
                }
                UiBgEvent::NiriResult(Ok(_)) => {
                    self.niri_status = Some((self.t(K::InsertOk).to_string(), true));
                }
                UiBgEvent::NiriResult(Err(e)) => {
                    self.niri_status =
                        Some((format!("{}: {e}", self.t(K::InsertFail)), false));
                }
                UiBgEvent::AddPaths(paths) => self.add_paths(&paths),
                UiBgEvent::MicCapture(listening) => {
                    if !listening {
                        self.set_status(self.t(K::MicNobodyListening), false);
                    }
                }
            }
        }
    }

    fn handle_ipc(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.ipc_rx.try_recv() {
            match msg {
                IpcToUi::Add(paths) => self.add_paths(&paths),
                IpcToUi::Reload => {
                    let old_mic = (self.cfg.settings.passthrough, self.cfg.settings.monitor_locally);
                    self.cfg = Config::load();
                    self.lang = self.cfg.settings.lang.unwrap_or(self.lang);
                    self.sort = self.cfg.settings.sort;
                    self.refresh_snapshot();
                    self.rebind_portal();
                    let new_mic = (self.cfg.settings.passthrough, self.cfg.settings.monitor_locally);
                    if new_mic != old_mic {
                        self.apply_mic_settings();
                    }
                    self.set_status(self.t(K::Saved), true);
                }
                IpcToUi::Quit => {
                    self.save();
                    mic::cleanup_on_exit(self.cfg.settings.keep_virtual_mic);
                    std::process::exit(0);
                }
                IpcToUi::Passthrough(on) => {
                    self.cfg.settings.passthrough = on;
                    self.passthrough = on;
                    self.dirty = true;
                }
                IpcToUi::Bass(db) => {
                    self.cfg.settings.bass_db = db;
                    self.dirty = true;
                }
                IpcToUi::SpeakersMuted(m) => {
                    self.cfg.settings.mute_speakers = m;
                    self.dirty = true;
                    if self.cfg.settings.monitor_locally {
                        self.apply_mic_settings();
                    }
                }
                IpcToUi::Focus => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            ctx.request_repaint();
        }
    }

    fn handle_portal_events(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.portal_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                PortalEvent::Status(msg, ok) => {
                    self.portal_status = (
                        if ok {
                            self.t(K::PortalStatusOk).to_string()
                        } else {
                            format!("{} ({msg})", self.t(K::PortalStatusFail))
                        },
                        ok,
                    );
                }
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        let wants_kb = ctx.wants_keyboard_input();
        for ev in events {
            let egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } = ev
            else {
                continue;
            };
            // Escape cancels hotkey capture
            if key == egui::Key::Escape {
                if self.capture.take().is_some() {
                    continue;
                }
            }
            let Some(token) = hotkey::token_from(modifiers, key) else {
                continue;
            };
            if let Some(cap) = self.capture {
                self.capture = None;
                match cap {
                    Capture::Sound(id) => {
                        // hotkeys are stored in the dialog and written on Save
                        if let Some(edit) = &mut self.edit {
                            if edit.id == id {
                                edit.hotkey = Some(token);
                            }
                        }
                    }
                    Capture::StopAll => {
                        self.cfg.settings.stop_all_hotkey = Some(token);
                        self.dirty = true;
                    }
                }
                continue;
            }
            if wants_kb {
                continue;
            }
            // play a sound bound to this key
            let hit = self
                .cfg
                .sounds
                .iter()
                .find(|s| s.hotkey.as_deref() == Some(token.as_str()))
                .map(|s| s.id);
            if let Some(id) = hit {
                self.toggle(id);
                continue;
            }
            if self.cfg.settings.stop_all_hotkey.as_deref() == Some(token.as_str()) {
                self.engine.stop_all();
            }
        }
    }

    fn handle_dnd(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            let paths: Vec<String> = dropped
                .iter()
                .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned()))
                .collect();
            if !paths.is_empty() {
                self.add_paths(&paths);
            }
        }
    }

    // ------------------------------------------------------------ mic config

    fn apply_mic_settings(&mut self) {
        let bg = self.bg_tx.clone();
        let pass = self.cfg.settings.passthrough;
        let src = self.cfg.settings.passthrough_source.clone();
        let mon = self.cfg.settings.monitor_locally && !self.cfg.settings.mute_speakers;
        let _ = self.engine.tx.send(EngineMsg::SetLocalMonitor(mon));
        std::thread::spawn(move || {
            let ready = mic::ensure_sink().is_ok();
            let source = mic::ensure_source().is_ok();
            let pass_state = if ready && pass {
                mic::set_passthrough(true, src.as_deref()).unwrap_or(false)
            } else if ready {
                mic::set_passthrough(false, None).unwrap_or(false)
            } else {
                false
            };
            if ready {
                let _ = mic::set_local_monitor(mon, None);
            }
            let _ = bg.send(UiBgEvent::MicState {
                ready,
                passthrough: pass_state,
                source,
            });
        });
    }

    // ------------------------------------------------------------ adding

    /// Background-scan a file (duration + peak) and update the sound entry.
    fn spawn_scan(&self, path: &str, id: u64) {
        let bg = self.bg_tx.clone();
        let path = path.to_string();
        std::thread::spawn(move || {
            let res = match scan::compute(&path) {
                Ok((d, p)) => scan::ScanResult {
                    id,
                    duration_secs: d,
                    peak: p,
                },
                Err(_) => scan::ScanResult {
                    id,
                    duration_secs: 0.0,
                    peak: 0.0,
                },
            };
            let _ = bg.send(UiBgEvent::Scan(res));
        });
    }

    fn add_files_dialog(&mut self) {
        let filters = AUDIO_EXTS.to_vec();
        let bg = self.bg_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok();
            let fut = rfd::AsyncFileDialog::new()
                .add_filter("Audio", &filters)
                .pick_files();
            let Some(rt) = rt else { return };
            let files = rt.block_on(fut).unwrap_or_default();
            let paths: Vec<String> = files
                .iter()
                .map(|f| f.path().to_string_lossy().into_owned())
                .collect();
            if !paths.is_empty() {
                let _ = bg.send(UiBgEvent::AddPaths(paths));
            }
        });
    }

    fn add_folder_dialog(&mut self) {
        let bg = self.bg_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok();
            let fut = rfd::AsyncFileDialog::new().pick_folder();
            let Some(rt) = rt else { return };
            if let Some(h) = rt.block_on(fut) {
                let p = h.path().to_string_lossy().into_owned();
                let _ = bg.send(UiBgEvent::AddPaths(vec![p]));
            }
        });
    }

    pub fn add_paths(&mut self, paths: &[String]) {
        let mut files: Vec<PathBuf> = Vec::new();
        for p in paths {
            collect_audio(Path::new(p), 0, &mut files);
        }
        // dedupe against existing library
        let existing: std::collections::HashSet<PathBuf> = self
            .cfg
            .sounds
            .iter()
            .map(|s| std::fs::canonicalize(&s.path).unwrap_or_else(|_| PathBuf::from(&s.path)))
            .collect();

        let default_cat = match &self.side {
            SideSelection::Category(c) => c.clone(),
            _ => "General".to_string(),
        };

        let mut added = 0usize;
        let mut skipped = 0usize;
        for f in files {
            let canon = std::fs::canonicalize(&f).unwrap_or_else(|_| f.clone());
            if existing.contains(&canon) {
                skipped += 1;
                continue;
            }
            let id = self.cfg.alloc_id();
            let name = f
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("sound {id}"));
            let sound = Sound {
                id,
                name,
                path: canon.to_string_lossy().into_owned(),
                volume: 1.0,
                hotkey: None,
                route: RouteOverride::Default,
                loop_: false,
                category: default_cat.clone(),
                normalize: self.cfg.settings.normalize,
                speed: 1.0,
                stop_others: None,
                duration_secs: 0.0,
                peak: 0.0,
                added_ms: now_ms(),
                last_played_ms: None,
                play_count: 0,
            };
            self.spawn_scan(&sound.path, id);
            self.cfg.sounds.push(sound);
            added += 1;
        }
        if added > 0 {
            self.dirty = true;
            self.rebind_portal();
        }
        let mut msg = format!("{}: {added}", self.t(K::Added));
        if skipped > 0 {
            msg.push_str(&format!(" ({})", self.t(K::DuplicateSkipped)));
        }
        self.set_status(msg, added > 0 || skipped > 0);
    }

    // ------------------------------------------------------------ portal

    fn all_bindings(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<(String, String, String)> = self
            .cfg
            .sounds
            .iter()
            .filter_map(|s| {
                s.hotkey
                    .as_ref()
                    .map(|hk| (s.id.to_string(), hk.clone(), s.name.clone()))
            })
            .collect();
        if let Some(hk) = &self.cfg.settings.stop_all_hotkey {
            v.push(("stop-all".into(), hk.clone(), "Stop all sounds".into()));
        }
        v
    }

    fn rebind_portal(&mut self) {
        self.portal = None; // drops old session
        self.portal_rx = None;
        if !self.cfg.settings.portal_hotkeys {
            self.portal_status = (self.t(K::PortalStatusOff).to_string(), false);
            return;
        }
        let bindings = self.all_bindings();
        if bindings.is_empty() {
            self.portal_status = (self.t(K::PortalStatusOff).to_string(), false);
            return;
        }
        // convert tokens to portal triggers
        let bindings = bindings
            .into_iter()
            .map(|(id, token, name)| (id, hotkey::to_portal(&token), name))
            .collect();
        let (tx, rx) = mpsc::channel();
        self.portal_rx = Some(rx);
        // Hotkeys go straight to the engine from the portal thread: the UI
        // loop only runs on repaint and may not run at all while the window
        // is hidden behind a game.
        let engine_tx = self.engine.tx.clone();
        let snapshot = self.ipc_snapshot.clone();
        let ctx = self.ctx.clone();
        let on_activate = move |id: &str| {
            if id == "stop-all" {
                let _ = engine_tx.send(EngineMsg::StopAll);
            } else if let Ok(n) = id.parse::<u64>() {
                let entry = snapshot.lock().unwrap().iter().find(|e| e.id == n).cloned();
                if let Some(e) = entry {
                    let _ = engine_tx.send(EngineMsg::Play(e.request()));
                }
            }
            ctx.request_repaint();
        };
        self.portal = Some(crate::portal::start(bindings, tx, on_activate));
    }

    // ------------------------------------------------------------ filtering

    fn filtered_ids(&self) -> Vec<u64> {
        let q = self.search.to_lowercase();
        let mut v: Vec<(u64, SortKey)> = self
            .cfg
            .sounds
            .iter()
            .filter(|s| match &self.side {
                SideSelection::All => true,
                SideSelection::Recent => self.cfg.recent.contains(&s.id),
                SideSelection::Category(c) => {
                    (s.category.is_empty() && c == "General") || &s.category == c
                }
            })
            .filter(|s| {
                q.is_empty()
                    || s.name.to_lowercase().contains(&q)
                    || s.path.to_lowercase().contains(&q)
            })
            .map(|s| {
                let key = match self.sort {
                    Sort::Manual => SortKey::Rank(0),
                    Sort::Name => SortKey::Str(s.name.to_lowercase()),
                    Sort::Added => SortKey::Num(s.added_ms),
                    Sort::RecentlyPlayed => SortKey::Num(s.last_played_ms.unwrap_or(0)),
                    Sort::MostPlayed => SortKey::Num(s.play_count),
                };
                (s.id, key)
            })
            .collect();

        match self.sort {
            Sort::Manual => {}
            Sort::Name => v.sort_by(|a, b| match (&a.1, &b.1) {
                (SortKey::Str(x), SortKey::Str(y)) => x.cmp(y),
                _ => std::cmp::Ordering::Equal,
            }),
            Sort::Added | Sort::RecentlyPlayed | Sort::MostPlayed => {
                v.sort_by_key(|(_, k)| match k {
                    SortKey::Num(n) => std::cmp::Reverse(*n),
                    _ => std::cmp::Reverse(0),
                });
            }
        }

        if self.side == SideSelection::Recent {
            // follow recency order from cfg.recent
            let mut ordered = Vec::with_capacity(v.len());
            for id in &self.cfg.recent {
                if v.iter().any(|(vid, _)| vid == id) {
                    ordered.push(*id);
                }
            }
            return ordered;
        }
        v.into_iter().map(|(id, _)| id).collect()
    }

    // ------------------------------------------------------------ panels

    fn view_title(&self) -> String {
        match &self.side {
            SideSelection::All => self.t(K::AllSounds).to_string(),
            SideSelection::Recent => self.t(K::Recent).to_string(),
            SideSelection::Category(c) => c.clone(),
        }
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("astra_side")
            .exact_width(SIDE_W)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin::symmetric(12.0, 14.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    star(ui, 22.0);
                    ui.label(RichText::new("ASTRA").size(20.0).strong().color(TEXT));
                    ui.label(RichText::new(self.t(K::AppTagline)).size(11.0).color(MUTED));
                });
                ui.add_space(18.0);

                // mic card pinned to the bottom, the library scrolls above it
                egui::TopBottomPanel::bottom("astra_side_mic")
                    .frame(egui::Frame::none())
                    .show_separator_line(false)
                    .show_inside(ui, |ui| {
                        ui.add_space(8.0);
                        self.mic_card(ui);
                    });

                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    section_label(ui, self.t(K::Library));
                    let all = self.cfg.sounds.len();
                    if nav_item(ui, "♫", self.t(K::AllSounds), all, self.side == SideSelection::All).clicked() {
                        self.side = SideSelection::All;
                    }
                    let recent = self.cfg.recent.len();
                    if recent > 0
                        && nav_item(ui, "🕘", self.t(K::Recent), recent, self.side == SideSelection::Recent)
                            .clicked()
                    {
                        self.side = SideSelection::Recent;
                    }

                    ui.add_space(14.0);
                    section_label(ui, self.t(K::Categories));
                    for cat in self.cfg.categories.clone() {
                        let count = self
                            .cfg
                            .sounds
                            .iter()
                            .filter(|s| (s.category.is_empty() && cat == "General") || s.category == cat)
                            .count();
                        let sel = self.side == SideSelection::Category(cat.clone());
                        let resp = nav_item(ui, "#", &cat, count, sel);
                        if resp.clicked() {
                            self.side = SideSelection::Category(cat.clone());
                        }
                        resp.context_menu(|ui| {
                            if ui.button(self.t(K::Rename)).clicked() {
                                self.category_dialog = Some((Some(cat.clone()), cat.clone()));
                                ui.close_menu();
                            }
                            if ui.button(self.t(K::Delete)).clicked() {
                                self.pending_delete = Some(PendingDelete::Category(cat.clone()));
                                ui.close_menu();
                            }
                        });
                    }
                    ui.add_space(4.0);
                    let add = egui::Button::new(
                        RichText::new(format!("+  {}", self.t(K::NewCategory))).color(MUTED),
                    )
                    .frame(false);
                    if ui.add(add).clicked() {
                        self.category_dialog = Some((None, String::new()));
                    }
                });
            });
    }

    /// Routing card: virtual mic state + the two switches people flip often.
    fn mic_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(SURFACE2)
            .rounding(12.0)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .inner_margin(egui::Margin::same(12.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let ready = self.mic_ready && self.source_ready;
                ui.horizontal(|ui| {
                    status_dot(ui, if ready { OK } else { DANGER });
                    ui.label(
                        RichText::new(if ready { self.t(K::MicReady) } else { self.t(K::MicNotReady) })
                            .strong()
                            .color(TEXT),
                    );
                });
                ui.label(RichText::new("ASTRA Virtual Microphone").size(11.0).color(MUTED));
                ui.add_space(10.0);

                let mut hear = !self.cfg.settings.mute_speakers;
                if switch_row(ui, self.t(K::HearInHeadphones), &mut hear)
                    .on_hover_text(self.t(K::HearHint))
                    .changed()
                {
                    self.set_speakers_muted(!hear);
                }
                ui.add_space(4.0);
                let mut pass = self.cfg.settings.passthrough;
                if switch_row(ui, self.t(K::VoiceToMic), &mut pass).changed() {
                    self.cfg.settings.passthrough = pass;
                    self.dirty = true;
                    self.apply_mic_settings();
                }
            });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(self.view_title()).size(22.0).strong().color(TEXT));
                let count = self.filtered_ids().len();
                ui.label(
                    RichText::new(format!("{count} {}", self.t(K::SoundsCount)))
                        .size(12.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let gear = egui::Button::new(RichText::new("⚙").size(18.0))
                    .min_size(egui::vec2(36.0, 36.0))
                    .rounding(10.0);
                if ui.add(gear).on_hover_text(self.t(K::Settings)).clicked() {
                    self.toggle_settings();
                }
                ui.menu_button(RichText::new(format!("+  {}", self.t(K::Add))).strong(), |ui| {
                    if ui.button(self.t(K::AddFiles)).clicked() {
                        self.add_files_dialog();
                        ui.close_menu();
                    }
                    if ui.button(self.t(K::AddFolder)).clicked() {
                        self.add_folder_dialog();
                        ui.close_menu();
                    }
                });
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(sort_name(self.sort, self.lang))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for s in [Sort::Manual, Sort::Name, Sort::Added, Sort::RecentlyPlayed, Sort::MostPlayed] {
                            ui.selectable_value(&mut self.sort, s, sort_name(s, self.lang));
                        }
                    });
                let hint = format!("🔍  {}", self.t(K::Search));
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(hint)
                        .desired_width((ui.available_width() - 8.0).clamp(140.0, 320.0))
                        .margin(egui::Margin::symmetric(10.0, 8.0)),
                );
            });
        });
        let (msg, ok) = &self.status;
        if !msg.is_empty() && self.status_at.elapsed() < std::time::Duration::from_secs(8) {
            ui.add_space(4.0);
            ui.label(RichText::new(msg.clone()).size(12.0).color(if *ok { OK } else { DANGER }));
        }
    }

    fn center_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::symmetric(20.0, 16.0)))
            .show(ctx, |ui| {
                self.header(ui);
                ui.add_space(12.0);
                let ids = self.filtered_ids();
                if ids.is_empty() {
                    ui.add_space(60.0);
                    ui.vertical_centered(|ui| {
                        star(ui, 48.0);
                        ui.add_space(6.0);
                        ui.label(RichText::new(self.t(K::NoSounds)).size(18.0).strong().color(TEXT));
                        ui.add_space(4.0);
                        ui.label(RichText::new(self.t(K::NoSoundsHint)).color(MUTED));
                        ui.add_space(14.0);
                        if ui
                            .add(accent_button(&format!("+  {}", self.t(K::AddFiles))))
                            .clicked()
                        {
                            self.add_files_dialog();
                        }
                    });
                    return;
                }
                let spacing = ui.spacing().item_spacing.y;
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show_rows(ui, ROW_H + spacing, ids.len(), |ui, range| {
                        for &id in &ids[range] {
                            self.sound_row(ui, id);
                        }
                    });
            });
    }

    fn sound_row(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(s) = self.cfg.sound(id).cloned() else {
            return;
        };
        let pos = self.playing.get(&id).copied();
        let playing = pos.is_some();
        let missing = !Path::new(&s.path).exists();

        // The whole card is clickable (select, double-click plays, right-click
        // menu); buttons placed on top of it take their own clicks.
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
        let selected = self.selected == Some(id);
        let fill = if playing {
            PLAYING_BG
        } else if selected {
            SURFACE2
        } else if resp.hovered() {
            HOVER_BG
        } else {
            SURFACE
        };
        let painter = ui.painter_at(rect);
        painter.rect(
            rect,
            12.0,
            fill,
            egui::Stroke::new(1.0_f32, if selected || playing { ACCENT_DIM } else { BORDER }),
        );
        if let Some(pos) = pos {
            let frac = if s.duration_secs > 0.0 { (pos / s.duration_secs).clamp(0.0, 1.0) } else { 1.0 };
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 12.0, rect.bottom() - 4.0),
                egui::pos2(rect.left() + 12.0 + (rect.width() - 24.0) * frac, rect.bottom() - 2.0),
            );
            painter.rect_filled(bar, 1.0, ACCENT);
        }

        let inner = rect.shrink2(egui::vec2(12.0, 6.0));
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::left_to_right(Align::Center)),
        );
        if play_button(&mut row, playing).clicked() {
            self.toggle(id);
        }
        row.add_space(10.0);

        let right_w = 210.0;
        let text_w = (row.available_width() - right_w).max(80.0);
        row.allocate_ui_with_layout(
            egui::vec2(text_w, inner.height()),
            Layout::top_down(Align::Min).with_main_justify(false),
            |ui| {
                ui.add_space(3.0);
                let mut name = RichText::new(&s.name).size(15.0).strong().color(TEXT);
                if missing {
                    name = name.color(DANGER);
                }
                ui.add(egui::Label::new(name).truncate().selectable(false));
                let route = s.route.resolve(self.cfg.settings.default_route);
                let mut meta = format!("{}  ·  {}", s.display_duration(), route_icon(route));
                if (s.speed - 1.0).abs() > 0.01 {
                    meta.push_str(&format!("  ·  {:.2}×", s.speed));
                }
                if s.loop_ {
                    meta.push_str("  ·  ⟲");
                }
                if missing {
                    meta = format!("{}  ·  {}", self.t(K::Missing), meta);
                }
                ui.add(egui::Label::new(RichText::new(meta).size(12.0).color(MUTED)).selectable(false));
            },
        );

        row.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, "🗑").on_hover_text(self.t(K::Delete)).clicked() {
                self.pending_delete = Some(PendingDelete::Sound(id));
            }
            if icon_button(ui, "✏").on_hover_text(self.t(K::Edit)).clicked() {
                self.edit = Some(EditSound::from(&s));
            }
            if let Some(hk) = &s.hotkey {
                ui.add_space(6.0);
                chip(ui, hk);
            }
        });

        if resp.clicked() {
            self.selected = Some(id);
        }
        if resp.double_clicked() {
            self.toggle(id);
        }
        resp.context_menu(|ui| {
            if ui.button(if playing { self.t(K::Stop) } else { self.t(K::Play) }).clicked() {
                self.toggle(id);
                ui.close_menu();
            }
            ui.separator();
            if ui.button(self.t(K::Edit)).clicked() {
                self.edit = Some(EditSound::from(&s));
                ui.close_menu();
            }
            if ui.button(self.t(K::CopyPath)).clicked() {
                ui.output_mut(|o| o.copied_text = s.path.clone());
                ui.close_menu();
            }
            if ui.button(self.t(K::OpenFolder)).clicked() {
                if let Some(dir) = Path::new(&s.path).parent() {
                    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                }
                ui.close_menu();
            }
            ui.separator();
            if ui.button(self.t(K::Delete)).clicked() {
                self.pending_delete = Some(PendingDelete::Sound(id));
                ui.close_menu();
            }
        });
    }

    /// Bottom transport: what is playing, stop all, route, master volume.
    fn transport_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("astra_transport")
            .exact_height(72.0)
            .frame(
                egui::Frame::none()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(18.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // now playing: the most recently started sound
                    let current = self
                        .cfg
                        .recent
                        .iter()
                        .find(|id| self.playing.contains_key(id))
                        .copied()
                        .or_else(|| self.playing.keys().next().copied());
                    ui.allocate_ui_with_layout(
                        egui::vec2(260.0, 48.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.label(RichText::new(self.t(K::NowPlaying).to_uppercase()).size(10.0).color(MUTED));
                            match current.and_then(|id| self.cfg.sound(id).map(|s| (id, s.clone()))) {
                                Some((id, s)) => {
                                    let extra = self.playing.len().saturating_sub(1);
                                    let mut name = s.name.clone();
                                    if extra > 0 {
                                        name.push_str(&format!("  +{extra}"));
                                    }
                                    ui.add(egui::Label::new(RichText::new(name).strong().color(TEXT)).truncate());
                                    let pos = self.playing.get(&id).copied().unwrap_or(0.0);
                                    let frac = if s.duration_secs > 0.0 {
                                        (pos / s.duration_secs).clamp(0.0, 1.0)
                                    } else {
                                        0.0
                                    };
                                    ui.add(
                                        egui::ProgressBar::new(frac)
                                            .desired_width(240.0)
                                            .desired_height(4.0)
                                            .fill(ACCENT),
                                    );
                                }
                                None => {
                                    ui.label(RichText::new(self.t(K::Idle)).color(MUTED));
                                }
                            }
                        },
                    );

                    ui.add_space(8.0);
                    let stop = egui::Button::new(
                        RichText::new(format!("⏹  {}", self.t(K::StopAll))).strong().color(Color32::WHITE),
                    )
                    .fill(if self.playing.is_empty() { STOP_IDLE } else { DANGER })
                    .rounding(10.0)
                    .min_size(egui::vec2(130.0, 38.0));
                    if ui.add(stop).clicked() {
                        self.engine.stop_all();
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut pct = (self.cfg.settings.master_volume * 100.0).round() as i32;
                        ui.spacing_mut().slider_width = 120.0;
                        if ui
                            .add(egui::Slider::new(&mut pct, 0..=150).suffix("%"))
                            .on_hover_text(self.t(K::MasterVolume))
                            .changed()
                        {
                            let vol = pct as f32 / 100.0;
                            self.cfg.settings.master_volume = vol;
                            self.engine.tx.send(EngineMsg::SetMasterVolume(vol)).ok();
                            self.dirty = true;
                        }
                        ui.label(RichText::new("🔊").size(16.0).color(MUTED));
                        ui.add_space(14.0);

                        let mut db = self.cfg.settings.bass_db.round() as i32;
                        ui.spacing_mut().slider_width = 100.0;
                        let bass = ui
                            .add(
                                egui::Slider::new(&mut db, BASS_MIN_DB as i32..=BASS_MAX_DB as i32)
                                    .suffix(" dB"),
                            )
                            .on_hover_text("0 dB = flat · double-click: reset");
                        if bass.changed() {
                            self.cfg.settings.bass_db = db as f32;
                            self.engine.tx.send(EngineMsg::SetBass(db as f32)).ok();
                            self.dirty = true;
                        }
                        if bass.double_clicked() {
                            self.cfg.settings.bass_db = 0.0;
                            self.engine.tx.send(EngineMsg::SetBass(0.0)).ok();
                            self.dirty = true;
                        }
                        ui.label(RichText::new(self.t(K::Bass)).size(12.0).color(MUTED));
                        ui.add_space(14.0);

                        let mut r = self.cfg.settings.default_route;
                        let opts = [
                            (Route::Both, self.t(K::RouteShortBoth)),
                            (Route::Mic, self.t(K::RouteShortMic)),
                            (Route::Speakers, self.t(K::RouteShortSpeakers)),
                        ];
                        if segmented(ui, &mut r, &opts) {
                            self.cfg.settings.default_route = r;
                            self.dirty = true;
                        }
                        ui.label(RichText::new(self.t(K::Output)).size(12.0).color(MUTED));
                    });
                });
            });
    }

    fn toggle_settings(&mut self) {
        if self.settings_open {
            self.close_settings();
        } else {
            self.settings_open = true;
            if self.devices_cache.is_none() {
                self.load_devices();
            }
        }
    }

    fn close_settings(&mut self) {
        self.settings_open = false;
        self.save();
        self.rebind_portal();
    }

    /// Play to the mic but keep ASTRA silent in your own headphones.
    fn set_speakers_muted(&mut self, muted: bool) {
        self.cfg.settings.mute_speakers = muted;
        self.speakers_muted.store(muted, std::sync::atomic::Ordering::Relaxed);
        let _ = self.engine.tx.send(EngineMsg::SetSpeakersMuted(muted));
        self.dirty = true;
        // local monitoring would bring the sounds back into the headphones
        if self.cfg.settings.monitor_locally {
            self.apply_mic_settings();
        }
    }

    // ------------------------------------------------------------ windows

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = self.settings_open;
        egui::Window::new(self.t(K::Settings))
            .open(&mut open)
            .resizable(true)
            .default_width(600.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.heading(self.t(K::GeneralSection));
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(self.t(K::Language));
                        if ui.button(lang_name(self.lang)).clicked() {
                            self.lang = self.lang.next();
                            self.cfg.settings.lang = Some(self.lang);
                            self.dirty = true;
                        }
                    });
                    let ipc_label = self.t(K::IpcEnabled);
                    if ui.checkbox(&mut self.cfg.settings.enable_ipc, ipc_label).changed() {
                        self.dirty = true;
                    }

                    ui.add_space(8.0);
                    ui.heading(self.t(K::PlaybackSection));
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(self.t(K::DefaultRoute));
                        let mut r = self.cfg.settings.default_route;
                        let combo = egui::ComboBox::from_id_salt("set_route")
                            .selected_text(route_name(r, self.lang))
                            .width(170.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut r, Route::Speakers, route_name(Route::Speakers, self.lang));
                                ui.selectable_value(&mut r, Route::Mic, route_name(Route::Mic, self.lang));
                                ui.selectable_value(&mut r, Route::Both, route_name(Route::Both, self.lang));
                            });
                        if combo.response.changed() {
                            self.cfg.settings.default_route = r;
                            self.dirty = true;
                        }
                    });
                    let so_label = self.t(K::StopOthersDefault);
                    ui.checkbox(&mut self.cfg.settings.stop_others, so_label);
                    let norm_label = self.t(K::NormalizeDefault);
                    ui.checkbox(&mut self.cfg.settings.normalize, norm_label);

                    ui.add_space(8.0);
                    ui.heading(self.t(K::DevicesSection));
                    ui.separator();
                    self.devices_combo(ui);

                    ui.add_space(8.0);
                    ui.heading(self.t(K::MicSection));
                    ui.separator();
                    self.mic_section(ui);

                    ui.add_space(8.0);
                    ui.heading(self.t(K::HotkeysSection));
                    ui.separator();
                    self.hotkeys_section(ui);

                    ui.add_space(8.0);
                    ui.heading(self.t(K::StorageSection));
                    ui.separator();
                    self.storage_section(ui);

                    ui.add_space(8.0);
                    ui.heading(self.t(K::About));
                    ui.separator();
                    ui.label(self.t(K::AppAbout));
                    ui.label(
                        RichText::new(format!(
                            "{} {} · {}",
                            self.t(K::Version),
                            env!("CARGO_PKG_VERSION"),
                            self.t(K::RepoLink)
                        ))
                        .weak()
                        .small(),
                    );
                });
            });
        if !open && self.settings_open {
            self.close_settings();
        }
    }

    fn load_devices(&mut self) {
        let bg = self.bg_tx.clone();
        // reuse a simple approach: load inline (pactl is fast enough here)
        let sinks = mic::list_sinks().unwrap_or_default();
        let sources = mic::list_sources().unwrap_or_default();
        self.devices_cache = Some(sinks);
        self.sources_cache = Some(sources);
        let _ = bg;
    }

    fn devices_combo(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(self.t(K::OutputDevice));
            let sinks = self.devices_cache.clone().unwrap_or_default();
            let current = self.cfg.settings.speaker_sink.clone().unwrap_or_default();
            let label: String = match sinks.iter().find(|(n, _)| n == &current) {
                Some((_, d)) => d.clone(),
                None if current.is_empty() => self.t(K::DefaultDevice).to_string(),
                None => current.clone(),
            };
            egui::ComboBox::from_id_salt("spk")
                .selected_text(label)
                .width(260.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            current.is_empty(),
                            self.t(K::DefaultDevice),
                        )
                        .clicked()
                    {
                        self.cfg.settings.speaker_sink = None;
                        self.engine.set_speaker_sink(None);
                        self.dirty = true;
                    }
                    for (name, desc) in &sinks {
                        if name.contains("astra_mic") {
                            continue;
                        }
                        if ui
                            .selectable_label(&current == name, desc)
                            .clicked()
                        {
                            self.cfg.settings.speaker_sink = Some(name.clone());
                            self.engine.set_speaker_sink(Some(name.clone()));
                            self.dirty = true;
                        }
                    }
                });
            if ui.button(self.t(K::Refresh)).clicked() {
                self.load_devices();
            }
        });
    }

    fn mic_section(&mut self, ui: &mut egui::Ui) {
        let sink_status = if self.mic_ready {
            RichText::new(format!("● {}", self.t(K::MicStatusSink))).color(Color32::from_rgb(120, 220, 140))
        } else {
            RichText::new(format!("● {}", self.t(K::MicStatusNoSink))).color(Color32::from_rgb(220, 120, 120))
        };
        ui.label(sink_status);
        let source_status = if self.source_ready {
            RichText::new(format!("● {}", self.t(K::MicStatusSource))).color(Color32::from_rgb(120, 220, 140))
        } else {
            RichText::new(format!("● {}", self.t(K::MicStatusSourceMissing))).color(Color32::from_rgb(220, 120, 120))
        };
        ui.label(source_status);
        ui.label(RichText::new(self.t(K::DiscordHint)).weak().small());

        ui.add_space(4.0);
        let pass_label = self.t(K::Passthrough);
        if ui
            .checkbox(&mut self.cfg.settings.passthrough, pass_label)
            .changed()
        {
            self.dirty = true;
            self.apply_mic_settings();
        }
        ui.horizontal(|ui| {
            ui.label(self.t(K::PassthroughSource));
            let sources = self.sources_cache.clone().unwrap_or_default();
            let current = self
                .cfg
                .settings
                .passthrough_source
                .clone()
                .unwrap_or_default();
            let label: String = match sources.iter().find(|(n, _)| n == &current) {
                Some((_, d)) => d.clone(),
                None if current.is_empty() => self.t(K::DefaultDevice).to_string(),
                None => current.clone(),
            };
            egui::ComboBox::from_id_salt("src")
                .selected_text(label)
                .width(260.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(current.is_empty(), self.t(K::DefaultDevice))
                        .clicked()
                    {
                        self.cfg.settings.passthrough_source = None;
                        self.dirty = true;
                        self.apply_mic_settings();
                    }
                    for (name, desc) in &sources {
                        if name.contains(".monitor") || name.starts_with("astra_") {
                            continue;
                        }
                        if ui.selectable_label(&current == name, desc).clicked() {
                            self.cfg.settings.passthrough_source = Some(name.clone());
                            self.dirty = true;
                            self.apply_mic_settings();
                        }
                    }
                });
        });

        let mon_label = self.t(K::LocalMonitor);
        if ui
            .checkbox(&mut self.cfg.settings.monitor_locally, mon_label)
            .changed()
        {
            self.dirty = true;
            self.apply_mic_settings();
        }
        ui.label(
            RichText::new(self.t(K::LocalMonitorHint))
                .weak()
                .text_style(egui::TextStyle::Small),
        );
        let keep_label = self.t(K::KeepVirtualMic);
        if ui
            .checkbox(&mut self.cfg.settings.keep_virtual_mic, keep_label)
            .changed()
        {
            self.dirty = true;
        }
    }

    fn hotkeys_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(self.t(K::StopAllHotkey));
            let capturing = self.capture == Some(Capture::StopAll);
            let label = if capturing {
                self.t(K::PressKeys).to_string()
            } else {
                self.cfg
                    .settings
                    .stop_all_hotkey
                    .clone()
                    .unwrap_or_else(|| self.t(K::SetHotkey).to_string())
            };
            if ui.add(egui::Button::new(RichText::new(label).monospace())).clicked() {
                self.capture = Some(Capture::StopAll);
            }
            if ui.button(self.t(K::Clear)).clicked() {
                self.cfg.settings.stop_all_hotkey = None;
                self.dirty = true;
            }
        });

        ui.add_space(6.0);
        let portal_label = self.t(K::PortalHotkeys);
        if ui
            .checkbox(&mut self.cfg.settings.portal_hotkeys, portal_label)
            .changed()
        {
            self.dirty = true;
            self.rebind_portal();
        }
        let st = &self.portal_status;
        let mut t = RichText::new(st.0.clone()).small();
        if st.1 && !st.0.is_empty() {
            t = t.color(Color32::from_rgb(120, 220, 140));
        } else if !st.0.is_empty() {
            t = t.color(Color32::from_rgb(230, 170, 120));
        }
        ui.label(t);
        ui.label(
            RichText::new(self.t(K::PortalHint))
                .weak()
                .text_style(egui::TextStyle::Small),
        );

        ui.add_space(6.0);
        ui.label(RichText::new(self.t(K::NiriSection)).strong());
        ui.label(
            RichText::new(self.t(K::NiriHint))
                .weak()
                .text_style(egui::TextStyle::Small),
        );
        let bindings = self.all_bindings();
        let snippet = niri::generate(
            &bindings
                .iter()
                .map(|(id, hk, name)| (hk.clone(), id.clone(), name.clone()))
                .collect::<Vec<_>>(),
            self.cfg.settings.stop_all_hotkey.as_deref(),
        );
        if snippet.is_empty() {
            ui.label(RichText::new(self.t(K::SetHotkey)).weak().small());
        } else {
            egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut snippet.as_str())
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .interactive(false),
                );
            });
            ui.horizontal(|ui| {
                if ui.button(self.t(K::Copy)).clicked() {
                    ui.output_mut(|o| o.copied_text = snippet.clone());
                    self.set_status(self.t(K::Copied), true);
                }
                let Some(cfg_path) = niri::niri_config_path() else {
                    ui.label(RichText::new(self.t(K::NiriMissing)).weak().small());
                    return;
                };
                if !cfg_path.exists() {
                    ui.label(
                        RichText::new(format!("{} {}", self.t(K::NiriMissing), cfg_path.display()))
                            .weak()
                            .small(),
                    );
                    return;
                }
                if ui
                    .add(egui::Button::new(format!("{} ({})", self.t(K::Insert), cfg_path.display())))
                    .clicked()
                {
                    let path = cfg_path.clone();
                    let bg = self.bg_tx.clone();
                    std::thread::spawn(move || {
                        let r = niri::insert_into_file(&path, &snippet);
                        let _ = bg.send(UiBgEvent::NiriResult(r.map(|_| String::new())));
                    });
                }
            });
            ui.label(
                RichText::new(self.t(K::InsertBtnHint))
                    .weak()
                    .text_style(egui::TextStyle::Small),
            );
            if let Some((msg, ok)) = &self.niri_status {
                let mut t = RichText::new(msg.clone()).small();
                if *ok {
                    t = t.color(Color32::from_rgb(120, 220, 140));
                } else {
                    t = t.color(Color32::from_rgb(240, 120, 120));
                }
                ui.label(t);
            }
        }
    }

    fn storage_section(&mut self, ui: &mut egui::Ui) {
        ui.label(format!(
            "{}: {}",
            self.t(K::ConfigPath),
            self.cfg_path.display()
        ));
        ui.horizontal(|ui| {
            if ui.button(self.t(K::RescanAll)).clicked() {
                for s in &self.cfg.sounds {
                    self.spawn_scan(&s.path, s.id);
                }
            }
            if ui.button(self.t(K::RemoveMissing)).clicked() {
                let before = self.cfg.sounds.len();
                self.cfg
                    .sounds
                    .retain(|s| Path::new(&s.path).exists());
                let removed = before - self.cfg.sounds.len();
                self.dirty = true;
                self.set_status(
                    format!("{}: {removed}", self.t(K::RemovedMissing)),
                    true,
                );
            }
        });
    }

    fn edit_window(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.edit.clone() else {
            return;
        };
        let title = format!("{} — {}", self.t(K::Edit), edit.name);
        let mut open = true;
        let mut close = false;
        let save_label = self.t(K::Save);
        let cancel_label = self.t(K::Cancel);
        let del_label = self.t(K::Delete);
        let copy_label = self.t(K::CopyPath);
        let open_label = self.t(K::OpenFolder);
        egui::Window::new(title)
            .open(&mut open)
            .resizable(false)
            .default_width(470.0)
            .show(ctx, |ui| {
                egui::Grid::new("edit_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(self.t(K::Name));
                        ui.add_sized(
                            [280.0, 24.0],
                            egui::TextEdit::singleline(&mut edit.name),
                        );
                        ui.end_row();

                        ui.label(self.t(K::Category));
                        ui.horizontal(|ui| {
                            let cats = self.cfg.categories.clone();
                            egui::ComboBox::from_id_salt("edit_cat")
                                .selected_text(&edit.category)
                                .width(160.0)
                                .show_ui(ui, |ui| {
                                    for c in cats {
                                        ui.selectable_value(&mut edit.category, c.clone(), c);
                                    }
                                });
                            let cat_hint = self.t(K::CategoryName);
                            ui.add_sized(
                                [110.0, 24.0],
                                egui::TextEdit::singleline(&mut edit.new_category)
                                    .hint_text(cat_hint),
                            );
                            let add_label = self.t(K::Add);
                            if !edit.new_category.trim().is_empty()
                                && ui.button(add_label).clicked()
                            {
                                let nc = edit.new_category.trim().to_string();
                                if !self.cfg.categories.contains(&nc) {
                                    self.cfg.categories.push(nc.clone());
                                }
                                edit.category = nc;
                                edit.new_category.clear();
                                self.dirty = true;
                            }
                        });
                        ui.end_row();

                        ui.label(self.t(K::Hotkey));
                        ui.horizontal(|ui| {
                            let capturing = self.capture == Some(Capture::Sound(edit.id));
                            let label = if capturing {
                                self.t(K::PressKeys).to_string()
                            } else {
                                edit.hotkey
                                    .clone()
                                    .unwrap_or_else(|| self.t(K::SetHotkey).to_string())
                            };
                            if ui
                                .add(egui::Button::new(RichText::new(label).monospace()))
                                .clicked()
                            {
                                self.capture = Some(Capture::Sound(edit.id));
                            }
                            let clear_label = self.t(K::Clear);
                            if edit.hotkey.is_some() && ui.button(clear_label).clicked() {
                                edit.hotkey = None;
                            }
                        });
                        ui.end_row();

                        ui.label(self.t(K::Volume));
                        ui.horizontal(|ui| {
                            let mut pct = (edit.volume * 100.0).round() as i32;
                            ui.add(egui::Slider::new(&mut pct, 0..=150).suffix("%"));
                            edit.volume = pct as f32 / 100.0;
                        });
                        ui.end_row();

                        ui.label(self.t(K::Speed));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Slider::new(&mut edit.speed, 0.5..=2.0)
                                    .step_by(0.05)
                                    .suffix("×"),
                            );
                            if ui.button("1.0×").clicked() {
                                edit.speed = 1.0;
                            }
                        });
                        ui.end_row();

                        ui.label(self.t(K::Route));
                        egui::ComboBox::from_id_salt("edit_route")
                            .selected_text(route_override_name(edit.route, self.lang))
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for r in [
                                    RouteOverride::Default,
                                    RouteOverride::Speakers,
                                    RouteOverride::Mic,
                                    RouteOverride::Both,
                                ] {
                                    ui.selectable_value(
                                        &mut edit.route,
                                        r,
                                        route_override_name(r, self.lang),
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("");
                        ui.horizontal(|ui| {
                            let loop_label = self.t(K::Loop);
                            ui.checkbox(&mut edit.loop_, loop_label);
                            let norm_label = self.t(K::Normalize);
                            ui.checkbox(&mut edit.normalize, norm_label);
                        });
                        ui.end_row();

                        ui.label(self.t(K::StopOthers));
                        ui.horizontal(|ui| {
                            use StopOthersOption as SO;
                            let mut cur = match edit.stop_others {
                                None => SO::Global,
                                Some(true) => SO::Yes,
                                Some(false) => SO::No,
                            };
                            egui::ComboBox::from_id_salt("edit_stop")
                                .selected_text(stop_others_name(cur, self.lang))
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for v in [SO::Global, SO::Yes, SO::No] {
                                        ui.selectable_value(&mut cur, v, stop_others_name(v, self.lang));
                                    }
                                });
                            edit.stop_others = match cur {
                                SO::Global => None,
                                SO::Yes => Some(true),
                                SO::No => Some(false),
                            };
                        });
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label(
                    RichText::new(&edit.path)
                        .weak()
                        .small(),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(open_label).clicked() {
                        if let Some(dir) = Path::new(&edit.path).parent() {
                            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                        }
                    }
                    if ui.button(copy_label).clicked() {
                        ui.output_mut(|o| o.copied_text = edit.path.clone());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new(save_label).strong()).clicked() {
                            let snapshot = edit.clone();
                            self.apply_edit(&snapshot);
                            self.edit = None;
                            close = true;
                        }
                        if ui.button(cancel_label).clicked() {
                            self.edit = None;
                            close = true;
                        }
                        let del = egui::Button::new(
                            RichText::new(del_label).color(Color32::from_rgb(240, 120, 120)),
                        );
                        if ui.add(del).clicked() {
                            let id = edit.id;
                            self.edit = None;
                            close = true;
                            self.pending_delete = Some(PendingDelete::Sound(id));
                        }
                    });
                });
            });
        if !open && !close {
            // closed via the [x] decoration
            self.edit = None;
        } else if !close {
            // keep what was typed/dragged this frame (handle_keys runs first,
            // so a captured hotkey is already in this copy)
            self.edit = Some(edit);
        }
    }

    fn apply_edit(&mut self, edit: &EditSound) {
        let cat = if edit.category.trim().is_empty() {
            "General".to_string()
        } else {
            edit.category.trim().to_string()
        };
        if !self.cfg.categories.contains(&cat) {
            self.cfg.categories.push(cat.clone());
        }
        if let Some(s) = self.cfg.sound_mut(edit.id) {
            s.name = edit.name.trim().to_string();
            if s.name.is_empty() {
                s.name = format!("sound {}", s.id);
            }
            s.category = cat;
            s.hotkey = edit.hotkey.clone();
            s.volume = edit.volume;
            s.speed = edit.speed;
            s.route = edit.route;
            s.loop_ = edit.loop_;
            s.normalize = edit.normalize;
            s.stop_others = edit.stop_others;
        }
        // drop conflicting hotkeys
        if let Some(hk) = &edit.hotkey {
            let id = edit.id;
            for s in &mut self.cfg.sounds {
                if s.id != id && s.hotkey.as_deref() == Some(hk.as_str()) {
                    s.hotkey = None;
                }
            }
        }
        self.dirty = true;
        self.save();
        self.rebind_portal();
    }

    fn confirm_window(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_delete.clone() else {
            return;
        };
        let (text, name) = match &pending {
            PendingDelete::Sound(id) => (
                self.t(K::DeleteSoundQ).to_string(),
                self.cfg
                    .sound(*id)
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
            ),
            PendingDelete::Category(c) => (self.t(K::DeleteCategoryQ).to_string(), c.clone()),
        };
        let yes_label = self.t(K::Yes);
        let no_label = self.t(K::No);
        let mut open = true;
        let mut done = false;
        egui::Window::new(self.t(K::Confirm))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(RichText::new(&name).strong());
                ui.separator();
                ui.label(text);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(yes_label).strong()).clicked() {
                        match pending {
                            PendingDelete::Sound(id) => {
                                self.engine.stop(id);
                                self.cfg.sounds.retain(|s| s.id != id);
                                self.cfg.recent.retain(|&x| x != id);
                                if self.selected == Some(id) {
                                    self.selected = None;
                                }
                            }
                            PendingDelete::Category(c) => {
                                for s in &mut self.cfg.sounds {
                                    if s.category == c
                                        || (s.category.is_empty() && c == "General")
                                    {
                                        s.category = "General".into();
                                    }
                                }
                                self.cfg.categories.retain(|x| x != &c);
                                if self.side == SideSelection::Category(c.clone()) {
                                    self.side = SideSelection::All;
                                }
                            }
                        }
                        self.dirty = true;
                        done = true;
                    }
                    if ui.button(no_label).clicked() {
                        done = true;
                    }
                });
            });
        if done || !open {
            if done {
                self.save();
                self.rebind_portal();
            }
            self.pending_delete = None;
        }
    }

    fn category_dialog_window(&mut self, ctx: &egui::Context) {
        if self.category_dialog.is_none() {
            return;
        }
        let is_rename = self.category_dialog.as_ref().unwrap().0.is_some();
        let title = if is_rename {
            self.t(K::RenameCategory).to_string()
        } else {
            self.t(K::NewCategory).to_string()
        };
        let label = self.t(K::CategoryName);
        let btn_label = if is_rename {
            self.t(K::Rename)
        } else {
            self.t(K::Add)
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new(title)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                let Some((_old, buf)) = self.category_dialog.as_mut() else {
                    return;
                };
                ui.horizontal(|ui| {
                    ui.label(label);
                    let resp = ui.add_sized(
                        [220.0, 24.0],
                        egui::TextEdit::singleline(buf).hint_text(label),
                    );
                    if (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || ui.button(btn_label).clicked()
                    {
                        submit = true;
                    }
                });
            });
        if submit {
            if let Some((old, buf)) = self.category_dialog.as_mut() {
                let name = buf.trim().to_string();
                if !name.is_empty() {
                    if let Some(old_name) = old.clone() {
                        for snd in &mut self.cfg.sounds {
                            if snd.category == old_name
                                || (snd.category.is_empty() && old_name == "General")
                            {
                                snd.category = name.clone();
                            }
                        }
                        for c in &mut self.cfg.categories {
                            if c == &old_name {
                                *c = name.clone();
                            }
                        }
                        if let SideSelection::Category(c) = &self.side {
                            if c == &old_name {
                                self.side = SideSelection::Category(name.clone());
                            }
                        }
                    } else if !self.cfg.categories.contains(&name) {
                        self.cfg.categories.push(name);
                    }
                    self.dirty = true;
                }
            }
            self.category_dialog = None;
        }
        if !open {
            self.category_dialog = None;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SortKey {
    Rank(u8),
    Str(String),
    Num(u64),
}

#[derive(Clone, Copy, PartialEq)]
enum StopOthersOption {
    Global,
    Yes,
    No,
}

impl eframe::App for AstraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save();
            mic::cleanup_on_exit(self.cfg.settings.keep_virtual_mic);
        }
        self.handle_engine_events();
        self.handle_bg_events();
        self.handle_ipc(ctx);
        self.handle_portal_events();
        self.handle_dnd(ctx);
        self.handle_keys(ctx);

        if self.dirty {
            self.save();
        }

        self.transport_panel(ctx);
        self.side_panel(ctx);
        self.center_panel(ctx);

        self.settings_window(ctx);
        self.edit_window(ctx);
        self.confirm_window(ctx);
        self.category_dialog_window(ctx);

        if !self.playing.is_empty() || self.capture.is_some() || self.settings_open {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if !self.status.0.is_empty() {
            // let the status line disappear on time
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

}

// ---------------------------------------------------------------- helpers

fn sort_name(s: Sort, lang: Lang) -> &'static str {
    match s {
        Sort::Manual => tr(lang, K::SortManual),
        Sort::Name => tr(lang, K::SortName),
        Sort::Added => tr(lang, K::SortAdded),
        Sort::RecentlyPlayed => tr(lang, K::SortRecent),
        Sort::MostPlayed => tr(lang, K::SortPlayed),
    }
}

fn route_name(r: Route, lang: Lang) -> &'static str {
    match r {
        Route::Speakers => tr(lang, K::RouteSpeakers),
        Route::Mic => tr(lang, K::RouteMic),
        Route::Both => tr(lang, K::RouteBoth),
    }
}

fn route_override_name(r: RouteOverride, lang: Lang) -> &'static str {
    match r {
        RouteOverride::Default => tr(lang, K::RouteDefault),
        RouteOverride::Speakers => tr(lang, K::RouteSpeakers),
        RouteOverride::Mic => tr(lang, K::RouteMic),
        RouteOverride::Both => tr(lang, K::RouteBoth),
    }
}

fn stop_others_name(v: StopOthersOption, lang: Lang) -> &'static str {
    match v {
        StopOthersOption::Global => tr(lang, K::UseGlobal),
        StopOthersOption::Yes => tr(lang, K::Yes),
        StopOthersOption::No => tr(lang, K::No),
    }
}

fn lang_name(l: Lang) -> &'static str {
    l.name()
}

fn collect_audio(path: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > 10 {
        return;
    }
    if path.is_file() {
        let is_audio = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| AUDIO_EXTS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false);
        if is_audio {
            out.push(path.to_path_buf());
        }
        return;
    }
    if path.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            collect_audio(&e.path(), depth + 1, out);
        }
    }
}

// ---------------------------------------------------------------- theme & widgets

fn apply_theme(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|style| {
        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.window_stroke = egui::Stroke::new(1.0_f32, BORDER);
        v.window_rounding = egui::Rounding::same(14.0);
        v.menu_rounding = egui::Rounding::same(10.0);
        v.extreme_bg_color = Color32::from_rgb(0x0b, 0x0c, 0x10);
        v.faint_bg_color = SURFACE2;
        v.override_text_color = None;
        v.selection.bg_fill = ACCENT_DIM;
        v.selection.stroke = egui::Stroke::new(1.2_f32, ACCENT);
        v.hyperlink_color = ACCENT;
        let round = egui::Rounding::same(8.0);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.rounding = round;
        }
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER);
        v.widgets.inactive.bg_fill = SURFACE2;
        v.widgets.inactive.weak_bg_fill = SURFACE2;
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x2a, 0x2d, 0x3c);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x2d, 0x3c);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT_DIM);
        v.widgets.active.bg_fill = ACCENT_DIM;
        v.widgets.active.weak_bg_fill = ACCENT_DIM;
        v.slider_trailing_fill = true;
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.window_margin = egui::Margin::same(16.0);
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
    });
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).color(MUTED));
    ui.add_space(2.0);
}

/// Sidebar entry: icon, name, count badge; full-width hit area.
fn nav_item(ui: &mut egui::Ui, icon: &str, name: &str, count: usize, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    let fill = if selected {
        ACCENT_DIM
    } else if resp.hovered() {
        SURFACE2
    } else {
        Color32::TRANSPARENT
    };
    let p = ui.painter();
    p.rect_filled(rect, 8.0, fill);
    let color = if selected { TEXT } else { Color32::from_rgb(0xc4, 0xc7, 0xd4) };
    p.text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(14.0),
        if selected { ACCENT } else { MUTED },
    );
    let name_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(34.0, 0.0),
        rect.right_bottom() - egui::vec2(44.0, 0.0),
    );
    let galley = ui.painter().layout(
        name.to_string(),
        egui::FontId::proportional(14.0),
        color,
        name_rect.width(),
    );
    let galley_pos = egui::pos2(name_rect.left(), rect.center().y - galley.size().y / 2.0);
    ui.painter().with_clip_rect(name_rect).galley(galley_pos, galley, color);
    ui.painter().text(
        rect.right_center() - egui::vec2(12.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        count.to_string(),
        egui::FontId::proportional(12.0),
        MUTED,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// iOS-style switch.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(36.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let bg = if *on { ACCENT } else { Color32::from_rgb(0x3a, 0x3d, 0x4c) };
    ui.painter().rect_filled(rect, 10.0, bg);
    let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 7.5, Color32::WHITE);
    resp
}

/// Label + switch on one line; the whole line toggles.
fn switch_row(ui: &mut egui::Ui, label: &str, on: &mut bool) -> egui::Response {
    ui.horizontal(|ui| {
        let text_w = (ui.available_width() - 44.0).max(40.0);
        let text = ui
            .allocate_ui(egui::vec2(text_w, 20.0), |ui| {
                ui.set_width(text_w);
                ui.add(
                    egui::Label::new(RichText::new(label).color(TEXT))
                        .truncate()
                        .sense(egui::Sense::click())
                        .selectable(false),
                )
            })
            .inner;
        let mut resp = ui
            .with_layout(Layout::right_to_left(Align::Center), |ui| toggle_switch(ui, on))
            .inner;
        if text.clicked() {
            *on = !*on;
            resp.mark_changed();
        }
        resp
    })
    .inner
}

/// Round play / stop button drawn by hand (crisp at any scale).
fn play_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::click());
    let hovered = resp.hovered();
    let fill = if playing {
        ACCENT
    } else if hovered {
        ACCENT_DIM
    } else {
        SURFACE2
    };
    let p = ui.painter();
    p.circle(rect.center(), 18.0, fill, egui::Stroke::new(1.0_f32, if playing { ACCENT } else { BORDER }));
    let c = rect.center();
    let fg = if playing || hovered { Color32::WHITE } else { TEXT };
    if playing {
        p.rect_filled(egui::Rect::from_center_size(c, egui::vec2(11.0, 11.0)), 2.0, fg);
    } else {
        let pts = vec![c + egui::vec2(-4.0, -7.0), c + egui::vec2(-4.0, 7.0), c + egui::vec2(8.0, 0.0)];
        p.add(egui::Shape::convex_polygon(pts, fg, egui::Stroke::NONE));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn icon_button(ui: &mut egui::Ui, icon: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(15.0).color(MUTED))
            .frame(false)
            .min_size(egui::vec2(30.0, 30.0)),
    )
}

fn chip(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(SURFACE2)
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(7.0, 3.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).monospace().size(12.0).color(Color32::from_rgb(0xc9, 0xc0, 0xff)));
        });
}

fn accent_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).strong().color(Color32::WHITE))
        .fill(ACCENT)
        .rounding(10.0)
        .min_size(egui::vec2(160.0, 38.0))
}

/// Segmented control; returns true when the value changed. Options are
/// laid out right-to-left by the caller's layout, so pass them reversed.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    egui::Frame::none()
        .fill(SURFACE2)
        .stroke(egui::Stroke::new(1.0_f32, BORDER))
        .rounding(10.0)
        .inner_margin(egui::Margin::same(3.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (v, label) in options {
                let sel = *value == *v;
                let b = egui::Button::new(RichText::new(*label).color(if sel { Color32::WHITE } else { MUTED }))
                    .fill(if sel { ACCENT_DIM } else { Color32::TRANSPARENT })
                    .stroke(egui::Stroke::NONE)
                    .rounding(8.0);
                if ui.add(b).clicked() && !sel {
                    *value = *v;
                    changed = true;
                }
            }
        });
    changed
}

fn route_icon(r: Route) -> &'static str {
    match r {
        Route::Speakers => "🔊",
        Route::Mic => "🎤",
        Route::Both => "🔊 🎤",
    }
}

/// ASTRA's four-point star logo, painted (the font has no ✦ glyph).
fn star(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let r = size * 0.48;
    let w = size * 0.12;
    let pts = vec![
        c + egui::vec2(0.0, -r),
        c + egui::vec2(w, -w),
        c + egui::vec2(r, 0.0),
        c + egui::vec2(w, w),
        c + egui::vec2(0.0, r),
        c + egui::vec2(-w, w),
        c + egui::vec2(-r, 0.0),
        c + egui::vec2(-w, -w),
    ];
    // a concave outline does not fill cleanly: draw the inner square plus
    // one triangle per arm, all convex
    let inner = vec![pts[1], pts[3], pts[5], pts[7]];
    ui.painter().add(egui::Shape::convex_polygon(inner, ACCENT, egui::Stroke::NONE));
    for i in [0usize, 2, 4, 6] {
        let arm = vec![pts[(i + 7) % 8], pts[i], pts[i + 1]];
        ui.painter().add(egui::Shape::convex_polygon(arm, ACCENT, egui::Stroke::NONE));
    }
}
