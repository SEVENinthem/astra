//! ASTRA — Soundpad-style soundboard for Linux (PipeWire / Wayland).

mod app;
mod audio;
mod config;
mod i18n;
mod ipc;
mod niri;
mod portal;

use clap::{Parser, Subcommand};
use eframe::egui;

#[derive(Parser)]
#[command(
    name = "astra",
    version,
    about = "ASTRA — a Soundpad-style soundboard for Linux (PipeWire, Wayland)",
    long_about = None,
    after_help = "Run without arguments to launch the GUI.\
\nMost commands talk to a running ASTRA instance over IPC.\
\nExamples: astra play 3, astra play \"air horn\", astra mic toggle, astra stop-all"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Play a sound (by id or name fragment)
    Play {
        target: String,
        /// Override volume in percent (0..150)
        #[arg(long)]
        volume: Option<f32>,
    },
    /// Play a sound, or stop it if it is already playing
    Toggle { target: String },
    /// Stop a sound (by id or name), or everything when omitted
    Stop { target: Option<String> },
    /// Stop all playing sounds
    StopAll,
    /// List sounds: id, hotkey, name
    List,
    /// Add files or folders to the library
    Add { paths: Vec<String> },
    /// Microphone passthrough: on | off | toggle | status
    Mic { state: String },
    /// Set master volume in percent (0..150)
    Vol { value: f32 },
    /// Re-read the config file from disk
    Reload,
    /// Close the running ASTRA instance
    Quit,
    /// Raise the window of the running instance
    Focus,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(cmd) => {
            if let Err(e) = ipc::client(cmd) {
                eprintln!("astra: {e}");
                std::process::exit(1);
            }
        }
        None => {
            if ipc::server_alive() {
                // Second launch: raise the existing window instead of dying silently.
                let _ = ipc::client(Command::Focus);
                return;
            }
            if let Err(e) = gui() {
                eprintln!("astra: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn gui() -> Result<(), eframe::Error> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ASTRA")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([820.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ASTRA",
        native,
        Box::new(|cc| Ok(Box::new(app::AstraApp::new(cc)))),
    )
}
