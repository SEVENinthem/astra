//! Config / library persistence and the sound data model.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::i18n::Lang;

/// Where a sound is played. `Mic` is the PipeWire virtual microphone sink,
/// so voice-chat apps that selected it as input will hear the sound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    Speakers,
    Mic,
    #[default]
    Both,
}

/// Per-sound route override.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteOverride {
    #[default]
    Default,
    Speakers,
    Mic,
    Both,
}

impl RouteOverride {
    pub fn resolve(self, default: Route) -> Route {
        match self {
            RouteOverride::Default => default,
            RouteOverride::Speakers => Route::Speakers,
            RouteOverride::Mic => Route::Mic,
            RouteOverride::Both => Route::Both,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    Manual,
    Name,
    Added,
    RecentlyPlayed,
    MostPlayed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sound {
    pub id: u64,
    pub name: String,
    pub path: String,
    #[serde(default = "d_one")]
    pub volume: f32,
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub route: RouteOverride,
    #[serde(default, rename = "loop")]
    pub loop_: bool,
    #[serde(default)]
    pub category: String,
    #[serde(default = "d_true")]
    pub normalize: bool,
    #[serde(default = "d_one")]
    pub speed: f32,
    /// None = follow global setting.
    #[serde(default)]
    pub stop_others: Option<bool>,
    #[serde(default)]
    pub duration_secs: f32,
    #[serde(default)]
    pub peak: f32,
    #[serde(default)]
    pub added_ms: u64,
    #[serde(default)]
    pub last_played_ms: Option<u64>,
    #[serde(default)]
    pub play_count: u64,
}

impl Sound {
    /// Extra gain from loudness normalization, clamped to a sane range.
    pub fn norm_gain(&self, _default_on: bool) -> f32 {
        if self.normalize && self.peak > 0.05 {
            (0.7 / self.peak).clamp(0.05, 8.0)
        } else {
            1.0
        }
    }

    pub fn display_duration(&self) -> String {
        fmt_secs(self.duration_secs)
    }
}

pub fn fmt_secs(s: f32) -> String {
    if s <= 0.0 {
        return "—".into();
    }
    let total = s.round() as u64;
    if total >= 60 {
        format!("{}:{:02}", total / 60, total % 60)
    } else {
        format!("0:{:02}", total)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub lang: Option<Lang>,
    #[serde(default = "d_true")]
    pub stop_others: bool,
    #[serde(default = "d_true")]
    pub normalize: bool,
    #[serde(default = "d_one")]
    pub master_volume: f32,
    #[serde(default)]
    pub default_route: Route,
    /// None = PipeWire default sink.
    #[serde(default)]
    pub speaker_sink: Option<String>,
    /// None = system default source.
    #[serde(default)]
    pub passthrough_source: Option<String>,
    #[serde(default = "d_true")]
    pub passthrough: bool,
    #[serde(default)]
    pub monitor_locally: bool,
    #[serde(default)]
    pub keep_virtual_mic: bool,
    #[serde(default = "d_true")]
    pub enable_ipc: bool,
    #[serde(default = "d_true")]
    pub portal_hotkeys: bool,
    #[serde(default)]
    pub stop_all_hotkey: Option<String>,
    #[serde(default)]
    pub sort: Sort,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "d_v1")]
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default = "d_categories")]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sounds: Vec<Sound>,
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub recent: Vec<u64>,
}

fn d_one() -> f32 {
    1.0
}
fn d_true() -> bool {
    true
}
fn d_v1() -> u32 {
    1
}
fn d_categories() -> Vec<String> {
    vec!["General".into()]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 1,
            settings: Settings::default(),
            categories: d_categories(),
            sounds: Vec::new(),
            next_id: 1,
            recent: Vec::new(),
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("astra")
            .join("config.json")
    }

    pub fn load() -> Config {
        let path = Self::config_path();
        let mut cfg: Config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if !cfg.categories.iter().any(|c| c == "General") {
            cfg.categories.insert(0, "General".into());
        }
        if cfg.next_id == 0 {
            cfg.next_id = cfg.sounds.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        }
        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn sound(&self, id: u64) -> Option<&Sound> {
        self.sounds.iter().find(|s| s.id == id)
    }

    pub fn sound_mut(&mut self, id: u64) -> Option<&mut Sound> {
        self.sounds.iter_mut().find(|s| s.id == id)
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// Hotkey token helpers. Tokens look like "Super+F1", "Ctrl+Shift+7", "F13".
pub mod hotkey {
    use eframe::egui;

    /// Build a hotkey token from a key press. Returns None for bare modifiers
    /// or keys we don't want to bind.
    pub fn token_from(mods: egui::Modifiers, key: egui::Key) -> Option<String> {
        let key_name = key_name(key)?;
        let mut parts = Vec::new();
        if mods.command {
            parts.push("Super");
        }
        if mods.ctrl {
            parts.push("Ctrl");
        }
        if mods.alt {
            parts.push("Alt");
        }
        if mods.shift {
            parts.push("Shift");
        }
        parts.push(key_name.as_str());
        Some(parts.join("+"))
    }

    fn key_name(key: egui::Key) -> Option<String> {
        use egui::Key::*;
        let name = match key {
            A => "A",
            B => "B",
            C => "C",
            D => "D",
            E => "E",
            F => "F",
            G => "G",
            H => "H",
            I => "I",
            J => "J",
            K => "K",
            L => "L",
            M => "M",
            N => "N",
            O => "O",
            P => "P",
            Q => "Q",
            R => "R",
            S => "S",
            T => "T",
            U => "U",
            V => "V",
            W => "W",
            X => "X",
            Y => "Y",
            Z => "Z",
            Num0 => "0",
            Num1 => "1",
            Num2 => "2",
            Num3 => "3",
            Num4 => "4",
            Num5 => "5",
            Num6 => "6",
            Num7 => "7",
            Num8 => "8",
            Num9 => "9",
            F1 => "F1",
            F2 => "F2",
            F3 => "F3",
            F4 => "F4",
            F5 => "F5",
            F6 => "F6",
            F7 => "F7",
            F8 => "F8",
            F9 => "F9",
            F10 => "F10",
            F11 => "F11",
            F12 => "F12",
            ArrowUp => "Up",
            ArrowDown => "Down",
            ArrowLeft => "Left",
            ArrowRight => "Right",
            Minus => "Minus",
            Equals => "Equal",
            Backtick => "Grave",
            Backslash => "Backslash",
            Comma => "Comma",
            Period => "Period",
            Slash => "Slash",
            Semicolon => "Semicolon",
            Quote => "Apostrophe",
            OpenBracket => "BracketLeft",
            CloseBracket => "BracketRight",
            Space => "Space",
            Insert => "Insert",
            Delete => "Delete",
            Home => "Home",
            End => "End",
            PageUp => "Prior",
            PageDown => "Next",
            Escape | Enter | Tab | Backspace => return None,
            _ => return None,
        };
        Some(name.to_string())
    }

    /// Convert our token to a niri bind key ("Super+F1" -> "Mod+F1").
    pub fn to_niri(token: &str) -> String {
        token.replace("Super+", "Mod+")
    }

    /// Convert our token to an XDG GlobalShortcuts portal trigger
    /// ("Super+F1" -> "SUPER+F1").
    pub fn to_portal(token: &str) -> String {
        let mut out = String::new();
        for part in token.split('+') {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(&part.to_uppercase());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut cfg = Config::default();
        cfg.sounds.push(Sound {
            id: 1,
            name: "airhorn".into(),
            path: "/tmp/a.mp3".into(),
            volume: 0.8,
            hotkey: Some("Super+F1".into()),
            route: RouteOverride::Mic,
            loop_: true,
            category: "General".into(),
            normalize: false,
            speed: 1.25,
            stop_others: Some(true),
            duration_secs: 1.5,
            peak: 0.5,
            added_ms: 1,
            last_played_ms: None,
            play_count: 3,
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sounds[0].loop_, true);
        assert_eq!(back.sounds[0].hotkey.as_deref(), Some("Super+F1"));
        assert_eq!(back.sounds[0].route, RouteOverride::Mic);
        assert_eq!(back.categories, vec!["General"]);
    }

    #[test]
    fn hotkey_tokens() {
        assert_eq!(hotkey::to_niri("Super+F1"), "Mod+F1");
        assert_eq!(hotkey::to_niri("Ctrl+Shift+7"), "Ctrl+Shift+7");
        assert_eq!(hotkey::to_portal("Ctrl+Shift+7"), "CTRL+SHIFT+7");
        assert_eq!(hotkey::to_portal("Super+Grave"), "SUPER+GRAVE");
    }

    #[test]
    fn norm_gain_clamped() {
        let mut s = Sound {
            id: 1,
            name: "x".into(),
            path: String::new(),
            volume: 1.0,
            hotkey: None,
            route: RouteOverride::default(),
            loop_: false,
            category: String::new(),
            normalize: true,
            speed: 1.0,
            stop_others: None,
            duration_secs: 0.0,
            peak: 0.01,
            added_ms: 0,
            last_played_ms: None,
            play_count: 0,
        };
        assert!((s.norm_gain(true) - 1.0).abs() < 1e-6); // peak below threshold: no boost
        s.peak = 0.1;
        assert!((s.norm_gain(true) - 7.0).abs() < 1e-6);
        s.normalize = false;
        assert!((s.norm_gain(true) - 1.0).abs() < 1e-6);
    }
}
