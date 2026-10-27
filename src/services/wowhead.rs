//! Wowhead: does what the Windows "Wowhead Client" 2.0.9771.6501 does.
//!
//! - logs in with a code the user approves on wowhead.com,
//! - uploads the game's WDB caches, the hotfix cache (DBCache.bin) and the
//!   `+Wowhead_Looter` SavedVariables, only what's new since the last upload,
//! - keeps the `+Wowhead_Looter` addon up to date in every game folder.

use super::{Meta, Service};
use crate::core::{self, lua, now_unix, store, wine, wow, Common, Env, Health};
use eframe::egui;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static META: Meta = Meta {
    id: "wowhead",
    name: "Wowhead",
    tagline: "Game data for wowhead.com, and the Wowhead Looter addon",
    icon: include_bytes!("../../assets/icons/wowhead.png"),
    website: "https://www.wowhead.com/client",
    upstream_app: "Wowhead Client",
    ported_version: "2.0.9771.6501",
    upstream_url: "https://www.wowhead.com/client",
    runs_official: false,
};

const URL_UPDATE: &str = "https://www.wowhead.com/client/update";
const URL_UPLOAD: &str = "https://www.wowhead.com/client/upload";
const URL_NEW_CODE: &str = "https://www.wowhead.com/client/new-login-code";
const URL_LOGIN: &str = "https://www.wowhead.com/client/login/";
const URL_CODE_STATUS: &str = "https://www.wowhead.com/client/login-code-status/";
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
                        text = Regex::new(r"(?i)wlExportData = .*\r?\n").unwrap().replace_all(&text, "").into_owned();
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
    for g in i.games() {
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
