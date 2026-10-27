//! Reading settings the Windows apps left in a Wine prefix, so switching is painless.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Values of one registry key from a prefix's `user.reg` (HKEY_CURRENT_USER).
/// `key` uses single backslashes, e.g. `Software\Wowhead\Client\Settings`.
pub fn user_reg(prefix: &Path, key: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(prefix.join("user.reg")) else { return out };
    let header = format!("[{}]", key.replace('\\', "\\\\")).to_lowercase();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with('[') {
            inside = line.to_lowercase().starts_with(&header);
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = line.strip_prefix('"') else { continue };
        let Some((name, value)) = rest.split_once("\"=") else { continue };
        let value = if let Some(v) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            unescape(v)
        } else if let Some(v) = value.strip_prefix("str(7):\"").and_then(|v| v.strip_suffix('"')) {
            unescape(v)
        } else if let Some(v) = value.strip_prefix("dword:") {
            u32::from_str_radix(v, 16).map(|n| n.to_string()).unwrap_or_default()
        } else {
            continue;
        };
        out.insert(name.to_string(), value);
    }
    out
}

fn unescape(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('0') => {}
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Wine/Lutris/Bottles prefixes that have a given file or folder inside `drive_c`.
pub fn prefixes_with(rel: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut roots = vec![home.join(".wine")];
    for dir in [home.join("Games"), home.join(".local/share/bottles/bottles")] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            roots.extend(rd.flatten().map(|e| e.path()));
        }
    }
    if let Ok(rd) = std::fs::read_dir(home.join(".local/share/Steam/steamapps/compatdata")) {
        roots.extend(rd.flatten().map(|e| e.path().join("pfx")));
    }
    roots
        .into_iter()
        .filter(|p| p.join("drive_c").join(rel).exists())
        .collect()
}

/// The Windows user's profile folder in a prefix (`drive_c/users/<name>`).
pub fn user_profiles(prefix: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(prefix.join("drive_c/users")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n != "public" && e.path().join("AppData").is_dir() {
                out.push(e.path());
            }
        }
    }
    out
}
