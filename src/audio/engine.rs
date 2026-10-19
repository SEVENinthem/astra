//! Low-latency playback engine.
//!
//! Every output (speakers, virtual mic) has one long-lived PulseAudio/PipeWire
//! stream and a mixer thread that writes 5 ms blocks into a ~10 ms server
//! buffer. Pressing a sound only hands a voice to the mixers, so playback
//! starts in the next block: no thread spawn, no `pactl`, no new connection
//! and no deep default buffers on the hot path.
//!
//! Decoded audio is kept in an LRU cache (short clips are preloaded), so a
//! repeated or preloaded sound starts without touching the disk. Uncached
//! files are decoded on a background thread while the voice already plays
//! whatever has been decoded so far.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use libpulse_binding as pa;
use libpulse_simple_binding as psimple;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use rubato::{FftFixedInOut, Resampler};

use crate::audio::mic;
use crate::config::Route;

/// Mixer output rate; sounds are resampled on the fly.
pub const OUT_RATE: u32 = 48_000;
/// Frames per mixer block (5 ms).
const BLOCK: usize = 240;
/// Server-side buffer target (10 ms): the whole output latency budget.
const TARGET_FRAMES: usize = BLOCK * 2;
/// Fade-out length on stop, avoids clicks (5 ms).
const FADE: usize = 240;
/// Decoded-audio cache budget in samples (f32): 64 Mi samples = 256 MB.
const CACHE_BUDGET: usize = 64 * 1024 * 1024;
/// Sounds up to this length are decoded ahead of time.
pub const PRELOAD_MAX_SECS: f32 = 30.0;

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
    SetSpeakerSink(Option<String>),
    /// Keep playing, but silence ASTRA's own speaker/headphone output.
    SetSpeakersMuted(bool),
    /// Local monitoring (mixer -> speakers loopback) is on: "Both" then
    /// skips the direct speaker copy, or you would hear every sound twice.
    SetLocalMonitor(bool),
    /// Decode a file into the cache ahead of time.
    Preload(String),
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

pub struct Engine {
    pub tx: Sender<EngineMsg>,
    pub rx: Receiver<EngineEvent>,
}

impl Engine {
    pub fn start(master_volume: f32, speaker_sink: Option<String>) -> Engine {
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let shared = Arc::new(Shared {
            master: AtomicU32::new(master_volume.to_bits()),
            active: Mutex::new(HashSet::new()),
            events: Mutex::new(etx),
            speakers_muted: AtomicBool::new(false),
        });
        let cache = Arc::new(Mutex::new(Cache::default()));

        let speakers = Output::spawn(OutputKind::Speakers(speaker_sink), shared.clone());
        let mic_out = Output::spawn(OutputKind::Mic, shared.clone());
        let preload = spawn_preloader(cache.clone());

        thread::Builder::new()
            .name("astra-engine".into())
            .spawn(move || control(rx, shared, cache, speakers, mic_out, preload))
            .expect("spawn engine thread");

        Engine { tx, rx: erx }
    }

    pub fn set_speaker_sink(&self, sink: Option<String>) {
        let _ = self.tx.send(EngineMsg::SetSpeakerSink(sink));
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

    pub fn preload(&self, path: &str) {
        let _ = self.tx.send(EngineMsg::Preload(path.to_string()));
    }
}

/// State shared by the control thread and the mixers.
struct Shared {
    master: AtomicU32,
    /// Ids with a reporting voice alive somewhere.
    active: Mutex<HashSet<u64>>,
    events: Mutex<Sender<EngineEvent>>,
    speakers_muted: AtomicBool,
}

impl Shared {
    fn emit(&self, ev: EngineEvent) {
        let _ = self.events.lock().unwrap().send(ev);
    }

    fn master(&self) -> f32 {
        f32::from_bits(self.master.load(Ordering::Relaxed))
    }

    fn finish(&self, id: u64, ev: EngineEvent) {
        self.active.lock().unwrap().remove(&id);
        self.emit(ev);
    }
}

fn control(
    rx: Receiver<EngineMsg>,
    shared: Arc<Shared>,
    cache: Arc<Mutex<Cache>>,
    speakers: Sender<MixCmd>,
    mic_out: Sender<MixCmd>,
    preload: Sender<String>,
) {
    let mut local_monitor = false;
    for msg in rx {
        match msg {
            EngineMsg::Play(req) => {
                let clip = Cache::get_or_decode(&cache, &req.path);
                for out in [&speakers, &mic_out] {
                    if req.stop_others {
                        let _ = out.send(MixCmd::StopExcept(req.id));
                    }
                    // re-pressing a playing sound restarts it
                    let _ = out.send(MixCmd::Stop(req.id));
                }
                // the first target reports progress; for Both that is the
                // mic, since it is what other people hear
                let targets: &[&Sender<MixCmd>] = match req.route {
                    Route::Speakers => &[&speakers],
                    Route::Mic => &[&mic_out],
                    Route::Both if local_monitor => &[&mic_out],
                    Route::Both => &[&mic_out, &speakers],
                };
                shared.active.lock().unwrap().insert(req.id);
                shared.emit(EngineEvent::Started(req.id));
                for (i, out) in targets.iter().enumerate() {
                    let _ = out.send(MixCmd::Add(Voice::new(&req, clip.clone(), i == 0)));
                }
            }
            EngineMsg::Stop(id) => {
                let _ = speakers.send(MixCmd::Stop(id));
                let _ = mic_out.send(MixCmd::Stop(id));
            }
            EngineMsg::StopAll => {
                let _ = speakers.send(MixCmd::StopAll);
                let _ = mic_out.send(MixCmd::StopAll);
            }
            EngineMsg::SetMasterVolume(v) => {
                shared.master.store(v.to_bits(), Ordering::Relaxed);
            }
            EngineMsg::SetSpeakerSink(sink) => {
                let _ = speakers.send(MixCmd::SetDevice(sink));
            }
            EngineMsg::SetSpeakersMuted(m) => {
                shared.speakers_muted.store(m, Ordering::Relaxed);
            }
            EngineMsg::SetLocalMonitor(on) => local_monitor = on,
            EngineMsg::Preload(path) => {
                let _ = preload.send(path);
            }
            EngineMsg::IsPlaying(id, reply) => {
                let _ = reply.send(shared.active.lock().unwrap().contains(&id));
            }
        }
    }
}

// ------------------------------------------------------------------ clips

/// Decoded audio, interleaved stereo, resampled to OUT_RATE at decode time. Filled
/// progressively by a decoder thread; mixers read what is there so far.
struct Clip {
    rate: AtomicU32,
    data: RwLock<Vec<f32>>,
    done: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Clip {
    fn empty() -> Clip {
        Clip {
            rate: AtomicU32::new(0),
            data: RwLock::new(Vec::new()),
            done: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn samples(&self) -> usize {
        self.data.read().unwrap().len()
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, CacheEntry>,
}

struct CacheEntry {
    clip: Arc<Clip>,
    mtime: Option<SystemTime>,
    used: Instant,
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Cache {
    /// Cached clip, or a fresh one being decoded in the background.
    fn get_or_decode(cache: &Arc<Mutex<Cache>>, path: &str) -> Arc<Clip> {
        let (clip, fresh) = Cache::lookup(cache, path);
        if fresh {
            let c = clip.clone();
            let p = path.to_string();
            let cache = cache.clone();
            thread::spawn(move || {
                decode_into(&p, &c);
                cache.lock().unwrap().evict();
            });
        }
        clip
    }

    /// Returns (clip, needs_decoding).
    fn lookup(cache: &Arc<Mutex<Cache>>, path: &str) -> (Arc<Clip>, bool) {
        let mt = mtime(path);
        let mut c = cache.lock().unwrap();
        if let Some(e) = c.entries.get_mut(path) {
            let failed = e.clip.error.lock().unwrap().is_some();
            if e.mtime == mt && !failed {
                e.used = Instant::now();
                return (e.clip.clone(), false);
            }
        }
        let clip = Arc::new(Clip::empty());
        c.entries.insert(
            path.to_string(),
            CacheEntry {
                clip: clip.clone(),
                mtime: mt,
                used: Instant::now(),
            },
        );
        (clip, true)
    }

    /// Drop least recently used clips that nobody plays until under budget.
    fn evict(&mut self) {
        let mut total: usize = self.entries.values().map(|e| e.clip.samples()).sum();
        while total > CACHE_BUDGET {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| Arc::strong_count(&e.clip) == 1)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            let Some(k) = victim else { break };
            if let Some(e) = self.entries.remove(&k) {
                total -= e.clip.samples();
            }
        }
    }
}

fn spawn_preloader(cache: Arc<Mutex<Cache>>) -> Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    thread::Builder::new()
        .name("astra-preload".into())
        .spawn(move || {
            for path in rx {
                let (clip, fresh) = Cache::lookup(&cache, &path);
                if fresh {
                    decode_into(&path, &clip);
                    cache.lock().unwrap().evict();
                }
            }
        })
        .expect("spawn preload thread");
    tx
}

/// Decode a whole file into `clip` (stereo f32), publishing as it goes.
fn decode_into(path: &str, clip: &Clip) {
    if let Err(e) = decode_inner(path, clip) {
        *clip.error.lock().unwrap() = Some(e);
    }
    clip.done.store(true, Ordering::Release);
}

fn decode_inner(path: &str, clip: &Clip) -> Result<(), String> {
    let p = Path::new(path);
    let file = std::fs::File::open(p).map_err(|e| format!("open: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
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
    // Reserve up front: growing the Vec later would hold the write lock
    // during a big copy while mixers wait to read.
    if let Some(n) = track.codec_params.n_frames {
        let rate = track.codec_params.sample_rate.unwrap_or(OUT_RATE).max(1);
        let out_frames = n * OUT_RATE as u64 / rate as u64;
        clip.data.write().unwrap().reserve(out_frames as usize * 2 + 8192);
    }
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut sample_buf: Option<(SampleBuffer<f32>, usize)> = None;
    let mut stereo: Vec<f32> = Vec::new();
    let mut rs: Option<ToOutRate> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(format!("read: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let frames_cap = decoded.capacity();
        if sample_buf.as_ref().map_or(true, |(_, cap)| *cap < frames_cap) {
            sample_buf = Some((SampleBuffer::<f32>::new(frames_cap as u64, spec), frames_cap));
        }
        let sb = &mut sample_buf.as_mut().unwrap().0;
        sb.copy_interleaved_ref(decoded);

        stereo.clear();
        for frame in sb.samples().chunks_exact(channels) {
            let (l, r) = if channels == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
            stereo.push(l);
            stereo.push(r);
        }
        if rs.is_none() {
            rs = Some(ToOutRate::new(spec.rate)?);
        }
        rs.as_mut().unwrap().push(&stereo, clip)?;
    }
    match rs {
        Some(mut r) => r.finish(clip),
        None => Err("no samples".into()),
    }
}

/// Streaming high-quality (FFT/sinc) conversion of interleaved stereo to
/// OUT_RATE. Doing it once at decode time keeps the mixer a plain copy at
/// normal speed, so the sound comes out bit-exact.
struct ToOutRate {
    rs: Option<FftFixedInOut<f32>>,
    pending: [Vec<f32>; 2],
    in_total: u64,
    out_total: u64,
    skip: usize,
    in_rate: u32,
}

impl ToOutRate {
    fn new(in_rate: u32) -> Result<ToOutRate, String> {
        let rs = if in_rate == OUT_RATE {
            None
        } else {
            Some(
                FftFixedInOut::<f32>::new(in_rate as usize, OUT_RATE as usize, 1024, 2)
                    .map_err(|e| format!("resampler: {e}"))?,
            )
        };
        let skip = rs.as_ref().map_or(0, |r| r.output_delay());
        Ok(ToOutRate {
            rs,
            pending: [Vec::new(), Vec::new()],
            in_total: 0,
            out_total: 0,
            skip,
            in_rate,
        })
    }

    fn push(&mut self, stereo: &[f32], clip: &Clip) -> Result<(), String> {
        self.in_total += stereo.len() as u64 / 2;
        if self.rs.is_none() {
            self.out_total += stereo.len() as u64 / 2;
            return Ok(publish(clip, stereo));
        }
        for f in stereo.chunks_exact(2) {
            self.pending[0].push(f[0]);
            self.pending[1].push(f[1]);
        }
        loop {
            let rs = self.rs.as_mut().unwrap();
            let need = rs.input_frames_next();
            if self.pending[0].len() < need {
                return Ok(());
            }
            let chunk = [&self.pending[0][..need], &self.pending[1][..need]];
            let out = rs.process(&chunk, None).map_err(|e| format!("resample: {e}"))?;
            self.pending[0].drain(..need);
            self.pending[1].drain(..need);
            self.emit(&out, clip);
        }
    }

    fn finish(&mut self, clip: &Clip) -> Result<(), String> {
        let expected = self.in_total * OUT_RATE as u64 / self.in_rate as u64;
        if let Some(rs) = self.rs.as_mut() {
            let rest = [&self.pending[0][..], &self.pending[1][..]];
            let mut outs = vec![rs.process_partial(Some(&rest), None).map_err(|e| e.to_string())?];
            // flush the filter tail until every expected frame is out
            let mut guard = 0;
            while self.out_total + outs.iter().map(|o| o[0].len() as u64).sum::<u64>()
                < expected + self.skip as u64
                && guard < 64
            {
                outs.push(rs.process_partial::<&[f32]>(None, None).map_err(|e| e.to_string())?);
                guard += 1;
            }
            for out in outs {
                let left = expected.saturating_sub(self.out_total) as usize + self.skip;
                let trimmed: Vec<Vec<f32>> =
                    out.iter().map(|c| c[..c.len().min(left)].to_vec()).collect();
                self.emit(&trimmed, clip);
            }
        }
        if clip.rate.load(Ordering::Relaxed) == 0 {
            return Err("no samples".into());
        }
        Ok(())
    }

    /// Interleave planar output, drop the filter delay, append.
    fn emit(&mut self, out: &[Vec<f32>], clip: &Clip) {
        let n = out[0].len();
        let start = self.skip.min(n);
        self.skip -= start;
        let mut buf = Vec::with_capacity((n - start) * 2);
        for i in start..n {
            buf.push(out[0][i]);
            buf.push(out[1][i]);
        }
        self.out_total += (n - start) as u64;
        publish(clip, &buf);
    }
}

fn publish(clip: &Clip, interleaved: &[f32]) {
    if interleaved.is_empty() {
        return;
    }
    clip.data.write().unwrap().extend_from_slice(interleaved);
    if clip.rate.load(Ordering::Relaxed) == 0 {
        clip.rate.store(OUT_RATE, Ordering::Release);
    }
}

// ------------------------------------------------------------------ voices

struct Voice {
    id: u64,
    clip: Arc<Clip>,
    /// Position in source frames (fractional, for resampling/speed).
    pos: f64,
    speed: f64,
    gain: f32,
    loop_: bool,
    /// Only one copy of a voice (per press) reports to the UI.
    report: bool,
    /// Remaining fade-out frames once stopping.
    fade: Option<usize>,
    last_progress: Instant,
}

impl Voice {
    fn new(req: &PlayRequest, clip: Arc<Clip>, report: bool) -> Voice {
        Voice {
            id: req.id,
            clip,
            pos: 0.0,
            speed: req.speed.clamp(0.25, 4.0) as f64,
            gain: req.volume * req.gain,
            loop_: req.loop_,
            report,
            fade: None,
            last_progress: Instant::now(),
        }
    }

    fn stop(&mut self) {
        if self.fade.is_none() {
            self.fade = Some(FADE);
        }
    }
}

enum VoiceState {
    Playing,
    Finished,
    Failed(String),
}

/// Mix one voice into `out` (stereo interleaved).
fn render(v: &mut Voice, out: &mut [f32], master: f32) -> VoiceState {
    let rate = v.clip.rate.load(Ordering::Acquire);
    if rate == 0 {
        if v.clip.done.load(Ordering::Acquire) {
            let err = v.clip.error.lock().unwrap().clone();
            return VoiceState::Failed(err.unwrap_or_else(|| "no samples".into()));
        }
        // still opening the file: stay silent this block
        return if v.fade.is_some() { VoiceState::Finished } else { VoiceState::Playing };
    }
    let done = v.clip.done.load(Ordering::Acquire);
    // never stall the mixer behind the decoder's write lock
    let Ok(data) = v.clip.data.try_read() else {
        return VoiceState::Playing;
    };
    let frames = data.len() / 2;
    let step = rate as f64 / OUT_RATE as f64 * v.speed;
    let gain = v.gain * master;

    for o in out.chunks_exact_mut(2) {
        let mut i = v.pos as usize;
        if i + 2 >= frames {
            if !done {
                break; // decoder is behind (only right after start)
            }
            if i >= frames {
                if v.loop_ && frames > 0 && v.fade.is_none() {
                    v.pos -= frames as f64;
                    i = v.pos as usize;
                } else {
                    return VoiceState::Finished;
                }
            }
        }
        let mut g = gain;
        if let Some(f) = v.fade.as_mut() {
            if *f == 0 {
                return VoiceState::Finished;
            }
            g *= *f as f32 / FADE as f32;
            *f -= 1;
        }
        let t = (v.pos - i as f64) as f32;
        let at = |k: isize, ch: usize| -> f32 {
            // hold the first sample before the start, silence past the end
            let idx = (i as isize + k).max(0) as usize;
            if idx >= frames {
                0.0
            } else {
                data[idx * 2 + ch]
            }
        };
        for ch in 0..2 {
            let s = if t == 0.0 {
                at(0, ch)
            } else {
                hermite(at(-1, ch), at(0, ch), at(1, ch), at(2, ch), t)
            };
            o[ch] += s * g;
        }
        v.pos += step;
    }
    if v.fade == Some(0) {
        return VoiceState::Finished;
    }
    VoiceState::Playing
}

/// Transparent below 0.9, then bends smoothly into ±1.0 instead of
/// hard-clipping loud mixes (master volume > 100 %, several sounds).
fn soft_clip(x: f32) -> f32 {
    const K: f32 = 0.9;
    let a = x.abs();
    if a <= K {
        x
    } else {
        x.signum() * (K + (1.0 - K) * ((a - K) / (1.0 - K)).tanh())
    }
}

/// 4-point cubic Hermite interpolation (only used for speed != 1).
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

// ------------------------------------------------------------------ outputs

enum MixCmd {
    Add(Voice),
    Stop(u64),
    StopExcept(u64),
    StopAll,
    SetDevice(Option<String>),
}

enum OutputKind {
    Speakers(Option<String>),
    Mic,
}

struct Output {
    kind: OutputKind,
    stream: Option<psimple::Simple>,
    voices: Vec<Voice>,
    shared: Arc<Shared>,
}

impl Output {
    fn spawn(kind: OutputKind, shared: Arc<Shared>) -> Sender<MixCmd> {
        let (tx, rx) = mpsc::channel();
        let name = match kind {
            OutputKind::Speakers(_) => "astra-mix-spk",
            OutputKind::Mic => "astra-mix-mic",
        };
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut out = Output {
                    kind,
                    stream: None,
                    voices: Vec::new(),
                    shared,
                };
                // connect up front so the first press is instant
                let _ = out.open();
                out.run(rx);
            })
            .expect("spawn mixer thread");
        tx
    }

    fn device(&self) -> Option<String> {
        match &self.kind {
            OutputKind::Speakers(s) => s.clone(),
            OutputKind::Mic => Some(mic::MIC_SINK_NAME.to_string()),
        }
    }

    fn open(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        if matches!(self.kind, OutputKind::Mic) && !mic::sink_exists(mic::MIC_SINK_NAME) {
            mic::ensure_sink()?;
        }
        let spec = pa::sample::Spec {
            format: pa::sample::Format::F32le,
            rate: OUT_RATE,
            channels: 2,
        };
        let frame = spec.frame_size() as u32;
        let attr = pa::def::BufferAttr {
            maxlength: u32::MAX,
            tlength: TARGET_FRAMES as u32 * frame,
            // start the moment the first block arrives (also after idle)
            prebuf: BLOCK as u32 * frame,
            minreq: BLOCK as u32 * frame,
            fragsize: u32::MAX,
        };
        let dev = self.device();
        let stream = psimple::Simple::new(
            None,
            "ASTRA",
            pa::stream::Direction::Playback,
            dev.as_deref(),
            match self.kind {
                OutputKind::Speakers(_) => "ASTRA speakers",
                OutputKind::Mic => "ASTRA virtual mic",
            },
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| match &dev {
            Some(d) => format!("pulse ({d}): {e}"),
            None => format!("pulse (default): {e}"),
        })?;
        self.stream = Some(stream);
        Ok(())
    }

    fn handle(&mut self, cmd: MixCmd) {
        match cmd {
            MixCmd::Add(v) => {
                if let Err(e) = self.open() {
                    if v.report {
                        self.shared.finish(v.id, EngineEvent::Error(v.id, e));
                    } else {
                        self.shared.emit(EngineEvent::Status(e));
                    }
                    return;
                }
                self.voices.push(v);
            }
            MixCmd::Stop(id) => self.voices.iter_mut().filter(|v| v.id == id).for_each(Voice::stop),
            MixCmd::StopExcept(id) => {
                self.voices.iter_mut().filter(|v| v.id != id).for_each(Voice::stop)
            }
            MixCmd::StopAll => self.voices.iter_mut().for_each(Voice::stop),
            MixCmd::SetDevice(dev) => {
                if let OutputKind::Speakers(cur) = &mut self.kind {
                    if *cur != dev {
                        *cur = dev;
                        self.stream = None;
                        let _ = self.open();
                    }
                }
            }
        }
    }

    fn run(&mut self, rx: Receiver<MixCmd>) {
        let mut block = vec![0f32; BLOCK * 2];
        let is_speakers = matches!(self.kind, OutputKind::Speakers(_));
        // output gain, ramped per block so muting never clicks
        let mut out_gain = 1.0f32;
        loop {
            // Idle: sleep until something arrives. The stream underruns and
            // restarts instantly thanks to the one-block prebuf.
            if self.voices.is_empty() {
                match rx.recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(_) => return,
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(cmd) => self.handle(cmd),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            if self.voices.is_empty() {
                continue;
            }

            block.iter_mut().for_each(|s| *s = 0.0);
            let master = self.shared.master();
            let shared = &self.shared;
            self.voices.retain_mut(|v| {
                let state = render(v, &mut block, master);
                let alive = matches!(state, VoiceState::Playing);
                if v.report {
                    match state {
                        VoiceState::Playing => {
                            if v.last_progress.elapsed() > Duration::from_millis(100) {
                                let rate = v.clip.rate.load(Ordering::Relaxed).max(1);
                                shared.emit(EngineEvent::Progress(v.id, v.pos as f32 / rate as f32));
                                v.last_progress = Instant::now();
                            }
                        }
                        VoiceState::Finished => shared.finish(v.id, EngineEvent::Finished(v.id)),
                        VoiceState::Failed(e) => shared.finish(v.id, EngineEvent::Error(v.id, e)),
                    }
                }
                alive
            });
            let target = if is_speakers && self.shared.speakers_muted.load(Ordering::Relaxed) {
                0.0
            } else {
                1.0
            };
            let from = out_gain;
            for (n, f) in block.chunks_exact_mut(2).enumerate() {
                let g = from + (target - from) * (n + 1) as f32 / BLOCK as f32;
                f[0] = soft_clip(f[0] * g);
                f[1] = soft_clip(f[1] * g);
            }
            out_gain = target;

            let res = match self.stream.as_ref() {
                Some(stream) => stream.write(bytemuck::cast_slice(&block)),
                None => Ok(()),
            };
            if let Err(e) = res {
                // the device went away: reconnect, dropping what was playing
                self.stream = None;
                self.shared.emit(EngineEvent::Status(format!("pulse write: {e}")));
                for v in self.voices.drain(..).filter(|v| v.report) {
                    self.shared.finish(v.id, EngineEvent::Finished(v.id));
                }
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(cmd) => self.handle(cmd),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let _ = self.open();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip_with(rate: u32, frames: &[f32]) -> Arc<Clip> {
        let c = Clip::empty();
        c.rate.store(rate, Ordering::Relaxed);
        *c.data.write().unwrap() = frames.iter().flat_map(|&s| [s, s]).collect();
        c.done.store(true, Ordering::Relaxed);
        Arc::new(c)
    }

    fn voice(clip: Arc<Clip>) -> Voice {
        let req = PlayRequest {
            id: 1,
            path: String::new(),
            volume: 1.0,
            gain: 1.0,
            route: Route::Mic,
            loop_: false,
            stop_others: false,
            speed: 1.0,
        };
        Voice::new(&req, clip, true)
    }

    #[test]
    fn plays_first_sample_in_first_block() {
        let mut v = voice(clip_with(OUT_RATE, &vec![0.5; 1000]));
        let mut out = vec![0f32; BLOCK * 2];
        assert!(matches!(render(&mut v, &mut out, 1.0), VoiceState::Playing));
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], 0.5);
    }

    #[test]
    fn finishes_at_end() {
        let mut v = voice(clip_with(OUT_RATE, &vec![0.5; 100]));
        let mut out = vec![0f32; BLOCK * 2];
        assert!(matches!(render(&mut v, &mut out, 1.0), VoiceState::Finished));
    }

    #[test]
    fn loops_back_to_start() {
        let mut v = voice(clip_with(OUT_RATE, &vec![0.5; 100]));
        v.loop_ = true;
        let mut out = vec![0f32; BLOCK * 2];
        assert!(matches!(render(&mut v, &mut out, 1.0), VoiceState::Playing));
        assert!(v.pos < 100.0);
    }

    #[test]
    fn stop_fades_out() {
        let mut v = voice(clip_with(OUT_RATE, &vec![1.0; 10_000]));
        v.stop();
        let mut out = vec![0f32; BLOCK * 2];
        let st = render(&mut v, &mut out, 1.0);
        assert!(matches!(st, VoiceState::Finished));
        assert!(out[0] > 0.9 && out[(FADE - 1) * 2] < 0.01);
    }

    #[test]
    fn resamples_44k() {
        let mut v = voice(clip_with(44_100, &vec![0.25; 44_100]));
        let mut out = vec![0f32; BLOCK * 2];
        render(&mut v, &mut out, 1.0);
        assert!((v.pos - BLOCK as f64 * 44_100.0 / 48_000.0).abs() < 1e-6);
        assert!(out.iter().all(|s| (s - 0.25).abs() < 1e-5));
    }

    #[test]
    fn soft_clip_is_transparent_then_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.9), -0.9);
        assert!(soft_clip(1.5) < 1.0 && soft_clip(1.5) > 0.95);
        assert!(soft_clip(-3.0) >= -1.0);
    }

    #[test]
    fn resampler_keeps_length_and_level() {
        let clip = Clip::empty();
        let mut rs = ToOutRate::new(44_100).unwrap();
        // 1 s of a 1 kHz sine at 44.1 kHz
        let input: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 44_100.0).sin() * 0.5;
                [v, v]
            })
            .collect();
        for chunk in input.chunks(2304) {
            rs.push(chunk, &clip).unwrap();
        }
        rs.finish(&clip).unwrap();
        let data = clip.data.read().unwrap();
        assert_eq!(data.len() / 2, 48_000);
        let peak = data[4800..43200].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn waits_for_decoder_without_skipping() {
        let c = Clip::empty();
        let clip = Arc::new(c);
        let mut v = voice(clip.clone());
        let mut out = vec![0f32; BLOCK * 2];
        assert!(matches!(render(&mut v, &mut out, 1.0), VoiceState::Playing));
        assert_eq!(v.pos, 0.0);
        clip.rate.store(OUT_RATE, Ordering::Relaxed);
        clip.data.write().unwrap().extend_from_slice(&[0.7; 4000]);
        render(&mut v, &mut out, 1.0);
        assert_eq!(out[0], 0.7);
    }
}
