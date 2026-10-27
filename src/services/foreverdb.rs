//! ForeverDB: ForeverDB publishes its own Linux uploader, so this module installs it,
//! keeps it up to date and runs it (instead of re-implementing their upload format).
//! Its own settings stay in ~/.config/foreverdb-uploader.

use super::{Meta, Service};
use crate::core::{self, store, wow, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static META: Meta = Meta {
    id: "foreverdb",
    name: "ForeverDB",
    tagline: "Uploads for foreverdb.net and the ForeverDB addons (WoW Forever)",
    icon: include_bytes!("../../assets/icons/foreverdb.png"),
    website: "https://foreverdb.net/uploader",
    upstream_app: "ForeverDB Uploader",
    ported_version: "2.10.0",
    upstream_url: "https://foreverdb.net/uploader",
    runs_official: true,
};

const SITE: &str = "https://foreverdb.net/";
const MANIFEST: &str = "https://foreverdb.net/downloads/addon-manifest.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    /// Keep `foreverdb-uploader watch` running while this app runs.
    background: bool,
    /// The game folder handed to the uploader (`find <folder>`).
    game_folder: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct Status {
    fields: Vec<(String, String)>,
    read_at: i64,
}

impl Status {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
    fn version(&self) -> Option<String> {
        self.get("_version").map(String::from)
    }
}

pub struct ForeverDb {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    status: Mutex<Status>,
    watch: Mutex<Option<Child>>,
    link: Mutex<Option<(String, String)>>, // (code, url) while linking
    http: reqwest::blocking::Client,
    latest: Mutex<Option<String>>,
    /// Forever game folders as of the last status read (the page must not scan disks).
    folders: Mutex<Vec<wow::Flavor>>,
}

impl ForeverDb {
    pub fn new(env: Arc<Env>) -> Self {
        let first = !store::config_dir().join("foreverdb.json").exists();
        let mut cfg: Config = store::load("foreverdb");
        if first {
            cfg.enabled = true;
            cfg.background = true;
        }
        let ua = format!("hearthlink/{} (Linux; +https://github.com/KushGene/hearthlink-linux)", env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                env,
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                status: Mutex::new(Status::default()),
                watch: Mutex::new(None),
                link: Mutex::new(None),
                http: core::http_client(&ua, 300),
                latest: Mutex::new(None),
                folders: Mutex::new(Vec::new()),
            }),
        }
    }
}

fn managed_binary() -> PathBuf {
    store::data_dir().join("foreverdb").join("foreverdb-uploader")
}

/// The official uploader: one the user installed (~/.local/bin), else ours.
fn binary() -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    [home.join(".local/bin/foreverdb-uploader"), managed_binary()].into_iter().find(|p| p.is_file())
}

impl Inner {
    fn save(&self) {
        store::save("foreverdb", &*self.cfg.lock().unwrap());
    }

    /// The WoW Forever game folders: `_forever_`, or a classic folder running a 1.60+ client.
    fn forever_folders(&self) -> Vec<wow::Flavor> {
        self.env
            .flavors()
            .into_iter()
            .filter(|f| {
                if f.folder.contains("forever") {
                    return true;
                }
                let v = f.executable().and_then(|e| wow::exe_file_version(&e)).unwrap_or_default();
                let mut p = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
                p.next() == Some(1) && p.next().unwrap_or(0) >= 60
            })
            .collect()
    }

    fn manifest(&self) -> anyhow::Result<serde_json::Value> {
        let url = format!("{MANIFEST}?t={}", core::now_unix());
        Ok(self.http.get(url).send()?.error_for_status()?.json()?)
    }

    /// Downloads the official uploader (checking its SHA-256 against ForeverDB's manifest).
    fn install(&self) -> anyhow::Result<()> {
        let c = &self.common;
        c.info("Downloading the ForeverDB Uploader for Linux from foreverdb.net…");
        let m = self.manifest()?;
        let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
        let entry = &m["helperLinux"][arch];
        let url = entry["url"].as_str().ok_or_else(|| anyhow::anyhow!("the site lists no Linux uploader"))?;
        let sha = entry["sha256"].as_str().unwrap_or("").to_lowercase();
        let url = if url.starts_with("http") { url.to_string() } else { format!("{SITE}{url}") };
        let mut data = Vec::new();
        self.http.get(&url).send()?.error_for_status()?.read_to_end(&mut data)?;
        anyhow::ensure!(core::sha256_hex(&data) == sha, "the download was damaged (checksum didn't match)");
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&data[..]));
        let target = managed_binary();
        let mut found = false;
        for e in ar.entries()? {
            let mut e = e?;
            let p = e.path()?.into_owned();
            if p.file_name().is_some_and(|n| n == "foreverdb-uploader") && e.header().entry_type().is_file() {
                let mut bytes = Vec::new();
                e.read_to_end(&mut bytes)?;
                core::write_atomic(&target, &bytes)?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
                found = true;
            }
        }
        anyhow::ensure!(found, "the download has no uploader in it");
        let v = m["helperLinux"]["version"].as_str().unwrap_or("");
        c.good(format!("Installed the ForeverDB Uploader {v}."));
        Ok(())
    }

    /// Runs one uploader command, logging each line it prints.
    fn run(&self, args: &[&str]) -> anyhow::Result<Vec<String>> {
        let bin = binary().ok_or_else(|| anyhow::anyhow!("the ForeverDB Uploader isn't installed"))?;
        let mut child = Command::new(bin)
            .args(args)
            .env("FDB_UPLOADER_NO_WINDOW", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let err = child.stderr.take().unwrap();
        let errs = std::thread::spawn(move || BufReader::new(err).lines().map_while(Result::ok).collect::<Vec<_>>());
        let mut lines = Vec::new();
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            self.on_line(&line, args.first().copied() != Some("status"));
            lines.push(line);
        }
        let status = child.wait()?;
        for l in errs.join().unwrap_or_default() {
            if !l.trim().is_empty() {
                self.common.warn(l.clone());
            }
            lines.push(l);
        }
        anyhow::ensure!(status.success(), "the uploader stopped with {status}");
        Ok(lines)
    }

    /// Picks up a link code and URL from the uploader's output, and logs the line.
    fn on_line(&self, line: &str, log: bool) {
        let t = line.trim();
        if t.is_empty() {
            return;
        }
        if let Some(i) = t.find("https://") {
            let url: String = t[i..].chars().take_while(|c| !c.is_whitespace() && *c != ')').collect();
            if url.contains("link") {
                let code = url.rsplit('=').next().unwrap_or("").to_string();
                *self.link.lock().unwrap() = Some((code, url.trim_end_matches('.').to_string()));
            }
        }
        if !log {
            return;
        }
        let lower = t.to_lowercase();
        if lower.contains("couldn't") || lower.contains("failed") || lower.contains("error") {
            self.common.warn(t);
        } else if lower.starts_with("uploaded") || lower.starts_with("linked") || lower.starts_with("installed") || lower.contains("up to date") {
            self.common.good(t);
        } else {
            self.common.info(t);
        }
    }

    fn refresh_status(&self) {
        *self.folders.lock().unwrap() = self.forever_folders();
        let Ok(lines) = self.run(&["status"]) else { return };
        let mut st = Status { read_at: core::now_unix(), ..Default::default() };
        for (i, l) in lines.iter().enumerate() {
            if i == 0 {
                if let Some(v) = l.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                    st.fields.push(("_version".into(), v.to_string()));
                }
                continue;
            }
            if let Some((k, v)) = l.split_once(':') {
                st.fields.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        *self.status.lock().unwrap() = st;
        self.common.repaint();
    }

    fn stop_watch(&self) {
        if let Some(mut c) = self.watch.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Keeps `foreverdb-uploader watch` running (it uploads after you play and updates
    /// the addons every 6 hours) and streams what it says into the activity list.
    fn ensure_watch(self: &Arc<Self>) {
        let mut w = self.watch.lock().unwrap();
        if let Some(c) = w.as_mut() {
            if c.try_wait().ok().flatten().is_none() {
                return;
            }
        }
        let Some(bin) = binary() else { return };
        let mut cmd = Command::new(bin);
        cmd.arg("watch").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // The watcher must not outlive Hearthlink.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(mut child) => {
                for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)].into_iter().flatten() {
                    let me = self.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(pipe).lines().map_while(Result::ok) {
                            me.on_line(&l, true);
                        }
                    });
                }
                *w = Some(child);
            }
            Err(e) => self.common.error(format!("Couldn't start the uploader in the background: {e}")),
        }
    }

    fn choose_folder(&self, folder: &Path) {
        match self.run(&["find", &folder.display().to_string()]) {
            Ok(_) => {
                self.cfg.lock().unwrap().game_folder = Some(folder.to_path_buf());
                self.save();
                self.refresh_status();
            }
            Err(e) => self.common.error(format!("Couldn't set the game folder: {e}")),
        }
    }

    fn worker(self: Arc<Self>) {
        let c = self.common.clone();
        let mut last_self_update = 0i64;
        loop {
            let cfg = self.cfg.lock().unwrap().clone();
            if !cfg.enabled {
                self.stop_watch();
                c.set_health(Health::Off, "Turned off");
                c.sleep(Duration::from_secs(30));
                continue;
            }
            if binary().is_none() {
                c.set_health(Health::Busy, "Installing the official ForeverDB Uploader…");
                if let Err(e) = c.exclusive(|| self.install()).unwrap_or(Ok(())) {
                    c.error(format!("Couldn't install the ForeverDB Uploader: {}", core::friendly_error(&e)));
                    c.set_health(Health::Error, "The ForeverDB Uploader isn't installed (see Activity)");
                    c.sleep(Duration::from_secs(600));
                    continue;
                }
            }
            // Keep it current: the official uploader updates itself.
            if core::now_unix() - last_self_update > 6 * 3600 {
                last_self_update = core::now_unix();
                if let Ok(m) = self.manifest() {
                    let latest = m["helperLinux"]["version"].as_str().map(String::from);
                    *self.latest.lock().unwrap() = latest.clone();
                    self.refresh_status();
                    let have = self.status.lock().unwrap().version();
                    if let (Some(l), Some(h)) = (latest, have) {
                        if core::version_cmp(&l, &h).is_gt() {
                            c.info(format!("Updating the ForeverDB Uploader {h} → {l}…"));
                            self.stop_watch();
                            let r = if binary() == Some(managed_binary()) { self.install() } else { self.run(&["self-update"]).map(|_| ()) };
                            if let Err(e) = r {
                                c.warn(format!("Couldn't update the ForeverDB Uploader: {e}"));
                            }
                        }
                    }
                }
            }
            if self.status.lock().unwrap().read_at < core::now_unix() - 120 {
                self.refresh_status();
            }
            // Hand it the game folder once, if it has none.
            let st = self.status.lock().unwrap().clone();
            let no_folder = st.get("Game folder").is_some_and(|v| v.starts_with("(none"));
            if no_folder {
                if let Some(f) = self.forever_folders().first() {
                    c.info(format!("Telling the uploader to use {}", f.path.display()));
                    self.choose_folder(&f.path);
                }
            }
            let st = self.status.lock().unwrap().clone();
            let theirs_bg = st.get("Background").is_some_and(|v| v.starts_with("on"));
            if cfg.background && !theirs_bg {
                self.ensure_watch();
            } else {
                self.stop_watch();
            }
            let linked = st.get("Account").map(|a| !a.starts_with("not")).unwrap_or(false);
            let (h, t) = if !linked {
                (Health::Attention, "Not linked to a ForeverDB account".to_string())
            } else if st.get("Game folder").is_some_and(|v| v.starts_with("(none")) {
                (Health::Attention, "No WoW Forever game folder found".to_string())
            } else {
                let last = st.get("Last upload").unwrap_or("none yet").to_string();
                (Health::Ok, format!("Last upload: {last}"))
            };
            if !c.is_busy() {
                c.set_health(h, t);
            }
            c.sleep(Duration::from_secs(60));
        }
    }
}

impl Service for ForeverDb {
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
    }
    fn summary(&self) -> String {
        self.inner.common.health().1
    }
    fn upload_now(&self) {
        let i = self.inner.clone();
        std::thread::spawn(move || {
            i.common.exclusive(|| {
                i.common.set_health(Health::Busy, "Uploading…");
                if let Err(e) = i.run(&["upload"]) {
                    i.common.error(format!("Upload failed: {e}"));
                }
                i.refresh_status();
                i.common.wake();
            });
        });
    }

    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let st = i.status.lock().unwrap().clone();
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();
        let bin = binary();
        let spawn = |f: Box<dyn FnOnce(&Arc<Inner>) + Send>| {
            let i = i.clone();
            std::thread::spawn(move || {
                i.common.exclusive(|| f(&i));
                i.common.wake();
            });
        };

        crate::ui::section(ui, "Official ForeverDB Uploader", |ui| {
            ui.label("ForeverDB makes its own Linux uploader. Hearthlink installs it, keeps it up to date and runs it for you, so uploads are exactly what the site expects.");
            match bin.clone() {
                Some(b) => {
                    ui.horizontal(|ui| {
                        ui.label(format!("Version {}", st.version().unwrap_or_else(|| "?".into())));
                        if let Some(l) = i.latest.lock().unwrap().clone() {
                            if st.version().is_some_and(|v| core::version_cmp(&l, &v).is_gt()) {
                                ui.colored_label(crate::ui::ACCENT, format!("{l} is available (updating automatically)"));
                            }
                        }
                        ui.weak(b.display().to_string());
                    });
                }
                None => {
                    if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, "Install it"))).clicked() {
                        spawn(Box::new(|i| {
                            if let Err(e) = i.install() {
                                i.common.error(format!("Couldn't install: {}", core::friendly_error(&e)));
                            }
                        }));
                    }
                }
            }
        });

        crate::ui::section(ui, "Account", |ui| {
            let account = st.get("Account").unwrap_or("–").to_string();
            ui.label(format!("Account: {account}"));
            if let Some((code, url)) = i.link.lock().unwrap().clone() {
                ui.horizontal(|ui| {
                    ui.label(format!("Approve the code {code} on foreverdb.net"));
                    if ui.button(crate::ui::ib(crate::ui::icon::EXTERNAL, "Open the page")).clicked() {
                        let _ = open::that(&url);
                    }
                });
            }
            ui.horizontal(|ui| {
                if account.starts_with("not") || account == "–" {
                    if crate::ui::primary(ui, !busy && bin.is_some(), crate::ui::icon::LINK, "Link my ForeverDB account").clicked() {
                        spawn(Box::new(|i| {
                            i.common.info("Linking: a page opens in your browser to approve the code.");
                            if let Err(e) = i.run(&["link"]) {
                                i.common.error(format!("Linking failed: {e}"));
                            }
                            *i.link.lock().unwrap() = None;
                            i.refresh_status();
                        }));
                    }
                } else if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::UNLINK, "Unlink"))).clicked() {
                    spawn(Box::new(|i| {
                        let _ = i.run(&["unlink"]);
                        i.refresh_status();
                    }));
                }
            });
        });

        crate::ui::section(ui, "Game and addon", |ui| {
            egui::ScrollArea::horizontal().id_salt("fdb-status-scroll").show(ui, |ui| {
                egui::Grid::new("fdb-status").num_columns(2).striped(true).show(ui, |ui| {
                    for key in ["Game folder", "Addon", "Helper addon", "Data", "Last upload"] {
                        let value = st.get(key).unwrap_or("–");
                        if key == "Helper addon" && value.starts_with("not") {
                            // The old stand-alone ForeverDB_Helper; its features are in ForeverDB now.
                            ui.weak("Helper addon").on_hover_text(
                                "ForeverDB_Helper was a separate companion addon. Its goals, wishlist and dungeon planner \
                                 are part of the ForeverDB addon now, so it isn't needed. If an old ForeverDB_Helper \
                                 folder is still there, the uploader keeps it updated and carries its data over.",
                            );
                            ui.label("not needed (now part of the ForeverDB addon)");
                        } else {
                            ui.weak(key);
                            ui.label(value);
                        }
                        ui.end_row();
                    }
                });
            });
            let folders = i.folders.lock().unwrap().clone();
            if folders.len() > 1 || (folders.len() == 1 && cfg.game_folder.as_deref() != Some(folders[0].path.as_path())) {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Use:");
                    for f in folders {
                        if ui.add_enabled(!busy, egui::Button::new(f.display_name()).small()).on_hover_text(f.path.display().to_string()).clicked() {
                            let p = f.path.clone();
                            spawn(Box::new(move |i| i.choose_folder(&p)));
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if crate::ui::primary(ui, !busy && bin.is_some(), crate::ui::icon::UPLOAD, "Upload now").clicked() {
                    self.upload_now();
                }
                if ui.add_enabled(!busy && bin.is_some(), egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, "Install or update the addons"))).clicked() {
                    spawn(Box::new(|i| {
                        if let Err(e) = i.run(&["update"]) {
                            i.common.error(format!("Update failed: {e}"));
                        }
                        i.refresh_status();
                    }));
                }
                if ui.add_enabled(!busy && bin.is_some(), egui::Button::new(crate::ui::ib(crate::ui::icon::SYNC, "Refresh"))).clicked() {
                    spawn(Box::new(|i| i.refresh_status()));
                }
            });
        });

        crate::ui::section(ui, "Options", |ui| {
            let mut bg = cfg.background;
            if ui.checkbox(&mut bg, "Upload after I play and keep the addons updated (while Hearthlink runs)").changed() {
                i.cfg.lock().unwrap().background = bg;
                i.save();
                i.common.wake();
            }
            if st.get("Background").is_some_and(|v| v.starts_with("on")) {
                ui.weak("The uploader's own background service is on, so Hearthlink leaves the background work to it.");
            }
            ui.horizontal(|ui| {
                if ui.button(crate::ui::ib(crate::ui::icon::FILE, "Open its log")).clicked() {
                    if let Some(l) = st.get("Log") {
                        let _ = open::that(l);
                    }
                }
                ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "What it reads and sends"), "https://foreverdb.net/uploader");
            });
        });
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop_watch();
    }
}
