//! The egui GUI: library, categories, playback controls and full settings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Align, Color32, Layout, RichText};

use crate::audio::engine::{Engine, EngineEvent, EngineMsg, PlayRequest};
use crate::audio::{mic, scan};
use crate::config::{hotkey, Config, Route, RouteOverride, Sort, Sound};
use crate::i18n::{tr, K, Lang};
use crate::ipc::{IpcToUi, PlayEntry, Snapshot};
use crate::niri;
use crate::portal::{PortalEvent, PortalHandle};

const ACCENT: Color32 = Color32::from_rgb(0x8f, 0x7a, 0xff);
const ACCENT_DIM: Color32 = Color32::from_rgb(0x3d, 0x35, 0x66);
const ROW_H: f32 = 34.0;
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
    MicState { ready: bool, passthrough: bool },
    NiriResult(Result<String, String>),
    AddPaths(Vec<String>),
    /// Is anything capturing from the virtual mic?
    MicCapture(bool),
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
    lang: Lang,
    cfg: Config,
    cfg_path: PathBuf,
    dirty: bool,

    engine: Engine,
    bg_tx: Sender<UiBgEvent>,
    bg_rx: Receiver<UiBgEvent>,
    ipc_rx: Receiver<IpcToUi>,
    ipc_snapshot: Snapshot,
    portal: Option<PortalHandle>,
    portal_rx: Option<Receiver<PortalEvent>>,
    portal_status: (String, bool),

    playing: HashMap<u64, f32>,
    mic_ready: bool,
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
}

impl AstraApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_visuals(egui::Visuals::dark());
        ctx.style_mut(|style| {
            let v = &mut style.visuals;
            v.panel_fill = Color32::from_rgb(15, 15, 21);
            v.window_fill = Color32::from_rgb(21, 21, 29);
            v.extreme_bg_color = Color32::from_rgb(11, 11, 16);
            v.selection.bg_fill = ACCENT_DIM;
            v.selection.stroke = egui::Stroke::new(1.2_f32, ACCENT);
            v.widgets.hovered.bg_fill = Color32::from_rgb(38, 38, 52);
            v.widgets.active.bg_fill = ACCENT_DIM;
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        });

        let cfg = Config::load();
        let lang = cfg.settings.lang.unwrap_or_else(Lang::from_env);
        let cfg_path = Config::config_path();

        let engine = Engine::start(cfg.settings.master_volume, cfg.settings.speaker_sink.clone());

        let (bg_tx, bg_rx) = mpsc::channel::<UiBgEvent>();
        let (ipc_tx, ipc_rx) = mpsc::channel::<IpcToUi>();
        let snapshot: Snapshot = Arc::new(Mutex::new(Vec::new()));

        crate::ipc::spawn_server(
            engine.tx.clone(),
            ipc_tx,
            snapshot.clone(),
            ctx.clone(),
        );

        let mut app = AstraApp {
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
        };
        app.sort = app.cfg.settings.sort;

        // Bring up the virtual microphone in the background.
        {
            let bg = bg_tx.clone();
            let settings_pass = app.cfg.settings.passthrough;
            let src = app.cfg.settings.passthrough_source.clone();
            let mon = app.cfg.settings.monitor_locally;
            std::thread::spawn(move || {
                let ready = mic::ensure_sink().is_ok();
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
                });
            });
        }

        app.refresh_snapshot();
        app.rebind_portal();
        app
    }

    fn t(&self, k: K) -> &'static str {
        tr(self.lang, k)
    }

    fn set_status(&mut self, msg: impl Into<String>, ok: bool) {
        self.status = (msg.into(), ok);
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
                    self.dirty = true;
                }
                UiBgEvent::MicState { ready, passthrough } => {
                    self.mic_ready = ready;
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
                PortalEvent::Activated(id) => {
                    if id == "stop-all" {
                        self.engine.stop_all();
                    } else if let Ok(n) = id.parse::<u64>() {
                        self.play(n);
                    }
                }
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
        let mon = self.cfg.settings.monitor_locally;
        std::thread::spawn(move || {
            let ready = mic::ensure_sink().is_ok();
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
        self.portal = Some(crate::portal::start(bindings, tx));
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

    fn top_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("astra_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("ASTRA").strong().size(20.0).color(ACCENT));
                ui.label(RichText::new(self.t(K::AppTagline)).weak().small());

                ui.separator();
                let search_hint = self.t(K::Search);
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text(search_hint)
                        .desired_width(220.0),
                );
                let _ = search;

                ui.separator();
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(sort_name(self.sort, self.lang))
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for s in [Sort::Manual, Sort::Name, Sort::Added, Sort::RecentlyPlayed, Sort::MostPlayed] {
                            ui.selectable_value(&mut self.sort, s, sort_name(s, self.lang));
                        }
                    });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(self.t(K::Settings)).clicked() {
                        self.settings_open = !self.settings_open;
                        if self.settings_open && self.devices_cache.is_none() {
                            self.load_devices();
                        }
                    }
                    if ui.button(self.t(K::AddFolder)).clicked() {
                        self.add_folder_dialog();
                    }
                    if ui.button(self.t(K::AddFiles)).clicked() {
                        self.add_files_dialog();
                    }
                    let stop = egui::Button::new(RichText::new(self.t(K::StopAll)).strong())
                        .fill(Color32::from_rgb(70, 30, 40));
                    if ui.add(stop).clicked() {
                        self.engine.stop_all();
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(self.t(K::Route));
                let mut r = self.cfg.settings.default_route;
                let combo = egui::ComboBox::from_id_salt("route")
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

                ui.label(self.t(K::MasterVolume));
                let mut pct = (self.cfg.settings.master_volume * 100.0).round() as i32;
                let slider = ui.add(
                    egui::Slider::new(&mut pct, 0..=150).suffix("%"),
                );
                if slider.changed() {
                    let vol = pct as f32 / 100.0;
                    self.cfg.settings.master_volume = vol;
                    self.engine.tx.send(EngineMsg::SetMasterVolume(vol)).ok();
                    self.dirty = true;
                }

                ui.separator();
                let mic_btn_label = format!(
                    "{}: {}",
                    self.t(K::VoiceToMic),
                    if self.passthrough { "ON" } else { "OFF" }
                );
                let mic_btn = egui::Button::new(RichText::new(&mic_btn_label).strong()).fill(
                    if self.passthrough {
                        ACCENT_DIM
                    } else {
                        Color32::from_rgb(40, 40, 48)
                    },
                );
                if ui.add(mic_btn).clicked() {
                    self.cfg.settings.passthrough = !self.cfg.settings.passthrough;
                    self.dirty = true;
                    self.apply_mic_settings();
                }
                ui.label(
                    RichText::new(if self.mic_ready {
                        self.t(K::MicStatusSink)
                    } else {
                        self.t(K::MicStatusNoSink)
                    })
                    .weak()
                    .small(),
                );
            });
            ui.add_space(4.0);
        });
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("astra_side")
            .default_width(190.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading(self.t(K::Categories));
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let all_count = self.cfg.sounds.len();
                    if ui
                        .selectable_label(self.side == SideSelection::All, format!("{} ({all_count})", self.t(K::AllSounds)))
                        .clicked()
                    {
                        self.side = SideSelection::All;
                    }
                    if !self.cfg.recent.is_empty()
                        && ui
                            .selectable_label(
                                self.side == SideSelection::Recent,
                                format!("{} ({})", self.t(K::Recent), self.cfg.recent.len()),
                            )
                            .clicked()
                    {
                        self.side = SideSelection::Recent;
                    }
                    ui.add_space(4.0);

                    let cats = self.cfg.categories.clone();
                    for cat in cats {
                        let count = self
                            .cfg
                            .sounds
                            .iter()
                            .filter(|s| {
                                (s.category.is_empty() && cat == "General") || s.category == cat
                            })
                            .count();
                        let label = format!("{cat} ({count})");
                        let resp = ui.selectable_label(
                            self.side == SideSelection::Category(cat.clone()),
                            label,
                        );
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
                });

                ui.separator();
                ui.add_space(2.0);
                if ui
                    .button(format!("＋ {}", self.t(K::NewCategory)))
                    .clicked()
                {
                    self.category_dialog = Some((None, String::new()));
                }
            });
    }

    fn center_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let ids = self.filtered_ids();
            if ids.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.heading(RichText::new("✦").color(ACCENT).size(40.0));
                    ui.heading(self.t(K::NoSounds));
                    ui.label(
                        RichText::new(self.t(K::NoSoundsHint))
                            .weak()
                            .text_style(egui::TextStyle::Small),
                    );
                });
                return;
            }
            ui.label(
                RichText::new(self.t(K::DoubleClickHint))
                    .weak()
                    .text_style(egui::TextStyle::Small),
            );
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show_rows(ui, ROW_H + 8.0, ids.len(), |ui, range| {
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
        let playing = self.playing.contains_key(&id);
        let missing = !Path::new(&s.path).exists();

        let frame = egui::Frame::none()
            .fill(if Some(id) == self.selected {
                ACCENT_DIM.linear_multiply(0.5)
            } else if playing {
                Color32::from_rgb(30, 40, 32)
            } else {
                Color32::TRANSPARENT
            })
            .inner_margin(egui::Margin::symmetric(6.0, 4.0));

        let resp = frame
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_size(egui::vec2(ui.available_width(), ROW_H));
                    let play_label = if playing {
                        self.t(K::Stop)
                    } else {
                        self.t(K::Play)
                    };
                    let btn = if playing {
                        egui::Button::new(RichText::new(play_label).strong())
                            .fill(ACCENT_DIM)
                            .min_size(egui::vec2(62.0, 24.0))
                    } else {
                        egui::Button::new(play_label).min_size(egui::vec2(62.0, 24.0))
                    };
                    if ui.add(btn).clicked() {
                        self.toggle(id);
                    }

                    let fixed = 250.0;
                    let name_w = (ui.available_width() - fixed).max(80.0);
                    let mut name_text = RichText::new(&s.name).strong();
                    if missing {
                        name_text = name_text.color(Color32::from_rgb(220, 100, 100));
                    }
                    let name_resp = ui.add_sized(
                        [name_w, 22.0],
                        egui::Label::new(name_text).truncate(),
                    );
                    if name_resp.clicked() {
                        self.selected = Some(id);
                    }

                    if let Some(pos) = self.playing.get(&id) {
                        let frac = if s.duration_secs > 0.0 {
                            (pos / s.duration_secs).clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        ui.add_sized(
                            [90.0, 6.0],
                            egui::ProgressBar::new(frac).desired_height(6.0),
                        );
                    } else {
                        ui.label(
                            RichText::new(s.display_duration())
                                .weak()
                                .monospace()
                                .small(),
                        );
                    }

                    if let Some(hk) = &s.hotkey {
                        ui.label(
                            RichText::new(hk)
                                .monospace()
                                .small()
                                .background_color(ACCENT_DIM),
                        );
                    } else {
                        ui.label(RichText::new(" ").small());
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("×").size(16.0)).small())
                            .clicked()
                        {
                            self.pending_delete = Some(PendingDelete::Sound(id));
                        }
                        if ui.add(egui::Button::new("…").small()).clicked() {
                            self.edit = Some(EditSound::from(&s));
                        }
                    });
                });
            })
            .response;

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
                    let _ = std::process::Command::new("xdg-open")
                        .arg(dir)
                        .spawn();
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

    fn bottom_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("astra_bottom").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let (msg, ok) = &self.status;
                let mut text = RichText::new(msg.clone()).small();
                if !ok {
                    text = text.color(Color32::from_rgb(240, 120, 120));
                } else if !msg.is_empty() {
                    text = text.color(Color32::from_rgb(140, 200, 150));
                }
                ui.label(text);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(self.portal_status.0.clone())
                            .small()
                            .weak(),
                    );
                    ui.separator();
                    let counts = format!(
                        "{} {} · {} {}",
                        self.cfg.sounds.len(),
                        self.t(K::SoundsCount),
                        self.playing.len(),
                        self.t(K::PlayingNow)
                    );
                    ui.label(RichText::new(counts).weak().small());
                });
            });
            ui.add_space(2.0);
        });
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
            self.settings_open = false;
            self.save();
            self.rebind_portal();
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
        let status = if self.mic_ready {
            RichText::new(format!("● {}", self.t(K::MicStatusSink))).color(Color32::from_rgb(120, 220, 140))
        } else {
            RichText::new(format!("● {}", self.t(K::MicStatusNoSink))).color(Color32::from_rgb(220, 120, 120))
        };
        ui.label(status);
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
                        if name.contains(".monitor") {
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

        self.top_panel(ctx);
        self.side_panel(ctx);
        self.center_panel(ctx);
        self.bottom_panel(ctx);

        self.settings_window(ctx);
        self.edit_window(ctx);
        self.confirm_window(ctx);
        self.category_dialog_window(ctx);

        if !self.playing.is_empty() || self.capture.is_some() || self.settings_open {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
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
