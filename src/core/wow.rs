//! Finding World of Warcraft on Linux: native paths, Lutris/Wine/Bottles/Proton prefixes,
//! and the game's flavour folders (`_retail_`, `_classic_era_`, ...).

use std::fs;
use std::path::{Path, PathBuf};

/// A flavour folder inside the WoW install, e.g. `_retail_` or `_classic_beta_`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flavor {
    pub folder: String,
    pub path: PathBuf,
}

impl Flavor {
    pub fn display_name(&self) -> String {
        flavor_display(&self.folder)
    }

    pub fn accounts_dir(&self) -> PathBuf {
        self.path.join("WTF").join("Account")
    }

    /// Account folders (`WTF/Account/<name>`) that have a SavedVariables folder.
    pub fn accounts(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(self.accounts_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.eq_ignore_ascii_case("SavedVariables") {
                    continue;
                }
                let sv = e.path().join("SavedVariables");
                if sv.is_dir() {
                    out.push((name, sv));
                }
            }
        }
        out.sort();
        out
    }

    pub fn addons_dir(&self) -> PathBuf {
        self.path.join("Interface").join("AddOns")
    }

    /// The game executable in this folder, if it's there (`Wow.exe`, `WowClassic.exe`, ...).
    pub fn executable(&self) -> Option<PathBuf> {
        let rd = fs::read_dir(&self.path).ok()?;
        let mut exes: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                n.starts_with("wow") && n.ends_with(".exe")
            })
            .collect();
        exes.sort();
        exes.into_iter().next()
    }
}

/// True for a WoW Forever game folder: `_forever_`, or a classic folder running a 1.60+ client
/// (the Forever beta ran in `_classic_beta_`).
pub fn is_forever(f: &Flavor) -> bool {
    if f.folder.contains("forever") {
        return true;
    }
    let v = f.executable().and_then(|e| exe_file_version(&e)).unwrap_or_default();
    let mut p = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    p.next() == Some(1) && p.next().unwrap_or(0) >= 60
}

pub fn flavor_display(folder: &str) -> String {
    match folder {
        "_retail_" => "Retail".into(),
        "_ptr_" => "Retail PTR".into(),
        "_xptr_" => "Retail PTR 2".into(),
        "_beta_" => "Retail Beta".into(),
        "_classic_" => "Classic (progression)".into(),
        "_classic_ptr_" => "Classic PTR".into(),
        "_classic_beta_" => "Classic Beta / Forever Beta".into(),
        "_classic_era_" => "Classic Era".into(),
        "_classic_era_ptr_" => "Classic Era PTR".into(),
        "_classic_era_beta_" => "Classic Era Beta".into(),
        "_anniversary_" => "Anniversary".into(),
        "_forever_" => "WoW Forever".into(),
        other => other.trim_matches('_').replace('_', " "),
    }
}

/// Lists the flavour folders of an install that look like a game (have a WTF folder or a
/// game executable).
pub fn flavors(root: &Path) -> Vec<Flavor> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with('_') && name.ends_with('_') && name.len() > 2) {
            continue;
        }
        let path = e.path();
        let f = Flavor { folder: name, path };
        if f.path.join("WTF").is_dir() || f.executable().is_some() {
            out.push(f);
        }
    }
    out.sort_by(|a, b| flavor_rank(&a.folder).cmp(&flavor_rank(&b.folder)).then(a.folder.cmp(&b.folder)));
    out
}

fn flavor_rank(folder: &str) -> u8 {
    match folder {
        "_retail_" => 0,
        "_classic_era_" => 1,
        "_classic_" => 2,
        "_anniversary_" => 3,
        "_forever_" => 4,
        "_classic_beta_" => 5,
        _ => 9,
    }
}

/// True when `path` is a WoW install root (contains at least one flavour folder).
pub fn is_install_root(path: &Path) -> bool {
    !flavors(path).is_empty()
}

/// Turns a folder the user picked into an install root: accepts the root itself, a flavour
/// folder, or anything inside one.
pub fn normalize_root(path: &Path) -> Option<PathBuf> {
    let mut p = Some(path);
    while let Some(cur) = p {
        if is_install_root(cur) {
            return Some(cur.to_path_buf());
        }
        p = cur.parent();
    }
    None
}

/// Searches the usual places for WoW installs. Returns install roots, best first.
pub fn detect_installs() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut candidates: Vec<PathBuf> = vec![
        home.join("Games/World of Warcraft"),
        home.join("Games/world-of-warcraft"),
        home.join("World of Warcraft"),
    ];
    // Wine-style prefixes: Lutris (~/Games/<slug>), plain Wine, Bottles, Steam/Proton.
    let mut prefixes: Vec<PathBuf> = vec![home.join(".wine")];
    for dir in [home.join("Games"), home.join(".local/share/bottles/bottles")] {
        if let Ok(rd) = fs::read_dir(&dir) {
            prefixes.extend(rd.flatten().map(|e| e.path()));
        }
    }
    if let Ok(rd) = fs::read_dir(home.join(".local/share/Steam/steamapps/compatdata")) {
        prefixes.extend(rd.flatten().map(|e| e.path().join("pfx")));
    }
    for pfx in prefixes {
        let c = pfx.join("drive_c");
        if !c.is_dir() {
            continue;
        }
        for sub in ["Program Files (x86)/World of Warcraft", "Program Files/World of Warcraft", "World of Warcraft"] {
            candidates.push(c.join(sub));
        }
        // Drive letters pointing elsewhere (Lutris often maps X: to the home folder).
        if let Ok(rd) = fs::read_dir(pfx.join("dosdevices")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.len() == 2 && n.ends_with(':') && n != "c:" && n != "z:" {
                    if let Ok(target) = fs::canonicalize(e.path()) {
                        candidates.push(target.join("Games/World of Warcraft"));
                        candidates.push(target.join("World of Warcraft"));
                    }
                }
            }
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if is_install_root(&c) {
            let c = fs::canonicalize(&c).unwrap_or(c);
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// A running WoW client. `folder` is the flavour folder it runs from (e.g. `_forever_`),
/// or None when that can't be told; such a client counts as running in every folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningGame {
    pub folder: Option<String>,
}

/// True while a WoW client is running (under Wine/Proton the process shows up with its
/// Windows executable name).
pub fn game_running() -> bool {
    !running_games().is_empty()
}

/// True when a running client belongs to this flavour folder (or to an unknown one), like
/// the Windows client's per-branch check.
pub fn runs_in(running: &[RunningGame], flavor: &Flavor) -> bool {
    running.iter().any(|r| r.folder.as_deref().is_none_or(|f| f.eq_ignore_ascii_case(&flavor.folder)))
}

const WOW_EXES: [&str; 6] = ["wow.exe", "wowt.exe", "wowb.exe", "wowclassic.exe", "wowclassict.exe", "wowclassicb.exe"];

/// The WoW clients running now, with the flavour folder each one runs from.
pub fn running_games() -> Vec<RunningGame> {
    if super::install_kind() == super::Install::Flatpak {
        // The sandbox has its own /proc; ask the host.
        let out = std::process::Command::new("flatpak-spawn")
            .args(["--host", "pgrep", "-ifa", r"(^|[/\\])wow(classic)?[tb]?\.exe"])
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(out) = out else { return Vec::new() };
        return String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                // "<pid> <command line>"; the arguments are joined with spaces, and the path
                // itself may have spaces, so look for the executable name in it.
                let cmd = l.trim_start().split_once(' ')?.1;
                let lower = cmd.to_lowercase();
                let end = WOW_EXES.iter().filter_map(|e| find_exe(&lower, e).map(|p| p + e.len())).min()?;
                Some(RunningGame { folder: flavor_folder_of(&cmd[..end], None) })
            })
            .collect();
    }
    let Ok(rd) = fs::read_dir("/proc") else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = fs::read(e.path().join("cmdline")) else { continue };
        let first = cmd.split(|b| *b == 0).next().unwrap_or_default();
        let exe = String::from_utf8_lossy(first);
        let base = exe.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
        if WOW_EXES.contains(&base.as_str()) {
            // Wine starts the game in its folder, so the working directory says it too.
            let cwd = fs::read_link(e.path().join("cwd")).ok();
            out.push(RunningGame { folder: flavor_folder_of(&exe, cwd.as_deref()) });
        }
    }
    out
}

/// Where `exe` (lowercase) appears in `cmd` as a file name, right after a path separator or
/// at the start.
fn find_exe(cmd: &str, exe: &str) -> Option<usize> {
    cmd.match_indices(exe).map(|(i, _)| i).find(|&i| i == 0 || matches!(cmd.as_bytes()[i - 1], b'/' | b'\\'))
}

/// The flavour folder of a game executable path (Windows or Unix style), else of the
/// process's working directory.
fn flavor_folder_of(exe: &str, cwd: Option<&Path>) -> Option<String> {
    let is_flavor = |s: &str| s.len() > 2 && s.starts_with('_') && s.ends_with('_');
    let mut parts = exe.rsplit(['/', '\\']);
    parts.next();
    if let Some(dir) = parts.next().filter(|d| is_flavor(d)) {
        return Some(dir.to_string());
    }
    let dir = cwd?.file_name()?.to_string_lossy().into_owned();
    is_flavor(&dir).then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_of_running_game() {
        let cwd = Path::new("/home/u/Games/World of Warcraft/_retail_");
        assert_eq!(flavor_folder_of(r"X:\Games\World of Warcraft\_forever_\WowClassic.exe", Some(cwd)).as_deref(), Some("_forever_"));
        assert_eq!(flavor_folder_of("/home/u/wow/_classic_beta_/WowClassicB.exe", None).as_deref(), Some("_classic_beta_"));
        // Started by a bare or relative name: the working directory tells.
        assert_eq!(flavor_folder_of("WoW.exe", Some(cwd)).as_deref(), Some("_retail_"));
        assert_eq!(flavor_folder_of("./WoW.exe", Some(cwd)).as_deref(), Some("_retail_"));
        assert_eq!(flavor_folder_of("WoW.exe", Some(Path::new("/home/u"))), None);
        assert_eq!(flavor_folder_of("WoW.exe", None), None);

        let f = |folder: &str| Flavor { folder: folder.into(), path: PathBuf::from("/x").join(folder) };
        let retail = [RunningGame { folder: Some("_retail_".into()) }];
        assert!(runs_in(&retail, &f("_retail_")));
        assert!(!runs_in(&retail, &f("_forever_")));
        assert!(!runs_in(&[], &f("_forever_")));
        // Unknown folder: counts everywhere, as before.
        assert!(runs_in(&[RunningGame { folder: None }], &f("_forever_")));
        assert!(runs_in(&[RunningGame { folder: Some("_FOREVER_".into()) }], &f("_forever_")));
    }

    #[test]
    fn exe_in_pgrep_line() {
        let cmd = r"X:\Games\World of Warcraft\_forever_\WowClassic.exe -launcherlogin -uid wow_forever";
        let lower = cmd.to_lowercase();
        let end = WOW_EXES.iter().filter_map(|e| find_exe(&lower, e).map(|p| p + e.len())).min().unwrap();
        assert_eq!(flavor_folder_of(&cmd[..end], None).as_deref(), Some("_forever_"));
        // "wow.exe" inside "mywow.exe" is not the game.
        assert_eq!(find_exe("c:\\mywow.exe", "wow.exe"), None);
    }
}

/// Maps a Windows path from a Wine prefix (e.g. `X:\Games\World of Warcraft`) to the Linux
/// path, using the prefix's `dosdevices` links.
pub fn wine_to_unix(prefix: &Path, win: &str) -> Option<PathBuf> {
    let win = win.trim();
    let bytes = win.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = win[2..].trim_start_matches(['\\', '/']).replace('\\', "/");
    let dev = prefix.join("dosdevices").join(format!("{drive}:"));
    let base = if drive == 'c' && !dev.exists() { prefix.join("drive_c") } else { fs::canonicalize(&dev).ok()? };
    let full = base.join(&rest);
    Some(resolve_case(&full).unwrap_or(full))
}

/// Windows paths are case-insensitive; find the real spelling on disk.
pub fn resolve_case(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(path.to_path_buf());
    }
    let mut cur = PathBuf::from("/");
    for comp in path.components().skip(1) {
        let want = comp.as_os_str().to_string_lossy().to_lowercase();
        let next = cur.join(comp.as_os_str());
        if next.exists() {
            cur = next;
            continue;
        }
        let found = fs::read_dir(&cur)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().to_lowercase() == want)?;
        cur = found.path();
    }
    Some(cur)
}

/// Reads the `FileVersion` string of a Windows executable (what .NET's FileVersionInfo
/// reports, e.g. `12.1.5.70575`). Blizzard's fixed-size version block is packed
/// differently, so the string table is used.
pub fn exe_file_version(path: &Path) -> Option<String> {
    // Executables are ~80 MB; remember the answer until the file changes.
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, std::time::SystemTime, Option<String>)>>> = Mutex::new(None);
    let meta = fs::metadata(path).ok()?;
    let stamp = (meta.len(), meta.modified().ok()?);
    if let Some((len, time, v)) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(path) {
        if (*len, *time) == stamp {
            return v.clone();
        }
    }
    let v = read_file_version(path);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (stamp.0, stamp.1, v.clone()));
    v
}

fn read_file_version(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    let key: Vec<u8> = "FileVersion\0".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let at = data.windows(key.len()).rposition(|w| w == key.as_slice())?;
    let mut p = at + key.len();
    while data.get(p..p + 2) == Some(&[0, 0]) {
        p += 2;
    }
    let mut units = Vec::new();
    while let Some(b) = data.get(p..p + 2) {
        let u = u16::from_le_bytes([b[0], b[1]]);
        if u == 0 || units.len() > 40 {
            break;
        }
        units.push(u);
        p += 2;
    }
    let v = String::from_utf16_lossy(&units).trim().to_string();
    (!v.is_empty() && v.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(v)
}
