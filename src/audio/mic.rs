//! Virtual microphone on top of PipeWire (via pipewire-pulse's pactl).
//!
//! Two devices are involved:
//!   * `astra_mic` — an internal null *sink*. ASTRA plays sounds into it and
//!     the microphone passthrough (real mic) is loopbacked into it, so the
//!     sink's monitor carries the full mix.
//!   * `astra_virtual_mic` — a *source* (module-pipe-source) fed by the
//!     bridge thread from that monitor. It is a real input device, so apps
//!     with Chromium-style monitor filtering (Discord) list it as a
//!     microphone, unlike the monitor itself.

use std::path::PathBuf;
use std::process::Command;

pub const MIC_SINK_NAME: &str = "astra_mic";
pub const SINK_DESCRIPTION: &str = "ASTRA Sounds (internal)";
pub const MIC_SOURCE_NAME: &str = "astra_virtual_mic";
/// NBSP (U+00A0) instead of spaces: the pipewire module arg parser splits
/// values at plain spaces, NBSP survives and renders as a space everywhere.
pub const SOURCE_DESCRIPTION: &str = "ASTRA\u{00a0}Virtual\u{00a0}Microphone";

fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|_| "pactl-not-found".to_string())?;
    if !out.status.success() {
        return Err(format!(
            "pactl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn pactl_available() -> bool {
    Command::new("pactl")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn source_fifo() -> PathBuf {
    runtime_dir().join(format!("{MIC_SOURCE_NAME}.pcm"))
}

/// All sinks as (name, description).
pub fn list_sinks() -> Result<Vec<(String, String)>, String> {
    parse_devices(&pactl(&["list", "sinks"])?, "Sink")
}

/// All sources as (name, description).
pub fn list_sources() -> Result<Vec<(String, String)>, String> {
    parse_devices(&pactl(&["list", "sources"])?, "Source")
}

fn parse_devices(out: &str, kind: &str) -> Result<Vec<(String, String)>, String> {
    let mut res = Vec::new();
    let mut name: Option<String> = None;
    for line in out.lines() {
        let t = line.trim();
        if t.starts_with(&format!("{kind} #")) {
            if let Some(n) = name.take() {
                res.push((n.clone(), n));
            }
            name = None;
        } else if let Some(v) = t.strip_prefix("Name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = t.strip_prefix("Description:") {
            if let Some(n) = name.take() {
                res.push((n, v.trim().to_string()));
            }
        }
    }
    if let Some(n) = name.take() {
        res.push((n.clone(), n));
    }
    Ok(res)
}

pub fn default_sink() -> Option<String> {
    pactl(&["get-default-sink"]).ok().map(|s| s.trim().into())
}

pub fn default_source() -> Option<String> {
    pactl(&["get-default-source"]).ok().map(|s| s.trim().into())
}

fn device_exists(kind: &str, name: &str) -> bool {
    pactl(&["list", "short", kind])
        .map(|out| out.lines().any(|l| l.split('\t').nth(1) == Some(name)))
        .unwrap_or(false)
}

pub fn sink_exists(name: &str) -> bool {
    device_exists("sinks", name)
}

pub fn source_exists(name: &str) -> bool {
    device_exists("sources", name)
}

fn sink_description(name: &str) -> Option<String> {
    list_sinks()
        .ok()?
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| d.clone())
}

/// Make sure the internal mixer sink exists (idempotent). Recreates it if an
/// older version left one with a mangled description.
pub fn ensure_sink() -> Result<(), String> {
    if !pactl_available() {
        return Err("pactl-not-found".into());
    }
    if sink_exists(MIC_SINK_NAME) {
        let ok_desc = sink_description(MIC_SINK_NAME).is_some_and(|d| d == SINK_DESCRIPTION);
        if ok_desc {
            return Ok(());
        }
        destroy_sink();
    }
    pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={MIC_SINK_NAME}"),
        &format!("sink_properties={{ device.description=\"{SINK_DESCRIPTION}\" }}"),
    ])
    .map(|_| ())
    .map_err(|e| format!("create mixer sink: {e}"))
}

/// Make sure the virtual microphone *source* exists (idempotent). This is
/// the device apps select as their input.
pub fn ensure_source() -> Result<(), String> {
    if !pactl_available() {
        return Err("pactl-not-found".into());
    }
    if source_exists(MIC_SOURCE_NAME) {
        return Ok(());
    }
    let fifo = source_fifo();
    let _ = std::fs::remove_file(&fifo);
    pactl(&[
        "load-module",
        "module-pipe-source",
        &format!("source_name={MIC_SOURCE_NAME}"),
        &format!("file={}", fifo.display()),
        "format=s16le",
        "rate=48000",
        "channels=2",
        &format!("source_properties=device.description={SOURCE_DESCRIPTION}"),
    ])
    .map(|_| ())
    .map_err(|e| format!("create virtual mic source: {e}"))
}

/// Remove the mixer sink and loopbacks attached to it (idempotent).
pub fn destroy_sink() {
    let _ = unload_loopbacks_to(MIC_SINK_NAME);
    if let Ok(mods) = modules() {
        for (idx, name, args) in mods {
            if name == "module-null-sink" && args.contains(MIC_SINK_NAME) {
                let _ = pactl(&["unload-module", &idx]);
            }
        }
    }
}

/// Remove everything ASTRA created in PipeWire (idempotent).
pub fn destroy_all() {
    if let Ok(mods) = modules() {
        for (idx, name, args) in mods {
            let loopback_into_sink = name == "module-loopback"
                && args.split_whitespace().any(|a| a == &format!("sink={MIC_SINK_NAME}"));
            let local_monitor = name == "module-loopback"
                && args
                    .split_whitespace()
                    .any(|a| a == &format!("source={MIC_SINK_NAME}.monitor"));
            let our_sink = name == "module-null-sink" && args.contains(MIC_SINK_NAME);
            let our_source = name == "module-pipe-source" && args.contains(MIC_SOURCE_NAME);
            if loopback_into_sink || local_monitor || our_sink || our_source {
                let _ = pactl(&["unload-module", &idx]);
            }
        }
    }
    let _ = std::fs::remove_file(source_fifo());
}

fn modules() -> Result<Vec<(String, String, String)>, String> {
    let out = pactl(&["list", "short", "modules"])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            if c.len() >= 3 {
                Some((c[0].into(), c[1].into(), c[2].into()))
            } else {
                None
            }
        })
        .collect())
}

fn unload_loopbacks_to(sink: &str) -> Result<(), String> {
    for (idx, name, args) in modules()? {
        if name == "module-loopback" && args.split_whitespace().any(|a| a == &format!("sink={sink}")) {
            pactl(&["unload-module", &idx])?;
        }
    }
    Ok(())
}

fn loopback_present(source: Option<&str>, sink: &str) -> bool {
    modules().map_or(false, |mods| {
        mods.iter().any(|(_, name, args)| {
            name == "module-loopback"
                && args.split_whitespace().any(|a| a == &format!("sink={sink}"))
                && source.map_or(true, |src| {
                    args.split_whitespace().any(|a| a == &format!("source={src}"))
                })
        })
    })
}

/// Toggle mic passthrough (real microphone -> mixer sink). Returns new state.
pub fn set_passthrough(on: bool, source: Option<&str>) -> Result<bool, String> {
    let src = source
        .map(|s| s.to_string())
        .or_else(default_source)
        .ok_or("no default source found")?;
    if on {
        if loopback_present(Some(&src), MIC_SINK_NAME) {
            return Ok(true);
        }
        let _ = unload_loopbacks_to(MIC_SINK_NAME); // switch source cleanly
        pactl(&[
            "load-module",
            "module-loopback",
            &format!("source={src}"),
            &format!("sink={MIC_SINK_NAME}"),
        ])
        .map_err(|e| format!("passthrough: {e}"))?;
        Ok(true)
    } else {
        unload_loopbacks_to(MIC_SINK_NAME)?;
        Ok(false)
    }
}

pub fn passthrough_active() -> bool {
    loopback_present(None, MIC_SINK_NAME)
}

/// Toggle local monitoring: mixer sink -> speakers (hear what apps hear).
pub fn set_local_monitor(on: bool, sink: Option<&str>) -> Result<bool, String> {
    let dst = sink
        .map(|s| s.to_string())
        .or_else(default_sink)
        .ok_or("no default sink found")?;
    if on {
        if local_monitor_active() {
            return Ok(true);
        }
        pactl(&[
            "load-module",
            "module-loopback",
            &format!("source={MIC_SINK_NAME}.monitor"),
            &format!("sink={dst}"),
        ])
        .map_err(|e| format!("monitor: {e}"))?;
        Ok(true)
    } else {
        for (idx, name, args) in modules()? {
            if name == "module-loopback"
                && args
                    .split_whitespace()
                    .any(|a| a == &format!("source={MIC_SINK_NAME}.monitor"))
            {
                let _ = pactl(&["unload-module", &idx]);
            }
        }
        Ok(false)
    }
}

#[allow(dead_code)]
pub fn local_monitor_active() -> bool {
    modules().map_or(false, |mods| {
        mods.iter().any(|(_, name, args)| {
            name == "module-loopback"
                && args
                    .split_whitespace()
                    .any(|a| a == &format!("source={MIC_SINK_NAME}.monitor"))
        })
    })
}

/// Is anything capturing from the virtual mic (did some app select it as
/// its input)?
pub fn has_capture() -> bool {
    pactl(&["list", "short", "source-outputs"])
        .map(|out| {
            out.lines().any(|l| {
                l.split('\t')
                    .nth(1)
                    .is_some_and(|s| s.starts_with(MIC_SOURCE_NAME))
            })
        })
        .unwrap_or(false)
}

/// Called on app exit: remove our PipeWire leftovers unless the user asked
/// to keep them around.
pub fn cleanup_on_exit(keep: bool) {
    if keep {
        return;
    }
    destroy_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_devices_works() {
        let sample = "Sink #53\n\tState: SUSPENDED\n\tName: alsa_out.foo\n\tDescription: Built-in Audio\n\tDriver: PipeWire\nSink #54\n\tName: astra_mic\n";
        let v = parse_devices(sample, "Sink").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, "alsa_out.foo");
        assert_eq!(v[0].1, "Built-in Audio");
        assert_eq!(v[1].0, "astra_mic");
    }
}
