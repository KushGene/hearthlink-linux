//! Goblinwise: installs and updates the Goblinwise Ledger addon in WoW Forever and hands the
//! addon's SavedVariables to goblinwise.eu/ledger through a small read-only HTTP endpoint on
//! 127.0.0.1. Nothing is uploaded anywhere: the website's page (running in the user's own
//! browser) reads the ledger from here and computes everything locally.
//!
//!   GET  /v1/status         {"app","version","enabled","addon":{"installed","job","error"}}  (also while turned off)
//!   GET  /v1/ledger         {"app","version","addon","ledgers":[{"account","modified","db"}]} (only while on)
//!   POST /v1/enable         turns the service on and installs the addon (button on the website)
//!   POST /v1/install-addon  installs/updates the addon (service must be on)
//!
//! POST needs the Goblinwise origin; other sites cannot trigger anything.
//!
//! Browsers may only read it from the Goblinwise site (CORS allow-list; Private Network
//! Access preflight answered); the Host header is checked against DNS rebinding.

use super::{Meta, Service};
use crate::core::{self, lua, store, wow, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub static META: Meta = Meta {
    id: "goblinwise",
    name: "Goblinwise",
    tagline: "Your gold, sales and purchases on goblinwise.eu – private, no account (WoW Forever)",
    icon: include_bytes!("../../assets/icons/goblinwise.png"),
    website: "https://goblinwise.eu/ledger",
    upstream_app: "Goblinwise Ledger",
    ported_version: "1.0.0",
    upstream_url: "https://goblinwise.eu/ledger",
    runs_official: true,
};

pub const PORT: u16 = 47291;
const ADDON: &str = "GoblinwiseLedger";
const SAVED: &str = "GoblinwiseLedger.lua";
const DEFAULT_SITE: &str = "https://goblinwise.eu";
/// Origins that may read the bridge besides the configured site (local development).
const DEV_ORIGINS: [&str; 2] = ["http://localhost:8080", "http://127.0.0.1:8080"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    /// The Goblinwise website (addon downloads, allowed browser origin).
    site: String,
    /// Install new addon versions automatically once the addon is installed.
    auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: false, site: DEFAULT_SITE.into(), auto_update: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Release {
    version: String,
    sha256: String,
    size: usize,
    url: String,
}

/// A parsed SavedVariables file, cached by modification time.
#[derive(Clone)]
struct Ledger {
    account: String,
    folder: String,
    modified: i64,
    db: Value,
    characters: usize,
    events: usize,
}

pub struct Goblinwise {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    http: reqwest::blocking::Client,
    latest: Mutex<Option<Release>>,
    folders: Mutex<Vec<wow::Flavor>>,
    cache: Mutex<HashMap<PathBuf, (SystemTime, Ledger)>>,
    /// Bridge state: Ok(()) while listening, Err(text) if the port could not be opened.
    bridge: Mutex<Option<Result<(), String>>>,
    /// When the website last read the ledger.
    last_read: Mutex<i64>,
    /// Addon install started from the website: "installing", "installed" or an error text.
    job: Mutex<Option<String>>,
}

impl Goblinwise {
    pub fn new(env: Arc<Env>) -> Self {
        let mut cfg: Config = store::load("goblinwise");
        if let Ok(site) = std::env::var("HEARTHLINK_GOBLINWISE_URL") {
            cfg.site = site; // development: e.g. http://127.0.0.1:8080
        }
        let ua = format!("hearthlink/{} (Linux; +https://github.com/KushGene/hearthlink-linux)", env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                env,
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                http: core::http_client(&ua, 60),
                latest: Mutex::new(None),
                folders: Mutex::new(Vec::new()),
                cache: Mutex::new(HashMap::new()),
                bridge: Mutex::new(None),
                last_read: Mutex::new(0),
                job: Mutex::new(None),
            }),
        }
    }
}

/// The `## Version:` of the installed addon in an AddOns folder.
fn installed_version(addons: &Path) -> Option<String> {
    let text = std::fs::read_to_string(addons.join(ADDON).join(format!("{ADDON}.toc"))).ok()?;
    text.lines().find_map(|l| l.strip_prefix("## Version:")).map(|v| v.trim().to_string())
}

/// Unpacks `GoblinwiseLedger/*` from the zip into `addons`, replacing an older copy in one step.
fn unpack(addons: &Path, zip_bytes: &[u8]) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))?;
    std::fs::create_dir_all(addons)?;
    let staging = addons.join(format!(".{ADDON}.new"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let mut files = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name()?.into_owned();
        let Some(rel) = name.strip_prefix(&format!("{ADDON}/")) else {
            continue;
        };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        // Only plain relative paths inside the addon folder.
        let rel_path = Path::new(rel);
        if rel_path.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            anyhow::bail!("unsafe path in the addon zip: {name}");
        }
        let target = staging.join(rel_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        std::fs::write(&target, data)?;
        files += 1;
    }
    if files == 0 || !staging.join(format!("{ADDON}.toc")).is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        anyhow::bail!("the download does not contain the {ADDON} addon");
    }
    let dest = addons.join(ADDON);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&staging, &dest)?;
    Ok(())
}

/// Origin of a site URL ("https://goblinwise.eu/x" → "https://goblinwise.eu").
fn origin_of(site: &str) -> String {
    let s = site.trim_end_matches('/');
    match s.find("://").map(|i| i + 3) {
        Some(start) => match s[start..].find('/') {
            Some(slash) => s[..start + slash].to_string(),
            None => s.to_string(),
        },
        None => s.to_string(),
    }
}

/// The origins allowed to read the bridge from a browser: the configured site, the public
/// site (also while a local test server is configured) and the local development servers.
fn allowed_origins(site: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for origin in [origin_of(site), DEFAULT_SITE.to_string()] {
        let www = origin.strip_prefix("https://").filter(|r| !r.starts_with("www.")).map(|r| format!("https://www.{r}"));
        for o in std::iter::once(origin).chain(www) {
            if !v.contains(&o) {
                v.push(o);
            }
        }
    }
    for o in DEV_ORIGINS {
        if !v.iter().any(|x| x == o) {
            v.push(o.to_string());
        }
    }
    v
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > 16 * 1024 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines.take_while(|l| !l.is_empty()).filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
    Some(Request { method, path, headers })
}

/// Status line, extra headers and body of a bridge response (pure, for tests).
fn respond(req: &Request, allowed: &[String], body: impl FnOnce(&str, &str) -> Option<(u16, Value)>) -> (u16, Vec<(String, String)>, String) {
    let mut headers = vec![("Cache-Control".to_string(), "no-store".to_string()), ("Vary".to_string(), "Origin".to_string())];
    // DNS rebinding: only our own loopback names.
    let host_ok = req.header("Host").is_some_and(|h| h == format!("127.0.0.1:{PORT}") || h == format!("localhost:{PORT}"));
    if !host_ok {
        return (403, headers, r#"{"error":"host"}"#.into());
    }
    match req.header("Origin") {
        Some(o) if allowed.iter().any(|a| a == o) => {
            headers.push(("Access-Control-Allow-Origin".into(), o.to_string()));
        }
        Some(_) => return (403, headers, r#"{"error":"origin"}"#.into()),
        // curl & co. on this computer may read (they can read the files anyway), never change.
        None if req.method == "POST" => return (403, headers, r#"{"error":"origin"}"#.into()),
        None => {}
    }
    if req.method == "OPTIONS" {
        headers.push(("Access-Control-Allow-Methods".into(), "GET, POST".into()));
        headers.push(("Access-Control-Max-Age".into(), "600".into()));
        if req.header("Access-Control-Request-Private-Network").is_some() {
            headers.push(("Access-Control-Allow-Private-Network".into(), "true".into()));
        }
        return (204, headers, String::new());
    }
    if req.method != "GET" && req.method != "POST" {
        return (405, headers, String::new());
    }
    let path = req.path.split('?').next().unwrap_or("");
    match body(&req.method, path) {
        Some((code, v)) => {
            headers.push(("Content-Type".into(), "application/json".into()));
            (code, headers, v.to_string())
        }
        None => (404, headers, r#"{"error":"not found"}"#.into()),
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        202 => "Accepted",
        403 => "Forbidden",
        409 => "Conflict",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

impl Inner {
    fn save(&self) {
        store::save("goblinwise", &*self.cfg.lock().unwrap());
    }

    fn forever_folders(&self) -> Vec<wow::Flavor> {
        self.env.flavors().into_iter().filter(wow::is_forever).collect()
    }

    fn release(&self) -> anyhow::Result<Release> {
        let site = self.cfg.lock().unwrap().site.clone();
        let url = format!("{}/addon/version.json", site.trim_end_matches('/'));
        let r: Release = self.http.get(url).send()?.error_for_status()?.json()?;
        *self.latest.lock().unwrap() = Some(r.clone());
        Ok(r)
    }

    /// Downloads the addon (checked against the SHA-256 the site lists) into every Forever folder.
    fn install(&self, only_installed: bool) -> anyhow::Result<usize> {
        let rel = self.release()?;
        let site = self.cfg.lock().unwrap().site.clone();
        let url = if rel.url.starts_with("http") { rel.url.clone() } else { format!("{}{}", site.trim_end_matches('/'), rel.url) };
        let bytes = self.http.get(url).send()?.error_for_status()?.bytes()?;
        if bytes.len() != rel.size || core::sha256_hex(&bytes) != rel.sha256 {
            anyhow::bail!("the download does not match its checksum; nothing was changed");
        }
        let mut n = 0;
        for f in self.forever_folders() {
            let addons = f.addons_dir();
            let have = installed_version(&addons);
            if only_installed && have.is_none() {
                continue;
            }
            if have.as_deref() == Some(rel.version.as_str()) {
                continue;
            }
            unpack(&addons, &bytes)?;
            self.common.good(format!("Goblinwise Ledger {} installed in {}", rel.version, f.display_name()));
            n += 1;
        }
        Ok(n)
    }

    /// All GoblinwiseLedger.lua files of all Forever accounts (re-read only when changed).
    fn ledgers(&self) -> Vec<Ledger> {
        let mut out = Vec::new();
        let mut cache = self.cache.lock().unwrap();
        for f in self.forever_folders() {
            for (account, sv) in f.accounts() {
                let path = sv.join(SAVED);
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if let Some((t, l)) = cache.get(&path) {
                    if *t == mtime {
                        out.push(l.clone());
                        continue;
                    }
                }
                let Ok(src) = std::fs::read(&path) else {
                    continue;
                };
                match lua::read_global(&src, "GoblinwiseLedgerDB") {
                    Ok(Some(v)) => {
                        let db = lua::to_json(&v);
                        let l = Ledger {
                            account: account.clone(),
                            folder: f.folder.clone(),
                            modified: mtime.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
                            characters: db["characters"].as_object().map_or(0, |m| m.len()),
                            events: db["events"].as_array().map_or(0, |a| a.len()),
                            db,
                        };
                        cache.insert(path, (mtime, l.clone()));
                        out.push(l);
                    }
                    Ok(None) => {}
                    Err(e) => self.common.warn(format!("{} ({account}) could not be read: {}", SAVED, e.message)),
                }
            }
        }
        out
    }

    fn addon_installed(&self) -> Option<String> {
        self.forever_folders().iter().find_map(|f| installed_version(&f.addons_dir()))
    }

    /// Installs the addon in the background; the website polls /v1/status for the result.
    fn start_install(self: &Arc<Self>) {
        {
            let mut job = self.job.lock().unwrap();
            if job.as_deref() == Some("installing") {
                return;
            }
            *job = Some("installing".into());
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let r = me.common.exclusive(|| me.install(false)).unwrap_or_else(|| Err(anyhow::anyhow!("Hearthlink is busy, try again in a moment")));
            *me.job.lock().unwrap() = Some(match r {
                Ok(_) => {
                    me.common.info("Restart WoW (or /reload) to load the Goblinwise Ledger addon.");
                    "installed".into()
                }
                Err(e) => {
                    let text = core::friendly_error(&e);
                    me.common.error(format!("Couldn't install the addon: {text}"));
                    text
                }
            });
            me.common.wake();
        });
    }

    fn status_json(&self) -> Value {
        let job = self.job.lock().unwrap().clone();
        let error = job.as_deref().filter(|j| *j != "installing" && *j != "installed").map(String::from);
        json!({
            "app": "hearthlink",
            "version": env!("CARGO_PKG_VERSION"),
            "enabled": self.cfg.lock().unwrap().enabled,
            "addon": { "installed": self.addon_installed(), "job": job.filter(|_| error.is_none()), "error": error, "gameFolders": self.forever_folders().len() },
        })
    }

    fn handle(self: &Arc<Self>, mut stream: TcpStream, allowed: &[String]) {
        let Some(req) = read_request(&mut stream) else {
            return;
        };
        let enabled = self.cfg.lock().unwrap().enabled;
        let (code, headers, body) = respond(&req, allowed, |method, path| match (method, path) {
            ("GET", "/v1/status") => Some((200, self.status_json())),
            ("GET", "/v1/ledger") if !enabled => Some((409, json!({ "error": "disabled" }))),
            ("GET", "/v1/ledger") => {
                *self.last_read.lock().unwrap() = core::now_unix();
                self.common.repaint();
                let ledgers: Vec<Value> = self.ledgers().into_iter().map(|l| json!({ "account": l.account, "folder": l.folder, "modified": l.modified, "db": l.db })).collect();
                let mut v = self.status_json();
                v["ledgers"] = Value::Array(ledgers);
                Some((200, v))
            }
            ("POST", "/v1/enable") => {
                if !enabled {
                    self.cfg.lock().unwrap().enabled = true;
                    self.save();
                    self.common.info("Turned on from the Goblinwise website.");
                    self.env.notify_info("Goblinwise turned on", "The Goblinwise website turned the service on and installs the Ledger addon.");
                    self.common.wake();
                }
                if self.addon_installed().is_none() {
                    self.start_install();
                }
                Some((202, self.status_json()))
            }
            ("POST", "/v1/install-addon") if !enabled => Some((409, json!({ "error": "disabled" }))),
            ("POST", "/v1/install-addon") => {
                self.start_install();
                Some((202, self.status_json()))
            }
            _ => None,
        });
        let mut head = format!("HTTP/1.1 {code} {}\r\nContent-Length: {}\r\nConnection: close\r\n", reason(code), body.len());
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    }

    fn serve(self: Arc<Self>) {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                *self.bridge.lock().unwrap() = Some(Err(format!("port {PORT} is in use ({e})")));
                self.common.warn(format!("The connection for goblinwise.eu could not start: port {PORT} is in use."));
                return;
            }
        };
        *self.bridge.lock().unwrap() = Some(Ok(()));
        // Always answers (also while turned off), so the website can offer to turn it on.
        for stream in listener.incoming().flatten() {
            let cfg = self.cfg.lock().unwrap().clone();
            let allowed = allowed_origins(&cfg.site);
            let me = self.clone();
            std::thread::spawn(move || me.handle(stream, &allowed));
        }
    }

    fn worker(self: Arc<Self>) {
        let c = self.common.clone();
        let mut last_check = 0i64;
        loop {
            let cfg = self.cfg.lock().unwrap().clone();
            *self.folders.lock().unwrap() = self.forever_folders();
            if !cfg.enabled {
                c.set_health(Health::Off, "Turned off");
                c.sleep(Duration::from_secs(30));
                continue;
            }
            if core::now_unix() - last_check > 6 * 3600 {
                last_check = core::now_unix();
                match self.release() {
                    Ok(rel) if cfg.auto_update => {
                        let outdated = self.addon_installed().is_some_and(|v| core::version_cmp(&rel.version, &v).is_gt());
                        if outdated {
                            if let Err(e) = c.exclusive(|| self.install(true)).unwrap_or(Ok(0)) {
                                c.warn(format!("Couldn't update the Goblinwise Ledger addon: {}", core::friendly_error(&e)));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => c.info(format!("goblinwise.eu not reachable: {}", core::friendly_error(&e))),
                }
            }
            let (h, t) = match (self.addon_installed(), self.bridge.lock().unwrap().clone()) {
                (_, Some(Err(e))) => (Health::Error, format!("Website connection off: {e}")),
                (None, _) if self.folders.lock().unwrap().is_empty() => (Health::Attention, "No WoW Forever game folder found".to_string()),
                (None, _) => (Health::Attention, "Install the Goblinwise Ledger addon".to_string()),
                (Some(v), _) => {
                    let read = *self.last_read.lock().unwrap();
                    let seen = if read > 0 { format!(", website read it {}", core::ago(read)) } else { String::new() };
                    (Health::Ok, format!("Addon {v} ready{seen}"))
                }
            };
            if !c.is_busy() {
                c.set_health(h, t);
            }
            c.sleep(Duration::from_secs(60));
        }
    }
}

impl Service for Goblinwise {
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
        let i = self.inner.clone();
        std::thread::spawn(move || i.worker());
        let i = self.inner.clone();
        std::thread::spawn(move || i.serve());
    }
    fn upload_now(&self) {
        let _ = open::that(format!("{}/ledger", self.inner.cfg.lock().unwrap().site.trim_end_matches('/')));
    }
    fn action_label(&self) -> &'static str {
        "Open my ledger"
    }
    fn action_icon(&self) -> &'static str {
        crate::ui::icon::EXTERNAL
    }
    fn in_upload_all(&self) -> bool {
        false
    }

    fn ui(&self, ui: &mut egui::Ui) {
        use crate::ui::{ib, icon};
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();
        let folders = i.folders.lock().unwrap().clone();
        let latest = i.latest.lock().unwrap().clone();
        let spawn = |f: Box<dyn FnOnce(&Arc<Inner>) + Send>| {
            let i = i.clone();
            std::thread::spawn(move || {
                i.common.exclusive(|| f(&i));
                i.common.wake();
            });
        };

        crate::ui::section(ui, "Addon", |ui| {
            if folders.is_empty() {
                ui.label("No WoW Forever game folder found.");
            }
            let mut rows = Vec::new();
            for f in &folders {
                let v = installed_version(&f.addons_dir()).unwrap_or_else(|| "not installed".into());
                rows.push((f.display_name(), v));
            }
            let rows: Vec<(&str, String, Option<String>)> = rows.iter().map(|(k, v)| (k.as_str(), v.clone(), None)).collect();
            crate::ui::facts(ui, "gw-addon", &rows);
            if let Some(l) = &latest {
                ui.weak(format!("Newest on {}: {}", origin_of(&cfg.site), l.version));
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let label = if i.addon_installed().is_some() { "Update the addon" } else { "Install the addon" };
                if crate::ui::primary(ui, !busy && !folders.is_empty(), icon::DOWNLOAD, label).clicked() {
                    spawn(Box::new(|i| match i.install(false) {
                        Ok(0) => i.common.info("The Goblinwise Ledger addon is up to date."),
                        Ok(_) => i.common.info("Restart WoW (or /reload) to load the addon."),
                        Err(e) => i.common.error(format!("Couldn't install the addon: {}", core::friendly_error(&e))),
                    }));
                }
            });
            let mut auto = cfg.auto_update;
            if ui.checkbox(&mut auto, "Keep the addon up to date").changed() {
                i.cfg.lock().unwrap().auto_update = auto;
                i.save();
            }
            crate::ui::hint(
                ui,
                "The addon records your auction house sales and purchases from the AH mails (open your mailbox), your gold, bags and bank. It never sends anything itself.",
            );
        });

        crate::ui::section(ui, "Website", |ui| {
            let ledgers = i.ledgers();
            let chars: usize = ledgers.iter().map(|l| l.characters).sum();
            let events: usize = ledgers.iter().map(|l| l.events).sum();
            let saved = ledgers.iter().map(|l| l.modified).max();
            let bridge = match i.bridge.lock().unwrap().clone() {
                Some(Ok(())) => format!("on (127.0.0.1:{PORT})"),
                Some(Err(e)) => e,
                None => "starting…".into(),
            };
            let read = *i.last_read.lock().unwrap();
            crate::ui::facts(
                ui,
                "gw-site",
                &[
                    ("Connection", bridge, Some(format!("Only {} can read it, from your browser. Nothing is uploaded.", origin_of(&cfg.site)))),
                    ("Ledger data", format!("{} account(s), {chars} character(s), {events} entries", ledgers.len()), None),
                    ("Saved by WoW", saved.map(core::ago).unwrap_or_else(|| "–".into()), Some("WoW writes the data on logout or /reload.".into())),
                    ("Read by the website", if read > 0 { core::ago(read) } else { "not yet".into() }, None),
                ],
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if crate::ui::primary(ui, true, icon::EXTERNAL, "Open my ledger").clicked() {
                    self.upload_now();
                }
            });
        });

        crate::ui::details(ui, "gw-details", |ui| {
            ui.horizontal(|ui| {
                ui.label("Website");
                let mut site = cfg.site.clone();
                if ui.text_edit_singleline(&mut site).lost_focus() && site != cfg.site && site.starts_with("http") {
                    i.cfg.lock().unwrap().site = site.trim_end_matches('/').to_string();
                    i.save();
                }
            });
            ui.weak("Change this only for testing a local Goblinwise server (e.g. http://127.0.0.1:8080).");
            ui.hyperlink_to(ib(icon::EXTERNAL, "About the ledger"), format!("{}/ledger", cfg.site.trim_end_matches('/')));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, host: &str, origin: Option<&str>) -> Request {
        let mut headers = vec![("Host".to_string(), host.to_string())];
        if let Some(o) = origin {
            headers.push(("Origin".to_string(), o.to_string()));
        }
        Request { method: method.into(), path: "/v1/ledger".into(), headers }
    }

    #[test]
    fn origins() {
        assert_eq!(origin_of("https://goblinwise.eu/"), "https://goblinwise.eu");
        assert_eq!(origin_of("http://127.0.0.1:8080/ledger"), "http://127.0.0.1:8080");
        let a = allowed_origins("https://goblinwise.eu");
        assert!(a.contains(&"https://www.goblinwise.eu".to_string()));
        assert_eq!(a.iter().filter(|o| *o == "https://goblinwise.eu").count(), 1);
        // A local test server configured: the public site keeps working.
        let dev = allowed_origins("http://127.0.0.1:8080");
        assert!(dev.contains(&"https://goblinwise.eu".to_string()));
        assert!(dev.contains(&"https://www.goblinwise.eu".to_string()));
        assert_eq!(dev.iter().filter(|o| *o == "http://127.0.0.1:8080").count(), 1);
    }

    #[test]
    fn bridge_answers_only_the_site_on_loopback() {
        let allowed = allowed_origins(DEFAULT_SITE);
        let host = format!("127.0.0.1:{PORT}");
        let ok = respond(&req("GET", &host, Some("https://goblinwise.eu")), &allowed, |_, _| Some((200, json!({"ok": true}))));
        assert_eq!(ok.0, 200);
        assert!(ok.1.iter().any(|(k, v)| k == "Access-Control-Allow-Origin" && v == "https://goblinwise.eu"));
        let evil = respond(&req("GET", &host, Some("https://evil.example")), &allowed, |_, _| Some((200, json!({}))));
        assert_eq!(evil.0, 403);
        let rebinding = respond(&req("GET", "evil.example:47291", Some("https://goblinwise.eu")), &allowed, |_, _| Some((200, json!({}))));
        assert_eq!(rebinding.0, 403);
        let mut pre = req("OPTIONS", &host, Some("https://goblinwise.eu"));
        pre.headers.push(("Access-Control-Request-Private-Network".into(), "true".into()));
        let pre = respond(&pre, &allowed, |_, _| None);
        assert_eq!(pre.0, 204);
        assert!(pre.1.iter().any(|(k, _)| k == "Access-Control-Allow-Private-Network"));
        // Changing requests need the site's origin: no origin or another one is refused before acting.
        let mut acted = false;
        assert_eq!(
            respond(&req("POST", &host, None), &allowed, |_, _| {
                acted = true;
                None
            })
            .0,
            403
        );
        assert_eq!(
            respond(&req("POST", &host, Some("https://evil.example")), &allowed, |_, _| {
                acted = true;
                None
            })
            .0,
            403
        );
        assert!(!acted);
        let post = respond(&req("POST", &host, Some("https://goblinwise.eu")), &allowed, |m, _| Some((202, json!({"method": m}))));
        assert_eq!(post.0, 202);
        assert_eq!(respond(&req("DELETE", &host, None), &allowed, |_, _| None).0, 405);
    }

    #[test]
    fn unpack_replaces_the_addon_and_rejects_unsafe_paths() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let make = |files: &[(&str, &str)]| {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for (n, c) in files {
                z.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(c.as_bytes()).unwrap();
            }
            z.finish().unwrap().into_inner()
        };
        let good = make(&[("GoblinwiseLedger/GoblinwiseLedger.toc", "## Version: 1.0.0\n"), ("GoblinwiseLedger/GoblinwiseLedger.lua", "--")]);
        std::fs::create_dir_all(dir.path().join("GoblinwiseLedger")).unwrap();
        std::fs::write(dir.path().join("GoblinwiseLedger/old.lua"), "old").unwrap();
        unpack(dir.path(), &good).unwrap();
        assert_eq!(installed_version(dir.path()).as_deref(), Some("1.0.0"));
        assert!(!dir.path().join("GoblinwiseLedger/old.lua").exists());
        let evil = make(&[("GoblinwiseLedger/GoblinwiseLedger.toc", "x"), ("GoblinwiseLedger/../../evil", "x")]);
        assert!(unpack(dir.path(), &evil).is_err());
        assert!(!dir.path().parent().unwrap().join("evil").exists());
    }

    #[test]
    fn reads_the_ledger_like_the_website_expects() {
        let src = br#"GoblinwiseLedgerDB = {
	["version"] = 1,
	["installation"] = "x",
	["characters"] = {
		["Gr\195\188nzahn-Forever"] = {
			["gold"] = { { 1700000000, 10000, }, },
			["bags"] = { [2589] = 20, },
			["bank"] = {},
		},
	},
	["events"] = { { ["id"] = 1, ["kind"] = "sale", ["t"] = 1700000100, ["total"] = 1500, }, },
}
"#;
        let v = lua::to_json(&lua::read_global(src, "GoblinwiseLedgerDB").unwrap().unwrap());
        let c = &v["characters"]["Grünzahn-Forever"];
        assert_eq!(c["gold"][0][1], 10000);
        assert_eq!(c["bags"]["2589"], 20);
        assert!(c["bank"].is_object());
        assert_eq!(v["events"][0]["kind"], "sale");
    }
}
