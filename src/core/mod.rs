pub mod lua;
pub mod oauth;
pub mod store;
pub mod wine;
pub mod wow;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const APP_NAME: &str = "Hearthlink";
/// Folder names (~/.config/hearthlink, …) and the single-instance socket.
pub const APP_ID: &str = "hearthlink";
/// The desktop/AppStream ID: window app id, .desktop file and icon name.
pub const DESKTOP_ID: &str = "io.github.kushgene.Hearthlink";
pub const REPO: &str = "KushGene/hearthlink-linux";

/// How this copy was installed, for the autostart entry and update hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    Flatpak,
    AppImage,
    Package,
    Local,
}

pub fn install_kind() -> Install {
    if std::path::Path::new("/.flatpak-info").exists() {
        Install::Flatpak
    } else if std::env::var_os("APPIMAGE").is_some() {
        Install::AppImage
    } else if std::env::current_exe().is_ok_and(|e| e.starts_with("/usr")) {
        Install::Package
    } else {
        Install::Local
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Settings shared by every service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub wow_root: Option<PathBuf>,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub notifications: bool,
    /// Upstream versions the user has already seen a notice for, per service.
    pub dismissed_upstream: std::collections::BTreeMap<String, String>,
    pub first_run_done: bool,
    /// Newest Hearthlink release the user was already told about.
    pub notified_app_version: Option<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            wow_root: None,
            start_minimized: false,
            close_to_tray: true,
            notifications: true,
            dismissed_upstream: Default::default(),
            first_run_done: false,
            notified_app_version: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: i64,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Health {
    #[default]
    Idle,
    Ok,
    Busy,
    Attention,
    Error,
    Off,
}

/// What the upstream (Windows) app looks like right now, compared to the version this
/// port was built from.
#[derive(Debug, Clone, Default)]
pub struct Upstream {
    pub latest: Option<String>,
    pub checked_at: i64,
    pub notes: Option<String>,
    pub error: Option<String>,
}

/// State every service shares with the UI.
#[derive(Default)]
pub struct Common {
    pub log: Mutex<VecDeque<LogLine>>,
    pub health: Mutex<(Health, String)>,
    pub upstream: Mutex<Upstream>,
    pub busy: AtomicBool,
    wake: (Mutex<bool>, Condvar),
    pub repaint: Mutex<Option<eframe::egui::Context>>,
}

impl Common {
    pub fn log(&self, level: Level, text: impl Into<String>) {
        let text = text.into();
        eprintln!("[{level:?}] {text}");
        let mut l = self.log.lock().unwrap();
        l.push_front(LogLine { at: now_unix(), level, text });
        l.truncate(400);
        drop(l);
        self.repaint();
    }
    pub fn info(&self, t: impl Into<String>) {
        self.log(Level::Info, t)
    }
    pub fn good(&self, t: impl Into<String>) {
        self.log(Level::Good, t)
    }
    pub fn warn(&self, t: impl Into<String>) {
        self.log(Level::Warn, t)
    }
    pub fn error(&self, t: impl Into<String>) {
        self.log(Level::Error, t)
    }

    pub fn set_health(&self, h: Health, text: impl Into<String>) {
        *self.health.lock().unwrap() = (h, text.into());
        self.repaint();
    }

    pub fn health(&self) -> (Health, String) {
        self.health.lock().unwrap().clone()
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    /// Wakes the service's worker loop early (after a button press).
    pub fn wake(&self) {
        let (m, c) = &self.wake;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    /// Sleeps up to `d`, returning early when woken.
    pub fn sleep(&self, d: Duration) {
        let (m, c) = &self.wake;
        let g = m.lock().unwrap();
        let (mut g, _) = c.wait_timeout_while(g, d, |woken| !*woken).unwrap();
        *g = false;
    }

    /// Runs `f` unless another job of this service is already running.
    pub fn exclusive<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return None;
        }
        let r = f();
        self.busy.store(false, Ordering::SeqCst);
        self.repaint();
        Some(r)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

/// Things every service can reach: the shared settings and an HTTP client.
pub struct Env {
    pub config: RwLock<GlobalConfig>,
    pub ctx: Mutex<Option<eframe::egui::Context>>,
}

impl Env {
    pub fn load() -> Arc<Self> {
        let mut cfg: GlobalConfig = store::load("config");
        if cfg.wow_root.as_ref().is_none_or(|p| !wow::is_install_root(p)) {
            cfg.wow_root = wow::detect_installs().into_iter().next().or(cfg.wow_root);
        }
        Arc::new(Self { config: RwLock::new(cfg), ctx: Mutex::new(None) })
    }

    pub fn save(&self) {
        store::save("config", &*self.config.read().unwrap());
    }

    pub fn wow_root(&self) -> Option<PathBuf> {
        self.config.read().unwrap().wow_root.clone()
    }

    pub fn flavors(&self) -> Vec<wow::Flavor> {
        self.wow_root().map(|r| wow::flavors(&r)).unwrap_or_default()
    }

    pub fn notify(&self, title: &str, body: &str) {
        if !self.config.read().unwrap().notifications {
            return;
        }
        let _ = notify_rust::Notification::new()
            .appname(APP_NAME)
            .summary(title)
            .body(body)
            .icon(DESKTOP_ID)
            .timeout(notify_rust::Timeout::Milliseconds(6000))
            .show();
    }
}

/// A user agent for a service, saying honestly that this is the Linux app.
pub fn http_client(user_agent: &str, timeout_secs: u64) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout_secs))
        .gzip(true)
        .build()
        .expect("http client")
}

/// Compares dotted version strings numerically (`2.10.0` > `2.9.1`).
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches(['v', 'V'])
            .split(['.', '-', '+'])
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
            .collect()
    };
    let (mut x, mut y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x.cmp(&y)
}

pub fn friendly_error(e: &dyn std::fmt::Display) -> String {
    let s = e.to_string();
    if s.contains("dns error") || s.contains("Connection refused") || s.contains("error sending request") {
        "couldn't reach the site (no internet connection?)".into()
    } else if s.contains("timed out") || s.contains("operation timed out") {
        "the connection timed out".into()
    } else {
        s
    }
}

pub fn fmt_time(t: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(t, 0).single() {
        Some(dt) => {
            let today = Local::now().date_naive();
            if dt.date_naive() == today {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%d %b %H:%M").to_string()
            }
        }
        None => "–".into(),
    }
}

pub fn ago(t: i64) -> String {
    if t <= 0 {
        return "never".into();
    }
    let d = now_unix() - t;
    match d {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{} min ago", d / 60),
        d if d < 86400 => format!("{} h ago", d / 3600),
        d => format!("{} days ago", d / 86400),
    }
}

pub fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    hex::encode(md5::Md5::digest(data))
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).expect("gzip");
    e.finish().expect("gzip")
}

/// Writes a file atomically (temp file + rename), creating its folder.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}
