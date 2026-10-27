//! WowUtils: what the WowUtils Bridge 1.1.0 does. It sends the character data the
//! `wowutils` addon collects (gear, vault, currencies, quests…) to wowutils.com, and
//! brings each selected raid group's wishlists and droptimizers back into the game by
//! writing the `wowutils_data` addon's `data.lua`.
//!
//! It checks in with the site (`/bridge/v1/heartbeat`) the way the Bridge does, which
//! tells it which groups changed and when to come back, and, for groups whose admins allow
//! it, sends RCLootCouncil's loot history (see [`loot_history`]).
//!
//! Reverse engineered from the Bridge (NativeAOT build) and checked against files a real
//! Bridge install wrote. The Bridge's config also has an (empty, unused) `connectors`
//! section; 1.1.0 has no code behind it, so there's nothing to port.

mod loot_history;
mod rclc;

use super::{Meta, Service};
use crate::core::lua;
use crate::core::oauth::url_encode;
use crate::core::{self, store, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static META: Meta = Meta {
    id: "wowutils",
    name: "WowUtils",
    tagline: "Character data and raid wishlists for wowutils.com",
    icon: include_bytes!("../../../assets/icons/wowutils.png"),
    website: "https://wowutils.com/bridge",
    upstream_app: "WowUtils Bridge",
    ported_version: "1.1.0",
    upstream_url: "https://wowutils.com/bridge",
    runs_official: false,
};

const API_BASE: &str = "https://api.wowutils.com";
const WEB_BASE: &str = "https://wowutils.com/viserio-cooldowns";
const ADDON: &str = "wowutils";
const DATA_ADDON: &str = "wowutils_data";
/// The Bridge writes this into every data.lua; keeping it lets people switch between
/// the Bridge and this app without either refusing the other's file.
const GENERATED_MARK: &str = "-- WowUtils Bridge generated file. Do not edit.";
const DATA_SCHEMA: i64 = 4;
/// `schemaVersion` of the character upload (accepted by the API, 2026-10-27).
const UPLOAD_SCHEMA: i64 = 1;
const DEFAULT_POLL: i64 = 300;
/// Data the site refused is sent again only when it changes, or after this long.
const REFUSED_RETRY: i64 = 6 * 3600;
const MIN_POLL: i64 = 60;

const DATA_TOC: &str = "## Interface: 120007, 120100\n## Title: wowutils_data\n## Notes: wowutils desktop data\n## Author: Ironi\n## Version: 0.0.1\n## IconTexture: Interface\\AddOns\\wowutils\\media\\logo.png\n## DefaultState: enabled\n## Category: wowutils\n\ncore.lua\ndata.lua\n";
const DATA_CORE: &str = "local addon_name, ns = ...\n\nWowUtilsPublicDataAPI = {}\n\nWowUtilsPublicDataAPI.GetFullData = function()\n    return ns.data\nend\n";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    token: String,
    device_id: String,
    battletag: String,
    /// Raid groups whose wishlists go into the game.
    selected_groups: Vec<String>,
    /// Raid groups that get RCLootCouncil's loot history.
    loot_history_groups: Vec<String>,
    /// Account folders (`<flavor>/<account>`) whose characters are not sent.
    disabled_accounts: Vec<String>,
    /// Per account: hash of the last character data the site accepted.
    sent_hashes: BTreeMap<String, String>,
    last_upload: i64,
    /// Whether the group choice of a Windows Bridge install was looked for already.
    imported_bridge: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Group {
    #[serde(rename = "groupId")]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "canImportLootHistory")]
    can_import_loot_history: bool,
    /// Version (ETag) of the group's data; the heartbeat says when it changed.
    #[serde(default)]
    version: Value,
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    role: String,
}

/// One raid group's data as the site sent it (`/bridge/v1/groups/{id}/loot`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct GroupData {
    etag: String,
    fetched_at: i64,
    payload: String,
}

#[derive(Default)]
struct State {
    groups: Option<Vec<Group>>,
    next_groups_poll: i64,
    next_data_poll: i64,
    next_upload: i64,
    signing_in: Option<String>,
    /// Account key -> (file mtime, characters, last status line) for the page.
    accounts: Vec<AccountView>,
    /// Account key -> (content hash, when) the site last refused, so the same data isn't
    /// sent again and again.
    refused: BTreeMap<String, (String, i64)>,
    /// The server answered the heartbeat with 404: pace by the older polling instead.
    no_heartbeat: bool,
    next_heartbeat: i64,
    /// The heartbeat's say on character uploads: "all", "own" or "none", and not before when.
    upload_scope: String,
    upload_not_before: i64,
    said_nothing_new: bool,
    /// Loot history state per group, for the page.
    loot: BTreeMap<String, loot_history::GroupStatus>,
}

#[derive(Clone)]
struct AccountView {
    key: String,
    flavor: String,
    account: String,
    characters: usize,
    file_time: i64,
    status: String,
    /// `wowutils.lua`'s SavedVariables folder, and the characters' roster keys
    /// (`droptimizerKey`), for the heartbeat's roster coverage.
    sv_dir: PathBuf,
    own_keys: Vec<String>,
    other_keys: Vec<String>,
    /// Data the site doesn't have yet.
    pending: bool,
}

pub struct WowUtils {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    st: Mutex<State>,
    rclc: Mutex<loot_history::RclcCache>,
    http: reqwest::blocking::Client,
}

impl WowUtils {
    pub fn new(env: Arc<Env>) -> Self {
        let mut cfg: Config = store::load("wowutils");
        // Older builds could store a group twice.
        for list in [&mut cfg.selected_groups, &mut cfg.loot_history_groups] {
            let mut seen = std::collections::HashSet::new();
            list.retain(|id| seen.insert(id.clone()));
        }
        let ua = format!("WowUtilsBridge/{} (Linux; hearthlink {})", META.ported_version, env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                env,
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                st: Mutex::new(State::default()),
                rclc: Mutex::new(Default::default()),
                http: core::http_client(&ua, 60),
            }),
        }
    }
}

#[derive(Debug)]
enum ApiError {
    SignedOut,
    Upgrade,
    Status(u16, String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::SignedOut => write!(f, "signed out"),
            ApiError::Upgrade => write!(f, "WowUtils wants a newer Bridge than the one this app is based on"),
            ApiError::Status(c, body) if body.is_empty() => write!(f, "server returned {c}"),
            ApiError::Status(c, body) => write!(f, "server returned {c}: {body}"),
            ApiError::Other(e) => write!(f, "{}", core::friendly_error(e)),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Other(e.into())
    }
}

fn error_text(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| ["message", "error", "detail", "title"].iter().find_map(|k| v[*k].as_str().map(String::from)))
        .unwrap_or_else(|| body.chars().take(2000).collect())
}

impl Inner {
    fn save(&self) {
        store::save("wowutils", &*self.cfg.lock().unwrap());
    }

    fn token(&self) -> Option<String> {
        let t = self.cfg.lock().unwrap().token.clone();
        (!t.is_empty()).then_some(t)
    }

    /// Sends a request with the device token; maps the Bridge's special answers.
    fn call(&self, req: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response, ApiError> {
        let token = self.token().ok_or(ApiError::SignedOut)?;
        let resp = req.bearer_auth(token).send()?;
        match resp.status().as_u16() {
            401 => {
                self.signed_out("You've been signed out. Sign in to link this device again.");
                Err(ApiError::SignedOut)
            }
            426 => Err(ApiError::Upgrade),
            s if s >= 400 => {
                let body = resp.text().unwrap_or_default();
                Err(ApiError::Status(s, error_text(&body)))
            }
            _ => Ok(resp),
        }
    }

    fn signed_out(&self, why: &str) {
        let mut c = self.cfg.lock().unwrap();
        if c.token.is_empty() {
            return;
        }
        c.token.clear();
        drop(c);
        self.save();
        self.common.warn(why);
        self.env.notify("WowUtils", why);
    }

    // ---- signing in --------------------------------------------------------------

    /// The Bridge's sign-in: PKCE in the browser, the site redirects to a listener on
    /// 127.0.0.1, then the code is exchanged at `/bridge/v1/token`.
    fn sign_in(self: &Arc<Self>) {
        let c = &self.common;
        let pkce = core::oauth::Pkce::new();
        let (verifier, challenge, state) = (pkce.verifier, pkce.challenge, pkce.state);
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => return c.error(format!("Couldn't start the sign-in listener: {e}")),
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let device = core::oauth::device_name();
        let enc = |s: &str| url_encode(s);
        let url = format!(
            "{WEB_BASE}/bridge/link?code_challenge={}&state={}&redirect_uri={}&device_name={}&os={}",
            enc(&challenge),
            enc(&state),
            enc(&redirect),
            enc(&device),
            enc("Linux")
        );
        self.st.lock().unwrap().signing_in = Some(url.clone());
        self.common.repaint();
        c.info("Opening your browser to sign in to WowUtils…");
        if let Err(e) = open::that(&url) {
            c.warn(format!("Couldn't open the browser ({e}). Use \"Copy link\" and open it yourself."));
        }
        let cancelled = || self.st.lock().unwrap().signing_in.is_none();
        let result = core::oauth::wait_for_callback(&listener, &state, &cancelled, core::oauth::Finish::Page);
        self.st.lock().unwrap().signing_in = None;
        let code = match result {
            Ok(Some(code)) => code,
            Ok(None) => return c.info("Sign-in was cancelled."),
            Err(e) => return c.error(format!("Couldn't finish signing in: {e}")),
        };
        let r = self
            .http
            .post(format!("{API_BASE}/bridge/v1/token"))
            .json(&json!({ "code": code, "code_verifier": verifier }))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<Value>());
        match r {
            Ok(v) if v["token"].as_str().is_some_and(|t| !t.is_empty()) => {
                let mut cfg = self.cfg.lock().unwrap();
                cfg.token = v["token"].as_str().unwrap_or_default().into();
                cfg.device_id = v["deviceId"].as_str().unwrap_or_default().into();
                cfg.battletag = v["battletag"].as_str().unwrap_or_default().into();
                let who = cfg.battletag.clone();
                drop(cfg);
                self.save();
                c.good(format!("Signed in as {who}."));
                let mut st = self.st.lock().unwrap();
                st.next_groups_poll = 0;
                st.next_heartbeat = 0;
                st.next_upload = 0;
                drop(st);
                c.wake();
            }
            Ok(_) => c.error("The token exchange at wowutils.com didn't succeed."),
            Err(e) => c.error(format!("The token exchange at wowutils.com didn't succeed: {}", core::friendly_error(&e))),
        }
    }

    fn sign_out(&self) {
        if self.token().is_some() {
            let _ = self.call(self.http.post(format!("{API_BASE}/bridge/v1/revoke")));
        }
        let mut c = self.cfg.lock().unwrap();
        c.token.clear();
        c.device_id.clear();
        drop(c);
        self.save();
        self.st.lock().unwrap().groups = None;
        self.common.info("Signed out.");
    }

    // ---- raid groups ---------------------------------------------------------------

    fn refresh_groups(&self) -> Result<(), ApiError> {
        let v: Value = self.call(self.http.get(format!("{API_BASE}/bridge/v1/groups")))?.json()?;
        // An unexpected answer must not look like "no groups": that would drop the selection.
        let groups: Vec<Group> = serde_json::from_value(v["groups"].clone())
            .map_err(|e| ApiError::Other(anyhow::anyhow!("unexpected group list from WowUtils: {e}")))?;
        let poll = v["nextPollSeconds"].as_i64().unwrap_or(DEFAULT_POLL).max(MIN_POLL);
        self.st.lock().unwrap().next_groups_poll = core::now_unix() + poll;
        self.apply_groups(groups);
        Ok(())
    }

    /// Takes a fresh group list (from the group poll or the heartbeat).
    fn apply_groups(&self, groups: Vec<Group>) {
        self.st.lock().unwrap().groups = Some(groups.clone());
        let mut c = self.cfg.lock().unwrap();
        let mut changed = false;
        // Only once: a Windows Bridge's choice seeds an empty selection, later changes are the user's.
        if !c.imported_bridge {
            c.imported_bridge = true;
            changed = true;
            let (wishlists, loot) = bridge_selected_groups();
            let known = |ids: Vec<String>| -> Vec<String> {
                ids.into_iter().filter(|id| groups.iter().any(|g| &g.id == id)).collect()
            };
            if c.selected_groups.is_empty() {
                c.selected_groups = known(wishlists);
                if !c.selected_groups.is_empty() {
                    self.common.info(format!(
                        "Took over the raid group choice of your WowUtils Bridge install ({} group(s)).",
                        c.selected_groups.len()
                    ));
                }
            }
            if c.loot_history_groups.is_empty() {
                c.loot_history_groups = known(loot);
            }
        }
        // Groups the user left disappear from the selection.
        let cm = &mut *c;
        for list in [&mut cm.selected_groups, &mut cm.loot_history_groups] {
            let before = list.len();
            list.retain(|id| groups.iter().any(|g| &g.id == id));
            changed |= list.len() != before;
        }
        drop(c);
        if changed {
            self.save();
        }
    }

    // ---- heartbeat ---------------------------------------------------------------------

    /// Checks in with the site like the Bridge: what this computer has and which group data
    /// it holds; the answer lists the groups (and which changed), how to pace uploads and
    /// when to check in again. `Ok(false)` = the server has no heartbeat.
    fn heartbeat(&self) -> Result<bool, ApiError> {
        let cfg = self.cfg.lock().unwrap().clone();
        let views = self.st.lock().unwrap().accounts.clone();
        let mut selected: Vec<String> = Vec::new();
        for id in cfg.selected_groups.iter().chain(&cfg.loot_history_groups) {
            if !selected.contains(id) && selected.len() < 50 {
                selected.push(id.clone());
            }
        }
        let rosters: BTreeMap<String, std::collections::HashSet<String>> = selected
            .iter()
            .map(|id| {
                let keys = group_payload(id)
                    .map(|p| roster_entries(&p).into_iter().map(|(n, realm)| format!("{n}-{realm}")).collect())
                    .unwrap_or_default();
                (id.clone(), keys)
            })
            .collect();
        let accounts: Vec<Value> = views
            .iter()
            .filter(|a| !cfg.disabled_accounts.contains(&a.key))
            .take(10)
            .map(|a| {
                let coverage: Map<String, Value> = rosters
                    .iter()
                    .map(|(id, roster)| {
                        let on = |k: &&String| roster.contains(k.as_str());
                        let own: std::collections::HashSet<&String> = a.own_keys.iter().filter(on).collect();
                        let seen: std::collections::HashSet<&String> =
                            a.own_keys.iter().chain(&a.other_keys).filter(on).collect();
                        (id.clone(), json!({ "seen": seen.len(), "own": own.len() }))
                    })
                    .collect();
                let mut o = json!({ "key": account_key(&a.sv_dir), "rosterCoverage": coverage });
                let written = mtime(&a.sv_dir.join(format!("{ADDON}.lua")));
                if written > 0 && written <= 4_102_444_800 {
                    o["lastFileWriteAt"] = json!(written);
                }
                o
            })
            .collect();
        let group_versions: Map<String, Value> =
            selected.iter().filter_map(|id| group_version(id).map(|v| (id.clone(), Value::String(v)))).collect();
        let body = json!({
            "schemaVersion": 1,
            "appVersion": META.ported_version,
            "wowRunning": core::wow::game_running(),
            "selectedGroupIds": selected,
            "groupVersions": group_versions,
            "accounts": accounts,
            "pendingUpload": views.iter().any(|a| a.pending) || self.files_changed(),
        });
        let resp = match self.call(self.http.post(format!("{API_BASE}/bridge/v1/heartbeat")).json(&body)) {
            Err(ApiError::Status(404, _)) => return Ok(false),
            r => r?,
        };
        let v: Value = resp.json()?;
        let now = core::now_unix();
        let next = v["nextHeartbeatSeconds"].as_i64().unwrap_or(900).clamp(MIN_POLL, 3600);
        let groups: Vec<Group> = serde_json::from_value(v["groups"].clone())
            .map_err(|e| ApiError::Other(anyhow::anyhow!("unexpected heartbeat answer from WowUtils: {e}")))?;
        let changed: Vec<String> = groups
            .iter()
            .filter(|g| g.changed && (cfg.selected_groups.contains(&g.id) || cfg.loot_history_groups.contains(&g.id)))
            .map(|g| g.id.clone())
            .collect();
        self.apply_groups(groups);
        let mut st = self.st.lock().unwrap();
        st.next_heartbeat = now + next;
        st.next_groups_poll = now + next;
        st.upload_scope = v["upload"]["scope"].as_str().unwrap_or("all").to_string();
        st.upload_not_before = match v["upload"]["notBeforeSeconds"].as_i64() {
            Some(n) if n > 0 => now + n,
            _ => 0,
        };
        // Group data is fetched when the site says it changed (or this app has none yet).
        let missing = cfg.selected_groups.iter().chain(&cfg.loot_history_groups).any(|id| group_version(id).is_none());
        if !changed.is_empty() || missing {
            st.next_data_poll = 0;
            st.said_nothing_new = false;
        } else {
            st.next_data_poll = now + next;
            if !st.said_nothing_new && !cfg.selected_groups.is_empty() {
                st.said_nothing_new = true;
                drop(st);
                self.common.info("Checked in with WowUtils, nothing new for your groups.");
            }
        }
        Ok(true)
    }

    fn group_cache_path(id: &str) -> PathBuf {
        store::data_dir().join("wowutils").join(format!("group-{id}.json"))
    }

    /// Fetches a group's wishlists (`If-None-Match`, so unchanged data costs nothing).
    /// Returns whether it changed and the server's wish for the next poll.
    fn fetch_group(&self, id: &str) -> Result<(bool, Option<i64>), ApiError> {
        let path = Self::group_cache_path(id);
        let cached: GroupData = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut req = self.http.get(format!("{API_BASE}/bridge/v1/groups/{}/loot", url_encode(id)));
        if !cached.etag.is_empty() && !cached.payload.is_empty() {
            req = req.header("If-None-Match", &cached.etag);
        }
        let resp = self.call(req)?;
        let next = resp
            .headers()
            .get("X-Bridge-Next-Poll")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|s| s as i64);
        if resp.status().as_u16() == 304 {
            return Ok((false, next));
        }
        let etag = resp.headers().get("ETag").and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
        let payload = resp.text()?;
        serde_json::from_str::<Value>(&payload).map_err(|e| ApiError::Other(anyhow::anyhow!("unreadable group data: {e}")))?;
        let changed = payload != cached.payload;
        let data = GroupData { etag, fetched_at: core::now_unix(), payload };
        if let Err(e) = core::write_atomic(&path, &serde_json::to_vec(&data).unwrap_or_default()) {
            self.common.warn(format!("Couldn't cache group data: {e}"));
        }
        Ok((changed, next))
    }

    /// Fetches the selected groups and rewrites `data.lua` in every game folder that has
    /// the WowUtils addon.
    fn sync_groups(&self) -> Result<(), ApiError> {
        let (selected, loot) = {
            let c = self.cfg.lock().unwrap();
            (c.selected_groups.clone(), c.loot_history_groups.clone())
        };
        let mut next = DEFAULT_POLL;
        // Loot history groups' data only provides their roster.
        for id in loot.iter().filter(|id| !selected.contains(id)) {
            if let (_, Some(n)) = self.fetch_group(id)? {
                next = next.min(n.max(MIN_POLL));
            }
        }
        if selected.is_empty() {
            // Nothing chosen yet: leave whatever is in the game alone.
            self.st.lock().unwrap().next_data_poll = core::now_unix() + next;
            return Ok(());
        }
        for id in &selected {
            let (_, n) = self.fetch_group(id)?;
            if let Some(n) = n {
                next = next.min(n.max(MIN_POLL));
            }
        }
        self.st.lock().unwrap().next_data_poll = core::now_unix() + next;
        let payloads: Vec<String> = selected
            .iter()
            .filter_map(|id| std::fs::read(Self::group_cache_path(id)).ok())
            .filter_map(|b| serde_json::from_slice::<GroupData>(&b).ok())
            .map(|g| g.payload)
            .filter(|p| serde_json::from_str::<Value>(p).is_ok())
            .collect();
        let flavors: Vec<_> = self.env.flavors().into_iter().filter(|f| f.addons_dir().join(ADDON).is_dir()).collect();
        if flavors.is_empty() {
            if !selected.is_empty() {
                self.common.warn("The WowUtils addon isn't installed in any game folder, so there's nowhere for your raid data to land.");
            }
            return Ok(());
        }
        for f in flavors {
            match write_data_lua(&f.addons_dir().join(DATA_ADDON), &payloads) {
                Ok(Some(rev)) => self.common.good(format!(
                    "{}: wrote raid data for {} group(s) into the game (rev {rev}). /reload in game to see it.",
                    f.display_name(),
                    payloads.len()
                )),
                Ok(None) => {}
                Err(e) => self.common.error(format!("{}: couldn't write the raid data: {e}", f.display_name())),
            }
        }
        Ok(())
    }

    // ---- character data --------------------------------------------------------------

    /// Every account folder with a `wowutils.lua`.
    fn find_accounts(&self) -> Vec<(String, String, String, PathBuf)> {
        let mut out = Vec::new();
        for f in self.env.flavors() {
            for (account, sv) in f.accounts() {
                let file = sv.join(format!("{ADDON}.lua"));
                if file.is_file() {
                    out.push((format!("{}/{}", f.folder, account), f.display_name(), account, file));
                }
            }
        }
        out
    }

    fn upload_characters(&self, force: bool) -> Result<(), ApiError> {
        let cfg = self.cfg.lock().unwrap().clone();
        let scope = self.st.lock().unwrap().upload_scope.clone();
        let mut views = Vec::new();
        let mut next = DEFAULT_POLL;
        for (key, flavor, account, file) in self.find_accounts() {
            let mut view = AccountView {
                key: key.clone(),
                flavor,
                account,
                characters: 0,
                file_time: mtime(&file),
                status: String::new(),
                sv_dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
                own_keys: Vec::new(),
                other_keys: Vec::new(),
                pending: false,
            };
            if cfg.disabled_accounts.contains(&key) {
                view.status = "not sent (turned off)".into();
                views.push(view);
                continue;
            }
            let mut body = match read_upload(&file) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    view.status = "the addon hasn't written any data yet".into();
                    views.push(view);
                    continue;
                }
                Err(e) => {
                    view.status = format!("couldn't read the addon's data: {e}");
                    self.common.warn(format!("{key}: couldn't read the addon's data: {e}"));
                    views.push(view);
                    continue;
                }
            };
            view.characters = body["ownCharacters"].as_array().map_or(0, |a| a.len())
                + body["others"].as_array().map_or(0, |a| a.len());
            let keys = |k: &str| -> Vec<String> {
                body[k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["droptimizerKey"].as_str())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            view.own_keys = keys("ownCharacters");
            view.other_keys = keys("others");
            // The site can ask for the player's own characters only, or for nothing for now.
            if scope == "own" {
                body["others"] = json!([]);
            }
            let hash = content_hash(&body);
            if !force && cfg.sent_hashes.get(&key) == Some(&hash) {
                view.status = "sent, nothing new since".into();
                views.push(view);
                continue;
            }
            view.pending = true;
            if scope == "none" {
                view.status = "waiting, WowUtils asked for a pause".into();
                views.push(view);
                continue;
            }
            let refused = self.st.lock().unwrap().refused.get(&key).cloned();
            if !force && refused.as_ref().is_some_and(|(h, at)| h == &hash && core::now_unix() - at < REFUSED_RETRY) {
                view.status = "refused, waiting for new data from the game".into();
                views.push(view);
                continue;
            }
            let resp = self.call(
                self.http
                    .post(format!("{API_BASE}/bridge/v1/characters"))
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(&body).unwrap_or_default()),
            );
            match resp {
                Ok(r) => {
                    let v: Value = r.json().unwrap_or(Value::Null);
                    let n = |k: &str| v[k].as_i64().unwrap_or(0);
                    if let Some(p) = v["nextPollSeconds"].as_i64() {
                        next = next.min(p.max(MIN_POLL));
                    }
                    let mut msg = format!("Sent {} character(s) to WowUtils", n("accepted"));
                    if n("droppedOffRoster") > 0 {
                        msg += &format!(", {} weren't on a raid group's roster", n("droppedOffRoster"));
                    }
                    if n("droppedNoKey") > 0 {
                        msg += &format!(", {} couldn't be identified yet", n("droppedNoKey"));
                    }
                    self.common.good(format!("{key}: {msg}."));
                    view.status = format!("sent {}", core::fmt_time(core::now_unix()));
                    view.pending = false;
                    self.st.lock().unwrap().refused.remove(&key);
                    let mut c = self.cfg.lock().unwrap();
                    c.sent_hashes.insert(key.clone(), hash);
                    c.last_upload = core::now_unix();
                    drop(c);
                    self.save();
                }
                Err(e @ ApiError::Status(..)) => {
                    self.st.lock().unwrap().refused.insert(key.clone(), (hash, core::now_unix()));
                    view.status = format!("refused: {e}");
                    self.common.error(format!("{key}: WowUtils wouldn't take your character data ({e})."));
                }
                Err(e) => {
                    self.st.lock().unwrap().accounts = views;
                    return Err(e);
                }
            }
            views.push(view);
        }
        let mut st = self.st.lock().unwrap();
        st.accounts = views;
        st.next_upload = core::now_unix() + next;
        Ok(())
    }

    // ---- the worker --------------------------------------------------------------------

    fn tick(&self) {
        if self.token().is_none() {
            self.common.set_health(Health::Attention, "Not signed in");
            return;
        }
        let now = core::now_unix();
        let run = || -> Result<(), ApiError> {
            let (heartbeat_due, no_heartbeat) = {
                let st = self.st.lock().unwrap();
                (now >= st.next_heartbeat || st.groups.is_none(), st.no_heartbeat)
            };
            if !no_heartbeat && heartbeat_due && !self.heartbeat()? {
                self.common.info("This server has no heartbeat; falling back to polling.");
                self.st.lock().unwrap().no_heartbeat = true;
            }
            let (groups_due, data_due, upload_due, not_before) = {
                let st = self.st.lock().unwrap();
                (
                    st.no_heartbeat && (st.groups.is_none() || now >= st.next_groups_poll),
                    now >= st.next_data_poll,
                    now >= st.next_upload,
                    st.upload_not_before,
                )
            };
            if groups_due {
                self.refresh_groups()?;
            }
            if data_due || groups_due {
                self.sync_groups()?;
            }
            if core::now_unix() >= not_before && (upload_due || self.files_changed()) {
                self.upload_characters(false)?;
            }
            self.sync_loot_history(false)?;
            Ok(())
        };
        match run() {
            Ok(()) => {
                let last = self.cfg.lock().unwrap().last_upload;
                let who = self.cfg.lock().unwrap().battletag.clone();
                let text = if last > 0 { format!("{who} · last sent {}", core::ago(last)) } else { format!("Signed in as {who}") };
                self.common.set_health(Health::Ok, text);
            }
            Err(ApiError::SignedOut) => self.common.set_health(Health::Attention, "Signed out, sign in again"),
            Err(ApiError::Upgrade) => {
                self.common.set_health(Health::Error, "WowUtils needs a newer app");
                self.common.error("WowUtils asks for a newer Bridge than the one this app is based on. Syncing waits for a Hearthlink update.");
                let mut st = self.st.lock().unwrap();
                st.next_heartbeat = now + 6 * 3600;
                st.next_groups_poll = now + 6 * 3600;
            }
            Err(e) => {
                self.common.warn(format!("Couldn't sync: {e}. It'll try again shortly."));
                self.common.set_health(Health::Error, format!("{e}"));
                let mut st = self.st.lock().unwrap();
                st.next_heartbeat = st.next_heartbeat.max(now + MIN_POLL);
                st.next_groups_poll = st.next_groups_poll.max(now + MIN_POLL);
                st.next_upload = st.next_upload.max(now + MIN_POLL);
            }
        }
    }

    /// Whether any addon file changed since the last read (the game saves on logout and
    /// /reload); uploads then go out right away instead of on the next poll.
    fn files_changed(&self) -> bool {
        let st = self.st.lock().unwrap();
        self.find_accounts().iter().any(|(key, _, _, file)| {
            let t = mtime(file);
            // Give the game a moment to finish writing.
            t + 5 < core::now_unix() && st.accounts.iter().find(|a| &a.key == key).is_none_or(|a| a.file_time != t)
        })
    }
}

impl Service for WowUtils {
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
            let v: serde_json::Value = http.get("https://dl.wowutils.com/bridge/latest.json").send()?.error_for_status()?.json()?;
            let ver = v["version"].as_str().ok_or_else(|| anyhow::anyhow!("no version"))?.to_string();
            Ok((ver, v["notes"].as_str().or(v["releaseNotes"].as_str()).map(String::from)))
        });
        let i = self.inner.clone();
        std::thread::spawn(move || loop {
            if i.cfg.lock().unwrap().enabled {
                i.common.exclusive(|| i.tick());
            } else {
                i.common.set_health(Health::Off, "Turned off");
            }
            i.common.sleep(Duration::from_secs(20));
        });
    }
    fn upload_now(&self) {
        let i = self.inner.clone();
        std::thread::spawn(move || {
            i.common.exclusive(|| {
                if let Err(e) = i.upload_characters(true).and_then(|_| i.sync_groups()).and_then(|_| i.sync_loot_history(true)) {
                    i.common.error(format!("Couldn't sync: {e}"));
                }
            });
        });
    }
    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();

        crate::ui::section(ui, "Account", |ui| {
            let signing = i.st.lock().unwrap().signing_in.clone();
            if let Some(url) = signing {
                ui.label("Waiting for you to sign in on wowutils.com in your browser…");
                ui.horizontal(|ui| {
                    if ui.button(crate::ui::ib(crate::ui::icon::EXTERNAL, "Open the page again")).clicked() {
                        let _ = open::that(&url);
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::COPY, "Copy link")).clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                    if ui.button(crate::ui::ib(crate::ui::icon::X, "Cancel")).clicked() {
                        i.st.lock().unwrap().signing_in = None;
                    }
                });
            } else if cfg.token.is_empty() {
                ui.label("Sign in to link this computer to your WowUtils account.");
                if crate::ui::primary(ui, !busy, crate::ui::icon::SIGN_IN, "Sign in to WowUtils").clicked() {
                    let i = i.clone();
                    std::thread::spawn(move || i.sign_in());
                }
            } else {
                ui.horizontal(|ui| {
                    ui.label(format!("Signed in as {}", cfg.battletag));
                    if ui.small_button(crate::ui::ib(crate::ui::icon::SIGN_OUT, "Sign out")).clicked() {
                        let i = i.clone();
                        std::thread::spawn(move || i.sign_out());
                    }
                });
            }
        });

        let flavors = i.env.flavors();
        if !flavors.is_empty() && !flavors.iter().any(|f| f.addons_dir().join(ADDON).is_dir()) {
            crate::ui::section(ui, "WowUtils addon missing", |ui| {
                ui.label(
                    "The WowUtils addon collects the character data and shows your raid's wishlists in game. \
                     Install it with an addon manager (CurseForge, WowUp, Wago App) so it stays up to date.",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "CurseForge"), "https://www.curseforge.com/wow/addons/wowutils");
                    ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "Wago"), "https://addons.wago.io/addons/wowutils");
                });
            });
        }

        if cfg.token.is_empty() {
            return;
        }

        crate::ui::section(ui, "Raid groups", |ui| {
            ui.weak("Wishlists and droptimizers of the ticked groups go into the game (wowutils_data addon).");
            let groups = i.st.lock().unwrap().groups.clone();
            match groups {
                None => {
                    ui.label("Loading your raid groups…");
                }
                Some(g) if g.is_empty() => {
                    ui.label("No raid groups yet. Join or create one on WowUtils, then refresh.");
                }
                Some(g) => {
                    let mut c = cfg.clone();
                    for group in &g {
                        let mut on = c.selected_groups.contains(&group.id);
                        let name = if group.name.is_empty() { group.id.clone() } else { group.name.clone() };
                        if ui.checkbox(&mut on, name).changed() {
                            if on {
                                c.selected_groups.push(group.id.clone());
                            } else {
                                c.selected_groups.retain(|x| x != &group.id);
                            }
                        }
                    }
                    if c.selected_groups != cfg.selected_groups {
                        i.cfg.lock().unwrap().selected_groups = c.selected_groups;
                        i.save();
                        i.st.lock().unwrap().next_data_poll = 0;
                        i.common.wake();
                    }
                }
            }
            if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::SYNC, "Refresh"))).clicked() {
                let mut st = i.st.lock().unwrap();
                st.next_groups_poll = 0;
                st.next_data_poll = 0;
                drop(st);
                i.common.wake();
            }
        });

        crate::ui::section(ui, "Character data", |ui| {
            let accounts = i.st.lock().unwrap().accounts.clone();
            if accounts.is_empty() {
                ui.label("No data from the WowUtils addon yet. Play with the addon installed and log out or /reload.");
            }
            let mut c = cfg.clone();
            egui::ScrollArea::horizontal().id_salt("wu-accounts-scroll").show(ui, |ui| {
                egui::Grid::new("wu-accounts").striped(true).num_columns(4).show(ui, |ui| {
                    ui.strong("Send");
                    ui.strong("Account");
                    ui.strong("Characters");
                    ui.strong("Status");
                    ui.end_row();
                    for a in &accounts {
                        let mut on = !c.disabled_accounts.contains(&a.key);
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                c.disabled_accounts.retain(|x| x != &a.key);
                            } else {
                                c.disabled_accounts.push(a.key.clone());
                            }
                        }
                        ui.label(format!("{} · {}", a.flavor, a.account));
                        ui.label(a.characters.to_string());
                        ui.label(&a.status);
                        ui.end_row();
                    }
                });
            });
            if c.disabled_accounts != cfg.disabled_accounts {
                i.cfg.lock().unwrap().disabled_accounts = c.disabled_accounts;
                i.save();
                i.st.lock().unwrap().next_upload = 0;
                i.common.wake();
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if crate::ui::primary(ui, !busy, crate::ui::icon::UPLOAD, "Sync now").clicked() {
                    self.upload_now();
                }
                if cfg.last_upload > 0 {
                    ui.weak(format!("Last sent {}", core::ago(cfg.last_upload)));
                }
            });
        });

        crate::ui::section(ui, "Loot history (RCLootCouncil)", |ui| {
            let (groups, loot) = {
                let st = i.st.lock().unwrap();
                (st.groups.clone().unwrap_or_default(), st.loot.clone())
            };
            let open: Vec<&Group> = groups.iter().filter(|g| g.can_import_loot_history).collect();
            if open.is_empty() {
                ui.label(
                    "None of your raid groups takes loot history from this app yet. \
                     A group admin turns it on for the group on wowutils.com.",
                );
                return;
            }
            ui.weak(
                "Awards from RCLootCouncil go into the ticked groups' loot history on wowutils.com, \
                 shared with everyone in the group. Awards deleted in RCLootCouncil are passed to the \
                 group's admins for review.",
            );
            let mut c = cfg.clone();
            for g in &open {
                let name = if g.name.is_empty() { g.id.clone() } else { g.name.clone() };
                let mut on = c.loot_history_groups.contains(&g.id);
                ui.horizontal_wrapped(|ui| {
                    if ui.checkbox(&mut on, &name).changed() {
                        if on {
                            c.loot_history_groups.push(g.id.clone());
                            i.common.info(format!("Now sending RCLootCouncil awards to {name}'s loot history."));
                        } else {
                            c.loot_history_groups.retain(|x| x != &g.id);
                            i.common.info(format!("Stopped sending RCLootCouncil awards to {name}'s loot history."));
                        }
                    }
                    if on {
                        if let Some(s) = loot.get(&g.id).filter(|s| !s.status.is_empty()) {
                            ui.weak(&s.status);
                        }
                    }
                });
            }
            if c.loot_history_groups != cfg.loot_history_groups {
                i.cfg.lock().unwrap().loot_history_groups = c.loot_history_groups;
                i.save();
                let mut st = i.st.lock().unwrap();
                st.loot.clear();
                st.next_heartbeat = 0;
                drop(st);
                i.common.wake();
            }
        });
    }
}

// ---- helpers -----------------------------------------------------------------------------

/// `selectedGroupIds` and `lootHistoryGroupIds` from a WowUtils Bridge running in a Wine prefix.
fn bridge_selected_groups() -> (Vec<String>, Vec<String>) {
    let configs: Vec<Value> = core::wine::prefixes_with("users")
        .iter()
        .flat_map(|p| core::wine::user_profiles(p))
        .filter_map(|u| std::fs::read(u.join("AppData/Local/WowUtilsBridge/config.json")).ok())
        .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
        .collect();
    let ids = |key: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for id in configs.iter().flat_map(|v| v[key].as_array().cloned().unwrap_or_default()) {
            if let Some(id) = id.as_str().filter(|id| !out.iter().any(|x| x == id)) {
                out.push(id.to_string());
            }
        }
        out
    };
    (ids("selectedGroupIds"), ids("lootHistoryGroupIds"))
}

/// The Bridge's key for a game account: SHA-256 of its SavedVariables path, lowercase and
/// with forward slashes, first 16 hex digits.
fn account_key(sv_dir: &Path) -> String {
    let p = sv_dir.to_string_lossy().replace('\\', "/");
    core::sha256_hex(p.trim_end_matches('/').to_lowercase().as_bytes())[..16].to_string()
}

/// A group's data as last fetched (`/bridge/v1/groups/{id}/loot`).
fn group_payload(id: &str) -> Option<Value> {
    let cached: GroupData = serde_json::from_slice(&std::fs::read(Inner::group_cache_path(id)).ok()?).ok()?;
    serde_json::from_str(&cached.payload).ok()
}

/// The data version (ETag) of a group's cached data.
fn group_version(id: &str) -> Option<String> {
    let cached: GroupData = serde_json::from_slice(&std::fs::read(Inner::group_cache_path(id)).ok()?).ok()?;
    (!cached.etag.is_empty() && !cached.payload.is_empty()).then_some(cached.etag)
}

/// Characters on a group's roster as (lowercase name, realm id), from the group data's
/// character list and its sync list.
fn roster_entries(payload: &Value) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let chars = payload["characters"].as_array().into_iter().flatten();
    let synced = payload["syncList"]["characters"].as_array().into_iter().flatten();
    for c in chars.chain(synced) {
        let name = c["characterName"].as_str().or(c["name"].as_str()).unwrap_or_default().trim().to_lowercase();
        if !name.is_empty() {
            out.push((name, c["realmId"].as_i64().unwrap_or_default()));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Hash of what would be sent, without the time it was collected.
fn content_hash(body: &Value) -> String {
    let mut b = body.clone();
    if let Some(o) = b.as_object_mut() {
        o.remove("collectedAt");
    }
    core::sha256_hex(&serde_json::to_vec(&b).unwrap_or_default())[..12].to_string()
}

/// Reads `WowUtilsDB` and builds the `/bridge/v1/characters` body. `Ok(None)` when the
/// addon hasn't written its data yet.
fn read_upload(file: &Path) -> anyhow::Result<Option<Value>> {
    let src = std::fs::read(file)?;
    let Some(db) = lua::read_global(&src, "WowUtilsDB").map_err(|e| anyhow::anyhow!("{e}"))? else {
        return Ok(None);
    };
    let db = lua::to_json(&db);
    Ok(build_upload(&db, chrono::Utc::now()))
}

fn build_upload(db: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<Value> {
    let chars = |k: &str| -> Vec<Value> {
        match &db[k] {
            Value::Object(m) => m.values().filter(|c| c.is_object()).map(character).collect(),
            Value::Array(a) => a.iter().filter(|c| c.is_object()).map(character).collect(),
            _ => Vec::new(),
        }
    };
    let own = chars("ownCharacters");
    let others = chars("others");
    if own.is_empty() && others.is_empty() {
        return None;
    }
    let region = db["lastCharacter"]["region"]
        .as_i64()
        .or_else(|| own.iter().chain(&others).find_map(|c| c["region"].as_i64()));
    // The API rejects nulls ("expected string, received null"): absent fields are left out.
    let mut body = json!({
        "schemaVersion": UPLOAD_SCHEMA,
        "collectedAt": now.timestamp(),
        "region": region,
        "lastSeenWeeklyReset": db["lastSeenWeeklyReset"],
        "ownCharacters": own,
        "others": others,
    });
    strip_nulls(&mut body);
    Some(body)
}

fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(strip_nulls);
        }
        Value::Array(a) => {
            a.retain(|x| !x.is_null());
            a.iter_mut().for_each(strip_nulls);
        }
        _ => {}
    }
}

/// A Lua table that may have come out as an array (keys 1..n) as an object.
fn as_object(v: &Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m.iter().filter(|(_, x)| !x.is_null()).map(|(k, x)| (k.clone(), x.clone())).collect(),
        Value::Array(a) => {
            a.iter().enumerate().filter(|(_, x)| !x.is_null()).map(|(i, x)| ((i + 1).to_string(), x.clone())).collect()
        }
        _ => Map::new(),
    }
}

fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Object(m) => {
            let mut e: Vec<(i64, Value)> = m.iter().filter_map(|(k, v)| k.parse().ok().map(|k| (k, v.clone()))).collect();
            e.sort_by_key(|x| x.0);
            e.into_iter().map(|x| x.1).collect()
        }
        _ => Vec::new(),
    }
}

fn pick(src: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().map(|k| (k.to_string(), src.get(*k).cloned().unwrap_or(Value::Null))).collect())
}

/// One character the way the Bridge sends it: the addon's record with a fixed set of
/// fields; the addon's own bookkeeping (`items`, `extra`, slugs, `itemCache`) stays home.
fn character(c: &Value) -> Value {
    let mut out = Map::new();
    for k in [
        "guid", "name", "class", "region", "droptimizerKey", "guildSlug", "lastUpdate", "lastUpdateReceived",
        "lastLogout", "lastWeeklyReset", "currencyUpdated", "watermarksUpdated", "craftingItems",
        "craftingItemsUpdated", "questsUpdated", "vaultDataLastUpdate", "bonusCoinUsageUpdated", "weeklyRewards",
        "weeklyRewardsUpdate", "lockouts", "lockoutsUpdated", "dungeonsDone", "dungeonsUpdated", "addonVersion",
    ] {
        out.insert(k.into(), c.get(k).cloned().unwrap_or(Value::Null));
    }
    let dict = |k: &str| c.get(k).filter(|v| !v.is_null()).map(|v| Value::Object(as_object(v)));
    out.insert("watermarks".into(), dict("watermarks").unwrap_or(Value::Null));
    for k in ["weeklyRewards", "dungeonsDone"] {
        if let Some(v) = dict(k) {
            out.insert(k.into(), v);
        }
    }
    out.insert(
        "currency".into(),
        c.get("currency").filter(|v| !v.is_null()).map_or(Value::Null, |v| {
            Value::Object(as_object(v).into_iter().map(|(k, x)| (k, pick(&x, &["current", "totalEarned"]))).collect())
        }),
    );
    out.insert(
        "quests".into(),
        c.get("quests").filter(|v| !v.is_null()).map_or(Value::Null, |v| {
            Value::Object(
                as_object(v).into_iter().map(|(k, x)| (k, pick(&x, &["completed", "completedWarbound", "isWeeklyQuest"]))).collect(),
            )
        }),
    );
    out.insert(
        "vaultData".into(),
        Value::Array(
            c.get("vaultData")
                .map(as_list)
                .unwrap_or_default()
                .iter()
                .map(|x| {
                    pick(
                        x,
                        &["itemId", "itemLevel", "itemLink", "itemLocationId", "itemClassId", "itemSubClassId", "tertiary", "claimID", "picked"],
                    )
                })
                .collect(),
        ),
    );
    out.insert(
        "bonusCoinUsage".into(),
        c.get("bonusCoinUsage").filter(|v| !v.is_null()).map_or(Value::Null, |v| {
            Value::Array(
                as_list(v)
                    .iter()
                    .map(|x| pick(x, &["difId", "itemLink", "encounterId", "receiveTime", "specId", "season", "instanceId"]))
                    .collect(),
            )
        }),
    );
    Value::Object(out)
}

/// A JSON value as a Lua literal the way the Bridge writes it: `["key"]=value`, arrays
/// as plain lists, nulls left out.
fn lua_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("nil"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&lua::quote(s)),
        Value::Array(a) => {
            out.push('{');
            let mut first = true;
            for x in a.iter().filter(|x| !x.is_null()) {
                if !first {
                    out.push(',');
                }
                first = false;
                lua_value(x, out);
            }
            out.push('}');
        }
        Value::Object(m) => {
            out.push('{');
            let mut first = true;
            for (k, x) in m.iter().filter(|(_, x)| !x.is_null()) {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push('[');
                out.push_str(&lua::quote(k));
                out.push_str("]=");
                lua_value(x, out);
            }
            out.push('}');
        }
    }
}

/// The `rev=` of an existing data.lua the Bridge (or this app) wrote, and its hash.
fn existing_data(path: &Path) -> Option<(i64, String)> {
    let head: String = std::fs::read_to_string(path).ok()?.lines().take(2).collect::<Vec<_>>().join("\n");
    if !head.contains(GENERATED_MARK) {
        return None;
    }
    let field = |name: &str| head.split_whitespace().find_map(|w| w.strip_prefix(name)).map(String::from);
    Some((field("rev=")?.parse().ok()?, field("hash=").unwrap_or_default()))
}

/// Writes `wowutils_data/data.lua` (and the addon's two other files when missing) from
/// the groups' payloads as the site sent them. Returns the new revision, or `None` when
/// the content was already there.
fn write_data_lua(dir: &Path, payloads: &[String]) -> anyhow::Result<Option<i64>> {
    let data = dir.join("data.lua");
    // Like the Bridge: the hash covers the payloads' JSON text as received.
    let hash = core::sha256_hex(format!("[{}]", payloads.join(",")).as_bytes())[..12].to_string();
    let existing = existing_data(&data);
    if data.exists() && existing.is_none() {
        let len = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
        if len > 200 {
            anyhow::bail!("{} wasn't written by the WowUtils Bridge or this app; leaving it alone", data.display());
        }
    }
    if existing.as_ref().is_some_and(|(_, h)| h == &hash) {
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    for (name, content) in [("wowutils_data.toc", DATA_TOC), ("core.lua", DATA_CORE)] {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(&p, content)?;
        }
    }
    let rev = existing.map_or(1, |(r, _)| r + 1);
    let now = chrono::Utc::now();
    let parsed = payloads.iter().map(|p| serde_json::from_str(p)).collect::<Result<Vec<Value>, _>>()?;
    let mut body = String::new();
    lua_value(&Value::Array(parsed), &mut body);
    let text = format!(
        "local addon_name, ns = ...\n{GENERATED_MARK} rev={rev} hash={hash} written={}\nns.data = {{[\"schemaVersion\"]={DATA_SCHEMA},[\"rev\"]={rev},[\"writtenAt\"]={},[\"groups\"]={body}}}\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Micros, false),
        now.timestamp()
    );
    core::write_atomic(&data, text.as_bytes())?;
    Ok(Some(rev))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_like_the_bridge() {
        let v = json!({"a": 1, "b": null, "c": ["x", null, true], "d": {"k\"": 1.5}});
        let mut s = String::new();
        lua_value(&v, &mut s);
        assert_eq!(s, r#"{["a"]=1,["c"]={"x",true},["d"]={["k\""]=1.5}}"#);
    }

    #[test]
    fn character_shape() {
        let c = json!({
            "guid": "Player-1", "name": "A", "class": "ROGUE", "region": 3, "items": [1], "extra": [],
            "quests": {"96441": {"completed": true, "completedWarbound": true}},
            "vaultData": [{"itemId": 1, "claimID": 2, "picked": false, "foo": 1}],
            "watermarks": [5, 6],
        });
        let o = character(&c);
        assert!(o.get("items").is_none() && o.get("extra").is_none());
        assert_eq!(o["quests"]["96441"]["isWeeklyQuest"], Value::Null);
        assert_eq!(o["vaultData"][0]["claimID"], 2);
        assert!(o["vaultData"][0].get("foo").is_none());
        assert_eq!(o["watermarks"]["2"], 6);
        assert_eq!(o["lastLogout"], Value::Null);
    }

    /// One worker round against the real site with the real config (as the app would do).
    /// `cargo test -- --ignored live_tick --nocapture`
    #[test]
    #[ignore]
    fn live_tick() {
        let w = WowUtils::new(core::Env::load());
        w.inner.tick();
        let st = w.inner.st.lock().unwrap();
        eprintln!("health: {:?}", w.inner.common.health());
        eprintln!("no_heartbeat={} next_heartbeat_in={}s scope={} groups={:?}", st.no_heartbeat,
            st.next_heartbeat - core::now_unix(), st.upload_scope,
            st.groups.as_ref().map(|g| g.iter().map(|g| (&g.name, g.changed, &g.role, g.can_import_loot_history)).collect::<Vec<_>>()));
        for a in &st.accounts {
            eprintln!("{} {} chars={} own_keys={} pending={} key={}", a.key, a.status, a.characters, a.own_keys.len(), a.pending, account_key(&a.sv_dir));
        }
        for l in w.inner.common.log.lock().unwrap().iter().rev() {
            eprintln!("log {:?}: {}", l.level, l.text);
        }
    }

    /// Loot history for one group, enabled in memory only (`WU_LH_GROUP=<id>`). With an
    /// empty roster on the site nothing is sent. `cargo test -- --ignored live_loot --nocapture`
    #[test]
    #[ignore]
    fn live_loot() {
        let w = WowUtils::new(core::Env::load());
        let group = std::env::var("WU_LH_GROUP").unwrap();
        w.inner.refresh_groups().unwrap();
        w.inner.cfg.lock().unwrap().loot_history_groups = vec![group.clone()];
        w.inner.sync_loot_history(true).unwrap();
        eprintln!("status: {:?}", w.inner.st.lock().unwrap().loot.get(&group).map(|s| s.status.clone()));
        for l in w.inner.common.log.lock().unwrap().iter().rev() {
            eprintln!("log {:?}: {}", l.level, l.text);
        }
    }

    /// Checks against a real WowUtils Bridge install (paths from the environment):
    /// WU_PAYLOAD = a Bridge cache/loot-<group>.json, WU_DATA = the data.lua it wrote,
    /// WU_SV = a wowutils.lua. `cargo test -- --ignored real_bridge_files`
    #[test]
    #[ignore]
    fn real_bridge_files() {
        let cache: Value = serde_json::from_slice(&std::fs::read(std::env::var("WU_PAYLOAD").unwrap()).unwrap()).unwrap();
        let payload = cache["payloadJson"].as_str().unwrap().to_string();
        let real = std::fs::read_to_string(std::env::var("WU_DATA").unwrap()).unwrap();
        let dir = std::env::temp_dir().join(format!("hearthlink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let up = read_upload(Path::new(&std::env::var("WU_SV").unwrap())).unwrap().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("upload.json"), serde_json::to_vec_pretty(&up).unwrap()).unwrap();
        eprintln!("upload written to {}", dir.display());
        write_data_lua(&dir, &[payload]).unwrap();
        let ours = std::fs::read_to_string(dir.join("data.lua")).unwrap();
        let body = |s: &str| s.split_once("[\"groups\"]=").unwrap().1.to_string();
        let hash = |s: &str| s.split_whitespace().find_map(|w| w.strip_prefix("hash=")).unwrap().to_string();
        assert_eq!(hash(&ours), hash(&real), "hash differs");
        assert!(body(&ours) == body(&real), "data.lua body differs");
        let up = read_upload(Path::new(&std::env::var("WU_SV").unwrap())).unwrap().unwrap();
        eprintln!("upload: {} own, {} others, {} bytes", up["ownCharacters"].as_array().unwrap().len(), up["others"].as_array().unwrap().len(), serde_json::to_vec(&up).unwrap().len());
        std::fs::write(dir.join("upload.json"), serde_json::to_vec_pretty(&up).unwrap()).unwrap();
        eprintln!("written to {}", dir.display());
    }
}
