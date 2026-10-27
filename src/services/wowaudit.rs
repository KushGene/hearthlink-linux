//! wowaudit: what the wowaudit desktop client 1.0.13 does.
//!
//! - **Wishlists**: with a team's API key it fetches the team's wishlists, bonus roll
//!   targets and trinket priorities (`/v1/rclootcouncil/combined`) and writes them where the
//!   RCLootCouncil_wowaudit module (`Data/db.lua`) and SimpleLootCouncil
//!   (`SLC_WoWAuditSimData.lua`) read them. The site sends the values as Lua text already.
//! - **Loot history**: RCLootCouncil's awards of the last three months go to the team
//!   (`/v1/rclootcouncil`).
//! - **Companion**: with the user signed in (or a team key), the WowauditCompanion addon's
//!   SavedVariables go to `/v1/game_data`, and the addon learns whether this user is a
//!   team admin (`Data/admin.lua`).
//!
//! Reverse engineered from the client (Tauri/Rust). Not ported: sending SimpleLootCouncil's
//! own loot history (its file layout couldn't be confirmed without a sample).

use super::{Meta, Service};
use crate::core::lua::{self, Lua, Table};
use crate::core::oauth::{self, url_encode};
use crate::core::{self, store, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static META: Meta = Meta {
    id: "wowaudit",
    name: "wowaudit",
    tagline: "Wishlists, loot history and companion data for wowaudit.com",
    icon: include_bytes!("../../assets/icons/wowaudit.png"),
    website: "https://wowaudit.com/desktop",
    upstream_app: "wowaudit desktop client",
    ported_version: "1.0.13",
    upstream_url: "https://wowaudit.com/desktop",
    runs_official: false,
};

const BASE: &str = "https://wowaudit.com";
const RCLC_MODULE: &str = "RCLootCouncil_wowaudit";
const SLC_ADDONS: [&str; 2] = ["SimpleLootCouncil", "EllesmereUISimpleLootCouncil"];
const COMPANION: &str = "WowauditCompanion";
const COMPANION_GLOBALS: [&str; 2] = ["WowauditDataSyncDB", "WowauditForeverSyncDB"];
/// The client sends awards of the last three months only.
const LOOT_MAX_AGE: i64 = 90 * 86400;
const LOOT_BATCH: usize = 500;
const WISHLIST_POLL: i64 = 300;
const MIN_RETRY: i64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Team {
    id: Value,
    name: String,
    guild_name: String,
    url: String,
    api_key: String,
    valid: bool,
    last_error: String,
}

impl Team {
    fn label(&self) -> String {
        match (self.name.is_empty(), self.guild_name.is_empty()) {
            (false, false) => format!("{} ({})", self.name, self.guild_name),
            (false, true) => self.name.clone(),
            (true, false) => self.guild_name.clone(),
            (true, true) => "Team".into(),
        }
    }
    fn key_id(&self) -> String {
        match &self.id {
            Value::Null => core::sha256_hex(self.api_key.as_bytes())[..12].to_string(),
            v => v.to_string().trim_matches('"').to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    teams: Vec<Team>,
    /// Sign-in for the companion (`/v1/desktop/token`).
    token: String,
    battletag: String,
    wishlists: bool,
    loot_history: bool,
    companion: bool,
    /// Team -> award id -> hash of what was sent.
    sent_loot: BTreeMap<String, BTreeMap<String, String>>,
    /// Companion SavedVariables file -> mtime last sent.
    sent_companion: BTreeMap<String, i64>,
    last_wishlist_update: String,
    last_sync: i64,
    guild_amount: Option<i64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            teams: Vec::new(),
            token: String::new(),
            battletag: String::new(),
            wishlists: true,
            loot_history: true,
            companion: true,
            sent_loot: BTreeMap::new(),
            sent_companion: BTreeMap::new(),
            last_wishlist_update: String::new(),
            last_sync: 0,
            guild_amount: None,
        }
    }
}

#[derive(Default)]
struct State {
    next_wishlists: i64,
    signing_in: Option<String>,
    key_input: String,
    adding_team: bool,
    /// File -> mtime last looked at, so unchanged files aren't parsed again.
    seen_loot: BTreeMap<PathBuf, i64>,
    wishlist_status: String,
    loot_status: String,
    companion_status: String,
}

pub struct WowAudit {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    st: Mutex<State>,
    http: reqwest::blocking::Client,
}

impl WowAudit {
    pub fn new(env: Arc<Env>) -> Self {
        let cfg: Config = store::load("wowaudit");
        let ua = format!("wowaudit/{} (Linux; hearthlink {})", META.ported_version, env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                env,
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                st: Mutex::new(State::default()),
                http: core::http_client(&ua, 60),
            }),
        }
    }
}

#[derive(Debug)]
enum ApiError {
    /// 401: the key or the sign-in isn't valid any more.
    Unauthorized,
    Status(u16, String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "not authorized"),
            ApiError::Status(c, b) if b.is_empty() => write!(f, "wowaudit returned {c}"),
            ApiError::Status(c, b) => write!(f, "wowaudit returned {c}: {b}"),
            ApiError::Other(e) => write!(f, "Could not reach wowaudit ({})", core::friendly_error(e)),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Other(e.into())
    }
}

fn url(path: &str) -> String {
    format!("{BASE}/v1/{path}")
}

impl Inner {
    fn save(&self) {
        store::save("wowaudit", &*self.cfg.lock().unwrap());
    }

    /// Sends a request the way the client does: bearer token plus its version header.
    fn call(&self, req: reqwest::blocking::RequestBuilder, bearer: &str) -> Result<Value, ApiError> {
        let resp = req.bearer_auth(bearer).header("APP_VERSION", META.ported_version).send()?;
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        match status {
            401 => Err(ApiError::Unauthorized),
            s if s >= 400 => {
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| ["error", "message"].iter().find_map(|k| v[*k].as_str().map(String::from)))
                    .unwrap_or_else(|| body.chars().take(300).collect());
                Err(ApiError::Status(s, msg))
            }
            _ if body.trim().is_empty() => Ok(Value::Null),
            _ => serde_json::from_str(&body).map_err(|e| ApiError::Other(anyhow::anyhow!("unreadable answer: {e}"))),
        }
    }

    fn valid_teams(&self) -> Vec<Team> {
        self.cfg.lock().unwrap().teams.iter().filter(|t| t.valid).cloned().collect()
    }

    fn mark_team(&self, key: &str, valid: bool, error: &str) {
        let mut c = self.cfg.lock().unwrap();
        if let Some(t) = c.teams.iter_mut().find(|t| t.api_key == key) {
            t.valid = valid;
            t.last_error = error.to_string();
        }
        drop(c);
        self.save();
    }

    // ---- teams and sign-in -----------------------------------------------------------

    /// Adds a team by its API key (Team settings on wowaudit.com): `/v1/team` names it.
    fn add_team(&self, key: String) {
        let c = &self.common;
        let key = key.trim().to_string();
        if self.cfg.lock().unwrap().teams.iter().any(|t| t.api_key == key) {
            return c.info("That team is already added.");
        }
        match self.call(self.http.get(url("team")), &key) {
            Ok(v) => {
                let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
                let team = Team {
                    id: v["id"].clone(),
                    name: s("name"),
                    guild_name: s("guild_name"),
                    url: s("url"),
                    api_key: key,
                    valid: true,
                    last_error: String::new(),
                };
                c.good(format!("Added team {}.", team.label()));
                self.cfg.lock().unwrap().teams.push(team);
                self.save();
                self.st.lock().unwrap().next_wishlists = 0;
                c.wake();
            }
            Err(ApiError::Unauthorized) => c.error("Failed to fetch info. Your API key is invalid."),
            Err(e) => c.error(format!("Failed to fetch info. {e}")),
        }
    }

    fn sign_in(&self) {
        let c = &self.common;
        let pkce = oauth::Pkce::new();
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => return c.error(format!("Could not start the login listener ({e})")),
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let link = format!(
            "{BASE}/desktop/authorize?code_challenge={}&state={}&redirect_uri={}",
            url_encode(&pkce.challenge),
            url_encode(&pkce.state),
            url_encode(&redirect)
        );
        self.st.lock().unwrap().signing_in = Some(link.clone());
        c.repaint();
        c.info("Opening your browser to log in to wowaudit…");
        if let Err(e) = open::that(&link) {
            c.warn(format!("Could not open the browser ({e}). Use \"Copy link\" and open it yourself."));
        }
        let cancelled = || self.st.lock().unwrap().signing_in.is_none();
        let result_page = |ok: bool| format!("{BASE}/desktop/authorize?result={}", if ok { "success" } else { "error" });
        let r = oauth::wait_for_callback(&listener, &pkce.state, &cancelled, oauth::Finish::Redirect(&result_page));
        self.st.lock().unwrap().signing_in = None;
        let code = match r {
            Ok(Some(code)) => code,
            Ok(None) => return c.info("Login was cancelled."),
            Err(e) => return c.error(format!("Login failed ({e})")),
        };
        let body = json!({
            "code": code,
            "code_verifier": pkce.verifier,
            "redirect_uri": redirect,
            "name": oauth::device_name(),
        });
        let r = self
            .http
            .post(url("desktop/token"))
            .header("APP_VERSION", META.ported_version)
            .json(&body)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<Value>());
        match r {
            Ok(v) if v["access_token"].as_str().is_some_and(|t| !t.is_empty()) => {
                let mut cfg = self.cfg.lock().unwrap();
                cfg.token = v["access_token"].as_str().unwrap_or_default().into();
                cfg.battletag = v["battletag"].as_str().unwrap_or_default().into();
                let who = cfg.battletag.clone();
                drop(cfg);
                self.save();
                c.good(format!("Logged in as {who}."));
                c.wake();
            }
            Ok(_) => c.error("Login failed (no token in the answer)."),
            Err(e) => c.error(format!("Login failed ({})", core::friendly_error(&e))),
        }
    }

    /// Checks the sign-in (`/v1/me`), like the client does at start.
    fn check_login(&self) -> Result<(), ApiError> {
        let token = self.cfg.lock().unwrap().token.clone();
        if token.is_empty() {
            return Ok(());
        }
        match self.call(self.http.get(url("me")), &token) {
            Ok(v) => {
                if let Some(b) = v["battletag"].as_str().filter(|b| !b.is_empty()) {
                    let mut c = self.cfg.lock().unwrap();
                    if c.battletag != b {
                        c.battletag = b.to_string();
                        drop(c);
                        self.save();
                    }
                }
                Ok(())
            }
            Err(ApiError::Unauthorized) => {
                self.cfg.lock().unwrap().token.clear();
                self.save();
                self.common.warn("Your login has expired or was revoked. Please log in again.");
                self.env.notify("wowaudit", "Your login has expired or was revoked. Please log in again.");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // ---- wishlists --------------------------------------------------------------------

    fn sync_wishlists(&self) -> Result<(), ApiError> {
        let teams = self.valid_teams();
        let (rclc_dirs, slc_dirs) = self.loot_addon_dirs();
        if teams.is_empty() {
            return Ok(());
        }
        if rclc_dirs.is_empty() && slc_dirs.is_empty() {
            self.st.lock().unwrap().wishlist_status =
                "No supported loot addon (RCLootCouncil_wowaudit or SimpleLootCouncil) is installed".into();
            return Ok(());
        }
        let keys: Vec<&str> = teams.iter().map(|t| t.api_key.as_str()).collect();
        let v = self.call(self.http.post(url("rclootcouncil/combined")).json(&json!({ "api_keys": keys })), keys[0])?;
        // One status per key sent, in the same order.
        for (key, s) in keys.iter().zip(v["keys"].as_array().into_iter().flatten()) {
            if s["valid"].as_bool() == Some(false) {
                self.mark_team(key, false, "API key is no longer valid. Please remove the team and add it back.");
                self.common.warn("A team's API key is no longer valid. Please remove the team and add it back.");
            }
        }
        if v["team_id"].is_null() {
            self.st.lock().unwrap().wishlist_status = "No valid API keys available".into();
            return Ok(());
        }
        let fields = [
            ("wowauditTimestamp", "updated_timestamp"),
            ("teamID", "team_id"),
            ("difficulties", "difficulties"),
            ("wishlistData", "wishlist_data"),
            ("bonusRollTargets", "bonus_roll_targets"),
            ("trinketPriorities", "trinket_priorities"),
        ];
        let pairs: Vec<(&str, String)> = fields.iter().map(|(name, key)| (*name, lua_text(&v[*key]))).collect();
        let db: String = pairs.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        let slc = format!(
            "SLC_WoWAuditSimData = {{\n{}}}\n",
            pairs.iter().map(|(k, v)| format!("    {k} = {v},\n")).collect::<String>()
        );
        let mut wrote = 0;
        for dir in &rclc_dirs {
            wrote += write_if_changed(&dir.join("Data").join("db.lua"), &db).map_err(|e| ApiError::Other(e.into()))? as usize;
        }
        for dir in &slc_dirs {
            wrote += write_if_changed(&dir.join("SLC_WoWAuditSimData.lua"), &slc).map_err(|e| ApiError::Other(e.into()))? as usize;
        }
        let updated = v["updated_timestamp"].to_string();
        let mut c = self.cfg.lock().unwrap();
        c.last_wishlist_update = updated;
        c.last_sync = core::now_unix();
        drop(c);
        self.save();
        if wrote > 0 {
            self.common.good("Wrote the team's wishlists into the game. /reload in game to see them.");
        }
        self.st.lock().unwrap().wishlist_status = format!(
            "{} wishes for {} characters · checked {}",
            v["wish_amount"].as_i64().unwrap_or(0),
            v["character_amount"].as_i64().unwrap_or(0),
            core::fmt_time(core::now_unix())
        );
        Ok(())
    }

    /// Folders of RCLootCouncil_wowaudit and of SimpleLootCouncil (either build) in every
    /// game folder.
    fn loot_addon_dirs(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut rclc = Vec::new();
        let mut slc = Vec::new();
        for f in self.env.flavors() {
            let a = f.addons_dir();
            if a.join(RCLC_MODULE).is_dir() {
                rclc.push(a.join(RCLC_MODULE));
            }
            slc.extend(SLC_ADDONS.iter().map(|n| a.join(n)).filter(|p| p.is_dir()));
        }
        (rclc, slc)
    }

    // ---- loot history -----------------------------------------------------------------

    fn sync_loot(&self, force: bool) -> Result<(), ApiError> {
        let teams = self.valid_teams();
        if teams.is_empty() {
            return Ok(());
        }
        let cfg = self.cfg.lock().unwrap().clone();
        let mut entries: BTreeMap<String, Value> = BTreeMap::new();
        let mut changed = force;
        // Recorded only once everything was sent, so a failed upload is tried again.
        let mut seen = BTreeMap::new();
        for f in self.env.flavors() {
            if !f.addons_dir().join(RCLC_MODULE).is_dir() {
                continue;
            }
            for (_, sv) in f.accounts() {
                let file = sv.join("RCLootCouncil.lua");
                let t = mtime(&file);
                if t == 0 {
                    continue;
                }
                changed |= self.st.lock().unwrap().seen_loot.get(&file) != Some(&t);
                seen.insert(file.clone(), t);
                match std::fs::read(&file).map_err(anyhow::Error::from).and_then(|b| loot_entries(&b, core::now_unix())) {
                    Ok(list) => {
                        for e in list {
                            let id = e["rclootcouncil_id"].as_str().unwrap_or_default().to_string();
                            entries.entry(id).or_insert(e);
                        }
                    }
                    Err(e) => self.common.warn(format!("Couldn't read {}: {e}", file.display())),
                }
            }
        }
        if !changed {
            return Ok(());
        }
        let mut sent_total = 0;
        for team in &teams {
            let tid = team.key_id();
            let sent = cfg.sent_loot.get(&tid).cloned().unwrap_or_default();
            let new: Vec<(&String, &Value, String)> = entries
                .iter()
                .map(|(id, e)| (id, e, core::sha256_hex(e.to_string().as_bytes())[..12].to_string()))
                .filter(|(id, _, h)| sent.get(*id) != Some(h))
                .collect();
            for batch in new.chunks(LOOT_BATCH) {
                let body = Value::Array(batch.iter().map(|(_, e, _)| (*e).clone()).collect());
                match self.call(self.http.post(url("rclootcouncil")).json(&body), &team.api_key) {
                    Ok(_) => {
                        let mut c = self.cfg.lock().unwrap();
                        let m = c.sent_loot.entry(tid.clone()).or_default();
                        for (id, _, h) in batch {
                            m.insert((*id).clone(), h.clone());
                        }
                        drop(c);
                        self.save();
                        sent_total += batch.len();
                    }
                    Err(ApiError::Unauthorized) => {
                        self.mark_team(&team.api_key, false, "API key is no longer valid. Please remove the team and add it back.");
                        self.common.warn(format!("{}: API key is no longer valid. Please remove the team and add it back.", team.label()));
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        self.st.lock().unwrap().seen_loot = seen;
        if sent_total > 0 {
            self.common.good(format!("Sent {sent_total} RCLootCouncil award(s) to wowaudit."));
        }
        self.st.lock().unwrap().loot_status = format!(
            "{} award(s) from the last 3 months, {}",
            entries.len(),
            if sent_total > 0 { format!("{sent_total} sent {}", core::fmt_time(core::now_unix())) } else { "nothing new".into() }
        );
        Ok(())
    }

    // ---- companion --------------------------------------------------------------------

    fn sync_companion(&self, force: bool) -> Result<(), ApiError> {
        let cfg = self.cfg.lock().unwrap().clone();
        let admin = cfg.teams.iter().any(|t| t.valid);
        let bearer = if !cfg.token.is_empty() {
            cfg.token.clone()
        } else if let Some(t) = cfg.teams.iter().find(|t| t.valid) {
            t.api_key.clone()
        } else {
            self.st.lock().unwrap().companion_status = "No valid team or login configured".into();
            return Ok(());
        };
        let mut installed = false;
        for f in self.env.flavors() {
            let addon = f.addons_dir().join(COMPANION);
            if !addon.is_dir() {
                continue;
            }
            installed = true;
            let flag = format!("WowauditCompanionAdmin = {admin}\n");
            if let Err(e) = write_if_changed(&addon.join("Data").join("admin.lua"), &flag) {
                self.common.warn(format!("Couldn't write the companion's admin flag: {e}"));
            }
            for (account, sv) in f.accounts() {
                let file = sv.join(format!("{COMPANION}.lua"));
                let t = mtime(&file);
                let key = file.to_string_lossy().into_owned();
                if t == 0 || (!force && cfg.sent_companion.get(&key) == Some(&t)) {
                    continue;
                }
                let src = match std::fs::read(&file) {
                    Ok(s) => s,
                    Err(e) => {
                        self.common.warn(format!("{account}: couldn't read {COMPANION}.lua: {e}"));
                        continue;
                    }
                };
                let (globals, err) = lua::read_globals(&src);
                if let Some(e) = err {
                    self.common.warn(format!("{account}: {COMPANION}.lua is damaged ({e}); sent what could be read."));
                }
                let body: Vec<Value> = COMPANION_GLOBALS
                    .iter()
                    .map(|g| globals.iter().rev().find(|(n, _)| n == g).map_or(Value::Null, |(_, v)| lua::to_json(v)))
                    .collect();
                if body.iter().all(Value::is_null) {
                    continue;
                }
                let v = self.call(self.http.post(url("game_data")).json(&body), &bearer);
                match v {
                    Ok(v) => {
                        let mut c = self.cfg.lock().unwrap();
                        c.sent_companion.insert(key, t);
                        c.guild_amount = v["guild_amount"].as_i64().or(c.guild_amount);
                        c.last_sync = core::now_unix();
                        drop(c);
                        self.save();
                        self.common.good(format!("{account}: sent companion data to wowaudit."));
                    }
                    Err(ApiError::Unauthorized) if !cfg.token.is_empty() => {
                        self.cfg.lock().unwrap().token.clear();
                        self.save();
                        self.common.warn("Your login has expired or was revoked. Please log in again.");
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        self.st.lock().unwrap().companion_status = if installed {
            match self.cfg.lock().unwrap().guild_amount {
                Some(n) => format!("Synced · {n} guild(s)"),
                None => "Waiting for companion data from the game".into(),
            }
        } else {
            "WowauditCompanion is not installed".into()
        };
        Ok(())
    }

    // ---- worker -----------------------------------------------------------------------

    fn tick(&self, force: bool) {
        let cfg = self.cfg.lock().unwrap().clone();
        if cfg.teams.is_empty() && cfg.token.is_empty() {
            self.common.set_health(Health::Attention, "Add a team or log in");
            return;
        }
        let now = core::now_unix();
        let run = || -> Result<(), ApiError> {
            let due = force || now >= self.st.lock().unwrap().next_wishlists;
            if due {
                self.check_login()?;
                if cfg.wishlists {
                    self.sync_wishlists()?;
                }
                self.st.lock().unwrap().next_wishlists = now + WISHLIST_POLL;
            }
            if cfg.loot_history {
                self.sync_loot(force)?;
            }
            if cfg.companion {
                self.sync_companion(force)?;
            }
            Ok(())
        };
        match run() {
            Ok(()) => {
                let c = self.cfg.lock().unwrap();
                let who = if c.battletag.is_empty() { "Not logged in".to_string() } else { c.battletag.clone() };
                let teams = c.teams.iter().filter(|t| t.valid).count();
                let text = format!("{who} · {teams} team(s)");
                drop(c);
                self.common.set_health(Health::Ok, text);
            }
            Err(e) => {
                self.common.warn(format!("{e}. It'll try again shortly."));
                self.common.set_health(Health::Error, e.to_string());
                let mut st = self.st.lock().unwrap();
                st.next_wishlists = st.next_wishlists.max(now + MIN_RETRY);
            }
        }
    }
}

impl Service for WowAudit {
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
        let http = self.inner.http.clone();
        super::watch_upstream(self.inner.env.clone(), self.inner.common.clone(), &META, move || {
            // The Tauri updater endpoint answers with the newest release for an old version.
            let v: serde_json::Value = http
                .get("https://wowaudit.com/updates/desktop_client.json?version=0.0.1&target=windows&arch=x86_64")
                .send()?
                .error_for_status()?
                .json()?;
            let ver = v["version"].as_str().ok_or_else(|| anyhow::anyhow!("no version"))?.to_string();
            Ok((ver, v["notes"].as_str().map(String::from)))
        });
        let i = self.inner.clone();
        std::thread::spawn(move || loop {
            if i.cfg.lock().unwrap().enabled {
                i.common.exclusive(|| i.tick(false));
            } else {
                i.common.set_health(Health::Off, "Turned off");
            }
            i.common.sleep(Duration::from_secs(20));
        });
    }
    fn upload_now(&self) {
        let i = self.inner.clone();
        std::thread::spawn(move || {
            i.common.exclusive(|| i.tick(true));
        });
    }
    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();

        crate::ui::section(ui, "Teams", |ui| {
            ui.weak("A team's API key is under Team settings → API on wowaudit.com. It brings the team's wishlists into the game and takes your loot history.");
            let mut remove = None;
            for (n, t) in cfg.teams.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    if t.valid {
                        ui.label(crate::ui::ib(crate::ui::icon::CHECK, &t.label()));
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(0xE5, 0x6B, 0x6B), crate::ui::ib(crate::ui::icon::WARNING, &t.label()));
                        if !t.last_error.is_empty() {
                            ui.weak(&t.last_error);
                        }
                    }
                    if !t.url.is_empty() {
                        ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "Open"), &t.url);
                    }
                    if ui.small_button(crate::ui::ib(crate::ui::icon::TRASH, "Remove")).clicked() {
                        remove = Some(n);
                    }
                });
            }
            if let Some(n) = remove {
                let t = i.cfg.lock().unwrap().teams.remove(n);
                i.cfg.lock().unwrap().sent_loot.remove(&t.key_id());
                i.save();
                i.common.info(format!("Removed team {}.", t.label()));
            }
            ui.horizontal(|ui| {
                let mut st = i.st.lock().unwrap();
                ui.add(egui::TextEdit::singleline(&mut st.key_input).hint_text("Team API key").password(true).desired_width(260.0));
                let can = !st.key_input.trim().is_empty() && !st.adding_team;
                if crate::ui::primary(ui, can, crate::ui::icon::LINK, "Add team").clicked() {
                    let key = std::mem::take(&mut st.key_input);
                    st.adding_team = true;
                    drop(st);
                    let i = i.clone();
                    std::thread::spawn(move || {
                        i.add_team(key);
                        i.st.lock().unwrap().adding_team = false;
                        i.common.repaint();
                    });
                }
            });
        });

        crate::ui::section(ui, "Account", |ui| {
            let signing = i.st.lock().unwrap().signing_in.clone();
            if let Some(link) = signing {
                ui.label("Waiting for you to log in on wowaudit.com in your browser…");
                ui.horizontal(|ui| {
                    if ui.button(crate::ui::ib(crate::ui::icon::EXTERNAL, "Open the page again")).clicked() {
                        let _ = open::that(&link);
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::COPY, "Copy link")).clicked() {
                        ui.ctx().copy_text(link.clone());
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::X, "Cancel")).clicked() {
                        i.st.lock().unwrap().signing_in = None;
                    }
                });
            } else if cfg.token.is_empty() {
                ui.label("Log in so the WowauditCompanion addon's data reaches wowaudit (no team needed).");
                if crate::ui::primary(ui, !busy, crate::ui::icon::SIGN_IN, "Log in to wowaudit").clicked() {
                    let i = i.clone();
                    std::thread::spawn(move || i.sign_in());
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(format!("Logged in as {}", cfg.battletag));
                    if ui.small_button(crate::ui::ib(crate::ui::icon::SIGN_OUT, "Log out")).clicked() {
                        let mut c = i.cfg.lock().unwrap();
                        c.token.clear();
                        c.battletag.clear();
                        drop(c);
                        i.save();
                        i.common.info("Logged out.");
                    }
                });
            }
        });

        let (wishlist_status, loot_status, companion_status) = {
            let st = i.st.lock().unwrap();
            (st.wishlist_status.clone(), st.loot_status.clone(), st.companion_status.clone())
        };
        let addon_links = |ui: &mut egui::Ui, curse: &str| {
            ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "CurseForge"), curse);
        };
        let flavors = i.env.flavors();
        let has = |name: &str| flavors.iter().any(|f| f.addons_dir().join(name).is_dir());

        crate::ui::section(ui, "Sync", |ui| {
            let mut c = cfg.clone();
            ui.checkbox(&mut c.wishlists, "Wishlists into RCLootCouncil / SimpleLootCouncil");
            if c.wishlists {
                ui.indent("wa-wish", |ui| {
                    if !has(RCLC_MODULE) && !SLC_ADDONS.iter().any(|a| has(a)) {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("Needs the RCLootCouncil - wowaudit module or SimpleLootCouncil.");
                            addon_links(ui, "https://www.curseforge.com/wow/addons/rclootcouncil-wowaudit");
                        });
                    } else if !wishlist_status.is_empty() {
                        ui.weak(&wishlist_status);
                    }
                });
            }
            ui.checkbox(&mut c.loot_history, "Loot history from RCLootCouncil (last 3 months)");
            if c.loot_history && !loot_status.is_empty() {
                ui.indent("wa-loot", |ui| ui.weak(&loot_status));
            }
            ui.checkbox(&mut c.companion, "WowauditCompanion data");
            if c.companion {
                ui.indent("wa-comp", |ui| {
                    if !has(COMPANION) {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("WowauditCompanion is not installed.");
                            addon_links(ui, "https://www.curseforge.com/wow/addons/wowaudit-companion");
                        });
                    } else if !companion_status.is_empty() {
                        ui.weak(&companion_status);
                    }
                });
            }
            if (c.wishlists, c.loot_history, c.companion) != (cfg.wishlists, cfg.loot_history, cfg.companion) {
                let mut g = i.cfg.lock().unwrap();
                g.wishlists = c.wishlists;
                g.loot_history = c.loot_history;
                g.companion = c.companion;
                drop(g);
                i.save();
                i.st.lock().unwrap().next_wishlists = 0;
                i.common.wake();
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if crate::ui::primary(ui, !busy, crate::ui::icon::SYNC, "Sync now").clicked() {
                    self.upload_now();
                }
                if cfg.last_sync > 0 {
                    ui.weak(format!("Last synced {}", core::ago(cfg.last_sync)));
                }
            });
        });
    }
}

// ---- helpers -------------------------------------------------------------------------

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Writes `content` unless the file already has it. Returns whether it wrote.
fn write_if_changed(path: &Path, content: &str) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|b| b == content.as_bytes()) {
        return Ok(false);
    }
    core::write_atomic(path, content.as_bytes())?;
    Ok(true)
}

/// A value from `/v1/rclootcouncil/combined` as Lua: strings are Lua already (that's how
/// the site sends them), anything else is converted.
fn lua_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => lua_literal(other),
    }
}

fn lua_literal(v: &Value) -> String {
    match v {
        Value::Null => "nil".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => lua::quote(s),
        Value::Array(a) => format!("{{{}}}", a.iter().map(lua_literal).collect::<Vec<_>>().join(",")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter().map(|(k, v)| format!("[{}]={}", lua::quote(k), lua_literal(v))).collect::<Vec<_>>().join(",")
        ),
    }
}

fn lua_str(t: &Table, k: &str) -> Option<String> {
    match t.get(k) {
        Some(Lua::Str(s)) => Some(s.clone()),
        Some(Lua::Num(n)) => Some(if n.fract() == 0.0 { (*n as i64).to_string() } else { n.to_string() }),
        _ => None,
    }
}

/// When RCLootCouncil recorded the award (its `date` and `time` are local time).
fn awarded_at(date: &str, time: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = format!("{date} {time}");
    let naive = chrono::NaiveDateTime::parse_from_str(&text, "%Y/%m/%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&text, "%d/%m/%y %H:%M:%S"))
        .ok()?;
    use chrono::TimeZone;
    chrono::Local.from_local_datetime(&naive).earliest().map(|d| d.with_timezone(&chrono::Utc))
}

/// The awards of an `RCLootCouncil.lua` as the client sends them to `/v1/rclootcouncil`.
fn loot_entries(src: &[u8], now: i64) -> anyhow::Result<Vec<Value>> {
    let Some(Lua::Table(db)) = lua::read_global(src, "RCLootCouncilLootDB").map_err(|e| anyhow::anyhow!("{e}"))? else {
        return Ok(Vec::new());
    };
    let Some(Lua::Table(realms)) = db.get("factionrealm") else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for (_, players) in &realms.keyed {
        let Lua::Table(players) = players else { continue };
        for (player, list) in &players.keyed {
            let (Lua::Str(player), Lua::Table(list)) = (player, list) else { continue };
            let items = list.positional.iter().chain(list.keyed.iter().filter(|(k, _)| matches!(k, Lua::Num(_))).map(|(_, v)| v));
            for e in items {
                let Lua::Table(e) = e else { continue };
                let (Some(id), Some(game_string)) = (lua_str(e, "id"), lua_str(e, "lootWon")) else { continue };
                let Some(at) = awarded_at(&lua_str(e, "date").unwrap_or_default(), &lua_str(e, "time").unwrap_or_default()) else {
                    continue;
                };
                if now - at.timestamp() > LOOT_MAX_AGE {
                    continue;
                }
                let color: Vec<Value> = match e.get("color") {
                    Some(Lua::Table(c)) => c
                        .positional
                        .iter()
                        .filter_map(|x| if let Lua::Num(n) = x { Some(json!((n * 255.0).round() as i64)) } else { None })
                        .collect(),
                    _ => Vec::new(),
                };
                let num = |k: &str| match e.get(k) {
                    Some(Lua::Num(n)) => json!(*n as i64),
                    _ => Value::Null,
                };
                let opt = |k: &str| lua_str(e, k).map_or(Value::Null, Value::String);
                let mut entry = Map::new();
                entry.insert("rclootcouncil_id".into(), json!(id));
                entry.insert("source".into(), json!("rclootcouncil"));
                entry.insert("recipient".into(), json!(player));
                entry.insert("master_looter".into(), opt("owner"));
                entry.insert("difficulty_id".into(), num("difficultyID"));
                entry.insert("response".into(), opt("response"));
                entry.insert("response_color".into(), if color.is_empty() { Value::Null } else { Value::Array(color) });
                entry.insert("awarded_at".into(), json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
                entry.insert("game_string".into(), json!(game_string));
                entry.insert("note".into(), opt("note"));
                entry.insert("old_item_1_game_string".into(), opt("itemReplaced1"));
                entry.insert("old_item_2_game_string".into(), opt("itemReplaced2"));
                entry.insert("same_response_amount".into(), num("same_response_amount"));
                entry.insert("wishes_when_awarded".into(), e.get("wishes").map_or(Value::Null, lua::to_json));
                out.push(Value::Object(entry));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loot_entry_shape() {
        let now = chrono::Utc::now();
        let date = now.with_timezone(&chrono::Local).format("%Y/%m/%d").to_string();
        let src = format!(
            r#"RCLootCouncilLootDB = {{ ["factionrealm"] = {{ ["Horde - Realm"] = {{ ["Abe-Realm"] = {{
                {{ ["id"] = "1787770170-39", ["lootWon"] = "|Hitem:1::|h[X]|h|r", ["date"] = "{date}", ["time"] = "00:00:01",
                   ["color"] = {{ 1, 0.5, 0, 1 }}, ["difficultyID"] = 16, ["response"] = "BiS", ["owner"] = "Ml-Realm",
                   ["same_response_amount"] = 2, ["wishes"] = {{ ["percentage"] = 3.5 }} }},
                {{ ["id"] = "1-1", ["lootWon"] = "x", ["date"] = "2020/01/01", ["time"] = "10:00:00" }},
            }} }} }} }}"#
        );
        let e = loot_entries(src.as_bytes(), now.timestamp() + 3600).unwrap();
        assert_eq!(e.len(), 1, "older than three months: left out");
        let e = &e[0];
        assert_eq!(e["recipient"], "Abe-Realm");
        assert_eq!(e["master_looter"], "Ml-Realm");
        assert_eq!(e["response_color"], json!([255, 128, 0, 255]));
        assert_eq!(e["difficulty_id"], 16);
        assert_eq!(e["same_response_amount"], 2);
        assert_eq!(e["wishes_when_awarded"]["percentage"], 3.5);
        assert_eq!(e["old_item_1_game_string"], Value::Null);
        assert!(e["awarded_at"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn lua_values() {
        assert_eq!(lua_text(&json!("{[1]=2}")), "{[1]=2}");
        assert_eq!(lua_text(&json!(17)), "17");
        assert_eq!(lua_text(&Value::Null), "nil");
        assert_eq!(lua_text(&json!({"a": [1, "b"]})), r#"{["a"]={1,"b"}}"#);
    }

    /// `WA_RCLC=<RCLootCouncil.lua> cargo test -- --ignored real_rclc --nocapture`
    #[test]
    #[ignore]
    fn real_rclc() {
        let src = std::fs::read(std::env::var("WA_RCLC").unwrap()).unwrap();
        let all = loot_entries(&src, chrono::Utc::now().timestamp()).unwrap();
        eprintln!("{} in the last 3 months; first: {}", all.len(), all.first().map(|v| v.to_string()).unwrap_or_default());
    }
}
