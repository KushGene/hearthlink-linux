//! Wowhead: does what the Windows "Wowhead Client" 2.0.9801.37696 does.
//!
//! - logs in with a code the user approves on wowhead.com,
//! - uploads the game's WDB caches, the hotfix cache (DBCache.bin) and the
//!   `+Wowhead_Looter` SavedVariables, only what's new since the last upload,
//! - keeps the `+Wowhead_Looter` addon up to date in every game folder,
//! - sends what the Looter's in-game Sync button asks for while WoW Forever runs.

use super::{Meta, Service};
use crate::core::{self, lua, now_unix, store, wine, wow, Common, Env, Health};
use eframe::egui;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub static META: Meta = Meta {
    id: "wowhead",
    name: "Wowhead",
    tagline: "Game data for wowhead.com, and the Wowhead Looter addon",
    icon: include_bytes!("../../assets/icons/wowhead.png"),
    website: "https://www.wowhead.com/client",
    upstream_app: "Wowhead Client",
    ported_version: "2.0.9801.37696",
    upstream_url: "https://www.wowhead.com/client",
    runs_official: false,
};

const URL_UPDATE: &str = "https://www.wowhead.com/client/update";
const URL_UPLOAD: &str = "https://www.wowhead.com/client/upload";
const URL_NEW_CODE: &str = "https://www.wowhead.com/client/new-login-code";
const URL_LOGIN: &str = "https://www.wowhead.com/client/login/";
const URL_CODE_STATUS: &str = "https://www.wowhead.com/client/login-code-status/";
const URL_SYNC: &str = "https://www.wowhead.com/client/sync";
const LOCALES: [&str; 11] = ["enUS", "esES", "esMX", "deDE", "frFR", "itIT", "koKR", "ptBR", "ruRU", "zhCN", "zhTW"];
const LOOTER_SV: &str = "+Wowhead_Looter.lua";

fn user_agent() -> String {
    format!("WowheadClient/{} (Linux; hearthlink {})", META.ported_version, env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------------------
// Game branches, numbered like the Windows client's WowBranch enum (the numbers end up in
// the cache manifests and decide the upload file names).

#[derive(Clone, Copy)]
struct Branch {
    id: u32,
    short: &'static str,
    upload: &'static str,
    display: &'static str,
}

const BRANCHES: [Branch; 22] = [
    Branch { id: 0, short: "LIVE", upload: "LIVE", display: "Retail" },
    Branch { id: 1, short: "PTR", upload: "PTR", display: "PTR" },
    Branch { id: 2, short: "BETA", upload: "BETA", display: "Beta" },
    Branch { id: 3, short: "CLASSIC", upload: "TBC", display: "TBC Classic" },
    Branch { id: 4, short: "CLASSIC_PTR", upload: "TBC", display: "TBC Classic PTR" },
    Branch { id: 5, short: "CLASSIC_BETA", upload: "TBC", display: "TBC Classic Beta" },
    Branch { id: 6, short: "CLASSICERA", upload: "CLASSIC", display: "Classic" },
    Branch { id: 7, short: "CLASSICERA_PTR", upload: "CLASSICPTR", display: "Classic PTR" },
    Branch { id: 8, short: "CLASSICERA_BETA", upload: "CLASSIC", display: "Classic Beta" },
    Branch { id: 9, short: "WRATH", upload: "WRATH", display: "WotLK" },
    Branch { id: 10, short: "WRATH_PTR", upload: "WRATH", display: "WotLK PTR" },
    Branch { id: 11, short: "WRATH_BETA", upload: "WRATH", display: "WotLK Beta" },
    Branch { id: 12, short: "PTR2", upload: "PTR2", display: "PTR 2" },
    Branch { id: 13, short: "CATA", upload: "CATA", display: "Cataclysm" },
    Branch { id: 14, short: "CATA_PTR", upload: "CATA", display: "Cataclysm PTR" },
    Branch { id: 15, short: "CATA_BETA", upload: "CATA", display: "Cataclysm Beta" },
    Branch { id: 16, short: "MISTS", upload: "MISTS", display: "Mists" },
    Branch { id: 17, short: "MISTS_PTR", upload: "MISTS", display: "Mists PTR" },
    Branch { id: 18, short: "MISTS_BETA", upload: "MISTS", display: "Mists Beta" },
    Branch { id: 19, short: "CLASSICPLUS", upload: "CLASSICPLUS", display: "Forever" },
    Branch { id: 20, short: "CLASSICPLUS_PTR", upload: "CLASSICPLUS", display: "Forever PTR" },
    Branch { id: 21, short: "CLASSICPLUS_BETA", upload: "CLASSICPLUS", display: "Forever Beta" },
];

/// The family a branch's addon files come from when the update has no file of its own
/// for it (Ptr/Beta use their main game's files).
fn family_main(id: u32) -> u32 {
    match id {
        3..=5 => 3,
        6..=8 => 6,
        9..=11 => 9,
        13..=15 => 13,
        16..=18 => 16,
        19..=21 => 19,
        12 => 1,
        x => x,
    }
}

/// Works out which branch a flavour folder is, using the game executable's version where
/// the folder name alone is ambiguous (`_classic_` has been Wrath, Cata and Mists).
fn branch_for(folder: &str, version: Option<&str>) -> Option<u32> {
    let (major, minor) = version
        .map(|v| {
            let mut p = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
            (p.next().unwrap_or(0), p.next().unwrap_or(0))
        })
        .unwrap_or((0, 0));
    let forever = major == 1 && minor >= 60;
    let by_major = |main: u32, era: u32, tbc: u32, forever_id: u32| match major {
        _ if forever => forever_id,
        1 => era,
        2 => tbc,
        3 => main - 16 + 9,
        4 => main - 16 + 13,
        _ => main,
    };
    Some(match folder {
        "_retail_" => 0,
        "_ptr_" => 1,
        "_xptr_" => 12,
        "_beta_" => 2,
        "_anniversary_" => 3,
        "_classic_era_" => 6,
        "_classic_era_ptr_" => if major == 2 { 4 } else { 7 },
        "_classic_era_beta_" => 8,
        "_forever_" => 19,
        "_classic_" => by_major(16, 6, 3, 19),
        "_classic_ptr_" => by_major(17, 7, 4, 20),
        "_classic_beta_" => by_major(18, 8, 5, 21),
        _ => return None,
    })
}

#[derive(Clone)]
struct GameFolder {
    branch: Branch,
    flavor: wow::Flavor,
    version: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Settings

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    user: String,
    token: String,
    /// None until the user answered the "upload character data too?" question.
    upload_character_data: Option<bool>,
    excluded_accounts: Vec<String>,
    upload_on_game_exit: bool,
    keep_looter_updated: bool,
    last_upload: i64,
    imported_from_wine: bool,
    /// Send what the Looter's Sync button asks for (WoW Forever). On, like in the Windows
    /// client, also for settings saved before this option existed.
    in_game_sync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            user: String::new(),
            token: String::new(),
            upload_character_data: None,
            excluded_accounts: Vec::new(),
            upload_on_game_exit: false,
            keep_looter_updated: false,
            last_upload: 0,
            imported_from_wine: false,
            in_game_sync: true,
        }
    }
}

impl Config {
    fn fresh() -> Self {
        Self { enabled: true, upload_on_game_exit: true, keep_looter_updated: true, ..Default::default() }
    }
}

fn data_dir() -> PathBuf {
    store::data_dir().join("wowhead")
}

// ---------------------------------------------------------------------------------------
// The client's change manifests: which cache records were already uploaded, by MD5.
// Same file format as the Windows client ("FMHW" v2), so its files can be reused.

const FLAG_SEEN: u8 = 1;
const FLAG_NEW: u8 = 2;

struct Manifest {
    path: PathBuf,
    entries: HashMap<(u32, u32), ([u8; 16], u8)>,
}

impl Manifest {
    fn open(sub_dir: &str, base: &str, sub: &str, locale: &str, build: u32) -> Self {
        let build = if build != 0 { format!("_{build}") } else { String::new() };
        let path = data_dir().join("cache").join(sub_dir).join(format!("{base}_{sub}_{locale}{build}.mf2"));
        let mut entries = HashMap::new();
        if let Ok(d) = fs::read(&path) {
            if d.len() >= 16 && &d[0..4] == b"FMHW" && u32::from_le_bytes(d[4..8].try_into().unwrap()) == 2 {
                let mut p = 16;
                while p + 24 <= d.len() {
                    let fid = u32::from_le_bytes(d[p..p + 4].try_into().unwrap());
                    let eid = u32::from_le_bytes(d[p + 4..p + 8].try_into().unwrap());
                    let mut h = [0u8; 16];
                    h.copy_from_slice(&d[p + 8..p + 24]);
                    entries.insert((fid, eid), (h, 0));
                    p += 24;
                }
            }
        }
        Self { path, entries }
    }

    /// Records a record's hash; true when it's new or changed.
    fn update(&mut self, fid: u32, eid: u32, hash: [u8; 16]) -> bool {
        match self.entries.get_mut(&(fid, eid)) {
            Some((h, f)) if *h == hash => {
                *f |= FLAG_SEEN;
                false
            }
            Some((h, f)) => {
                *h = hash;
                *f |= FLAG_SEEN | FLAG_NEW;
                true
            }
            None => {
                self.entries.insert((fid, eid), (hash, FLAG_SEEN | FLAG_NEW));
                true
            }
        }
    }

    fn should_upload(&self, fid: u32, eid: u32) -> bool {
        self.entries.get(&(fid, eid)).is_some_and(|(_, f)| f & FLAG_NEW != 0)
    }

    fn clear_new(&mut self, fid: u32, eid: u32) {
        if let Some((_, f)) = self.entries.get_mut(&(fid, eid)) {
            *f &= !FLAG_NEW;
        }
    }

    fn new_count(&self) -> usize {
        self.entries.values().filter(|(_, f)| f & FLAG_NEW != 0).count()
    }

    /// Saves the records seen in this scan.
    fn save(&self) -> std::io::Result<()> {
        let mut out = Vec::with_capacity(16 + self.entries.len() * 24);
        out.extend_from_slice(b"FMHW");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut keys: Vec<_> = self.entries.iter().filter(|(_, (_, f))| f & FLAG_SEEN != 0).collect();
        keys.sort_by_key(|(k, _)| **k);
        for ((fid, eid), (h, _)) in keys {
            out.extend_from_slice(&fid.to_le_bytes());
            out.extend_from_slice(&eid.to_le_bytes());
            out.extend_from_slice(h);
        }
        core::write_atomic(&self.path, &out)
    }
}

fn md5(data: &[u8]) -> [u8; 16] {
    use md5::Digest;
    md5::Md5::digest(data).into()
}

// ---------------------------------------------------------------------------------------
// WDB caches (Cache/WDB/<locale>/*.wdb): a 24-byte header, then (id, length, bytes).

const WDB_FILES: [&str; 4] = ["creaturecache.wdb", "gameobjectcache.wdb", "pagetextcache.wdb", "questcache.wdb"];

struct WdbFile {
    path: PathBuf,
    kind: u32,
    locale: String,
    header: Vec<u8>,
    records: Vec<(i32, Vec<u8>)>,
    manifest: Manifest,
}

fn read_wdb(path: &Path) -> Option<(Vec<u8>, Vec<(i32, Vec<u8>)>)> {
    let d = fs::read(path).ok()?;
    if d.len() < 24 {
        return None;
    }
    let header = d[..24].to_vec();
    let mut recs = Vec::new();
    let mut p = 24;
    while p + 8 <= d.len() {
        let id = i32::from_le_bytes(d[p..p + 4].try_into().unwrap());
        let len = i32::from_le_bytes(d[p + 4..p + 8].try_into().unwrap());
        p += 8;
        let n = len.max(0) as usize;
        let body = d.get(p..(p + n).min(d.len())).unwrap_or_default().to_vec();
        p += n;
        recs.push((id, body));
        if id <= 0 || len <= 0 {
            break;
        }
    }
    Some((header, recs))
}

fn load_wdbs(game: &GameFolder) -> Vec<WdbFile> {
    let mut out = Vec::new();
    let root = game.flavor.path.join("Cache").join("WDB");
    let Ok(rd) = fs::read_dir(&root) else { return out };
    let mut locales: Vec<String> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    locales.sort();
    for locale in locales {
        for (kind, name) in WDB_FILES.iter().enumerate() {
            let path = root.join(&locale).join(name);
            let Some((header, records)) = read_wdb(&path) else { continue };
            let base = name.trim_end_matches(".wdb");
            let mut manifest = Manifest::open("wdbcache", base, game.branch.short, &locale, 0);
            for (id, body) in &records {
                if *id > 0 && !body.is_empty() {
                    manifest.update(kind as u32, *id as u32, md5(body));
                }
            }
            out.push(WdbFile { path, kind: kind as u32, locale: locale.clone(), header, records, manifest });
        }
    }
    out
}

/// The upload body for one WDB file: its header, then the new records, then a (0, 0)
/// terminator when the file had one.
fn pack_wdb(w: &mut WdbFile) -> (Vec<u8>, usize) {
    let mut out = w.header.clone();
    let mut n = 0;
    for (id, body) in &w.records {
        if *id == 0 {
            out.extend_from_slice(&0i32.to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes());
            break;
        }
        if w.manifest.should_upload(w.kind, *id as u32) {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(body.len() as i32).to_le_bytes());
            out.extend_from_slice(body);
            w.manifest.clear_new(w.kind, *id as u32);
            n += 1;
        }
    }
    (out, n)
}

// ---------------------------------------------------------------------------------------
// The hotfix cache (Cache/ADB/<locale>/DBCache.bin): "XFTH" header (44 bytes), then
// entries with a version-dependent header.

struct DbEntry {
    push_id: i32,
    file_id: u32,
    entry_id: u32,
    raw: Vec<u8>, // entry header + data, as uploaded
}

struct DbCacheFile {
    #[allow(dead_code)]
    path: PathBuf,
    locale: String,
    header: Vec<u8>,
    entries: Vec<DbEntry>,
    header_size: usize,
    manifest: Manifest,
}

fn read_dbcache(path: &Path) -> Option<(Vec<u8>, u32, usize, Vec<DbEntry>)> {
    let d = fs::read(path).ok()?;
    if d.len() < 44 || &d[0..4] != b"XFTH" {
        return None;
    }
    let version = u32::from_le_bytes(d[4..8].try_into().unwrap());
    let build = u32::from_le_bytes(d[8..12].try_into().unwrap());
    let hsize = if version == 8 { 28 } else { 32 };
    let mut entries = Vec::new();
    let mut p = 44;
    while p + hsize <= d.len() {
        let h = &d[p..p + hsize];
        if &h[0..4] != b"XFTH" {
            break;
        }
        let rd = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        // v8: sig, push, unk, file, entry, len, index; v9 adds an unknown before push.
        let (push, file, entry, len) = if version == 8 {
            (rd(4) as i32, rd(12), rd(16), rd(20))
        } else {
            (rd(8) as i32, rd(16), rd(20), rd(24))
        };
        if len > 4096 {
            break;
        }
        let end = p + hsize + len as usize;
        if end > d.len() {
            break;
        }
        entries.push(DbEntry { push_id: push, file_id: file, entry_id: entry, raw: d[p..end].to_vec() });
        p = end;
    }
    Some((d[..44].to_vec(), build, hsize, entries))
}

/// Tables the client ignores when an entry carries no data and no push id.
fn ignore_entry(e: &DbEntry, header_size: usize) -> bool {
    (e.file_id == 0xDF2F53CF || e.file_id == 0xAFC190D1) && e.raw.len() == header_size && e.push_id == -1
}

fn load_dbcaches(game: &GameFolder) -> Vec<DbCacheFile> {
    let mut out = Vec::new();
    let root = game.flavor.path.join("Cache").join("ADB");
    for locale in LOCALES {
        let path = root.join(locale).join("DBCache.bin");
        let Some((header, build, hsize, entries)) = read_dbcache(&path) else { continue };
        let mut manifest = Manifest::open("dbcache", "DBCache", game.branch.short, locale, build);
        // Only the newest push of each record counts.
        let mut newest: HashMap<(u32, u32), i32> = HashMap::new();
        for e in &entries {
            if ignore_entry(e, hsize) {
                continue;
            }
            let k = (e.file_id, e.entry_id);
            match newest.get(&k) {
                Some(p) if e.push_id <= *p => {}
                _ => {
                    newest.insert(k, e.push_id);
                }
            }
        }
        for e in &entries {
            if ignore_entry(e, hsize) || newest.get(&(e.file_id, e.entry_id)) != Some(&e.push_id) {
                continue;
            }
            manifest.update(e.file_id, e.entry_id, md5(&e.raw));
        }
        out.push(DbCacheFile { path, locale: locale.into(), header, entries, header_size: hsize, manifest });
    }
    out
}

fn pack_dbcache(f: &mut DbCacheFile) -> (Vec<u8>, usize) {
    let mut out = f.header.clone();
    let mut sent: HashSet<(u32, u32, i32)> = HashSet::new();
    let mut n = 0;
    for e in &f.entries {
        let k = (e.file_id, e.entry_id, e.push_id);
        if sent.contains(&k) || ignore_entry(e, f.header_size) {
            continue;
        }
        sent.insert(k);
        if f.manifest.should_upload(e.file_id, e.entry_id) {
            out.extend_from_slice(&e.raw);
            n += 1;
        }
    }
    for (fid, eid, _) in sent {
        f.manifest.clear_new(fid, eid);
    }
    (out, n)
}

// ---------------------------------------------------------------------------------------
// The Looter addon's SavedVariables.

#[derive(Clone, Debug)]
struct LooterFile {
    account: String,
    path: PathBuf,
    uploaded: bool,
    locale: String,
    stats: [u64; 6], // units, objects, quests, drops, auctions, profiles
}

fn load_looter_files(game: &GameFolder) -> Vec<LooterFile> {
    let stats_re = Regex::new(r#"(?i)wlStats = "(\d+)\^(\d+)\^(\d+)\^(\d+)\^(\d+)\^(\d+)\^(\w+)\^(\d+)""#).unwrap();
    let up_re = Regex::new(r"(?i)wlUploaded = (\d)").unwrap();
    let mut out = Vec::new();
    for (account, sv) in game.flavor.accounts() {
        let path = sv.join(LOOTER_SV);
        let Ok(text) = fs::read(&path) else { continue };
        if text.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(&text);
        let mut f = LooterFile { account, path, uploaded: false, locale: String::new(), stats: [0; 6] };
        if let Some(c) = stats_re.captures(&text) {
            for i in 0..6 {
                f.stats[i] = c[i + 1].parse().unwrap_or(0);
            }
            f.locale = c[7].to_string();
        }
        if let Some(c) = up_re.captures(&text) {
            f.uploaded = c[1].parse::<u32>().unwrap_or(0) > 0;
        }
        out.push(f);
    }
    out
}

fn set_uploaded(path: &Path) -> std::io::Result<()> {
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    let re = Regex::new(r"(?i)wlUploaded = \d+").unwrap();
    let new = re.replace_all(&text, "wlUploaded = 1");
    fs::write(path, new.as_bytes())
}

// ---------------------------------------------------------------------------------------
// SavedVariables split into their top-level assignments, like the Windows client's
// SavedVariablesParser: character data is cut out by name, and an in-game sync sends
// single variables.

/// The Looter's character data, left out unless the user uploads character data.
const CHARACTER_DATA_VARIABLES: [&str; 3] = ["wlExportData", "wlProfileData", "wlCharacterData"];

/// The top-level assignments of a SavedVariables file in file order, as (name, block). A
/// block runs from its name to the next top-level name, so it includes the trailing
/// newline. A name only counts at the start of a line, outside strings and tables. When a
/// name appears twice, the later block replaces the earlier one (at the earlier position).
fn split_top_level(text: &str) -> Vec<(String, String)> {
    let name_re = Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*) = ").unwrap();
    let b = text.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut record = |name: String, block: &str| match out.iter_mut().find(|(n, _)| *n == name) {
        Some(e) => e.1 = block.to_string(),
        None => out.push((name, block.to_string())),
    };
    let (mut in_string, mut depth, mut line_start) = (false, 0i64, true);
    let mut open: Option<(String, usize)> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if line_start && depth == 0 {
            // Line starts are always char boundaries (right after a '\n', or 0).
            if let Some(m) = text.get(i..).and_then(|rest| name_re.captures(rest)) {
                if let Some((name, start)) = open.take() {
                    record(name, &text[start..i]);
                }
                open = Some((m[1].to_string(), i));
            }
        }
        line_start = false;
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'\n' => line_start = true,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // A comment: go on at its line's newline, which starts the next line.
                match b[i..].iter().position(|x| *x == b'\n') {
                    Some(p) => i += p - 1,
                    None => i = b.len(),
                }
            }
            _ => {}
        }
        i += 1;
    }
    if let Some((name, start)) = open {
        record(name, &text[start..]);
    }
    out
}

/// The text without the named top-level assignments; everything else stays as it was.
fn remove_top_level(text: &str, names: &[&str]) -> String {
    let blocks = split_top_level(text);
    let mut out = text.to_string();
    for name in names {
        if let Some((_, block)) = blocks.iter().find(|(n, _)| n == name) {
            if let Some(p) = out.find(block.as_str()) {
                out.replace_range(p..p + block.len(), "");
            }
        }
    }
    out
}

/// What the Looter's Sync button wrote into `wlSyncRequest`.
#[derive(Debug, Clone, PartialEq)]
struct SyncRequest {
    time: i64,
    version: i32,
    vars: Vec<String>,
}

fn parse_sync_request(block: &str) -> Option<SyncRequest> {
    let time = Regex::new(r#"\["time"\]\s*=\s*(\d+)"#).unwrap().captures(block)?[1].parse().ok()?;
    let version = Regex::new(r#"\["version"\]\s*=\s*(\d+)"#)
        .unwrap()
        .captures(block)
        .and_then(|c| c[1].parse().ok())
        .unwrap_or(0);
    let mut vars = Vec::new();
    if let Some(m) = Regex::new(r#"\["vars"\]\s*=\s*\{"#).unwrap().find(block) {
        let body = table_body(block, m.end());
        for c in Regex::new(r#"(?s)"((?:[^"\\]|\\.)*)""#).unwrap().captures_iter(body) {
            vars.push(c[1].to_string());
        }
    }
    Some(SyncRequest { time, version, vars })
}

/// The inside of the table whose `{` ends right before `start`, up to its matching `}`
/// (or the rest of the text when there's none).
fn table_body(text: &str, start: usize) -> &str {
    let b = text.as_bytes();
    let (mut depth, mut in_string, mut i) = (1, false, start);
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return &text[start..i];
            }
        }
        i += 1;
    }
    &text[start..]
}

// ---------------------------------------------------------------------------------------
// In-game sync (WoW Forever): the Looter's Sync button writes `wlSyncRequest` and reloads
// the UI, which saves the Looter file. While the game runs, a changed file with a new
// request is sent to /client/sync, like the Windows client's SyncManager does.

/// Game branches the Windows client syncs: Forever, Forever PTR, Forever Beta.
const SYNC_BRANCHES: [u32; 3] = [19, 20, 21];
/// Sent with every sync, besides the variables the request names.
const ALWAYS_SENT: [&str; 3] = ["wlVersion", "wlSyncRequest", "wlRealmList"];

/// What the sync knows about one account's Looter file.
#[derive(Default)]
struct FileState {
    mtime: Option<SystemTime>,
    len: u64,
    stable_since: Option<Instant>,
    checked: bool,
    retry_at: Option<Instant>,
    failed_sends: u32,
    version_checks: u32,
    waiting_for_login: bool,
    pending: Option<(SyncRequest, Vec<(String, String)>)>,
}

/// The file's state after looking at it again: a changed file (time or size) starts over.
fn file_state_update(old: Option<FileState>, mtime: SystemTime, len: u64, now: Instant) -> FileState {
    match old {
        Some(s) if s.mtime == Some(mtime) && s.len == len => s,
        _ => FileState { mtime: Some(mtime), len, stable_since: Some(now), ..Default::default() },
    }
}

/// True when the file should be looked at now: unchanged for 2 seconds, and either not
/// checked since it changed or due for a retry.
fn file_ready(s: &FileState, now: Instant) -> bool {
    if s.stable_since.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(2)) {
        return false;
    }
    if s.checked {
        s.retry_at.is_some_and(|t| now >= t)
    } else {
        true
    }
}

fn keep_pending(s: &mut FileState, req: SyncRequest, blocks: Vec<(String, String)>, secs: u64) {
    s.pending = Some((req, blocks));
    s.retry_at = Some(Instant::now() + Duration::from_secs(secs));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncResult {
    Accepted,
    Rejected,
    Throttled,
    Unauthorized,
    Failed,
}

/// The site's answer to a sync; every field may be missing.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SyncResponse {
    error: Option<bool>,
    message: Option<String>,
    token: Option<String>,
    wait: Option<i64>,
    results: Option<BTreeMap<String, serde_json::Value>>,
}

/// How the Windows client reads the site's answer, in its order.
fn classify_sync(status: u16, resp: Option<&SyncResponse>) -> SyncResult {
    if status == 401 {
        return SyncResult::Unauthorized;
    }
    if status == 429 || resp.is_some_and(|r| r.wait.unwrap_or(0) > 0) {
        return SyncResult::Throttled;
    }
    if (200..300).contains(&status) {
        return match resp {
            None => SyncResult::Failed,
            Some(r) if r.error == Some(true) => SyncResult::Rejected,
            Some(_) => SyncResult::Accepted,
        };
    }
    if status >= 500 {
        return SyncResult::Failed;
    }
    SyncResult::Rejected
}

/// The parsed answer and how to treat it; a body that isn't JSON counts as no answer.
fn read_sync_answer(status: u16, text: &str) -> (SyncResult, Option<SyncResponse>) {
    let resp: Option<SyncResponse> = serde_json::from_str(text).ok();
    (classify_sync(status, resp.as_ref()), resp)
}

/// What a sync sends: the always-sent variables, then the requested ones, as their blocks
/// from the Looter file.
#[derive(Debug, Default, PartialEq)]
struct SyncBody {
    body: String,
    sent: Vec<String>,
    /// Character data, not sent because this account doesn't upload it.
    left_out: Vec<String>,
    /// Requested names that aren't Looter variables.
    skipped: Vec<String>,
    /// Names not in the file.
    missing: Vec<String>,
}

fn build_sync_body(blocks: &[(String, String)], req: &SyncRequest, with_characters: bool) -> SyncBody {
    let var_re = Regex::new(r"^wl\w+$").unwrap();
    let mut out = SyncBody::default();
    let mut names: Vec<String> = ALWAYS_SENT.iter().map(|s| s.to_string()).collect();
    for v in &req.vars {
        if !var_re.is_match(v) {
            out.skipped.push(v.clone());
        } else if !names.contains(v) {
            names.push(v.clone());
        }
    }
    for n in names {
        let Some((_, block)) = blocks.iter().find(|(k, _)| *k == n) else {
            out.missing.push(n);
            continue;
        };
        if !with_characters && CHARACTER_DATA_VARIABLES.contains(&n.as_str()) {
            out.left_out.push(n);
            continue;
        }
        out.body.push_str(block);
        if !block.ends_with('\n') {
            out.body.push_str("\r\n");
        }
        out.sent.push(n);
    }
    out
}

fn sync_file_name(g: &GameFolder, version: &str, account: &str) -> String {
    format!("{}_{}_{}_Sync.lua.gz", g.branch.upload, version, account)
}

/// POSTs a sync like the Windows client: `file0` metadata.json (application/json), `file1`
/// the gzipped variables (application/binary), the login in X-Wowhead-Client-Token.
/// Returns the status and the body.
fn post_sync(
    http: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    metadata: String,
    name: String,
    data: Vec<u8>,
) -> reqwest::Result<(u16, String)> {
    use reqwest::blocking::multipart::{Form, Part};
    let form = Form::new()
        .part("file0", Part::text(metadata).file_name("metadata.json").mime_str("application/json")?)
        .part("file1", Part::bytes(data).file_name(name).mime_str("application/binary")?);
    let res = http.post(url).header("X-Wowhead-Client-Token", token).multipart(form).send()?;
    let status = res.status().as_u16();
    Ok((status, res.text().unwrap_or_default()))
}

/// " key: value." for each result the site listed, else " message", else nothing.
fn describe_results(resp: Option<&SyncResponse>) -> String {
    let results = resp.and_then(|r| r.results.as_ref()).filter(|r| !r.is_empty());
    match results {
        Some(r) => {
            let parts: Vec<String> = r
                .iter()
                .map(|(k, v)| format!("{k}: {}.", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
                .collect();
            format!(" {}", parts.join(" "))
        }
        None => match resp.and_then(|r| r.message.as_deref()).filter(|m| !m.is_empty()) {
            Some(m) => format!(" {m}"),
            None => String::new(),
        },
    }
}

fn sync_json_path() -> PathBuf {
    data_dir().join("cache").join("sync.json")
}

/// The time of the last request handled per Looter file (path -> request time).
fn load_handled() -> HashMap<String, i64> {
    fs::read(sync_json_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The locales the game folders have caches for (realm lists are looked up per locale), all
/// folders together like the Windows client's DataManager.locales.
fn all_locales(games: &[GameFolder]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in games {
        if let Ok(rd) = fs::read_dir(g.flavor.path.join("Cache").join("WDB")) {
            out.extend(rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()));
        }
        for l in LOCALES {
            if g.flavor.path.join("Cache").join("ADB").join(l).join("DBCache.bin").is_file() {
                out.push(l.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn read_looter_file(path: &Path, c: &Common) -> Option<String> {
    for attempt in 1..=4 {
        match fs::read(path) {
            Ok(d) => return Some(String::from_utf8_lossy(&d).into_owned()),
            Err(e) if attempt == 4 => c.warn(format!("In-game sync: could not read {}: {e}", short_path(path))),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    None
}

#[derive(Default)]
struct SyncState {
    files: HashMap<PathBuf, FileState>,
    refused_token: Option<String>,
    handled: Option<HashMap<String, i64>>,
    running: Vec<wow::RunningGame>,
    running_checked: Option<Instant>,
    /// The Forever game folders, and the locales of all game folders (the Windows client
    /// looks up realm lists for every locale it has seen, not only this game's).
    games: Vec<GameFolder>,
    locales: Vec<String>,
    games_checked: Option<Instant>,
}

impl SyncState {
    fn handled_time(&mut self, path: &Path) -> i64 {
        self.handled.get_or_insert_with(load_handled).get(&path_key(path)).copied().unwrap_or(0)
    }

    fn set_handled(&mut self, path: &Path, time: i64) -> std::io::Result<()> {
        let h = self.handled.get_or_insert_with(load_handled);
        h.insert(path_key(path), time);
        core::write_atomic(&sync_json_path(), &serde_json::to_vec_pretty(h).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------------------
// The Looter addon update ("updates.dat", magic MTWC).

struct LooterUpdate {
    source: String,
    client_version: Option<String>,
    files: Vec<(String, String, String)>, // (md5, file name, destination with %WoW..% token)
}

fn parse_update(d: &[u8]) -> anyhow::Result<LooterUpdate> {
    anyhow::ensure!(d.len() > 48 && &d[0..4] == b"MTWC", "the update file has an unknown format");
    let mut p = 8; // magic + header size
    let take = |n: usize, p: &mut usize| -> anyhow::Result<Vec<u8>> {
        let v = d.get(*p..*p + n).ok_or_else(|| anyhow::anyhow!("the update file is cut short"))?.to_vec();
        *p += n;
        Ok(v)
    };
    let cstr = |p: &mut usize| -> anyhow::Result<String> {
        let rest = d.get(*p..).unwrap_or_default();
        let end = rest.iter().position(|b| *b == 0).ok_or_else(|| anyhow::anyhow!("the update file is cut short"))?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        *p += end + 1;
        Ok(s)
    };
    let _client_hash = take(32, &mut p)?;
    take(4, &mut p)?;
    let count = i32::from_le_bytes(take(4, &mut p)?.try_into().unwrap()).max(0) as usize;
    let source = cstr(&mut p)?;
    let _client_file = cstr(&mut p)?;
    let mut files = Vec::new();
    for _ in 0..count {
        let hash = String::from_utf8_lossy(&take(32, &mut p)?).to_lowercase();
        take(8, &mut p)?; // reserved + size
        let name = cstr(&mut p)?;
        let dest = cstr(&mut p)?;
        files.push((hash, name, dest));
    }
    let client_version = cstr(&mut p).ok().filter(|v| !v.is_empty());
    Ok(LooterUpdate { source, client_version, files })
}

fn token_branch(dest: &str) -> Option<Option<u32>> {
    // None inside = "%WoW%", meaning every game.
    for (tok, id) in [
        ("%WoW-LIVE%", 0),
        ("%WoW-PTR%", 1),
        ("%WoW-BETA%", 2),
        ("%WoW-CLASSIC%", 6),
        ("%WoW-TBC%", 3),
        ("%WoW-WRATH%", 9),
        ("%WoW-CATA%", 13),
        ("%WoW-MISTS%", 16),
        ("%WoW-CLASSICPLUS%", 19),
    ] {
        if dest.contains(tok) {
            return Some(Some(id));
        }
    }
    dest.contains("%WoW%").then_some(None)
}

/// The addon files one game folder should have, following the Windows client's rules:
/// a game's own files, else its family's, and the shared files for any name not covered.
fn looter_files_for(update: &LooterUpdate, branch: u32) -> Vec<(String, String, String)> {
    let mut specific: BTreeMap<u32, BTreeMap<String, (String, String, String)>> = BTreeMap::new();
    let mut shared: Vec<(String, String, String)> = Vec::new();
    for f in &update.files {
        let base = f.1.rsplit('/').next().unwrap_or(&f.1).to_string();
        match token_branch(&f.2) {
            Some(Some(b)) => {
                specific.entry(b).or_default().entry(base).or_insert_with(|| f.clone());
            }
            Some(None) => shared.push(f.clone()),
            None => {}
        }
    }
    let own = specific.get(&branch).or_else(|| specific.get(&family_main(branch)));
    let mut out: Vec<(String, String, String)> = own.map(|m| m.values().cloned().collect()).unwrap_or_default();
    let names: HashSet<String> = own.map(|m| m.keys().cloned().collect()).unwrap_or_default();
    for f in shared {
        let base = f.1.rsplit('/').next().unwrap_or(&f.1).to_string();
        if !names.contains(&base) {
            out.push(f);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Pending {
    scanned_at: i64,
    games: Vec<(String, String, usize, usize, usize)>, // (game, version, wdb new, hotfix new, looter files)
    total: usize,
}

#[derive(Default)]
struct LoginState {
    code: Option<String>,
    url: Option<String>,
}

pub struct Wowhead {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    pending: Mutex<Pending>,
    login: Mutex<LoginState>,
    http: reqwest::blocking::Client,
    looter_note: Mutex<String>,
    game_was_running: Mutex<bool>,
    sync: Mutex<SyncState>,
    /// True during a sync tick; an upload waits for it.
    sync_busy: AtomicBool,
    sync_http: reqwest::blocking::Client,
}

impl Wowhead {
    pub fn new(env: Arc<Env>) -> Self {
        let mut cfg: Config = store::load("wowhead");
        if !std::path::Path::new(&store::config_dir().join("wowhead.json")).exists() {
            cfg = Config::fresh();
        }
        let inner = Arc::new(Inner {
            env,
            common: Arc::new(Common::default()),
            cfg: Mutex::new(cfg),
            pending: Mutex::new(Pending::default()),
            login: Mutex::new(LoginState::default()),
            http: core::http_client(&user_agent(), 120),
            looter_note: Mutex::new(String::new()),
            game_was_running: Mutex::new(false),
            sync: Mutex::new(SyncState::default()),
            sync_busy: AtomicBool::new(false),
            sync_http: core::http_client(&user_agent(), 30),
        });
        Self { inner }
    }
}

impl Inner {
    fn save(&self) {
        store::save("wowhead", &*self.cfg.lock().unwrap());
    }

    fn games(&self) -> Vec<GameFolder> {
        let mut out: Vec<GameFolder> = Vec::new();
        for flavor in self.env.flavors() {
            let version = flavor.executable().and_then(|e| wow::exe_file_version(&e));
            let Some(id) = branch_for(&flavor.folder, version.as_deref()) else { continue };
            out.push(GameFolder { branch: BRANCHES[id as usize], flavor, version });
        }
        out.sort_by_key(|g| g.branch.id);
        out
    }

    /// Takes over login, settings and upload history from a Windows Wowhead Client in a
    /// Wine prefix, so nothing is uploaded twice.
    fn import_from_wine(&self) -> Option<String> {
        let prefixes = wine::prefixes_with("ProgramData/Wowhead/Wowhead Client");
        let prefix = prefixes.first()?;
        let reg = wine::user_reg(prefix, r"Software\Wowhead\Client\Settings");
        let mut cfg = self.cfg.lock().unwrap();
        let mut what = Vec::new();
        if let (Some(t), Some(u)) = (reg.get("Token"), reg.get("UserName")) {
            if !t.is_empty() && cfg.token.is_empty() {
                cfg.token = t.clone();
                cfg.user = u.clone();
                what.push(format!("login ({u})"));
            }
        }
        if let Some(v) = reg.get("CharacterDataUploadRequested") {
            if v == "True" && cfg.upload_character_data.is_none() {
                cfg.upload_character_data = Some(reg.get("UploadCharacterData").map(|v| v == "True").unwrap_or(true));
            }
        }
        if let Some(v) = reg.get("ExcludedAccounts") {
            cfg.excluded_accounts = v.split([',', ';', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        }
        if let Some(v) = reg.get("UploadOnGameExit") {
            cfg.upload_on_game_exit = v == "True";
        }
        if let Some(v) = reg.get("LastUpload").and_then(|v| v.parse().ok()) {
            cfg.last_upload = v;
        }
        let app_data = reg
            .get("AppDataPath")
            .and_then(|p| wow::wine_to_unix(prefix, p))
            .unwrap_or_else(|| prefix.join("drive_c/ProgramData/Wowhead/Wowhead Client"));
        let mut copied = 0;
        for sub in ["wdbcache", "dbcache"] {
            let src = app_data.join("cache").join(sub);
            let dst = data_dir().join("cache").join(sub);
            if let Ok(rd) = fs::read_dir(&src) {
                let _ = fs::create_dir_all(&dst);
                for e in rd.flatten() {
                    let to = dst.join(e.file_name());
                    if !to.exists() && fs::copy(e.path(), &to).is_ok() {
                        copied += 1;
                    }
                }
            }
        }
        if copied > 0 {
            what.push(format!("upload history ({copied} files)"));
        }
        cfg.imported_from_wine = true;
        drop(cfg);
        self.save();
        (!what.is_empty()).then(|| what.join(" and "))
    }

    fn scan(&self) -> Pending {
        let mut p = Pending { scanned_at: now_unix(), ..Default::default() };
        for g in self.games() {
            let wdb: usize = load_wdbs(&g).iter().map(|w| w.manifest.new_count()).sum();
            let hot: usize = load_dbcaches(&g).iter().map(|f| f.manifest.new_count()).sum();
            let lua = load_looter_files(&g).iter().filter(|f| !f.uploaded).count();
            p.total += wdb + hot + lua;
            p.games.push((g.branch.display.to_string(), g.version.clone().unwrap_or_default(), wdb, hot, lua));
        }
        *self.pending.lock().unwrap() = p.clone();
        self.common.repaint();
        p
    }

    fn upload(&self, manual: bool) {
        let c = &self.common;
        if wow::game_running() {
            if manual {
                c.warn("Can't upload while WoW is running (the game still writes its caches). Close it first.");
            }
            return;
        }
        let cfg = self.cfg.lock().unwrap().clone();
        if now_unix() - cfg.last_upload < 60 {
            if manual {
                c.warn("You may only upload once per minute.");
            }
            return;
        }
        // An in-game sync that's under way finishes first (it doesn't start while we run).
        while self.sync_busy.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
        }
        let games = self.games();
        if games.is_empty() {
            c.warn("No WoW game folders found. Pick your World of Warcraft folder in Settings.");
            return;
        }
        c.set_health(Health::Busy, "Checking data…");
        c.info("Checking data…");

        // Load everything first, per game.
        let mut wdbs: Vec<(GameFolder, Vec<WdbFile>)> = Vec::new();
        let mut dbcs: Vec<(GameFolder, Vec<DbCacheFile>)> = Vec::new();
        let mut luas: Vec<(GameFolder, Vec<LooterFile>)> = Vec::new();
        let mut locales: Vec<String> = Vec::new();
        for g in &games {
            let w = load_wdbs(g);
            let d = load_dbcaches(g);
            for l in w.iter().map(|x| x.locale.clone()).chain(d.iter().map(|x| x.locale.clone())) {
                if !locales.contains(&l) {
                    locales.push(l);
                }
            }
            wdbs.push((g.clone(), w));
            dbcs.push((g.clone(), d));
            luas.push((g.clone(), load_looter_files(g)));
        }
        if locales.is_empty() {
            locales = luas.iter().flat_map(|(_, l)| l.iter().map(|f| f.locale.clone())).filter(|l| !l.is_empty()).collect();
            locales.dedup();
        }
        let metadata = self.metadata(&games, &locales, cfg.upload_character_data.unwrap_or(false));
        let mut any_sent = false;
        let mut failed = false;
        for locale in &locales {
            let mut parts: Vec<(String, Vec<u8>, &'static str)> =
                vec![("metadata.json".into(), metadata.clone().into_bytes(), "application/json")];
            let mut done_wdb: Vec<(usize, usize)> = Vec::new();
            let mut done_dbc: Vec<(usize, usize)> = Vec::new();
            let mut done_lua: Vec<PathBuf> = Vec::new();
            for (gi, g) in games.iter().enumerate() {
                let version = g.version.clone().unwrap_or_default();
                let branch_name = if (13..=15).contains(&g.branch.id) && version.starts_with("3.") {
                    "WRATH"
                } else {
                    g.branch.upload
                };
                for (fi, w) in wdbs[gi].1.iter_mut().enumerate() {
                    if &w.locale != locale {
                        continue;
                    }
                    let (data, n) = pack_wdb(w);
                    if n == 0 {
                        c.info(format!("{} has no new data.", short_path(&w.path)));
                        continue;
                    }
                    let name = format!("{branch_name}_{version}_{}_{}.gz", w.locale, WDB_FILES[w.kind as usize]);
                    parts.push((name, core::gzip(&data), "application/binary"));
                    done_wdb.push((gi, fi));
                }
                for (fi, f) in dbcs[gi].1.iter_mut().enumerate() {
                    if &f.locale != locale {
                        continue;
                    }
                    let (data, n) = pack_dbcache(f);
                    if n == 0 {
                        continue;
                    }
                    let name = format!("{branch_name}_{version}_{}_DBCache.bin.gz", f.locale);
                    parts.push((name, core::gzip(&data), "application/binary"));
                    done_dbc.push((gi, fi));
                }
                for f in &luas[gi].1 {
                    if f.uploaded || (!f.locale.is_empty() && &f.locale != locale) || done_lua.contains(&f.path) {
                        continue;
                    }
                    let Ok(raw) = fs::read(&f.path) else { continue };
                    let mut text = String::from_utf8_lossy(&raw).into_owned();
                    let excluded = cfg.excluded_accounts.iter().any(|a| a == &f.account);
                    if excluded || !cfg.upload_character_data.unwrap_or(false) {
                        text = remove_top_level(&text, &CHARACTER_DATA_VARIABLES);
                    }
                    let name = format!("{branch_name}_{version}_{}_SavedVariables.lua.gz", f.account);
                    parts.push((name, core::gzip(text.as_bytes()), "application/binary"));
                    done_lua.push(f.path.clone());
                }
            }
            if parts.len() == 1 {
                c.info(format!("Nothing new for {locale}."));
                continue;
            }
            let size: usize = parts.iter().map(|p| p.1.len()).sum();
            c.set_health(Health::Busy, format!("Sending {} files ({} KB) for {locale}…", parts.len() - 1, size / 1024));
            c.info(format!("Uploading {} files ({} KB) for {locale}…", parts.len() - 1, size / 1024));
            match self.post(parts, &cfg.token) {
                Ok(()) => {
                    for (gi, fi) in done_wdb {
                        let _ = wdbs[gi].1[fi].manifest.save();
                    }
                    for (gi, fi) in done_dbc {
                        let _ = dbcs[gi].1[fi].manifest.save();
                    }
                    for p in done_lua {
                        if let Err(e) = set_uploaded(&p) {
                            c.warn(format!("Couldn't mark {} as uploaded: {e}", short_path(&p)));
                        }
                    }
                    any_sent = true;
                    c.good(format!("Upload done for {locale}."));
                }
                Err(e) => {
                    failed = true;
                    c.error(format!("Upload failed for {locale}: {e}"));
                    if e.contains("Authorization") {
                        let mut cfg = self.cfg.lock().unwrap();
                        cfg.token.clear();
                    }
                    break;
                }
            }
        }
        {
            let mut cfg = self.cfg.lock().unwrap();
            cfg.last_upload = now_unix();
        }
        self.save();
        if failed {
            c.set_health(Health::Error, "The last upload failed. See the activity below.");
        } else if any_sent {
            c.good("Upload complete! Thank you!");
            c.set_health(Health::Ok, format!("Uploaded {}", core::fmt_time(now_unix())));
            self.env.notify("Wowhead", "Upload complete! Thank you!");
        } else {
            c.info("No new data detected. Nothing to upload.");
            c.set_health(Health::Ok, "Nothing new to upload");
        }
        self.scan();
    }

    fn metadata(&self, games: &[GameFolder], locales: &[String], character_data: bool) -> String {
        let mut realms = Vec::new();
        let mut portals = Vec::new();
        if let Some(root) = self.env.wow_root() {
            let re = Regex::new(r"^set realmlist ([^\r\n\s$]+)").unwrap();
            for l in locales {
                if let Ok(t) = fs::read_to_string(root.join("Data").join(l).join("realmlist.wtf")) {
                    if let Some(c) = re.captures(&t) {
                        realms.push(c[1].to_string());
                    }
                }
            }
        }
        let re = Regex::new(r#"^set portal "(.*)""#).unwrap();
        for g in games {
            if let Ok(t) = fs::read_to_string(g.flavor.path.join("WTF").join("Config.wtf")) {
                if let Some(c) = re.captures(&t) {
                    portals.push(c[1].to_string());
                }
            }
        }
        serde_json::json!({ "portals": portals, "realms": realms, "processCharacterData": character_data }).to_string()
    }

    fn post(&self, parts: Vec<(String, Vec<u8>, &'static str)>, token: &str) -> Result<(), String> {
        let mut form = reqwest::blocking::multipart::Form::new();
        for (i, (name, data, mime)) in parts.into_iter().enumerate() {
            let part = reqwest::blocking::multipart::Part::bytes(data).file_name(name).mime_str(mime).map_err(|e| e.to_string())?;
            form = form.part(format!("file{i}"), part);
        }
        let mut req = self.http.post(URL_UPLOAD).multipart(form);
        if !token.is_empty() {
            req = req.header("X-Wowhead-Client-Token", token);
        }
        let res = req.send().map_err(|e| core::friendly_error(&e))?;
        let status = res.status();
        let body = res.text().unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let msg = parsed.get("message").and_then(|m| m.as_str()).filter(|m| !m.is_empty());
        let err = parsed.get("error").and_then(|m| m.as_str()).filter(|m| !m.is_empty());
        if status.is_success() && err.is_none() {
            return Ok(());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err("Authorization failed. Log in again.".into());
        }
        Err(err.or(msg).map(String::from).unwrap_or_else(|| format!("the site answered {status}")))
    }

    fn login(&self) {
        let c = &self.common;
        let res = (|| -> anyhow::Result<String> {
            let r = self.http.post(URL_NEW_CODE).body(Vec::new()).send()?;
            let v: serde_json::Value = r.json()?;
            v.get("code").and_then(|c| c.as_str()).filter(|c| !c.trim().is_empty()).map(String::from).ok_or_else(|| anyhow::anyhow!("no login code"))
        })();
        let code = match res {
            Ok(c) => c,
            Err(e) => {
                c.error(format!("Couldn't start the login: {}", core::friendly_error(&e)));
                return;
            }
        };
        let url = format!("{URL_LOGIN}{code}");
        *self.login.lock().unwrap() = LoginState { code: Some(code.clone()), url: Some(url.clone()) };
        c.info("Waiting for login via wowhead.com. Please check your web browser.");
        let _ = open::that(&url);
        let started = now_unix();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if self.login.lock().unwrap().code.as_deref() != Some(code.as_str()) {
                c.info("Login has been cancelled.");
                return;
            }
            if now_unix() - started > 15 * 60 {
                c.warn("Login has timed out. Please try again.");
                break;
            }
            let Ok(r) = self.http.get(format!("{URL_CODE_STATUS}{code}")).send() else { continue };
            let v: serde_json::Value = r.json().unwrap_or_default();
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            if v.get("error").and_then(|e| e.as_bool()).unwrap_or(false) {
                c.error(format!("Login failed: {}", s("message")));
                break;
            }
            if v.get("cancelled").and_then(|e| e.as_bool()).unwrap_or(false) {
                c.info("Login has been cancelled.");
                break;
            }
            if !s("user").is_empty() && !s("token").is_empty() {
                let mut cfg = self.cfg.lock().unwrap();
                cfg.user = s("user");
                cfg.token = s("token");
                drop(cfg);
                self.save();
                c.good(format!("Logged in successfully as {}.", s("user")));
                break;
            }
        }
        *self.login.lock().unwrap() = LoginState::default();
        c.repaint();
    }

    fn fetch_update(&self) -> anyhow::Result<LooterUpdate> {
        let mut data = Vec::new();
        self.http.get(URL_UPDATE).send()?.error_for_status()?.read_to_end(&mut data)?;
        parse_update(&data)
    }

    /// Installs or updates `+Wowhead_Looter` in every game folder.
    fn update_looter(&self, update: &LooterUpdate) {
        let c = &self.common;
        let mut updated = 0;
        let mut failed = 0;
        for g in self.games() {
            let mut fresh = 0;
            for (hash, name, dest) in looter_files_for(update, g.branch.id) {
                let rel = Regex::new(r"(?i)%WoW.*?%").unwrap().replace(&dest, "").replace('\\', "/");
                let dir = g.flavor.path.join(rel.trim_start_matches('/'));
                let file = dir.join(name.rsplit('/').next().unwrap_or(&name));
                if fs::read(&file).map(|d| core::md5_hex(&d) == hash).unwrap_or(false) {
                    continue;
                }
                let url = format!("{}{}", update.source, name);
                let got = self.http.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes());
                match got {
                    Ok(b) if core::md5_hex(&b) == hash => {
                        if core::write_atomic(&file, &b).is_ok() {
                            fresh += 1;
                        } else {
                            failed += 1;
                        }
                    }
                    Ok(_) => {
                        c.warn(format!("{name}: the download didn't match its checksum, skipped."));
                        failed += 1;
                    }
                    Err(e) => {
                        c.warn(format!("Couldn't download {name}: {}", core::friendly_error(&e)));
                        failed += 1;
                    }
                }
            }
            if fresh > 0 {
                c.good(format!("Updated the Wowhead Looter addon for {} ({fresh} files).", g.branch.display));
                updated += 1;
            }
        }
        let note = if failed > 0 {
            "Some Looter files couldn't be updated; it's tried again later.".to_string()
        } else if updated > 0 {
            format!("Looter addon updated {}", core::fmt_time(now_unix()))
        } else {
            format!("Looter addon is up to date (checked {})", core::fmt_time(now_unix()))
        };
        *self.looter_note.lock().unwrap() = note;
    }

    /// The in-game sync: every 2 seconds, like the Windows client's sync timer.
    fn sync_loop(self: Arc<Self>) {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let on = {
                let cfg = self.cfg.lock().unwrap();
                cfg.enabled && cfg.in_game_sync
            };
            if on {
                self.sync_tick();
            } else {
                self.sync.lock().unwrap().files.clear();
            }
        }
    }

    fn sync_tick(&self) {
        let mut st = self.sync.lock().unwrap();
        let now = Instant::now();
        if st.running_checked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10)) {
            st.running = wow::running_games();
            st.running_checked = Some(now);
        }
        if st.running.is_empty() {
            st.files.clear();
            return;
        }
        // Mark the sync first, then look for an upload: one of the two always sees the other.
        self.sync_busy.store(true, Ordering::SeqCst);
        if !self.common.is_busy() {
            if st.games_checked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60)) {
                let all = self.games();
                st.locales = all_locales(&all);
                st.games = all.into_iter().filter(|g| SYNC_BRANCHES.contains(&g.branch.id)).collect();
                st.games_checked = Some(now);
            }
            // Only the game folders whose client runs, like the Windows client: a running
            // retail client doesn't make Hearthlink look at the Forever files.
            let games: Vec<GameFolder> = st.games.iter().filter(|g| wow::runs_in(&st.running, &g.flavor)).cloned().collect();
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut stopped = false;
            'games: for g in &games {
                for (account, sv) in g.flavor.accounts() {
                    let path = sv.join(LOOTER_SV);
                    seen.insert(path.clone());
                    let Ok(meta) = fs::metadata(&path) else { continue };
                    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let s = file_state_update(st.files.remove(&path), mtime, meta.len(), Instant::now());
                    if !file_ready(&s, Instant::now()) || self.common.is_busy() {
                        // An upload started: stop this tick and keep every file's state.
                        stopped = self.common.is_busy();
                        st.files.insert(path, s);
                        if stopped {
                            break 'games;
                        }
                        continue;
                    }
                    let s = self.sync_file(&mut st, g, &account, &path, s);
                    st.files.insert(path, s);
                }
            }
            if !stopped {
                st.files.retain(|p, _| seen.contains(p));
            }
        }
        self.sync_busy.store(false, Ordering::SeqCst);
    }

    /// Looks at one account's Looter file and sends its sync request if it has a new one.
    fn sync_file(&self, st: &mut SyncState, g: &GameFolder, account: &str, path: &Path, mut s: FileState) -> FileState {
        let c = &self.common;
        let file = short_path(path);
        s.checked = true;
        s.retry_at = None;
        let (req, blocks) = match s.pending.take() {
            Some(p) => p,
            None => {
                let Some(text) = read_looter_file(path, c) else {
                    s.retry_at = Some(Instant::now() + Duration::from_secs(30));
                    return s;
                };
                let blocks = split_top_level(text.trim_start_matches('\u{feff}'));
                let Some((_, request_block)) = blocks.iter().find(|(n, _)| n == "wlSyncRequest") else { return s };
                let Some(req) = parse_sync_request(request_block) else {
                    c.warn(format!("In-game sync: wlSyncRequest has no time in {file}."));
                    return s;
                };
                if req.time <= st.handled_time(path) {
                    return s;
                }
                let blocks: Vec<(String, String)> =
                    blocks.into_iter().filter(|(n, _)| ALWAYS_SENT.contains(&n.as_str()) || req.vars.contains(n)).collect();
                (req, blocks)
            }
        };
        let token = self.cfg.lock().unwrap().token.clone();
        if token.is_empty() || st.refused_token.as_deref() == Some(token.as_str()) {
            if !s.waiting_for_login {
                c.warn(format!("In-game sync: request {} in {file} waits for a login.", req.time));
                s.waiting_for_login = true;
            }
            keep_pending(&mut s, req, blocks, 10);
            return s;
        }
        s.waiting_for_login = false;
        let Some(version) = g.version.clone().filter(|v| !v.is_empty()) else {
            s.version_checks += 1;
            if s.version_checks < 5 {
                c.warn(format!("In-game sync: no game version known for {}, can't send request {}.", g.branch.display, req.time));
                keep_pending(&mut s, req, blocks, 30);
            } else {
                c.warn(format!("In-game sync: still no game version known for {}, not retrying request {}.", g.branch.display, req.time));
                self.sync_notice(false, "Sync not sent: Hearthlink couldn't find the game's version. Check the WoW folder in Settings, then sync again.");
            }
            return s;
        };
        c.info(format!("In-game sync: sending request {} from {file}.", req.time));
        let locales = st.locales.clone();
        let (result, resp, left_out) = self.sync_send(g, &version, account, &blocks, &req, &locales);
        let req_time = req.time;
        let save_handled = |st: &mut SyncState| {
            if let Err(e) = st.set_handled(path, req_time) {
                c.warn(format!("In-game sync: couldn't save {}: {e}", sync_json_path().display()));
            }
        };
        match result {
            SyncResult::Accepted => {
                save_handled(st);
                c.info(format!("In-game sync: request {} done.{}", req.time, describe_results(resp.as_ref())));
                if left_out.is_empty() {
                    self.sync_notice(true, "Character data synced to Wowhead.");
                } else {
                    self.sync_notice(true, "Character data was not sent: this account doesn't upload character data.");
                }
            }
            SyncResult::Rejected => {
                save_handled(st);
                let msg = resp.as_ref().and_then(|r| r.message.clone()).filter(|m| !m.trim().is_empty());
                c.warn(format!("In-game sync: request {} rejected by the site: {}", req.time, msg.clone().unwrap_or_default()));
                match msg {
                    Some(m) => self.sync_notice(false, &format!("Wowhead couldn't take this sync: {m}")),
                    None => self.sync_notice(false, "Wowhead couldn't take this sync."),
                }
            }
            SyncResult::Throttled => {
                let wait = resp.as_ref().and_then(|r| r.wait).filter(|w| *w > 0).unwrap_or(60) as u64;
                c.info(format!("In-game sync: request {} throttled, retrying in {wait} seconds.", req.time));
                keep_pending(&mut s, req, blocks, wait);
            }
            SyncResult::Unauthorized => {
                st.refused_token = Some(token);
                c.warn(format!("In-game sync: login refused, request {} waits for a new login.", req.time));
                keep_pending(&mut s, req, blocks, 10);
            }
            SyncResult::Failed => {
                s.failed_sends += 1;
                if s.failed_sends <= 1 {
                    c.warn(format!("In-game sync: request {} failed, retrying in 30 seconds.", req.time));
                    keep_pending(&mut s, req, blocks, 30);
                } else {
                    c.warn(format!("In-game sync: request {} failed again, not retrying.", req.time));
                    self.sync_notice(false, "Sync failed: Hearthlink couldn't get an answer from Wowhead. Sync again later.");
                }
            }
        }
        s
    }

    fn sync_notice(&self, good: bool, text: &str) {
        if good {
            self.common.good(text);
        } else {
            self.common.warn(text);
        }
        self.env.notify("Wowhead", text);
    }

    /// Sends one sync request: metadata plus the requested Looter variables.
    fn sync_send(
        &self,
        g: &GameFolder,
        version: &str,
        account: &str,
        blocks: &[(String, String)],
        req: &SyncRequest,
        locales: &[String],
    ) -> (SyncResult, Option<SyncResponse>, Vec<String>) {
        let c = &self.common;
        let cfg = self.cfg.lock().unwrap().clone();
        let excluded = cfg.excluded_accounts.iter().any(|a| a == account);
        let with_characters = cfg.upload_character_data == Some(true) && !excluded;
        let b = build_sync_body(blocks, req, with_characters);
        for v in &b.skipped {
            c.warn(format!("In-game sync: skipping requested variable \"{v}\", not a Looter variable."));
        }
        for n in &b.missing {
            c.info(format!("In-game sync: {n} is not in the Looter file."));
        }
        if !b.left_out.is_empty() {
            let reason = if cfg.upload_character_data != Some(true) {
                "character data upload is off".to_string()
            } else {
                format!("account {account} is excluded")
            };
            c.info(format!("In-game sync: {reason}, not sending {}.", b.left_out.join(", ")));
        }
        let metadata = self.metadata(std::slice::from_ref(g), locales, with_characters);
        let data = core::gzip(b.body.as_bytes());
        let name = sync_file_name(g, version, account);
        c.info(format!("In-game sync: sending {name}, {} bytes, variables {}.", data.len(), b.sent.join(", ")));
        let (status, text) = match post_sync(&self.sync_http, URL_SYNC, &cfg.token, metadata, name, data) {
            Ok(r) => r,
            Err(e) => {
                let why = if e.is_timeout() { "timed out".to_string() } else { core::friendly_error(&e) };
                c.warn(format!("In-game sync: {why}"));
                return (SyncResult::Failed, None, b.left_out);
            }
        };
        let (result, resp) = read_sync_answer(status, &text);
        if let Some(t) = resp.as_ref().and_then(|r| r.token.clone()).filter(|t| !t.trim().is_empty()) {
            self.cfg.lock().unwrap().token = t;
            self.save();
        }
        if result == SyncResult::Failed {
            if status >= 500 {
                c.warn(format!("In-game sync: server error {status}."));
            } else {
                c.warn("In-game sync: the site's reply could not be read.");
            }
        }
        (result, resp, b.left_out)
    }

    fn worker(self: Arc<Self>) {
        let c = self.common.clone();
        if !self.cfg.lock().unwrap().imported_from_wine {
            if let Some(what) = self.import_from_wine() {
                c.good(format!("Took over the {what} from the Windows Wowhead Client in Wine."));
            }
        }
        let mut last_update_check = 0i64;
        loop {
            if !self.cfg.lock().unwrap().enabled {
                c.set_health(Health::Off, "Turned off");
                c.sleep(Duration::from_secs(30));
                continue;
            }
            if now_unix() - last_update_check > 6 * 3600 {
                last_update_check = now_unix();
                match self.fetch_update() {
                    Ok(u) => {
                        if self.cfg.lock().unwrap().keep_looter_updated {
                            if wow::game_running() {
                                // Don't swap addon files under a running game; try again in 10 min.
                                last_update_check = now_unix() - 6 * 3600 + 600;
                            } else {
                                self.update_looter(&u);
                            }
                        }
                    }
                    Err(e) => c.warn(format!("Couldn't check for Looter updates: {}", core::friendly_error(&e))),
                }
            }
            let running = wow::game_running();
            let was = std::mem::replace(&mut *self.game_was_running.lock().unwrap(), running);
            if was && !running && self.cfg.lock().unwrap().upload_on_game_exit {
                c.info("WoW was closed: uploading.");
                std::thread::sleep(Duration::from_secs(5));
                c.exclusive(|| self.upload(false));
            } else if !c.is_busy() {
                if self.pending.lock().unwrap().scanned_at < now_unix() - 600 && !running {
                    self.scan();
                }
                let token = self.cfg.lock().unwrap().token.clone();
                let health = if running {
                    (Health::Ok, "WoW is running: uploads after you close it".to_string())
                } else if token.is_empty() {
                    (Health::Attention, "Not logged in (uploads go in anonymously)".to_string())
                } else {
                    let p = self.pending.lock().unwrap().total;
                    if p > 0 {
                        (Health::Ok, format!("{p} new records ready to upload"))
                    } else {
                        (Health::Ok, "Up to date".to_string())
                    }
                };
                if c.health().0 != Health::Error {
                    c.set_health(health.0, health.1);
                }
            }
            c.sleep(Duration::from_secs(20));
        }
    }
}

fn short_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    match s.find("/_") {
        Some(i) => s[i + 1..].to_string(),
        None => s.into_owned(),
    }
}

impl Service for Wowhead {
    fn meta(&self) -> &'static Meta {
        &META
    }
    fn common(&self) -> &Arc<Common> {
        &self.inner.common
    }
    fn enabled(&self) -> bool {
        self.inner.cfg.lock().unwrap().enabled
    }
    fn set_enabled(&self, on: bool) {
        self.inner.cfg.lock().unwrap().enabled = on;
        self.inner.save();
        self.inner.common.wake();
    }

    fn start(&self) {
        let w = self.inner.clone();
        std::thread::spawn(move || w.worker());
        let s = self.inner.clone();
        std::thread::spawn(move || s.sync_loop());
        let i = self.inner.clone();
        let http = self.inner.http.clone();
        super::watch_upstream(self.inner.env.clone(), self.inner.common.clone(), &META, move || {
            let mut data = Vec::new();
            http.get(URL_UPDATE).send()?.error_for_status()?.read_to_end(&mut data)?;
            let u = parse_update(&data)?;
            let _ = &i;
            u.client_version.map(|v| (v, None)).ok_or_else(|| anyhow::anyhow!("no client version in the update file"))
        });
    }

    fn upload_now(&self) {
        let i = self.inner.clone();
        std::thread::spawn(move || {
            i.common.exclusive(|| i.upload(true));
        });
    }

    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();

        crate::ui::section(ui, "Account", |ui| {
            let login = i.login.lock().unwrap();
            if let Some(url) = login.url.clone() {
                ui.label("Waiting for you to approve the login on wowhead.com…");
                ui.horizontal(|ui| {
                    if ui.button(crate::ui::ib(crate::ui::icon::EXTERNAL, "Open the page again")).clicked() {
                        let _ = open::that(&url);
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::COPY, "Copy link")).clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::X, "Cancel")).clicked() {
                        drop(login);
                        *i.login.lock().unwrap() = LoginState::default();
                        return;
                    }
                });
            } else if cfg.token.is_empty() {
                ui.label("Not logged in. Uploads still work, but log in to get credit for them on Wowhead.");
                if crate::ui::primary(ui, true, crate::ui::icon::SIGN_IN, "Log in via Wowhead.com").clicked() {
                    let i = i.clone();
                    std::thread::spawn(move || i.login());
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(format!("Logged in as {}", cfg.user));
                    if ui.small_button(crate::ui::ib(crate::ui::icon::SIGN_OUT, "Log out")).clicked() {
                        let mut c = i.cfg.lock().unwrap();
                        c.token.clear();
                        c.user.clear();
                        drop(c);
                        i.save();
                        i.common.info("Authentication data cleared.");
                    }
                });
            }
        });

        crate::ui::section(ui, "Upload", |ui| {
            let p = i.pending.lock().unwrap().clone();
            if p.games.is_empty() {
                ui.label("No game folders found yet. Check the WoW folder in Settings.");
            }
            egui::ScrollArea::horizontal().id_salt("wh-games-scroll").show(ui, |ui| {
                egui::Grid::new("wh-games").striped(true).num_columns(4).show(ui, |ui| {
                    ui.strong("Game");
                    ui.strong("New cache records");
                    ui.strong("New hotfixes");
                    ui.strong("Looter data");
                    ui.end_row();
                    for (game, version, wdb, hot, lua) in &p.games {
                        ui.label(format!("{game}  {version}"));
                        ui.label(wdb.to_string());
                        ui.label(hot.to_string());
                        ui.label(if *lua > 0 { format!("{lua} account(s) to send") } else { "sent".into() });
                        ui.end_row();
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let btn = crate::ui::primary(ui, !busy, crate::ui::icon::UPLOAD, "Upload now");
                if btn.clicked() {
                    self.upload_now();
                }
                if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::SYNC, "Check again"))).clicked() {
                    let i = i.clone();
                    std::thread::spawn(move || {
                        i.scan();
                    });
                }
                if cfg.last_upload > 0 {
                    ui.weak(format!("Last upload {}", core::ago(cfg.last_upload)));
                }
            });
        });

        crate::ui::section(ui, "Options", |ui| {
            let mut c = cfg.clone();
            ui.checkbox(&mut c.upload_on_game_exit, "Upload automatically when I close WoW");
            ui.checkbox(&mut c.keep_looter_updated, "Keep the Wowhead Looter addon up to date (checked every 6 hours)");
            ui.checkbox(&mut c.in_game_sync, "In-game sync for WoW Forever: send what the Looter's Sync button asks for while you play");
            let mut chars = c.upload_character_data.unwrap_or(false);
            if ui
                .checkbox(&mut chars, "Also upload my character data, so I can use it on wowhead.com")
                .changed()
            {
                c.upload_character_data = Some(chars);
            }
            let accounts: Vec<String> = i
                .env
                .flavors()
                .iter()
                .flat_map(|f| f.accounts().into_iter().map(|a| a.0))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if !accounts.is_empty() {
                ui.collapsing("Accounts without character data", |ui| {
                    ui.weak("Game data from these accounts is still sent, their characters aren't.");
                    for a in accounts {
                        let mut ex = c.excluded_accounts.contains(&a);
                        if ui.checkbox(&mut ex, &a).changed() {
                            if ex {
                                c.excluded_accounts.push(a.clone());
                            } else {
                                c.excluded_accounts.retain(|x| x != &a);
                            }
                        }
                    }
                });
            }
            if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, "Update the Looter addon now"))).clicked() {
                let i = i.clone();
                std::thread::spawn(move || match i.fetch_update() {
                    Ok(u) => i.update_looter(&u),
                    Err(e) => i.common.error(format!("Update error: {}", core::friendly_error(&e))),
                });
            }
            let note = i.looter_note.lock().unwrap().clone();
            if !note.is_empty() {
                ui.weak(note);
            }
            ui.weak("Not in this version yet: reading combat logs (the Windows client's optional \"use combat log\" setting).");
            if c.upload_character_data != cfg.upload_character_data
                || c.upload_on_game_exit != cfg.upload_on_game_exit
                || c.keep_looter_updated != cfg.keep_looter_updated
                || c.in_game_sync != cfg.in_game_sync
                || c.excluded_accounts != cfg.excluded_accounts
            {
                *i.cfg.lock().unwrap() = c;
                i.save();
            }
        });
        let _ = lua::quote; // shared helpers live in core
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches() {
        assert_eq!(branch_for("_classic_beta_", Some("1.60.1.70450")), Some(21));
        assert_eq!(branch_for("_classic_", Some("5.5.1.123")), Some(16));
        assert_eq!(branch_for("_classic_", Some("4.4.2.1")), Some(13));
        assert_eq!(branch_for("_classic_", Some("3.4.3.1")), Some(9));
        assert_eq!(branch_for("_classic_era_", Some("1.15.8.1")), Some(6));
        assert_eq!(branch_for("_retail_", None), Some(0));
        for (i, b) in BRANCHES.iter().enumerate() {
            assert_eq!(b.id as usize, i);
        }
        for id in SYNC_BRANCHES {
            assert_eq!(BRANCHES[id as usize].upload, "CLASSICPLUS");
        }
    }

    const LOOTER: &str = concat!(
        "wlVersion = 110207\n",
        "wlUploaded = 0\n",
        "wlStats = \"a{b}c\\\"d -- not a comment\"\n",
        "wlExportData = \"x = 1\\nwlProfileData = 2\"\n",
        "wlProfileData = {\n",
        "\t[\"Name^Realm\"] = {\n",
        "\t\t[\"note\"] = \"} { \\\" --\",\n",
        "-- {\n",
        "wlCharacterData = \"nested, not top level\",\n",
        "\t},\n",
        "}\n",
        "  wlRealmList = \"indented, not top level\"\n",
        "wlRealmList = {\n",
        "[\"eu.logon\"] = 1,\n",
        "}\n",
        "wlCharacterData = {\n",
        "}\n",
        "wlSyncRequest = nil",
    );

    #[test]
    fn top_level_blocks() {
        let b = split_top_level(LOOTER);
        let names: Vec<&str> = b.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["wlVersion", "wlUploaded", "wlStats", "wlExportData", "wlProfileData", "wlRealmList", "wlCharacterData", "wlSyncRequest"]
        );
        let block = |n: &str| b.iter().find(|(k, _)| k == n).unwrap().1.clone();
        assert!(block("wlProfileData").starts_with("wlProfileData = {\n"));
        assert!(block("wlProfileData").contains("nested, not top level"));
        assert!(block("wlProfileData").ends_with("  wlRealmList = \"indented, not top level\"\n"));
        assert_eq!(block("wlRealmList"), "wlRealmList = {\n[\"eu.logon\"] = 1,\n}\n");
        assert_eq!(block("wlSyncRequest"), "wlSyncRequest = nil");
        // The blocks put back together are the whole file.
        assert_eq!(b.iter().map(|(_, t)| t.as_str()).collect::<String>(), LOOTER);
    }

    #[test]
    fn top_level_crlf_and_duplicates() {
        let t = "a = 1\r\nb = {\r\n  x = 2,\r\n}\r\na = 3\r\n";
        let b = split_top_level(t);
        assert_eq!(b, vec![("a".to_string(), "a = 3\r\n".to_string()), ("b".to_string(), "b = {\r\n  x = 2,\r\n}\r\n".to_string())]);
        assert!(split_top_level("").is_empty());
        assert!(split_top_level("-- only a comment").is_empty());
    }

    #[test]
    fn character_data_removed() {
        let out = remove_top_level(LOOTER, &CHARACTER_DATA_VARIABLES);
        let expected = concat!(
            "wlVersion = 110207\n",
            "wlUploaded = 0\n",
            "wlStats = \"a{b}c\\\"d -- not a comment\"\n",
            "wlRealmList = {\n",
            "[\"eu.logon\"] = 1,\n",
            "}\n",
            "wlSyncRequest = nil",
        );
        assert_eq!(out, expected);
        // Nothing to remove: unchanged.
        assert_eq!(remove_top_level("wlVersion = 1\n", &CHARACTER_DATA_VARIABLES), "wlVersion = 1\n");
    }

    #[test]
    fn sync_request() {
        let block = concat!(
            "wlSyncRequest = {\n",
            "[\"time\"] = 1762268000,\n",
            "[\"version\"] = 1,\n",
            "[\"vars\"] = {\n",
            "\"wlProfileData\", -- [1]\n",
            "\"wlCharacterData\", -- [2]\n",
            "},\n",
            "[\"announced\"] = true,\n",
            "}\n",
        );
        assert_eq!(
            parse_sync_request(block),
            Some(SyncRequest { time: 1762268000, version: 1, vars: vec!["wlProfileData".into(), "wlCharacterData".into()] })
        );
        assert_eq!(
            parse_sync_request("wlSyncRequest = {\n[\"time\"] = 5,\n}\n"),
            Some(SyncRequest { time: 5, version: 0, vars: vec![] })
        );
        assert_eq!(parse_sync_request("wlSyncRequest = {\n[\"version\"] = 1,\n}\n"), None);
        assert_eq!(parse_sync_request("wlSyncRequest = nil"), None);
        // A table body with nested tables and strings stops at its own closing brace.
        assert_eq!(table_body("{a = {\"}\"}, b} rest", 1), "a = {\"}\"}, b");
        assert_eq!(table_body("{open", 1), "open");
    }

    #[test]
    fn in_game_sync_defaults_on() {
        let old: Config = serde_json::from_str(r#"{"enabled": true, "token": "t"}"#).unwrap();
        assert!(old.in_game_sync);
        assert!(old.enabled);
        let off: Config = serde_json::from_str(r#"{"in_game_sync": false}"#).unwrap();
        assert!(!off.in_game_sync);
        assert!(Config::fresh().in_game_sync);
    }

    #[test]
    fn file_readiness() {
        let t0 = Instant::now();
        let m1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let m2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let s = file_state_update(None, m1, 10, t0);
        assert!(!file_ready(&s, t0 + Duration::from_secs(1)));
        assert!(file_ready(&s, t0 + Duration::from_secs(2)));
        // Checked and unchanged: only again when a retry is due.
        let mut s = file_state_update(Some(s), m1, 10, t0 + Duration::from_secs(5));
        s.checked = true;
        assert!(!file_ready(&s, t0 + Duration::from_secs(5)));
        s.retry_at = Some(t0 + Duration::from_secs(30));
        s.failed_sends = 1;
        assert!(!file_ready(&s, t0 + Duration::from_secs(29)));
        assert!(file_ready(&s, t0 + Duration::from_secs(30)));
        // A changed file starts over.
        let s = file_state_update(Some(s), m2, 10, t0 + Duration::from_secs(40));
        assert!(!s.checked && s.retry_at.is_none() && s.failed_sends == 0);
        assert!(!file_ready(&s, t0 + Duration::from_secs(41)));
        let s = file_state_update(Some(s), m2, 11, t0 + Duration::from_secs(50));
        assert_eq!(s.stable_since, Some(t0 + Duration::from_secs(50)));
    }

    const SYNC_LOOTER: &str = concat!(
        "wlVersion = 16001\r\n",
        "wlStats = \"1^2\"\r\n",
        "wlProfileData = {\r\n[\"Name^Realm\"] = {\r\n[\"level\"] = 60,\r\n},\r\n}\r\n",
        "wlCharacterData = {\r\n}\r\n",
        "wlRealmList = {\r\n[\"eu.logon\"] = 1,\r\n}\r\n",
        "wlSyncRequest = {\r\n[\"time\"] = 1762268000,\r\n[\"version\"] = 1,\r\n[\"vars\"] = {\r\n",
        "\"wlProfileData\", -- [1]\r\n\"wlCharacterData\", -- [2]\r\n\"wlProfileData\", -- [3]\r\n\"os.exit\", -- [4]\r\n\"wlMissing\", -- [5]\r\n",
        "},\r\n[\"announced\"] = true,\r\n}",
    );

    fn sync_parts() -> (Vec<(String, String)>, SyncRequest) {
        let blocks = split_top_level(SYNC_LOOTER);
        let req = parse_sync_request(&blocks.iter().find(|(n, _)| n == "wlSyncRequest").unwrap().1).unwrap();
        (blocks, req)
    }

    #[test]
    fn sync_body() {
        let (blocks, req) = sync_parts();
        assert_eq!(req.vars, ["wlProfileData", "wlCharacterData", "wlProfileData", "os.exit", "wlMissing"]);
        let block = |n: &str| blocks.iter().find(|(k, _)| k == n).unwrap().1.clone();

        let b = build_sync_body(&blocks, &req, true);
        assert_eq!(b.sent, ["wlVersion", "wlSyncRequest", "wlRealmList", "wlProfileData", "wlCharacterData"]);
        assert_eq!(b.skipped, ["os.exit"]);
        assert_eq!(b.missing, ["wlMissing"]);
        assert!(b.left_out.is_empty());
        // The last block has no newline; one is added. wlStats is never sent.
        let want = [block("wlVersion"), block("wlSyncRequest") + "\r\n", block("wlRealmList"), block("wlProfileData"), block("wlCharacterData")].concat();
        assert_eq!(b.body, want);
        assert!(!b.body.contains("wlStats"));

        // Character data off (or account excluded): only the always-sent variables go.
        let b = build_sync_body(&blocks, &req, false);
        assert_eq!(b.sent, ["wlVersion", "wlSyncRequest", "wlRealmList"]);
        assert_eq!(b.left_out, ["wlProfileData", "wlCharacterData"]);
        assert!(!b.body.contains("Name^Realm"));
    }

    /// One HTTP exchange on a local port: returns the URL and, once the client has sent its
    /// request, the raw request bytes. Answers with `status` and `body`.
    fn mock_server(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/client/sync", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            loop {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let Some(p) = find(&buf, b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap());
                match len {
                    Some(len) if buf.len() >= p + 4 + len => break,
                    None if head.contains("transfer-encoding: chunked") && buf.ends_with(b"0\r\n\r\n") => break,
                    _ => {}
                }
            }
            write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                .unwrap();
            buf
        });
        (url, h)
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// The multipart parts of a raw request as (headers, content).
    fn multipart(raw: &[u8]) -> (String, Vec<(String, Vec<u8>)>) {
        let p = find(raw, b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..p]).into_owned();
        let boundary = head.lines().find_map(|l| l.split_once("boundary=")).unwrap().1.trim().to_string();
        let delim = format!("--{boundary}");
        let mut body = &raw[p + 4..];
        let mut parts = Vec::new();
        while let Some(i) = find(body, delim.as_bytes()) {
            body = &body[i + delim.len()..];
            if body.starts_with(b"--") {
                break;
            }
            let Some(h) = find(body, b"\r\n\r\n") else { break };
            let headers = String::from_utf8_lossy(&body[..h]).trim().to_string();
            let rest = &body[h + 4..];
            let end = find(rest, format!("\r\n{delim}").as_bytes()).unwrap();
            parts.push((headers, rest[..end].to_vec()));
            body = &rest[end + 2..];
        }
        (head, parts)
    }

    #[test]
    fn sync_request_on_the_wire() {
        let (blocks, req) = sync_parts();
        let b = build_sync_body(&blocks, &req, false);
        let flavor = wow::Flavor { folder: "_forever_".into(), path: PathBuf::from("/nowhere/_forever_") };
        let g = GameFolder { branch: BRANCHES[19].clone(), flavor, version: Some("1.60.1.12345".into()) };
        let name = sync_file_name(&g, "1.60.1.12345", "ACCOUNT#1");
        assert_eq!(name, "CLASSICPLUS_1.60.1.12345_ACCOUNT#1_Sync.lua.gz");
        let metadata = r#"{"portals":["eu"],"realms":[],"processCharacterData":false}"#.to_string();
        let http = core::http_client(&user_agent(), 30);

        let (url, server) = mock_server(200, r#"{"error": false, "results": {"wlProfileData": "stored"}}"#);
        let (status, text) = post_sync(&http, &url, "tok123", metadata.clone(), name.clone(), core::gzip(b.body.as_bytes())).unwrap();
        let raw = server.join().unwrap();
        let (head, parts) = multipart(&raw);
        assert!(head.starts_with("POST /client/sync HTTP/1.1\r\n"), "{head}");
        let lower = head.to_lowercase();
        assert!(lower.contains("\r\nx-wowhead-client-token: tok123"), "{head}");
        assert!(lower.contains(&format!("\r\nuser-agent: {}", user_agent().to_lowercase())), "{head}");
        assert!(lower.contains("content-type: multipart/form-data; boundary="), "{head}");
        assert_eq!(parts.len(), 2);
        let (h0, d0) = &parts[0];
        assert!(h0.contains(r#"name="file0"; filename="metadata.json""#), "{h0}");
        assert!(h0.to_lowercase().contains("content-type: application/json"), "{h0}");
        assert_eq!(String::from_utf8_lossy(d0), metadata);
        let (h1, d1) = &parts[1];
        assert!(h1.contains(&format!(r#"name="file1"; filename="{name}""#)), "{h1}");
        assert!(h1.to_lowercase().contains("content-type: application/binary"), "{h1}");
        let mut unzipped = String::new();
        flate2::read::GzDecoder::new(&d1[..]).read_to_string(&mut unzipped).unwrap();
        assert_eq!(unzipped, b.body);
        // Character data off: the profile itself isn't in it (its name is, in wlSyncRequest).
        assert!(unzipped.starts_with("wlVersion = 16001\r\n") && !unzipped.contains("Name^Realm"));
        assert!(!unzipped.contains("wlProfileData = "));
        let (result, resp) = read_sync_answer(status, &text);
        assert_eq!(result, SyncResult::Accepted);
        assert_eq!(describe_results(resp.as_ref()), " wlProfileData: stored.");

        // Each kind of answer, through a real exchange.
        for (status, body, want) in [
            (200, r#"{"error": true, "message": "not accepted"}"#, SyncResult::Rejected),
            (200, r#"{"wait": 45}"#, SyncResult::Throttled),
            (429, "", SyncResult::Throttled),
            (401, r#"{"error": true}"#, SyncResult::Unauthorized),
            (503, "<html>busy</html>", SyncResult::Failed),
            (200, "<html>not json</html>", SyncResult::Failed),
            (404, "", SyncResult::Rejected),
        ] {
            let (url, server) = mock_server(status, body);
            let (s, text) = post_sync(&http, &url, "tok123", metadata.clone(), name.clone(), core::gzip(b"x")).unwrap();
            server.join().unwrap();
            assert_eq!((s, read_sync_answer(s, &text).0), (status, want), "{status} {body}");
        }
    }

    #[test]
    fn locales_of_all_games() {
        let dir = std::env::temp_dir().join(format!("hearthlink-locales-{}", std::process::id()));
        let mk = |p: &str| fs::create_dir_all(dir.join(p)).unwrap();
        mk("_retail_/Cache/WDB/enUS");
        mk("_forever_/Cache/WDB/deDE");
        mk("_forever_/Cache/ADB/enUS");
        fs::write(dir.join("_forever_/Cache/ADB/enUS/DBCache.bin"), b"").unwrap();
        mk("_forever_/Cache/ADB/frFR");
        let g = |folder: &str, id: usize| GameFolder {
            branch: BRANCHES[id].clone(),
            flavor: wow::Flavor { folder: folder.into(), path: dir.join(folder) },
            version: None,
        };
        let locales = all_locales(&[g("_retail_", 0), g("_forever_", 19)]);
        fs::remove_dir_all(&dir).unwrap();
        // frFR has no DBCache.bin, so it doesn't count.
        assert_eq!(locales, ["deDE", "enUS"]);
    }

    /// `HEARTHLINK_LOOTER_FILES=a.lua:b.lua cargo test real_looter_files -- --ignored`
    #[test]
    #[ignore]
    fn real_looter_files() {
        let files = std::env::var("HEARTHLINK_LOOTER_FILES").unwrap_or_default();
        for f in files.split(':').filter(|f| !f.is_empty()) {
            let text = String::from_utf8_lossy(&fs::read(f).unwrap()).into_owned();
            let b = split_top_level(&text);
            // Everything after a possible blank start belongs to a block.
            let joined: String = b.iter().map(|(_, t)| t.as_str()).collect();
            assert!(text.ends_with(&joined) && text[..text.len() - joined.len()].trim().is_empty(), "{f}");
            let names: Vec<&str> = b.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.iter().all(|n| n.starts_with("wl")), "{f}: {names:?}");
            let stripped = remove_top_level(&text, &CHARACTER_DATA_VARIABLES);
            let left: Vec<String> = split_top_level(&stripped).into_iter().map(|(n, _)| n).collect();
            let want: Vec<&str> = names.iter().copied().filter(|n| !CHARACTER_DATA_VARIABLES.contains(n)).collect();
            assert_eq!(left, want, "{f}");
            println!("{f}: {} bytes, {names:?}, {} bytes without character data", text.len(), stripped.len());

            // The same file after a Sync press: the request the Looter (16001) writes, at the
            // end, the way the game saves it. Only an in-memory copy; the file isn't touched.
            let mut synced = text.trim_end().to_string();
            synced.push_str(concat!(
                "\nwlSyncRequest = {\n[\"time\"] = 1762268000,\n[\"version\"] = 1,\n",
                "[\"vars\"] = {\n\"wlProfileData\", -- [1]\n\"wlCharacterData\", -- [2]\n},\n}\n",
            ));
            let blocks = split_top_level(&synced);
            let req = parse_sync_request(&blocks.iter().find(|(n, _)| n == "wlSyncRequest").unwrap().1).unwrap();
            assert_eq!(req.time, 1762268000);
            for with_characters in [false, true] {
                let b = build_sync_body(&blocks, &req, with_characters);
                let again: Vec<String> = split_top_level(&b.body).into_iter().map(|(n, _)| n).collect();
                assert_eq!(again, b.sent, "{f}");
                assert!(b.sent.iter().all(|n| ALWAYS_SENT.contains(&n.as_str()) || req.vars.contains(n)), "{f}");
                if !with_characters {
                    assert!(b.sent.iter().all(|n| !CHARACTER_DATA_VARIABLES.contains(&n.as_str())), "{f}");
                }
                println!("  sync with_characters={with_characters}: sends {:?}, {} bytes, leaves out {:?}", b.sent, b.body.len(), b.left_out);
            }
        }
    }

    #[test]
    fn sync_answers() {
        let r = |j: &str| serde_json::from_str::<SyncResponse>(j).unwrap();
        assert_eq!(classify_sync(401, None), SyncResult::Unauthorized);
        assert_eq!(classify_sync(429, None), SyncResult::Throttled);
        assert_eq!(classify_sync(200, Some(&r(r#"{"wait": 30}"#))), SyncResult::Throttled);
        assert_eq!(classify_sync(200, Some(&r(r#"{"error": false}"#))), SyncResult::Accepted);
        assert_eq!(classify_sync(200, Some(&r("{}"))), SyncResult::Accepted);
        assert_eq!(classify_sync(200, Some(&r(r#"{"error": true, "message": "no"}"#))), SyncResult::Rejected);
        assert_eq!(classify_sync(200, None), SyncResult::Failed);
        assert_eq!(classify_sync(503, None), SyncResult::Failed);
        assert_eq!(classify_sync(400, None), SyncResult::Rejected);
        assert_eq!(classify_sync(400, Some(&r(r#"{"message": null, "token": null}"#))), SyncResult::Rejected);
        assert_eq!(describe_results(Some(&r(r#"{"results": {"wlProfileData": "stored", "wlX": 2}}"#))), " wlProfileData: stored. wlX: 2.");
        assert_eq!(describe_results(Some(&r(r#"{"message": "ok"}"#))), " ok");
        assert_eq!(describe_results(None), "");
    }
}

/// `hearthlink --wowhead-dry-run`: shows what would be uploaded and which Looter files
/// would change, without sending or writing anything (besides importing Wine settings).
pub fn dry_run(env: Arc<Env>) {
    let w = Wowhead::new(env);
    let i = &w.inner;
    if !i.cfg.lock().unwrap().imported_from_wine {
        println!("import from Wine: {:?}", i.import_from_wine());
    }
    let running = wow::running_games();
    println!("running WoW clients: {:?}", running);
    let games = i.games();
    println!("locales for realm lists: {:?}", all_locales(&games));
    for g in games {
        println!("{} ({}) {:?} -> {}", g.branch.display, g.flavor.folder, g.version, g.branch.short);
        for mut f in load_wdbs(&g) {
            let (data, n) = pack_wdb(&mut f);
            println!("  wdb {} {}: {} records, {} new, {} bytes to send", f.locale, WDB_FILES[f.kind as usize], f.records.len(), n, data.len());
        }
        for mut f in load_dbcaches(&g) {
            let (data, n) = pack_dbcache(&mut f);
            println!("  hotfix {}: {} entries, {} new, {} bytes", f.locale, f.entries.len(), n, data.len());
        }
        for f in load_looter_files(&g) {
            println!("  looter {} uploaded={} locale={} stats={:?}", f.account, f.uploaded, f.locale, f.stats);
        }
        if SYNC_BRANCHES.contains(&g.branch.id) {
            let handled = load_handled();
            for (account, sv) in g.flavor.accounts() {
                let path = sv.join(LOOTER_SV);
                let Ok(d) = fs::read(&path) else { continue };
                let text = String::from_utf8_lossy(&d);
                let blocks = split_top_level(text.trim_start_matches('\u{feff}'));
                match blocks.iter().find(|(n, _)| n == "wlSyncRequest").and_then(|(_, b)| parse_sync_request(b)) {
                    Some(r) => {
                        let h = handled.get(&path_key(&path)).copied().unwrap_or(0);
                        let what = match (r.time > h, wow::runs_in(&running, &g.flavor)) {
                            (false, _) => "already handled",
                            (true, true) => "would be sent now (this game runs)",
                            (true, false) => "would be sent while this game runs",
                        };
                        println!("  sync {account}: request {} (version {}) vars {:?}, last handled {h}: {what}", r.time, r.version, r.vars);
                    }
                    None => println!("  sync {account}: no sync request"),
                }
            }
        }
    }
    match i.fetch_update() {
        Ok(u) => {
            println!("upstream client version: {:?}", u.client_version);
            for g in i.games() {
                for (hash, name, dest) in looter_files_for(&u, g.branch.id) {
                    let rel = Regex::new(r"(?i)%WoW.*?%").unwrap().replace(&dest, "").replace('\\', "/");
                    let file = g.flavor.path.join(rel.trim_start_matches('/')).join(name.rsplit('/').next().unwrap_or(&name));
                    let same = fs::read(&file).map(|d| core::md5_hex(&d) == hash).unwrap_or(false);
                    if !same {
                        println!("  {} would update {}", g.branch.display, short_path(&file));
                    }
                }
            }
        }
        Err(e) => println!("update check failed: {e}"),
    }
}
