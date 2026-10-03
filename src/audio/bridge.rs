//! The bridge: captures the internal mixer sink's monitor and feeds the
//! virtual microphone source (module-pipe-source FIFO). This is what makes
//! the mix (sounds + passthrough voice) appear inside a real input device.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use libpulse_binding as pa;
use libpulse_simple_binding as psimple;

use crate::audio::mic;

static BRIDGE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Spawn the bridge once. `done` is called with Err(message) when the bridge
/// could not be set up (pactl missing, module failed, capture failed).
pub fn spawn<F>(on_fail: F)
where
    F: FnOnce(Result<(), String>) + Send + 'static,
{
    if BRIDGE_RUNNING.swap(true, Ordering::Relaxed) {
        on_fail(Ok(()));
        return;
    }
    thread::spawn(move || {
        let res = run();
        if res.is_err() {
            BRIDGE_RUNNING.store(false, Ordering::Relaxed);
        }
        on_fail(res);
    });
}

fn run() -> Result<(), String> {
    mic::ensure_sink()?;
    mic::ensure_source()?;

    // Record the full mix (sounds + passthrough voice) from the monitor.
    let spec = pa::sample::Spec {
        format: pa::sample::Format::F32le,
        rate: 48_000,
        channels: 2,
    };
    if !spec.is_valid() {
        return Err("invalid sample spec".into());
    }
    let record = psimple::Simple::new(
        None,
        "astra",
        pa::stream::Direction::Record,
        Some(&format!("{}.monitor", mic::MIC_SINK_NAME)),
        "astra virtual mic bridge",
        &spec,
        None,
        None,
    )
    .map_err(|e| format!("capture monitor: {e}"))?;

    // Open the FIFO non-blocking: when nobody captures from the virtual mic
    // and the module pauses reading, we drop chunks instead of stalling.
    let fifo_path = mic::source_fifo();
    let mut fifo = open_fifo(&fifo_path);

    let mut buf = vec![0u8; 8192]; // 1024 stereo f32 frames
    loop {
        if record.read(&mut buf).is_err() {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        let samples: &[f32] = bytemuck::cast_slice(&buf);
        let mut out = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        if fifo.is_none() {
            thread::sleep(Duration::from_millis(200));
            fifo = open_fifo(&fifo_path);
            if fifo.is_none() {
                continue;
            }
        }
        match fifo.as_mut().unwrap().write(&out) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // nobody is capturing — drop the chunk, keep pacing
            }
            Err(_) => {
                // stale/broken fifo: reopen shortly
                fifo = None;
            }
        }
    }
}

fn open_fifo(path: &std::path::Path) -> Option<std::fs::File> {
    for _ in 0..20 {
        if let Ok(f) = OpenOptions::new()
            .write(true)
            .custom_flags(0o4000) // O_NONBLOCK
            .open(path)
        {
            return Some(f);
        }
        thread::sleep(Duration::from_millis(250));
    }
    None
}
