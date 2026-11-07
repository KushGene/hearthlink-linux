//! TradeSkillMaster: TSM publishes its own Linux desktop app (an Electron AppImage), so this
//! module installs it, keeps it up to date and runs it, instead of re-implementing it.
//!
//! The AppImage comes from the feed TSM's own updater reads (electron-updater, generic
//! provider: `latest-linux.yml`), checked against the SHA-512 listed there. Hearthlink unpacks
//! it (`--appimage-extract`) to ~/.local/share/hearthlink/tsm/app and starts `AppRun` from there,
//! so it runs without FUSE/libfuse2 and from the Flatpak (on the host). It adds a menu entry
//! (TradeSkillMaster.desktop, the app's own desktop name, so the window gets its icon) with
//! TSM's icons. The app's own settings and login stay in ~/.config/TradeSkillMaster.
//!
//! The app is tied to Hearthlink: while the service is on it starts with Hearthlink and closes
//! with it (PR_SET_PDEATHSIG, so also when Hearthlink is killed). Its own "Run at startup"
//! entry is removed, and the menu entry opens it through Hearthlink (`hearthlink --open-tsm`).

use super::{Meta, Service};
use crate::core::{self, store, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub static META: Meta = Meta {
    id: "tsm",
    name: "TradeSkillMaster",
    tagline: "The official TSM desktop app: auction data for the TSM addon",
    icon: include_bytes!("../../assets/icons/tsm.png"),
    website: "https://www.tradeskillmaster.com/",
    upstream_app: "TradeSkillMaster app",
    ported_version: "1.0.8",
    upstream_url: "https://www.tradeskillmaster.com/",
    runs_official: true,
};

/// What the app's own updater reads (resources/app-update.yml); redirects to the version folder.
const FEED: &str = "https://desktop-backend.tradeskillmaster.com/app-updates/release/latest-linux.yml";
/// The app's process name (`tradeskillmaster`, cut to 15 characters by the kernel).
const PROCESS: &str = "tradeskillmaste";
/// The app's desktop name (package.json `desktopName`) and icon name.
const DESKTOP_FILE: &str = "TradeSkillMaster.desktop";
const ICON_NAME: &str = "tradeskillmaster";
/// Marks a menu entry as Hearthlink's (others are left alone).
const MARK: &str = "X-Hearthlink=true";
/// The feed version Hearthlink installed, written into the unpacked app.
const VERSION_FILE: &str = ".hearthlink-version";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    /// Put TradeSkillMaster in the desktop's app menu.
    menu_entry: bool,
}

/// One release as the feed lists it.
#[derive(Debug, Clone, PartialEq)]
struct Release {
    version: String,
    url: String,
    sha512: String,
    size: u64,
}

pub struct Tsm {
    inner: Arc<Inner>,
}

struct Inner {
    common: Arc<Common>,
    cfg: Mutex<Config>,
    http: reqwest::blocking::Client,
    latest: Mutex<Option<Release>>,
    /// Facts for the page, refreshed by the worker (the page must not touch /proc or the disk).
    running: Mutex<bool>,
    version: Mutex<Option<String>>,
    /// Someone else's TradeSkillMaster menu entry, if there is one.
    foreign_entry: Mutex<Option<PathBuf>>,
    /// The app Hearthlink started, and when.
    child: Mutex<Option<(Child, Instant)>>,
    /// "Open the app" was asked for (the worker starts it, so it is tied to a thread that lives
    /// as long as Hearthlink).
    want_open: AtomicBool,
}

impl Tsm {
    pub fn new(_env: Arc<Env>) -> Self {
        let first = !store::config_dir().join("tsm.json").exists();
        let mut cfg: Config = store::load("tsm");
        if first {
            // The menu entry is on by default; the service itself stays off until turned on
            // (turning it on downloads a 150 MB app).
            cfg.menu_entry = true;
            store::save("tsm", &cfg);
        }
        let ua = format!("hearthlink/{} (Linux; +https://github.com/KushGene/hearthlink-linux)", env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                // The blocking client's timeout applies to each read of the body, not to the
                // whole download, so it only catches a stalled connection.
                http: core::http_client(&ua, 60),
                latest: Mutex::new(None),
                running: Mutex::new(false),
                version: Mutex::new(installed_version()),
                foreign_entry: Mutex::new(None),
                child: Mutex::new(None),
                want_open: AtomicBool::new(false),
            }),
        }
    }
}

fn base_dir() -> PathBuf {
    store::data_dir().join("tsm")
}
fn app_dir() -> PathBuf {
    base_dir().join("app")
}
/// A newer version, unpacked while the app was running; it replaces `app` at a restart.
fn staged_dir() -> PathBuf {
    base_dir().join("staged")
}
fn app_run() -> PathBuf {
    app_dir().join("AppRun")
}
fn menu_entry_path() -> PathBuf {
    core::host_data_dir().join("applications").join(DESKTOP_FILE)
}
fn icons_dir() -> PathBuf {
    core::host_data_dir().join("icons/hicolor")
}

/// The version of the unpacked app in `dir`: the one Hearthlink noted when installing it, or
/// else X-AppImage-Version of the app's desktop file (whatever its name's case).
fn version_in(dir: &Path) -> Option<String> {
    if let Some(v) = std::fs::read_to_string(dir.join(VERSION_FILE)).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return Some(v);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("desktop")))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|t| t.lines().find_map(|l| l.strip_prefix("X-AppImage-Version=")).map(|v| v.trim().to_string()))
}

/// True when the app is installed (it can be started).
pub fn installed() -> bool {
    app_run().is_file()
}

fn installed_version() -> Option<String> {
    if !installed() {
        return None;
    }
    // An app that doesn't say its version is still installed (it is updated at the next check).
    Some(version_in(&app_dir()).unwrap_or_else(|| "unknown".into()))
}

/// The app's own autostart entries (its "Run at startup" setting, via auto-launch).
fn own_autostarts() -> Vec<PathBuf> {
    let dir = core::host_config_dir().join("autostart");
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_lowercase().starts_with("tradeskillmaster")))
        .collect()
}

/// Removes the app's own autostart entries that start Hearthlink's copy. True if any was.
fn remove_own_autostarts() -> bool {
    // count(), not any(): every one of them goes.
    own_autostarts().iter().filter(|a| own_autostart_is_ours(a) && std::fs::remove_file(a).is_ok()).count() > 0
}

/// True when the app's own autostart entry starts Hearthlink's copy (removed: Hearthlink
/// starts it).
fn own_autostart_is_ours(entry: &Path) -> bool {
    let base = base_dir().display().to_string();
    std::fs::read_to_string(entry).is_ok_and(|t| t.lines().any(|l| l.starts_with("Exec=") && l.contains(&base)))
}

/// Reads `latest-linux.yml` (only the top-level keys and the first file's size).
fn parse_feed(text: &str) -> Option<(String, String, String, u64)> {
    let unquote = |v: &str| v.trim().trim_matches(['\'', '"']).to_string();
    let top = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':'))).map(unquote);
    let size = text
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("size:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    Some((top("version")?, top("path")?, top("sha512")?, size))
}

/// AppImages the user downloaded already (Downloads, ~/Applications): one that matches the
/// release is used instead of downloading it again.
fn local_copies(size: u64) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut dirs_: Vec<PathBuf> = vec![home.join("Downloads"), home.join("Applications")];
    if let Some(d) = dirs::download_dir() {
        if !dirs_.contains(&d) {
            dirs_.insert(0, d);
        }
    }
    dirs_
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
            n.starts_with("tradeskillmaster") && n.ends_with(".appimage")
        })
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() == size))
        .collect()
}

/// The command that starts the app (on the host, with the session's WAYLAND_DISPLAY back).
/// `tied`: in the Flatpak, the host app ends when Hearthlink's flatpak-spawn does.
fn app_command(tied: bool) -> std::process::Command {
    let wayland: Vec<(&str, &std::ffi::OsStr)> =
        core::SESSION_WAYLAND_DISPLAY.get().map(|w| ("WAYLAND_DISPLAY", w.as_os_str())).into_iter().collect();
    core::host_app_command(&app_run(), Some(&app_dir()), &wayland, tied)
}

/// Waits up to `limit` for every process of the app to end. True when none is left.
fn wait_gone(limit: Duration) -> bool {
    let until = Instant::now() + limit;
    loop {
        if !core::process_running(PROCESS) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Asks every process of the app to end (one started without Hearthlink, or helpers left
/// behind) and waits for them. True when none is left.
fn close_all() -> bool {
    let _ = core::host_command(Path::new("pkill"), None)
        .args(["-TERM", "-x", PROCESS])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    wait_gone(Duration::from_secs(10))
}

fn sha512_b64_of(path: &Path) -> std::io::Result<String> {
    use base64::Engine;
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha512::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(h.finalize()))
}

impl Inner {
    fn save(&self) {
        store::save("tsm", &*self.cfg.lock().unwrap());
    }

    fn refresh_facts(&self) {
        *self.running.lock().unwrap() = core::process_running(PROCESS);
        *self.version.lock().unwrap() = installed_version();
        let entry = menu_entry_path();
        *self.foreign_entry.lock().unwrap() = (entry.exists() && !Self::is_ours(&entry)).then_some(entry);
        self.common.repaint();
    }

    fn release(&self) -> anyhow::Result<Release> {
        let resp = self.http.get(FEED).send()?.error_for_status()?;
        // The feed redirects to the version's folder; the AppImage's path is relative to it.
        let base = resp.url().clone();
        let text = resp.text()?;
        let (version, path, sha512, size) = parse_feed(&text).ok_or_else(|| anyhow::anyhow!("TSM's update feed lists no Linux app"))?;
        let url = base.join(&path)?.to_string();
        Ok(Release { version, url, sha512, size })
    }

    /// Downloads the AppImage to `to`, checking its SHA-512 (or copies a matching local one).
    fn fetch(&self, r: &Release, to: &Path) -> anyhow::Result<()> {
        let c = &self.common;
        for p in local_copies(r.size) {
            if sha512_b64_of(&p).is_ok_and(|h| h == r.sha512) {
                c.info(format!("Using the TradeSkillMaster {} AppImage in {} (it matches the official one).", r.version, p.display()));
                std::fs::copy(&p, to)?;
                return Ok(());
            }
        }
        c.info(format!("Downloading TradeSkillMaster {} for Linux from tradeskillmaster.com…", r.version));
        use base64::Engine;
        use sha2::Digest;
        let mut resp = self.http.get(&r.url).send()?.error_for_status()?;
        // The size, if the server or the feed says it (else progress is shown in MB).
        let total = resp.content_length().filter(|&n| n > 0).or((r.size > 0).then_some(r.size));
        let mut out = std::fs::File::create(to)?;
        let mut h = sha2::Sha512::new();
        let mut buf = vec![0u8; 1 << 16];
        let (mut done, mut shown) = (0u64, 0u64);
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            h.update(&buf[..n]);
            done += n as u64;
            let (step, text) = match total {
                Some(t) => {
                    let pct = (done * 100 / t).min(100);
                    (pct / 5, format!("{pct}%"))
                }
                None => {
                    let mb = done >> 20;
                    (mb / 10, format!("{mb} MB"))
                }
            };
            if step != shown {
                shown = step;
                c.set_health(Health::Busy, format!("Downloading TradeSkillMaster {}… {text}", r.version));
            }
        }
        out.flush()?;
        let got = base64::engine::general_purpose::STANDARD.encode(h.finalize());
        anyhow::ensure!(got == r.sha512, "the download was damaged (checksum didn't match)");
        Ok(())
    }

    /// Installs or updates the app: fetch, unpack, and put it in place (or stage it while the
    /// app runs).
    fn install(&self, r: &Release) -> anyhow::Result<()> {
        let c = &self.common;
        c.set_health(Health::Busy, format!("Installing TradeSkillMaster {}…", r.version));
        let base = base_dir();
        std::fs::create_dir_all(&base)?;
        let image = base.join("TradeSkillMaster.AppImage");
        let work = base.join("unpack");
        let result = (|| {
            self.fetch(r, &image)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755))?;
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work)?;
            let out = core::host_command(&image, Some(&work)).arg("--appimage-extract").stdin(Stdio::null()).output()?;
            anyhow::ensure!(out.status.success(), "couldn't unpack the AppImage: {}", String::from_utf8_lossy(&out.stderr).trim());
            let root = work.join("squashfs-root");
            anyhow::ensure!(root.join("AppRun").is_file(), "the AppImage has no app in it");
            let got = version_in(&root).unwrap_or_default();
            if !got.is_empty() && got != r.version {
                c.warn(format!("The AppImage says it is version {got}, the feed said {}.", r.version));
            }
            // The feed's version, so the next check compares like with like.
            std::fs::write(root.join(VERSION_FILE), &r.version)?;
            let _ = std::fs::remove_dir_all(staged_dir());
            std::fs::rename(&root, staged_dir())?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&image);
        let _ = std::fs::remove_dir_all(&work);
        result?;
        if !self.swap_in()? {
            c.info(format!("TradeSkillMaster {} is ready: it is put in place when the app restarts.", r.version));
        }
        Ok(())
    }

    /// Puts a staged version in place unless the app runs. True when nothing is left staged.
    fn swap_in(&self) -> anyhow::Result<bool> {
        let staged = staged_dir();
        if !staged.exists() {
            return Ok(true);
        }
        if core::process_running(PROCESS) {
            return Ok(false);
        }
        let old = base_dir().join("old");
        let _ = std::fs::remove_dir_all(&old);
        if app_dir().exists() {
            std::fs::rename(app_dir(), &old)?;
        }
        std::fs::rename(&staged, app_dir())?;
        let _ = std::fs::remove_dir_all(&old);
        let v = installed_version().unwrap_or_else(|| "?".into());
        self.common.good(format!("Installed TradeSkillMaster {v}."));
        self.sync_menu_entry(true);
        self.refresh_facts();
        Ok(true)
    }

    fn is_ours(entry: &Path) -> bool {
        std::fs::read_to_string(entry).is_ok_and(|t| t.lines().any(|l| l.trim() == MARK))
    }

    /// Writes or removes the menu entry and icons to match the settings. `force` rewrites them
    /// (after an update, the icons may be new).
    fn sync_menu_entry(&self, force: bool) {
        let cfg = self.cfg.lock().unwrap().clone();
        let entry = menu_entry_path();
        let want = cfg.enabled && cfg.menu_entry && app_run().is_file();
        if entry.exists() && !Self::is_ours(&entry) {
            return; // Someone else's TradeSkillMaster entry (another install): leave it.
        }
        if !want {
            if entry.exists() {
                let _ = std::fs::remove_file(&entry);
                for size in self.icon_sizes() {
                    let _ = std::fs::remove_file(icons_dir().join(&size).join("apps").join(format!("{ICON_NAME}.png")));
                }
                self.refresh_desktop_caches();
            }
            return;
        }
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=TradeSkillMaster\nGenericName=Auction House Addon Companion\n\
             Comment=TradeSkillMaster desktop app (installed and kept up to date by Hearthlink)\n\
             Exec={} --open-tsm\nIcon={ICON_NAME}\nTerminal=false\nCategories=Game;\nKeywords=tsm;wow;warcraft;auction;\n\
             StartupWMClass=TradeSkillMaster\n{MARK}\n",
            core::self_exec()
        );
        if !force && std::fs::read_to_string(&entry).is_ok_and(|t| t == text) {
            return;
        }
        // TSM's own icons, all sizes it ships.
        let src = app_dir().join("usr/share/icons/hicolor");
        for size in self.icon_sizes() {
            let from = src.join(&size).join("apps").join(format!("{ICON_NAME}.png"));
            let to = icons_dir().join(&size).join("apps").join(format!("{ICON_NAME}.png"));
            if let Ok(bytes) = std::fs::read(&from) {
                let _ = core::write_atomic(&to, &bytes);
            }
        }
        match core::write_atomic(&entry, text.as_bytes()) {
            Ok(()) => self.common.info("Added TradeSkillMaster to the app menu."),
            Err(e) => self.common.warn(format!("Couldn't add TradeSkillMaster to the app menu: {e}")),
        }
        self.refresh_desktop_caches();
    }

    /// The icon sizes the app ships (`16x16` … `1024x1024`).
    fn icon_sizes(&self) -> Vec<String> {
        let src = app_dir().join("usr/share/icons/hicolor");
        let mut sizes: Vec<String> = std::fs::read_dir(src)
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).collect())
            .unwrap_or_default();
        if sizes.is_empty() {
            sizes = ["16x16", "24x24", "32x32", "48x48", "64x64", "96x96", "128x128", "256x256", "512x512", "1024x1024"]
                .map(String::from)
                .to_vec();
        }
        sizes
    }

    /// Updates the desktop's caches of entries and icons, where there are any (best effort; a
    /// cache that isn't there isn't made, as a stale one would hide other apps' icons later).
    fn refresh_desktop_caches(&self) {
        let apps = core::host_data_dir().join("applications");
        for (tool, arg, cache) in [
            ("update-desktop-database", apps.clone(), apps.join("mimeinfo.cache")),
            ("gtk-update-icon-cache", icons_dir(), icons_dir().join("icon-theme.cache")),
        ] {
            if !cache.exists() {
                continue;
            }
            let mut cmd = core::host_command(Path::new(tool), None);
            if tool == "gtk-update-icon-cache" {
                cmd.args(["-q", "-t", "-f"]);
            } else {
                cmd.arg("-q");
            }
            let _ = cmd.arg(arg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }

    /// True while the app Hearthlink started runs.
    fn ours_running(&self) -> bool {
        self.child.lock().unwrap().as_mut().is_some_and(|(c, _)| c.try_wait().ok().flatten().is_none())
    }

    /// Starts the app, tied to the calling thread (the worker): it gets SIGTERM when that
    /// thread ends, i.e. when Hearthlink quits or dies. What it prints goes to tsm/launch.log.
    fn launch(&self) {
        // Held while starting, so remove() (which turns the service off first and then stops
        // the app) either sees the new app or keeps it from starting.
        let mut guard = self.child.lock().unwrap();
        if !self.cfg.lock().unwrap().enabled {
            return;
        }
        if !app_run().is_file() {
            self.common.warn("TradeSkillMaster isn't installed yet.");
            return;
        }
        let log_path = base_dir().join("launch.log");
        let (out, err) = match std::fs::File::create(&log_path).and_then(|f| Ok((f.try_clone()?, f))) {
            Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
            Err(_) => (Stdio::null(), Stdio::null()),
        };
        let mut cmd = app_command(true);
        cmd.stdin(Stdio::null()).stdout(out).stderr(err);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                // Its own session (a Ctrl+C in Hearthlink's terminal isn't for it), but it ends
                // with Hearthlink.
                libc::setsid();
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                self.common.info("Starting TradeSkillMaster…");
                *guard = Some((child, Instant::now()));
            }
            Err(e) => self.common.error(format!("Couldn't start TradeSkillMaster: {e}")),
        }
    }

    /// Notices the app Hearthlink started has ended: says why if it stopped right away.
    fn reap(&self) {
        let mut guard = self.child.lock().unwrap();
        let Some((child, started)) = guard.as_mut() else { return };
        let Ok(Some(status)) = child.try_wait() else { return };
        let quick = started.elapsed() < Duration::from_secs(20);
        *guard = None;
        drop(guard);
        if quick && !status.success() {
            let log_path = base_dir().join("launch.log");
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = lines[lines.len().saturating_sub(6)..].join("\n");
            self.common.error(format!(
                "TradeSkillMaster stopped right away ({status}). {}",
                if tail.is_empty() { format!("It printed nothing ({}).", log_path.display()) } else { format!("It said:\n{tail}") }
            ));
        } else {
            self.common.info("TradeSkillMaster was closed. It starts again with Hearthlink, or with \"Open the app\".");
        }
    }

    /// Closes the app Hearthlink started (SIGTERM to its process group, then SIGKILL after
    /// 10 s), and waits a little for the app's other processes to end.
    fn stop(&self) {
        let Some((mut child, _)) = self.child.lock().unwrap().take() else { return };
        if child.try_wait().ok().flatten().is_none() {
            // It runs in its own session (setsid), so its id is also its process group's.
            let group = -(child.id() as i32);
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
            let mut ended = false;
            for _ in 0..100 {
                if child.try_wait().ok().flatten().is_some() {
                    ended = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !ended {
                unsafe {
                    libc::kill(group, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // Electron's helpers (same process name) can outlive the main process for a moment.
        wait_gone(Duration::from_secs(10));
    }

    /// Brings up the running app's window: a second start hands over to it and ends at once.
    fn show_window(&self) {
        let mut cmd = app_command(false);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Ok(mut c) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        }
    }

    fn remove(&self) {
        // Off first, so the worker doesn't start the app again meanwhile (launch() checks).
        self.cfg.lock().unwrap().enabled = false;
        self.save();
        self.stop();
        if core::process_running(PROCESS) {
            self.common.warn("TradeSkillMaster was turned off, but it is still running: close it, then remove the app.");
            self.refresh_facts();
            return;
        }
        self.sync_menu_entry(false);
        remove_own_autostarts();
        match std::fs::remove_dir_all(base_dir()) {
            Ok(()) => self.common.good("Removed the TradeSkillMaster app (its own settings in ~/.config/TradeSkillMaster are kept)."),
            Err(e) => self.common.error(format!("Couldn't remove the app: {e}")),
        }
        self.refresh_facts();
    }

    fn worker(self: Arc<Self>) {
        let c = self.common.clone();
        let mut last_check = 0i64;
        let mut first = true;
        // A restart couldn't put the update in place: don't keep restarting the app for it
        // (it goes in at the next start of Hearthlink, or of the app).
        let mut restart_failed = false;
        loop {
            let cfg = self.cfg.lock().unwrap().clone();
            self.sync_menu_entry(false);
            self.reap();
            if !cfg.enabled {
                if self.ours_running() {
                    c.info("Closing TradeSkillMaster (the service was turned off).");
                    self.stop();
                }
                self.want_open.store(false, Ordering::SeqCst);
                self.refresh_facts();
                c.set_health(Health::Off, "Turned off");
                first = true;
                c.sleep(Duration::from_secs(30));
                continue;
            }
            // New versions: every 6 hours (sooner when nothing is installed yet).
            let installed = installed_version();
            let due = if installed.is_none() { 600 } else { 6 * 3600 };
            if core::now_unix() - last_check > due {
                last_check = core::now_unix();
                match self.release() {
                    Ok(r) => {
                        *self.latest.lock().unwrap() = Some(r.clone());
                        let staged = version_in(&staged_dir());
                        let have = staged.or(installed.clone());
                        if have.as_deref().is_none_or(|h| core::version_cmp(&r.version, h).is_gt()) {
                            if let Some(h) = &have {
                                c.info(format!("Updating TradeSkillMaster {h} → {}…", r.version));
                            }
                            if let Some(Err(e)) = c.exclusive(|| self.install(&r)) {
                                c.error(format!("Couldn't install TradeSkillMaster: {}", core::friendly_error(&e)));
                                if installed.is_none() {
                                    c.set_health(Health::Error, "TradeSkillMaster isn't installed (see Activity)");
                                    c.sleep(Duration::from_secs(600));
                                    continue;
                                }
                            }
                        }
                    }
                    Err(e) => c.warn(format!("Couldn't check for a new TradeSkillMaster: {}", core::friendly_error(&e))),
                }
            }
            // An update waiting: restart the app Hearthlink runs to put it in place. Moving the
            // folders runs as an exclusive job, so it can't overlap an install from the page.
            if !staged_dir().exists() {
                restart_failed = false;
            }
            let mut restart = false;
            let swapped = if staged_dir().exists() && self.ours_running() && !restart_failed {
                c.exclusive(|| {
                    c.info("Restarting TradeSkillMaster to put the new version in place…");
                    self.stop();
                    if !wait_gone(Duration::ZERO) {
                        close_all(); // helpers that outlived it
                    }
                    restart = true;
                    self.swap_in()
                })
            } else {
                c.exclusive(|| self.swap_in())
            };
            match swapped {
                Some(Ok(false)) if restart => {
                    restart_failed = true;
                    c.warn("TradeSkillMaster didn't close completely, so the new version is put in place at its next start.");
                }
                Some(Err(e)) => c.error(format!("Couldn't put the new TradeSkillMaster in place: {e}")),
                _ => {}
            }
            // Hearthlink starts it, so the app's own autostart would start a second, untied one.
            if remove_own_autostarts() {
                c.info("Removed TradeSkillMaster's own \"Run at startup\" entry: it starts and closes with Hearthlink.");
            }
            let want_open = self.want_open.swap(false, Ordering::SeqCst);
            if want_open && !app_run().is_file() {
                c.warn("TradeSkillMaster isn't installed yet.");
            }
            if app_run().is_file() && (first || restart || want_open) {
                if self.ours_running() {
                    if want_open {
                        self.show_window();
                    }
                } else {
                    if core::process_running(PROCESS) {
                        // Started some other way: replace it with one tied to Hearthlink.
                        c.info("TradeSkillMaster was started without Hearthlink; restarting it so it closes with Hearthlink.");
                        close_all();
                    }
                    self.launch();
                }
                first = false;
            }
            self.refresh_facts();
            let running = *self.running.lock().unwrap();
            if !c.is_busy() {
                let (h, t) = match installed_version() {
                    None => (Health::Attention, "Not installed yet".to_string()),
                    Some(v) if staged_dir().exists() => (Health::Ok, format!("Version {v} · update ready, put in place at the next start")),
                    Some(v) => (Health::Ok, format!("Version {v} · {}", if running { "running" } else { "not running" })),
                };
                c.set_health(h, t);
            }
            // Closer look while the app has just started (a failed start, its autostart entry).
            let young = self.child.lock().unwrap().as_ref().is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(40));
            c.sleep(Duration::from_secs(if young { 3 } else { 60 }));
        }
    }
}

impl Service for Tsm {
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
    /// The app does its own uploads and downloads: the quick action opens it (the worker
    /// starts it, or brings up its window).
    fn upload_now(&self) {
        self.inner.want_open.store(true, Ordering::SeqCst);
        self.inner.common.wake();
    }
    fn action_label(&self) -> &'static str {
        "Open the app"
    }
    fn action_icon(&self) -> &'static str {
        crate::ui::icon::EXTERNAL
    }
    fn tray_label(&self) -> String {
        "    Open TradeSkillMaster".into()
    }

    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();
        let version = i.version.lock().unwrap().clone();
        let running = *i.running.lock().unwrap();
        let latest = i.latest.lock().unwrap().clone();
        let spawn = |f: Box<dyn FnOnce(&Arc<Inner>) + Send>| {
            let i = i.clone();
            std::thread::spawn(move || {
                i.common.exclusive(|| f(&i));
                i.common.wake();
            });
        };

        crate::ui::section(ui, "Official TradeSkillMaster app", |ui| {
            ui.label(
                "TradeSkillMaster makes its own Linux app. Hearthlink downloads it from tradeskillmaster.com, \
                 checks it, keeps it up to date and adds it to your app menu. Sign in, choose the game folder and \
                 pick your realms in the app itself.",
            );
            ui.horizontal_wrapped(|ui| {
                match &version {
                    Some(v) => {
                        ui.label(format!("Version {v}"));
                        if let Some(l) = &latest {
                            if core::version_cmp(&l.version, v).is_gt() {
                                ui.colored_label(crate::ui::ACCENT, format!("{} is available (updating automatically)", l.version));
                            }
                        }
                        ui.weak(if running { "running" } else { "not running" });
                        ui.weak(app_dir().display().to_string());
                    }
                    None => {
                        ui.label("Not installed yet.");
                    }
                }
            });
            ui.horizontal(|ui| {
                if crate::ui::primary(ui, version.is_some() && cfg.enabled, crate::ui::icon::EXTERNAL, "Open TradeSkillMaster").clicked() {
                    self.upload_now();
                }
                let label = if version.is_some() { "Check for updates" } else { "Install it" };
                if ui.add_enabled(!busy, egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, label))).clicked() {
                    spawn(Box::new(|i| match i.release() {
                        Ok(r) => {
                            *i.latest.lock().unwrap() = Some(r.clone());
                            let have = version_in(&staged_dir()).or_else(installed_version);
                            if have.as_deref().is_some_and(|h| !core::version_cmp(&r.version, h).is_gt()) {
                                i.common.good(format!("TradeSkillMaster {} is the newest version.", r.version));
                            } else if let Err(e) = i.install(&r) {
                                i.common.error(format!("Couldn't install TradeSkillMaster: {}", core::friendly_error(&e)));
                            }
                            i.refresh_facts();
                        }
                        Err(e) => i.common.error(format!("Couldn't reach TSM's update feed: {}", core::friendly_error(&e))),
                    }));
                }
            });
        });

        crate::ui::section(ui, "Options", |ui| {
            let mut menu = cfg.menu_entry;
            if ui.checkbox(&mut menu, "Show TradeSkillMaster in my app menu").changed() {
                i.cfg.lock().unwrap().menu_entry = menu;
                i.save();
                let i = i.clone();
                std::thread::spawn(move || {
                    i.sync_menu_entry(false);
                    i.refresh_facts();
                });
            }
            if let Some(entry) = i.foreign_entry.lock().unwrap().as_ref() {
                ui.weak(format!("There is already a TradeSkillMaster menu entry ({}), so Hearthlink doesn't add one.", entry.display()));
            }
            ui.weak(
                "TradeSkillMaster runs while Hearthlink runs: it starts with Hearthlink and closes with it. \
                 Closing its window keeps it working in its tray icon; the menu entry opens it through Hearthlink.",
            );
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy && version.is_some(), egui::Button::new(crate::ui::ib(crate::ui::icon::TRASH, "Remove the app")))
                    .on_hover_text("Deletes the app Hearthlink installed and its menu entry, and turns this off. TSM's own settings and login stay.")
                    .clicked()
                {
                    spawn(Box::new(|i| i.remove()));
                }
                if ui.button(crate::ui::ib(crate::ui::icon::FOLDER, "Open its settings folder")).clicked() {
                    // The app runs on the host, so its settings are in the host's ~/.config.
                    let _ = open::that(core::host_config_dir().join("TradeSkillMaster"));
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_update_feed() {
        let feed = "version: 1.0.8\nfiles:\n  - url: TradeSkillMaster.AppImage\n    sha512: ZzxN==\n    size: 153463323\n    blockMapSize: 162351\npath: TradeSkillMaster.AppImage\nsha512: ZzxN==\nreleaseDate: '2026-11-05T06:14:06.589Z'\n";
        let (v, p, s, size) = parse_feed(feed).unwrap();
        assert_eq!((v.as_str(), p.as_str(), s.as_str(), size), ("1.0.8", "TradeSkillMaster.AppImage", "ZzxN==", 153463323));
        assert!(parse_feed("files: []\n").is_none());
    }

    #[test]
    fn finds_the_installed_version() {
        let dir = std::env::temp_dir().join(format!("hearthlink-tsm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(version_in(&dir), None);
        // The desktop file's name may differ in case.
        std::fs::write(dir.join("tradeskillmaster.desktop"), "[Desktop Entry]\nX-AppImage-Version=1.0.7\n").unwrap();
        assert_eq!(version_in(&dir).as_deref(), Some("1.0.7"));
        // Hearthlink's own note wins.
        std::fs::write(dir.join(VERSION_FILE), "1.0.8\n").unwrap();
        assert_eq!(version_in(&dir).as_deref(), Some("1.0.8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Talks to tradeskillmaster.com: run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn official_feed_has_a_linux_app() {
        let c = core::http_client("hearthlink-test", 30);
        let resp = c.get(FEED).send().unwrap().error_for_status().unwrap();
        let base = resp.url().clone();
        let (v, p, s, size) = parse_feed(&resp.text().unwrap()).unwrap();
        assert!(!v.is_empty() && !s.is_empty() && size > 1_000_000);
        let url = base.join(&p).unwrap();
        let head = c.head(url).send().unwrap();
        assert!(head.status().is_success());
    }
}
