//! Background file scanning: duration + peak amplitude for normalization.

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Event sent back to the UI when a scan completes.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub id: u64,
    pub duration_secs: f32,
    pub peak: f32,
}

/// Full decode pass: returns (duration seconds, peak abs amplitude).
pub fn compute(path: &str) -> Result<(f32, f32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
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

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut frames: u64 = 0;
    let mut rate: u32 = 0;
    let mut channels: usize = 2;
    let mut peak: f32 = 0.0;

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
        if sample_buf.is_none() {
            let spec = *decoded.spec();
            rate = spec.rate;
            channels = spec.channels.count();
            sample_buf = Some(SampleBuffer::<f32>::new(
                decoded.capacity() as u64,
                spec,
            ));
        }
        let sb = sample_buf.as_mut().unwrap();
        sb.copy_interleaved_ref(decoded);
        for &s in sb.samples() {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        frames += sb.len() as u64 / channels.max(1) as u64;
    }

    if rate == 0 {
        return Err("no samples".into());
    }
    Ok((frames as f32 / rate as f32, peak))
}
