//! `astra latency`: measures trigger -> sound latency end to end, the way a
//! voice app hears it. Plays a generated click into the virtual mic and
//! records the virtual mic source until the click shows up.

use std::time::{Duration, Instant};

use libpulse_binding as pa;
use libpulse_simple_binding as psimple;

use crate::audio::engine::{Engine, PlayRequest};
use crate::audio::mic;
use crate::config::Route;

const RATE: u32 = 48_000;
/// 2.5 ms capture chunks keep the measurement's own overhead small.
const READ_FRAMES: usize = 120;

pub fn run(runs: usize) -> Result<(), String> {
    mic::ensure_sink()?;
    mic::ensure_source()?;

    let click = std::env::temp_dir().join("astra-latency-click.wav");
    write_click(&click).map_err(|e| format!("write test sound: {e}"))?;

    let engine = Engine::start(1.0, None);
    let spec = pa::sample::Spec {
        format: pa::sample::Format::F32le,
        rate: RATE,
        channels: 1,
    };
    let frag = (READ_FRAMES * 4) as u32;
    let attr = pa::def::BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frag,
    };
    let rec = psimple::Simple::new(
        None,
        "astra-latency",
        pa::stream::Direction::Record,
        Some(mic::MIC_SOURCE_NAME),
        "latency probe",
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| format!("record {}: {e}", mic::MIC_SOURCE_NAME))?;

    let mut buf = vec![0u8; READ_FRAMES * 4];
    let mut results = Vec::new();
    for i in 0..runs {
        // let the previous click and any backlog drain out
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_millis(400) {
            rec.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        }
        let t0 = Instant::now();
        engine.play(PlayRequest {
            id: 1,
            path: click.to_string_lossy().into_owned(),
            volume: 1.0,
            gain: 1.0,
            route: Route::Mic,
            loop_: false,
            stop_others: true,
            speed: 1.0,
        });
        let found = loop {
            rec.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            let samples: &[f32] = bytemuck::cast_slice(&buf);
            if samples.iter().any(|s| s.abs() > 0.25) {
                break Some(t0.elapsed());
            }
            if t0.elapsed() > Duration::from_secs(3) {
                break None;
            }
        };
        match found {
            Some(d) => {
                println!("run {:>2}: {:>6.1} ms", i + 1, d.as_secs_f64() * 1000.0);
                results.push(d.as_secs_f64() * 1000.0);
            }
            None => println!("run {:>2}: no sound within 3 s", i + 1),
        }
    }
    let _ = std::fs::remove_file(&click);
    if results.is_empty() {
        return Err("the click never reached the virtual microphone".into());
    }
    results.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "\nvirtual mic latency: median {:.1} ms, min {:.1} ms, max {:.1} ms",
        results[results.len() / 2],
        results[0],
        results[results.len() - 1]
    );
    Ok(())
}

/// 50 ms full-scale square burst at 48 kHz, 16-bit mono WAV.
fn write_click(path: &std::path::Path) -> std::io::Result<()> {
    let n = (RATE / 20) as usize;
    let data: Vec<u8> = (0..n)
        .flat_map(|i| {
            let v: i16 = if (i / 24) % 2 == 0 { 30000 } else { -30000 };
            v.to_le_bytes()
        })
        .collect();
    let mut w = Vec::with_capacity(44 + data.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    std::fs::write(path, w)
}
