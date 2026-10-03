//! niri compositor integration: generate keybind snippets that call the CLI,
//! and safely insert them into ~/.config/niri/config.kdl.

use crate::config::hotkey;
use std::path::Path;

pub const BEGIN: &str = "// === ASTRA soundboard: begin (managed by the app) ===";
pub const END: &str = "// === ASTRA soundboard: end ===";

/// Build the `binds { ... }` block for sounds that have hotkeys assigned.
/// `bindings` is a list of (hotkey token, id-as-string, name).
pub fn generate(bindings: &[(String, String, String)], stop_all: Option<&str>) -> String {
    let mut out = String::new();
    let mut any = false;
    for (token, id, _name) in bindings {
        if token.is_empty() {
            continue;
        }
        any = true;
        out.push_str(&format!(
            "    {} {{ spawn \"astra\" \"play\" \"{}\"; }}\n",
            hotkey::to_niri(token),
            id
        ));
    }
    if let Some(token) = stop_all {
        if !token.is_empty() {
            any = true;
            out.push_str(&format!(
                "    {} {{ spawn \"astra\" \"stop-all\"; }}\n",
                hotkey::to_niri(token)
            ));
        }
    }
    if !any {
        return String::new();
    }
    format!("{BEGIN}\nbinds {{\n{out}}}\n{END}\n")
}

/// Insert (or replace) the ASTRA block in the niri config file content.
/// Returns the new content. Pure function so it is testable.
pub fn insert_into_content(config: &str, block: &str) -> String {
    if let Some(bidx) = config.find(BEGIN) {
        if let Some(eidx_rel) = config[bidx..].find(END) {
            let eidx = bidx + eidx_rel + END.len();
            let mut out = String::with_capacity(config.len() + block.len() + 2);
            out.push_str(&config[..bidx]);
            out.push_str(block.trim_end());
            out.push('\n');
            out.push_str(&config[eidx..].trim_start_matches('\n').to_string());
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }

    // No block yet: try to insert inside the first top-level `binds {` node.
    let block_body = block
        .lines()
        .filter(|l| !l.starts_with("// ===") && !l.starts_with("binds {") && *l != "}")
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(insert_at) = find_binds_insert_pos(config) {
        let mut out = String::with_capacity(config.len() + block_body.len() + 8);
        out.push_str(&config[..insert_at]);
        out.push_str(&block_body);
        out.push('\n');
        out.push_str(&config[insert_at..]);
        return out;
    }

    // No existing binds node — append a fresh one.
    let mut out = config.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block.trim_end());
    out.push('\n');
    out
}

/// Find the byte offset just after the opening brace line of the first
/// top-level `binds {` node, skipping comments and strings. Returns None if
/// the opening line has more content after `{` (caller should handle by
/// appending a separate block… niri forbids a second `binds` node, but that
/// case is rare; we then still append and let validation decide).
fn find_binds_insert_pos(config: &str) -> Option<usize> {
    let bytes = config.as_bytes();
    let mut i = 0usize;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'b' if depth == 0 => {
                // word-boundary check: previous char must not be an identifier char
                let prev_ok = i == 0
                    || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                // check word "binds" followed by optional spaces and '{'
                if prev_ok && config[i..].starts_with("binds") {
                    let after = &config[i + 5..];
                    let trimmed = after.trim_start();
                    if trimmed.starts_with('{') {
                        // offset of the char right after the '{'
                        let brace_off = i + 5 + (after.len() - trimmed.len()) + 1;
                        let rest = &config[brace_off..];
                        let line_end = rest.find('\n').map(|p| brace_off + p).unwrap_or(config.len());
                        if rest[..line_end - brace_off].trim().is_empty() {
                            // insert right after the opening line
                            return Some(line_end + 1);
                        } else {
                            // first child on the same line — insert before matching close brace
                            let close = matching_brace(config, brace_off - 1)?;
                            return Some(close);
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn matching_brace(config: &str, open: usize) -> Option<usize> {
    let bytes = config.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Insert into the actual file, then validate with `niri validate`.
/// Restores the original content if validation fails.
pub fn insert_into_file(path: &Path, block: &str) -> Result<String, String> {
    let original = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let new_content = insert_into_content(&original, block);
    if new_content == original {
        return Ok("nothing to change".into());
    }
    std::fs::write(path, &new_content).map_err(|e| format!("write: {e}"))?;

    let ok = std::process::Command::new("niri")
        .args(["validate", "--config"])
        .arg(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if ok {
        Ok(String::new())
    } else {
        let _ = std::fs::write(path, &original);
        let msg = std::process::Command::new("niri")
            .args(["validate", "--config"])
            .arg(path)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        Err(msg)
    }
}

pub fn niri_config_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("niri").join("config.kdl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_basic() {
        let b = vec![
            ("Super+F1".to_string(), "1".to_string(), "horn".to_string()),
            (String::new(), "2".to_string(), "no hotkey".to_string()),
        ];
        let s = generate(&b, Some("Ctrl+Shift+Space"));
        assert!(s.contains("Mod+F1 { spawn \"astra\" \"play\" \"1\"; }"));
        assert!(!s.contains("\"2\""));
        assert!(s.contains("Ctrl+Shift+Space { spawn \"astra\" \"stop-all\"; }"));
        assert!(s.starts_with(BEGIN));
    }

    #[test]
    fn generate_empty() {
        assert_eq!(generate(&[], None), "");
    }

    #[test]
    fn insert_into_config_with_binds() {
        let cfg = "spawn-at-startup \"bar\"\n\nbinds {\n    Mod+T { spawn \"foot\"; }\n    Mod+Q { exit; }\n}\n";
        let block = generate(&[("Super+F1".into(), "7".into(), "x".into())], None);
        let out = insert_into_content(cfg, &block);
        assert!(out.contains("    Mod+F1 { spawn \"astra\" \"play\" \"7\"; }"));
        assert!(out.contains("Mod+T { spawn \"foot\"; }"));
        assert!(out.starts_with("spawn-at-startup \"bar\""));
        // must still be a single binds node
        assert_eq!(out.matches("binds {").count(), 1);
        // braces balanced
        assert_eq!(out.matches('{').count(), out.matches('}').count());
    }

    #[test]
    fn insert_replaces_existing_block() {
        let cfg = "binds {\n    Mod+T { spawn \"foot\"; }\n}\n\n// === ASTRA soundboard: begin (managed by the app) ===\nbinds {\n    Mod+F9 { spawn \"old\"; }\n}\n// === ASTRA soundboard: end ===\n";
        let block = generate(&[("Super+F2".into(), "8".into(), "x".into())], None);
        let out = insert_into_content(cfg, &block);
        assert!(out.contains("Mod+F2"));
        assert!(!out.contains("\"old\""));
        assert_eq!(out.matches("binds {").count(), 2); // real one + our managed one
    }

    #[test]
    fn insert_appends_when_no_binds() {
        let cfg = "spawn-at-startup \"bar\"\n";
        let block = generate(&[("Super+F1".into(), "7".into(), "x".into())], None);
        let out = insert_into_content(cfg, &block);
        assert!(out.contains("binds {"));
        assert_eq!(out.matches("binds {").count(), 1);
    }
}
