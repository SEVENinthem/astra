//! IPC: a unix socket the CLI talks to, so niri binds / scripts / OBS can
//! control a running ASTRA instance.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::audio::engine::{EngineMsg, PlayRequest};
use crate::config::Route;

/// Entry snapshot the UI publishes so the IPC thread can resolve targets and
/// build play requests without touching UI state.
#[derive(Clone, Debug)]
pub struct PlayEntry {
    pub id: u64,
    pub name: String,
    pub hotkey: Option<String>,
    pub path: String,
    pub volume: f32,
    pub gain: f32,
    pub route: Route,
    pub loop_: bool,
    pub stop_others: bool,
    pub speed: f32,
}

/// Messages the IPC server forwards to the UI.
pub enum IpcToUi {
    Add(Vec<String>),
    Reload,
    Quit,
    Focus,
}

pub type Snapshot = Arc<Mutex<Vec<PlayEntry>>>;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("astra.sock")
}

pub fn server_alive() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

pub fn spawn_server(
    engine_tx: Sender<EngineMsg>,
    ui_tx: Sender<IpcToUi>,
    snapshot: Snapshot,
    repaint: eframe::egui::Context,
) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // stale socket
    let listener = match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(_) => return, // another instance owns it
    };
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { break };
            let engine_tx = engine_tx.clone();
            let ui_tx = ui_tx.clone();
            let snapshot = snapshot.clone();
            let repaint = repaint.clone();
            thread::spawn(move || {
                let _ = handle_conn(stream, engine_tx, ui_tx, snapshot);
                repaint.request_repaint();
            });
        }
    });
}

fn handle_conn(
    stream: UnixStream,
    engine_tx: Sender<EngineMsg>,
    ui_tx: Sender<IpcToUi>,
    snapshot: Snapshot,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| format!("bad request: {e}"))?;
    let reply = handle_cmd(&v, &engine_tx, &ui_tx, &snapshot);
    let mut stream = stream;
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
    Ok(())
}

fn handle_cmd(
    v: &Value,
    engine_tx: &Sender<EngineMsg>,
    ui_tx: &Sender<IpcToUi>,
    snapshot: &Snapshot,
) -> String {
    let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let entries = snapshot.lock().unwrap().clone();
    match cmd {
        "play" | "toggle" => {
            let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("");
            let vol = v.get("volume").and_then(|t| t.as_f64()).map(|f| f as f32);
            match resolve(entries.as_slice(), target) {
                Some(e) => {
                    let should_stop = cmd == "toggle" && {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let _ = engine_tx.send(EngineMsg::IsPlaying(e.id, tx));
                        rx.recv_timeout(Duration::from_millis(300)).unwrap_or(false)
                    };
                    if should_stop {
                        let _ = engine_tx.send(EngineMsg::Stop(e.id));
                        format!("Stopped: {}", e.name)
                    } else {
                        let mut req = PlayRequest {
                            id: e.id,
                            path: e.path.clone(),
                            volume: e.volume,
                            gain: e.gain,
                            route: e.route,
                            loop_: e.loop_,
                            stop_others: e.stop_others,
                            speed: e.speed,
                        };
                        if let Some(v) = vol {
                            req.volume = (v / 100.0).clamp(0.0, 1.5);
                        }
                        let _ = engine_tx.send(EngineMsg::Play(req));
                        format!("Playing: {}", e.name)
                    }
                }
                None => format!("ERR no sound matches '{target}'"),
            }
        }
        "stop" => {
            match v.get("target").and_then(|t| t.as_str()) {
                Some(target) if !target.is_empty() => match resolve(&entries, target) {
                    Some(e) => {
                        let _ = engine_tx.send(EngineMsg::Stop(e.id));
                        format!("Stopped: {}", e.name)
                    }
                    None => format!("ERR no sound matches '{target}'"),
                },
                _ => {
                    let _ = engine_tx.send(EngineMsg::StopAll);
                    "Stopped everything".into()
                }
            }
        }
        "stop-all" => {
            let _ = engine_tx.send(EngineMsg::StopAll);
            "Stopped everything".into()
        }
        "list" => {
            let mut out = String::from("id\thotkey\tname\n");
            for e in &entries {
                out.push_str(&format!(
                    "{}\t{}\t{}\n",
                    e.id,
                    e.hotkey.as_deref().unwrap_or("-"),
                    e.name
                ));
            }
            out
        }
        "add" => {
            let paths: Vec<String> = v
                .get("paths")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                return "ERR no paths given".into();
            }
            let _ = ui_tx.send(IpcToUi::Add(paths));
            "queued".into()
        }
        "mic" => {
            if let Err(e) = crate::audio::mic::ensure_sink().and_then(|_| crate::audio::mic::ensure_source()) {
                return format!("ERR {e}");
            }
            let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("status");
            match state {
                "on" => match crate::audio::mic::set_passthrough(true, v.get("source").and_then(|s| s.as_str())) {
                    Ok(s) => format!("passthrough: {s}"),
                    Err(e) => format!("ERR {e}"),
                },
                "off" => match crate::audio::mic::set_passthrough(false, None) {
                    Ok(s) => format!("passthrough: {s}"),
                    Err(e) => format!("ERR {e}"),
                },
                "toggle" => {
                    let cur = crate::audio::mic::passthrough_active();
                    match crate::audio::mic::set_passthrough(!cur, None) {
                        Ok(s) => format!("passthrough: {s}"),
                        Err(e) => format!("ERR {e}"),
                    }
                }
                _ => {
                    let source = crate::audio::mic::source_exists(crate::audio::mic::MIC_SOURCE_NAME);
                    let pass = crate::audio::mic::passthrough_active();
                    format!(
                        "virtual-mic: {}, passthrough: {}",
                        if source { "on" } else { "off" },
                        if pass { "on" } else { "off" }
                    )
                }
            }
        }
        "vol" => {
            let val = v.get("value").and_then(|x| x.as_f64()).unwrap_or(100.0) as f32;
            let _ = engine_tx.send(EngineMsg::SetMasterVolume((val / 100.0).clamp(0.0, 1.5)));
            format!("master volume: {}%", val.round())
        }
        "reload" => {
            let _ = ui_tx.send(IpcToUi::Reload);
            "reloaded".into()
        }
        "quit" => {
            let _ = ui_tx.send(IpcToUi::Quit);
            "bye".into()
        }
        "focus" => {
            let _ = ui_tx.send(IpcToUi::Focus);
            "focused".into()
        }
        other => format!("ERR unknown command '{other}'"),
    }
}

fn resolve(entries: &[PlayEntry], target: &str) -> Option<PlayEntry> {
    if let Ok(id) = target.parse::<u64>() {
        if let Some(e) = entries.iter().find(|e| e.id == id) {
            return Some(e.clone());
        }
    }
    let t = target.to_lowercase();
    let hits: Vec<&PlayEntry> = entries
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&t))
        .collect();
    match hits.len() {
        1 => Some(hits[0].clone()),
        _ => None,
    }
}

/// CLI-side: send a command to the running instance and print the reply.
pub fn client(cmd: crate::Command) -> Result<(), String> {
    let v = match cmd {
        crate::Command::Play { target, volume } => json!({"cmd": "play", "target": target, "volume": volume}),
        crate::Command::Toggle { target } => json!({"cmd": "toggle", "target": target}),
        crate::Command::Stop { target } => json!({"cmd": "stop", "target": target}),
        crate::Command::StopAll => json!({"cmd": "stop-all"}),
        crate::Command::List => json!({"cmd": "list"}),
        crate::Command::Add { paths } => json!({"cmd": "add", "paths": paths}),
        crate::Command::Mic { state } => json!({"cmd": "mic", "state": state}),
        crate::Command::Vol { value } => json!({"cmd": "vol", "value": value}),
        crate::Command::Reload => json!({"cmd": "reload"}),
        crate::Command::Quit => json!({"cmd": "quit"}),
        crate::Command::Focus => json!({"cmd": "focus"}),
    };
    let out = send(&v.to_string())?;
    if !out.is_empty() {
        if out.ends_with('\n') {
            print!("{out}");
        } else {
            println!("{out}");
        }
    }
    if out.starts_with("ERR") {
        std::process::exit(1);
    }
    Ok(())
}

pub fn send(line: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket_path())
        .map_err(|_| "ASTRA GUI is not running (start it with: astra)".to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u64, name: &str) -> PlayEntry {
        PlayEntry {
            id,
            name: name.into(),
            hotkey: None,
            path: String::new(),
            volume: 1.0,
            gain: 1.0,
            route: Route::Speakers,
            loop_: false,
            stop_others: true,
            speed: 1.0,
        }
    }

    #[test]
    fn resolve_by_id_and_name() {
        let v = vec![e(1, "Air Horn"), e(2, "drum roll")];
        assert_eq!(resolve(&v, "2").unwrap().id, 2);
        assert_eq!(resolve(&v, "air horn").unwrap().id, 1);
        assert_eq!(resolve(&v, "DRUM").unwrap().id, 2);
        assert!(resolve(&v, "drum roll extra").is_none()); // ambiguous: no match on substring
        assert!(resolve(&v, "r").is_none()); // multiple hits
        assert!(resolve(&v, "nope").is_none());
    }
}
