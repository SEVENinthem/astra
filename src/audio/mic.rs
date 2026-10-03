//! Virtual microphone management on top of PipeWire (via pipewire-pulse's
//! pactl interface). Creates a null sink that apps can use as a microphone
//! input, and optional loopbacks:
//!   * passthrough — real mic -> virtual mic (your voice keeps working)
//!   * local monitor — virtual mic -> speakers (hear what Discord hears)

use std::process::Command;

pub const MIC_SINK_NAME: &str = "astra_mic";
pub const MIC_DESCRIPTION: &str = "ASTRA Virtual Microphone";

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

pub fn sink_exists(name: &str) -> bool {
    pactl(&["list", "short", "sinks"])
        .map(|out| out.lines().any(|l| l.split('\t').nth(1) == Some(name)))
        .unwrap_or(false)
}

/// Make sure the virtual mic sink exists (idempotent).
pub fn ensure_sink() -> Result<(), String> {
    if !pactl_available() {
        return Err("pactl-not-found".into());
    }
    if sink_exists(MIC_SINK_NAME) {
        return Ok(());
    }
    pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={MIC_SINK_NAME}"),
        &format!("sink_properties=device.description='{MIC_DESCRIPTION}'"),
    ])
    .map(|_| ())
    .map_err(|e| format!("create virtual mic: {e}"))
}

/// Remove the virtual mic sink and any loopbacks attached to it (idempotent).
pub fn destroy_sink() {
    let _ = unload_loopbacks_to(MIC_SINK_NAME);
    if let Ok(mods) = pactl(&["list", "short", "modules"]) {
        for line in mods.lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 3 && cols[1] == "module-null-sink" && cols[2].contains(MIC_SINK_NAME) {
                let _ = pactl(&["unload-module", cols[0]]);
            }
        }
    }
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

/// Toggle mic passthrough (real microphone -> virtual mic). Returns new state.
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

/// Toggle local monitoring: virtual mic -> speakers (hear what apps hear).
pub fn set_local_monitor(on: bool, sink: Option<&str>) -> Result<bool, String> {
    let dst = sink
        .map(|s| s.to_string())
        .or_else(default_sink)
        .ok_or("no default sink found")?;
    if on {
        if loopback_present(Some(&format!("{MIC_SINK_NAME}.monitor")), &dst) {
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
        // unload monitor loopbacks only (source = our monitor, any sink)
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

/// Called on app exit: remove our PipeWire leftovers unless the user asked
/// to keep the virtual mic around.
pub fn cleanup_on_exit(keep_virtual_mic: bool) {
    if keep_virtual_mic {
        return;
    }
    destroy_sink();
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
