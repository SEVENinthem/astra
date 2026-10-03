//! Playback engine: decodes files with symphonia and writes raw samples to
//! PulseAudio/PipeWire sinks chosen per-sound (speakers and/or virtual mic).

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use libpulse_binding as pa;
use libpulse_simple_binding as psimple;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::audio::mic;
use crate::config::Route;

#[derive(Clone, Debug)]
pub struct PlayRequest {
    pub id: u64,
    pub path: String,
    pub volume: f32,
    pub gain: f32,
    pub route: Route,
    pub loop_: bool,
    pub stop_others: bool,
    pub speed: f32,
}

pub enum EngineMsg {
    Play(PlayRequest),
    Stop(u64),
    StopAll,
    SetMasterVolume(f32),
    /// Ask whether a sound is currently playing (for `astra toggle`).
    IsPlaying(u64, Sender<bool>),
}

pub enum EngineEvent {
    Started(u64),
    Progress(u64, f32),
    Finished(u64),
    Error(u64, String),
    Status(String),
}

#[derive(Clone, Copy)]
enum JobCmd {
    Stop,
}

pub struct Engine {
    pub tx: Sender<EngineMsg>,
    pub rx: Receiver<EngineEvent>,
    speaker_sink: Option<String>,
}

impl Engine {
    pub fn start(master_volume: f32, speaker_sink: Option<String>) -> Engine {
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let master = Arc::new(Mutex::new(master_volume));
        let jobs: Arc<Mutex<HashMap<u64, Sender<JobCmd>>>> = Arc::new(Mutex::new(HashMap::new()));

        {
            let jobs = jobs.clone();
            let etx = etx.clone();
            let master = master.clone();
            let speaker_sink = speaker_sink.clone();
            thread::spawn(move || {
                for msg in rx {
                    match msg {
                        EngineMsg::Play(req) => {
                            if req.stop_others {
                                let map = jobs.lock().unwrap();
                                for (id, jtx) in map.iter() {
                                    if *id != req.id {
                                        let _ = jtx.send(JobCmd::Stop);
                                    }
                                }
                            }
                            let (stop_tx, stop_rx) = mpsc::channel();
                            // re-pressing a playing sound restarts it
                            if let Some(old) = jobs.lock().unwrap().insert(req.id, stop_tx) {
                                let _ = old.send(JobCmd::Stop);
                            }
                            let etx = etx.clone();
                            let master = master.clone();
                            let speaker_sink = speaker_sink.clone();
                            thread::spawn(move || run_job(req, etx, master, stop_rx, speaker_sink));
                        }
                        EngineMsg::Stop(id) => {
                            if let Some(jtx) = jobs.lock().unwrap().remove(&id) {
                                let _ = jtx.send(JobCmd::Stop);
                            }
                        }
                        EngineMsg::StopAll => {
                            let mut map = jobs.lock().unwrap();
                            for (_, jtx) in map.drain() {
                                let _ = jtx.send(JobCmd::Stop);
                            }
                        }
                        EngineMsg::SetMasterVolume(v) => {
                            *master.lock().unwrap() = v;
                        }
                        EngineMsg::IsPlaying(id, reply) => {
                            let _ = reply.send(jobs.lock().unwrap().contains_key(&id));
                        }
                    }
                }
                let _ = etx;
            });
        }

        Engine {
            tx,
            rx: erx,
            speaker_sink,
        }
    }

    pub fn set_speaker_sink(&mut self, sink: Option<String>) {
        self.speaker_sink = sink;
    }

    pub fn play(&self, req: PlayRequest) {
        let _ = self.tx.send(EngineMsg::Play(req));
    }

    pub fn stop(&self, id: u64) {
        let _ = self.tx.send(EngineMsg::Stop(id));
    }

    pub fn stop_all(&self) {
        let _ = self.tx.send(EngineMsg::StopAll);
    }
}

fn run_job(
    req: PlayRequest,
    events: Sender<EngineEvent>,
    master: Arc<Mutex<f32>>,
    stop_rx: Receiver<JobCmd>,
    speaker_sink: Option<String>,
) {
    let _ = events.send(EngineEvent::Started(req.id));
    let mut played_any = false;
    loop {
        if matches!(stop_rx.try_recv(), Ok(JobCmd::Stop)) {
            break;
        }
        match play_once(&req, &events, &master, &stop_rx, speaker_sink.as_deref()) {
            Ok(()) => {
                played_any = true;
                if req.loop_ && !matches!(stop_rx.try_recv(), Ok(JobCmd::Stop)) {
                    continue;
                }
                break;
            }
            Err(e) => {
                let _ = events.send(EngineEvent::Error(req.id, e));
                break;
            }
        }
    }
    if played_any {
        let _ = events.send(EngineEvent::Finished(req.id));
    }
}

/// Decode the file and write samples to the routed sinks. Returns after EOF
/// or stop. Errors are strings for easy UI display.
fn play_once(
    req: &PlayRequest,
    events: &Sender<EngineEvent>,
    master: &Arc<Mutex<f32>>,
    stop_rx: &Receiver<JobCmd>,
    speaker_sink: Option<&str>,
) -> Result<(), String> {
    let path = Path::new(&req.path);
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Streams are created lazily once we know the real sample spec.
    let mut writers: Vec<psimple::Simple> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut base_rate: u32 = 48_000;
    let mut channels: usize = 2;
    let mut frames_written: u64 = 0;
    let mut last_progress = Instant::now();
    let speed = req.speed.clamp(0.25, 4.0);

    loop {
        if matches!(stop_rx.try_recv(), Ok(JobCmd::Stop)) {
            for w in &writers {
                let _ = w.flush();
            }
            return Ok(());
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                for w in &writers {
                    let _ = w.flush();
                }
                return Err(format!("read: {e}"));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => {
                for w in &writers {
                    let _ = w.flush();
                }
                return Err(format!("decode: {e}"));
            }
        };

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            base_rate = spec.rate;
            channels = spec.channels.count();
            let out_rate = (base_rate as f32 * speed).round() as u32;
            let pspec = pa::sample::Spec {
                format: pa::sample::Format::F32le,
                rate: out_rate,
                channels: channels as u8,
            };
            if !pspec.is_valid() {
                return Err("invalid sample spec".into());
            }
            let open = |dev: Option<&str>| -> Result<psimple::Simple, String> {
                psimple::Simple::new(
                    None,
                    "astra",
                    pa::stream::Direction::Playback,
                    dev,
                    "ASTRA playback",
                    &pspec,
                    None,
                    None,
                )
                .map_err(|e| match dev {
                    Some(d) => format!("pulse ({d}): {e}"),
                    None => format!("pulse (default): {e}"),
                })
            };
            match req.route {
                Route::Speakers => writers.push(open(speaker_sink)?),
                Route::Mic => {
                    if !mic::sink_exists(mic::MIC_SINK_NAME) {
                        mic::ensure_sink()?;
                    }
                    writers.push(open(Some(mic::MIC_SINK_NAME))?);
                }
                Route::Both => {
                    if !mic::sink_exists(mic::MIC_SINK_NAME) {
                        mic::ensure_sink()?;
                    }
                    // Try mic first; if it fails, at least play on speakers.
                    match open(Some(mic::MIC_SINK_NAME)) {
                        Ok(w) => writers.push(w),
                        Err(e) => {
                            let _ = events.send(EngineEvent::Status(format!("virtual mic: {e}")));
                        }
                    }
                    match open(speaker_sink) {
                        Ok(w) => writers.push(w),
                        Err(e) => {
                            let _ = events.send(EngineEvent::Status(format!("speakers: {e}")));
                        }
                    }
                    if writers.is_empty() {
                        return Err("no output could be opened".into());
                    }
                }
            }
            sample_buf = Some(SampleBuffer::<f32>::new(
                decoded.capacity() as u64,
                *decoded.spec(),
            ));
        }

        let sb = sample_buf.as_mut().unwrap();
        sb.copy_interleaved_ref(decoded);
        let vol = req.volume * req.gain * (*master.lock().unwrap());
        let scaled: Vec<f32> = if (vol - 1.0).abs() < 1e-4 {
            sb.samples().to_vec()
        } else {
            sb.samples().iter().map(|s| s * vol).collect()
        };
        let bytes: &[u8] = bytemuck::cast_slice(&scaled);
        if bytes.is_empty() {
            continue;
        }
        for w in &writers {
            w.write(bytes).map_err(|e| format!("pulse write: {e}"))?;
        }
        frames_written += (scaled.len() / channels.max(1)) as u64;

        if last_progress.elapsed() > Duration::from_millis(200) {
            let secs = frames_written as f32 / base_rate as f32;
            let _ = events.send(EngineEvent::Progress(req.id, secs));
            last_progress = Instant::now();
        }
    }

    for w in &writers {
        let _ = w.drain();
    }
    let _ = events.send(EngineEvent::Progress(
        req.id,
        frames_written as f32 / base_rate as f32,
    ));
    Ok(())
}
