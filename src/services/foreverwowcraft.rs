//! ForeverWowCraft: the helper is plain Python (MIT, github.com/ForeverWowCraft/ForeverWowCraft-Helper)
//! and runs on Linux as it is. Only its Windows parts (tray icon, autostart, folder picker) don't;
//! Hearthlink does those. So this module downloads the official release from GitHub (checking
//! the SHA-256 GitHub lists for it), keeps it up to date and runs its commands with the system's
//! Python 3:
//!
//!   install-addon   the scanner and the ForeverWowCraft Plans addon into the game
//!   lists           the lists the addon scans
//!   whoami          the name the helper sends under
//!   send --watch    sends every scan the addon saves; fetches the routes and the freshness
//!
//! The helper's state (its pipe name and token, what it sent, the routes) is kept in
//! ~/.local/share/hearthlink/foreverwowcraft/state (FWC_STATE). A Windows helper's state in a
//! Wine prefix is taken over once, so the helper keeps its name.

use super::{Meta, Service};
use crate::core::{self, store, wine, wow, Common, Env, Health};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static META: Meta = Meta {
    id: "foreverwowcraft",
    name: "ForeverWowCraft",
    tagline: "Auction prices for foreverwowcraft.com and profession routes in game (WoW Forever)",
    icon: include_bytes!("../../assets/icons/foreverwowcraft.png"),
    website: "https://foreverwowcraft.com/helper",
    upstream_app: "ForeverWowCraft Helper",
    ported_version: "1.5.3",
    upstream_url: "https://github.com/ForeverWowCraft/ForeverWowCraft-Helper/releases",
    runs_official: true,
};

const RELEASES: &str = "https://api.github.com/repos/ForeverWowCraft/ForeverWowCraft-Helper/releases/latest";
const RELEASES_FALLBACK: &str = "https://api.github.com/repos/ForeverWowCraft/ForeverWowCraft-Helper/releases?per_page=20";
const HELP_URL: &str = "https://foreverwowcraft.com/helper";
const PRIVACY_URL: &str = "https://foreverwowcraft.com/privacy.html";
const DISCORD_URL: &str = "https://discord.gg/XPt2JUCxAg";
const SCANNER: &str = "ForeverWowCraftScanner";
const PLANS_ADDON: &str = "ForeverWowCraftPlans";
/// What the helper keeps (settings.STATE_FILES), taken over from a Windows helper.
const STATE_FILES: [&str; 5] = ["identity.json", "sent.json", "Plans.lua", "Fresh.lua", "Queue.lua"];
/// Lines of the helper that mean trouble (fwchelper/tray.py TROUBLE_STARTS, and a few more).
const TROUBLE: [&str; 11] = [
    "not sent",
    "routes not fetched",
    "lists not fetched",
    "the pipe could not",
    "could not register",
    "addon not installed",
    "AddOns folder not found",
    "Game not found",
    "the helper stopped",
    "Traceback",
    "what the service sent is not usable",
];
/// Lines that say something went right (tray.py GOOD_STARTS).
const GOOD: [&str; 5] = ["sent ", "Installed ForeverWowCraft", "routes for the addon's /fwc window: ", "This helper sends as ", "not needed: "];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    enabled: bool,
    /// Keep `send --watch` running while Hearthlink runs.
    background: bool,
    /// The Forever game folder (a flavour folder) the helper works on; None = the first found.
    game_folder: Option<PathBuf>,
    /// Looked for a Windows helper's state in the Wine prefixes already.
    took_over: bool,
    /// Where a Windows helper's state was taken over from.
    took_over_from: Option<String>,
    name: Option<String>,
    trust: Option<String>,
    last_sent: i64,
    last_sent_text: Option<String>,
    routes: Option<String>,
}

pub struct ForeverWowCraft {
    inner: Arc<Inner>,
}

struct Inner {
    env: Arc<Env>,
    common: Arc<Common>,
    cfg: Mutex<Config>,
    watch: Mutex<Option<(Child, PathBuf)>>,
    http: reqwest::blocking::Client,
    /// The newest release on GitHub, as last checked.
    latest: Mutex<Option<String>>,
    /// `Some(error)` when no usable Python was found.
    python: Mutex<Option<Result<String, String>>>,
    /// Forever game folders as of the worker's last look (the page must not scan disks).
    folders: Mutex<Vec<wow::Flavor>>,
    /// Trouble notifications: when each text was last shown (the helper's own rule: once a day).
    told: Mutex<HashMap<String, i64>>,
}

fn base_dir() -> PathBuf {
    store::data_dir().join("foreverwowcraft")
}
fn helper_dir() -> PathBuf {
    base_dir().join("helper")
}
fn state_dir() -> PathBuf {
    base_dir().join("state")
}

/// The installed helper's version (`__version__` in fwchelper/__init__.py).
fn installed_version() -> Option<String> {
    let text = std::fs::read_to_string(helper_dir().join("fwchelper/__init__.py")).ok()?;
    let re = regex::Regex::new(r#"(?m)^__version__\s*=\s*["']([^"']+)["']"#).ok()?;
    re.captures(&text).map(|c| c[1].to_string())
}

/// The `## Version:` of an installed addon.
fn toc_version(addons: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(addons.join(name).join(format!("{name}.toc"))).ok()?;
    text.lines().find_map(|l| l.strip_prefix("## Version:")).map(|v| v.trim().to_string())
}

/// The Windows helper's state folders in Wine prefixes (`%LOCALAPPDATA%\ForeverWowCraft\Helper`).
fn windows_states() -> Vec<PathBuf> {
    wine::prefixes_with("users")
        .iter()
        .flat_map(|p| wine::user_profiles(p))
        .map(|u| u.join("AppData/Local/ForeverWowCraft/Helper"))
        .filter(|d| d.join("identity.json").is_file())
        .collect()
}

/// `python3` when it is 3.10 or newer (what the helper needs).
fn find_python() -> Result<String, String> {
    let out = Command::new("python3")
        .args(["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "Python 3 isn't installed (the helper needs python3, 3.10 or newer)".to_string())?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if core::version_cmp(&v, "3.10").is_lt() {
        return Err(format!("Python {v} is too old: the helper needs 3.10 or newer"));
    }
    Ok(v)
}

/// The helper prints `2026-09-30 14:05:09  text` in its send window; the time is ours to add.
fn strip_stamp(line: &str) -> &str {
    let b = line.as_bytes();
    let stamped = b.len() > 21
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b' '
        && b[20] == b' ';
    if stamped {
        &line[21..]
    } else {
        line
    }
}

/// Retry transient GitHub/proxy failures, including errors while reading a download.
/// A release existing does not mean GitHub's API or asset CDN cannot return a 504.
fn github_get(http: &reqwest::blocking::Client, url: &str, accept: &str, common: &Common, delay: Duration) -> anyhow::Result<Vec<u8>> {
    for attempt in 1..=3 {
        let result = http.get(url).header("Accept", accept).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes());
        match result {
            Ok(bytes) => return Ok(bytes.to_vec()),
            Err(e) => {
                let transient = e.is_timeout() || e.is_connect() || e.is_body() || e.status().is_some_and(|s| s.is_server_error());
                if !transient || attempt == 3 {
                    return Err(e.into());
                }
                common.warn(format!("GitHub request failed ({e}); retrying ({}/3)…", attempt + 1));
                std::thread::sleep(delay * attempt);
            }
        }
    }
    unreachable!()
}

fn github_release(http: &reqwest::blocking::Client, latest: &str, releases: &str, common: &Common, delay: Duration) -> anyhow::Result<serde_json::Value> {
    let accept = "application/vnd.github+json";
    match github_get(http, latest, accept, common, delay) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(e) => {
            // GitHub's latest endpoint can time out while the release list still works.
            let transient = e.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                e.is_timeout() || e.is_connect() || e.is_body() || e.status().is_some_and(|s| s.is_server_error())
            });
            if !transient {
                return Err(e);
            }
            common.warn("GitHub's latest-release endpoint is unavailable; trying its release list…");
            let data = github_get(http, releases, accept, common, delay)?;
            let releases: Vec<serde_json::Value> = serde_json::from_slice(&data)?;
            releases.into_iter().find(|r| r["draft"] == false && r["prerelease"] == false)
                .ok_or_else(|| anyhow::anyhow!("GitHub lists no stable release of the helper"))
        }
    }
}

impl ForeverWowCraft {
    pub fn new(env: Arc<Env>) -> Self {
        let first = !store::config_dir().join("foreverwowcraft.json").exists();
        let mut cfg: Config = store::load("foreverwowcraft");
        if first {
            // On by default only for someone who already used the Windows helper: turning it on
            // registers with ForeverWowCraft's server.
            cfg.enabled = !windows_states().is_empty();
            cfg.background = true;
            store::save("foreverwowcraft", &cfg);
        }
        let ua = format!("hearthlink/{} (Linux; +https://github.com/KushGene/hearthlink-linux)", env!("CARGO_PKG_VERSION"));
        Self {
            inner: Arc::new(Inner {
                env,
                common: Arc::new(Common::default()),
                cfg: Mutex::new(cfg),
                watch: Mutex::new(None),
                http: core::http_client(&ua, 120),
                latest: Mutex::new(None),
                python: Mutex::new(None),
                folders: Mutex::new(Vec::new()),
                told: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl Inner {
    fn save(&self) {
        store::save("foreverwowcraft", &*self.cfg.lock().unwrap());
    }

    fn forever_folders(&self) -> Vec<wow::Flavor> {
        self.env.flavors().into_iter().filter(wow::is_forever).collect()
    }

    /// The game folder the helper works on: the chosen one while it exists, else the first found.
    fn game(&self) -> Option<wow::Flavor> {
        let folders = self.folders.lock().unwrap().clone();
        let chosen = self.cfg.lock().unwrap().game_folder.clone();
        chosen
            .and_then(|c| folders.iter().find(|f| f.path == c).cloned())
            .or_else(|| folders.into_iter().next())
    }

    // ---- the official helper ------------------------------------------------------------------
    /// The newest release: (version, zip URL, sha256, notes).
    fn release(&self) -> anyhow::Result<(String, String, String, Option<String>)> {
        let r = github_release(&self.http, RELEASES, RELEASES_FALLBACK, &self.common, Duration::from_secs(2))?;
        let version = r["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
        anyhow::ensure!(!version.is_empty(), "GitHub lists no release of the helper");
        let asset = r["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| {
                let n = a["name"].as_str().unwrap_or("");
                n.starts_with("ForeverWowCraft-Helper-") && n.ends_with(".zip")
            })
            .ok_or_else(|| anyhow::anyhow!("the release {version} has no zip of the helper"))?;
        let url = asset["browser_download_url"].as_str().unwrap_or("").to_string();
        let sha = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).unwrap_or("").to_lowercase();
        anyhow::ensure!(sha.len() == 64, "GitHub gives no checksum for the helper's zip, so it isn't installed");
        let notes = r["body"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Ok((version, url, sha, notes))
    }

    /// Downloads the official helper, checks it against the SHA-256 GitHub lists and puts it in
    /// place of the old one.
    fn install(&self) -> anyhow::Result<()> {
        let (version, url, sha, _) = self.release()?;
        self.install_release(&version, &url, &sha)
    }

    fn install_release(&self, version: &str, url: &str, sha: &str) -> anyhow::Result<()> {
        let c = &self.common;
        c.info(format!("Downloading the ForeverWowCraft Helper {version} from GitHub…"));
        let data = github_get(&self.http, url, "application/octet-stream", c, Duration::from_secs(2))?;
        anyhow::ensure!(core::sha256_hex(&data) == sha, "the download was damaged (checksum didn't match)");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))?;
        let fresh = base_dir().join("helper.new");
        let _ = std::fs::remove_dir_all(&fresh);
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.is_dir() {
                continue;
            }
            let Some(path) = f.enclosed_name() else { continue };
            // ForeverWowCraft-Helper/<what> → <what>; the tests and the Windows installer aren't needed.
            let rel: PathBuf = path.components().skip(1).collect();
            let top = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
            if rel.as_os_str().is_empty() || matches!(top.as_str(), "tests" | "installer" | ".github") {
                continue;
            }
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            core::write_atomic(&fresh.join(rel), &bytes)?;
        }
        anyhow::ensure!(fresh.join("fwch.py").is_file() && fresh.join("addon").join(SCANNER).is_dir(), "the download isn't the helper");
        self.stop_watch();
        let old = base_dir().join("helper.old");
        let _ = std::fs::remove_dir_all(&old);
        if helper_dir().exists() {
            std::fs::rename(helper_dir(), &old)?;
        }
        std::fs::rename(&fresh, helper_dir())?;
        let _ = std::fs::remove_dir_all(&old);
        c.good(format!("Installed the ForeverWowCraft Helper {version}."));
        Ok(())
    }

    /// The helper's command line, with the game folder and the state folder it should use.
    fn command(&self, args: &[&str]) -> anyhow::Result<Command> {
        anyhow::ensure!(helper_dir().join("fwch.py").is_file(), "the ForeverWowCraft Helper isn't installed");
        let mut cmd = Command::new("python3");
        // -u: every line at once; -s: no user site-packages (the helper needs none).
        cmd.arg("-u").arg("-s").arg("fwch.py").args(args).current_dir(helper_dir());
        for v in ["PYTHONPATH", "PYTHONHOME", "PYTHONSTARTUP", "FC_STATE", "FC_WOW_DIR", "FC_WOW_FLAVOR"] {
            cmd.env_remove(v);
        }
        cmd.env("PYTHONIOENCODING", "utf-8").env("PYTHONDONTWRITEBYTECODE", "1").env("FWC_STATE", state_dir());
        if let Some(g) = self.game() {
            if let Some(root) = g.path.parent() {
                cmd.env("FWC_WOW_DIR", root).env("FWC_WOW_FLAVOR", &g.folder);
            }
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(cmd)
    }

    /// Runs one helper command, handling each line it prints. Errors carry what it said last.
    fn run(&self, args: &[&str], log: bool) -> anyhow::Result<Vec<String>> {
        let mut child = self.command(args)?.spawn()?;
        let err = child.stderr.take().unwrap();
        let errs = std::thread::spawn(move || BufReader::new(err).lines().map_while(Result::ok).collect::<Vec<_>>());
        let mut lines = Vec::new();
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            self.on_line(&line, log);
            lines.push(strip_stamp(&line).to_string());
        }
        let status = child.wait()?;
        let errs: Vec<String> = errs.join().unwrap_or_default().into_iter().filter(|l| !l.trim().is_empty()).collect();
        if !status.success() {
            // A SystemExit's message (or a traceback's last line) is what went wrong.
            let why = errs.last().or(lines.last()).cloned().unwrap_or_else(|| format!("the helper stopped with {status}"));
            for l in &errs[..errs.len().saturating_sub(1)] {
                eprintln!("[foreverwowcraft] {l}");
            }
            anyhow::bail!(why);
        }
        for l in errs {
            self.on_line(&l, log);
            lines.push(l);
        }
        Ok(lines)
    }

    /// Takes what a line of the helper says into the status, and logs it.
    fn on_line(&self, raw: &str, log: bool) {
        let t = strip_stamp(raw).trim();
        if t.is_empty() || t.starts_with("Wrote ") || t.starts_with("In game: /reload") {
            if log && t.starts_with("Wrote ") {
                eprintln!("[foreverwowcraft] {t}");
            }
            return;
        }
        let mut changed = true;
        {
            let mut cfg = self.cfg.lock().unwrap();
            if let Some(rest) = t.strip_prefix("This helper sends as ") {
                let rest = rest.trim_end_matches('.');
                let (name, trust) = rest.split_once(" (").unwrap_or((rest, ""));
                cfg.name = Some(name.to_string());
                cfg.trust = Some(trust.trim_end_matches(')').to_string()).filter(|s| !s.is_empty());
            } else if t.starts_with("sent ") {
                cfg.last_sent = core::now_unix();
                cfg.last_sent_text = Some(t["sent ".len()..].to_string());
            } else if let Some(r) = t.strip_prefix("routes for the addon's /fwc window: ") {
                cfg.routes = Some(r.to_string());
            } else {
                changed = false;
            }
        }
        if changed {
            self.save();
        }
        if !log {
            return;
        }
        if TROUBLE.iter().any(|s| t.starts_with(s)) {
            self.common.warn(t);
            let now = core::now_unix();
            let mut told = self.told.lock().unwrap();
            if told.get(t).is_none_or(|at| now - at > 86400) {
                told.insert(t.to_string(), now);
                self.env.notify("ForeverWowCraft", t);
            }
        } else if GOOD.iter().any(|s| t.starts_with(s)) {
            self.common.good(t);
        } else {
            self.common.info(t);
        }
    }

    fn stop_watch(&self) {
        if let Some((mut c, _)) = self.watch.lock().unwrap().take() {
            // SIGTERM, so the helper's `finally` lets go of its lock file.
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
            for _ in 0..30 {
                if c.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Keeps `send --watch` running for the current game folder: it sends after every SAVE in
    /// game and fetches the routes (every minute) and the freshness (every 5 minutes).
    fn ensure_watch(self: &Arc<Self>, game: &wow::Flavor) {
        let mut w = self.watch.lock().unwrap();
        if let Some((c, folder)) = w.as_mut() {
            if c.try_wait().ok().flatten().is_none() && *folder == game.path {
                return;
            }
        }
        if let Some((mut c, _)) = w.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let mut cmd = match self.command(&["send", "--watch"]) {
            Ok(c) => c,
            Err(e) => return self.common.error(format!("Couldn't start the helper: {e}")),
        };
        // The helper must not outlive Hearthlink.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let pipes = [
                    child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
                    child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
                ];
                for pipe in pipes.into_iter().flatten() {
                    let me = self.clone();
                    std::thread::spawn(move || {
                        for l in BufReader::new(pipe).lines().map_while(Result::ok) {
                            me.on_line(&l, true);
                        }
                    });
                }
                *w = Some((child, game.path.clone()));
            }
            Err(e) => self.common.error(format!("Couldn't start the helper in the background: {e}")),
        }
    }

    /// Takes over the Windows helper's state (its pipe name and token, what it sent, the routes)
    /// once, when Hearthlink has none of its own yet.
    fn take_over(&self) {
        if self.cfg.lock().unwrap().took_over {
            return;
        }
        let ours = state_dir();
        if !ours.join("identity.json").exists() {
            if let Some(theirs) = windows_states().into_iter().next() {
                let mut copied = 0;
                for name in STATE_FILES {
                    let from = theirs.join(name);
                    if from.is_file() && std::fs::copy(&from, ours.join(name)).is_ok() {
                        copied += 1;
                    }
                }
                if copied > 0 {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(ours.join("identity.json"), std::fs::Permissions::from_mode(0o600));
                    }
                    self.common.good(format!("Took over the Windows helper's name and history from {}", theirs.display()));
                    self.cfg.lock().unwrap().took_over_from = Some(theirs.display().to_string());
                }
            }
        }
        self.cfg.lock().unwrap().took_over = true;
        self.save();
    }

    /// Puts the two addons into the game (the helper's own `install-addon`: it leaves a newer
    /// scanner from CurseForge alone) and fetches the lists.
    fn install_addon(&self, game: &wow::Flavor) -> anyhow::Result<()> {
        let had = toc_version(&game.addons_dir(), SCANNER).is_some();
        self.run(&["install-addon"], true)?;
        if !had && wow::game_running() {
            let t = "The addon was installed while the game ran: restart it, so that it loads the addon.";
            self.common.warn(t);
            self.env.notify("ForeverWowCraft: restart the game", t);
        }
        if let Err(e) = self.run(&["lists"], false) {
            self.common.warn(format!("lists not fetched: {e}"));
        }
        Ok(())
    }

    /// Unlike an automatic repair, an explicit update must fetch the newest bundled addon.
    fn update_addon(&self) -> anyhow::Result<()> {
        self.common.set_health(Health::Busy, "Checking for addon updates…");
        find_python().map_err(anyhow::Error::msg)?;
        let game = self.game().ok_or_else(|| anyhow::anyhow!("No WoW Forever game folder found"))?;
        let (version, url, sha, _) = self.release()?;
        *self.latest.lock().unwrap() = Some(version.clone());
        if installed_version().is_none_or(|have| core::version_cmp(&version, &have).is_gt()) {
            self.install_release(&version, &url, &sha)?;
        }
        self.common.set_health(Health::Busy, "Installing the addon…");
        self.install_addon(&game)?;
        self.common.set_health(Health::Ok, "Addon installed; restart the game or /reload");
        Ok(())
    }

    fn whoami(&self) {
        if let Err(e) = self.run(&["whoami"], false) {
            self.common.warn(format!("the pipe could not be reached: {}", core::friendly_error(&e)));
        }
    }

    fn worker(self: Arc<Self>) {
        let c = self.common.clone();
        let mut checked = 0i64;
        let mut started_for: Option<(PathBuf, String)> = None;
        let mut looked_addon = 0i64;
        loop {
            let cfg = self.cfg.lock().unwrap().clone();
            *self.folders.lock().unwrap() = self.forever_folders();
            if !cfg.enabled {
                self.stop_watch();
                started_for = None;
                c.set_health(Health::Off, "Turned off");
                c.sleep(Duration::from_secs(30));
                continue;
            }
            let python = find_python();
            *self.python.lock().unwrap() = Some(python.clone());
            if let Err(e) = python {
                self.stop_watch();
                c.set_health(Health::Error, e);
                c.sleep(Duration::from_secs(300));
                continue;
            }
            let _ = std::fs::create_dir_all(state_dir());
            self.take_over();
            // The official helper: installed, and kept current (every 6 hours).
            if installed_version().is_none() || core::now_unix() - checked > 6 * 3600 {
                c.set_health(Health::Busy, "Checking for the newest ForeverWowCraft Helper…");
                match self.release() {
                    Ok((latest, url, sha, _)) => {
                        checked = core::now_unix();
                        *self.latest.lock().unwrap() = Some(latest.clone());
                        let have = installed_version();
                        if have.as_deref().is_none_or(|h| core::version_cmp(&latest, h).is_gt()) {
                            if let Some(h) = &have {
                                c.info(format!("Updating the ForeverWowCraft Helper {h} → {latest}…"));
                            }
                            match c.exclusive(|| self.install_release(&latest, &url, &sha)) {
                                Some(Ok(())) => started_for = None, // the new one installs its addon and starts again
                                Some(Err(e)) => {
                                    checked = core::now_unix() - 5 * 3600; // retry in an hour, not six
                                    c.error(format!("Couldn't install the ForeverWowCraft Helper: {}", core::friendly_error(&e)));
                                }
                                None => checked = 0, // another job won the lock; check next time
                            }
                        }
                    }
                    Err(e) => {
                        checked = core::now_unix() - 5 * 3600; // try again in an hour
                        c.warn(format!("Couldn't check for a newer ForeverWowCraft Helper: {}", core::friendly_error(&e)));
                    }
                }
            }
            let Some(version) = installed_version() else {
                c.set_health(Health::Error, "The ForeverWowCraft Helper isn't installed (see Activity)");
                c.sleep(Duration::from_secs(600));
                continue;
            };
            let Some(game) = self.game() else {
                self.stop_watch();
                started_for = None;
                c.set_health(Health::Attention, "No WoW Forever game folder found");
                c.sleep(Duration::from_secs(60));
                continue;
            };
            // What the Windows app does at start: the addon, the lists, the name. Again after an
            // update and when the game folder changes.
            let key = (game.path.clone(), version.clone());
            if started_for.as_ref() != Some(&key) {
                self.stop_watch();
                let installed = c.exclusive(|| {
                    c.set_health(Health::Busy, "Installing the addon…");
                    self.install_addon(&game)?;
                    self.whoami();
                    Ok::<_, anyhow::Error>(())
                });
                looked_addon = core::now_unix();
                match installed {
                    Some(Ok(())) => started_for = Some(key),
                    Some(Err(e)) => {
                        c.error(format!("addon not installed: {e}"));
                        c.set_health(Health::Error, format!("Addon installation failed: {e}"));
                        c.sleep(Duration::from_secs(60));
                        continue;
                    }
                    None => {}
                }
            } else if core::now_unix() - looked_addon > 600 {
                // A game reinstalled or a folder emptied by hand: put the addon back.
                looked_addon = core::now_unix();
                if toc_version(&game.addons_dir(), SCANNER).is_none() {
                    c.info("The addon isn't in the game any more: putting it back.");
                    if let Some(Err(e)) = c.exclusive(|| self.install_addon(&game)) {
                        c.error(format!("addon not installed: {e}"));
                    }
                }
            }
            if cfg.background {
                self.ensure_watch(&game);
            } else {
                self.stop_watch();
            }
            let cfg = self.cfg.lock().unwrap().clone();
            let (h, t) = if !cfg.background {
                (Health::Idle, "Sends only when you click Send prices now".to_string())
            } else if cfg.last_sent > 0 {
                (Health::Ok, format!("Last sent {}", core::ago(cfg.last_sent)))
            } else {
                (Health::Ok, "Nothing sent yet: open the auction house and click Save Prices".to_string())
            };
            if !c.is_busy() {
                c.set_health(h, t);
            }
            c.sleep(Duration::from_secs(60));
        }
    }
}

impl Service for ForeverWowCraft {
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
    /// "Send prices now": the scan file as it is, even what was sent already (the Windows
    /// app's menu does the same).
    fn upload_now(&self) {
        let i = self.inner.clone();
        std::thread::spawn(move || {
            i.common.exclusive(|| {
                i.common.set_health(Health::Busy, "Sending…");
                match i.run(&["send", "--again"], true) {
                    Ok(_) => {}
                    // No scan saved yet: nothing wrong, as the Windows app says it.
                    Err(e) if e.to_string().contains(".lua found under") => {
                        i.common.info("Nothing to send yet: open the auction house in game and click Save Prices.")
                    }
                    Err(e) => i.common.warn(format!("not sent: {e}")),
                }
            });
            i.common.wake();
        });
    }

    fn ui(&self, ui: &mut egui::Ui) {
        let i = &self.inner;
        let cfg = i.cfg.lock().unwrap().clone();
        let busy = i.common.is_busy();
        let version = installed_version();
        let python = i.python.lock().unwrap().clone();
        let ready = version.is_some() && python.as_ref().is_some_and(|p| p.is_ok());
        let spawn = |f: Box<dyn FnOnce(&Arc<Inner>) + Send>| {
            let i = i.clone();
            std::thread::spawn(move || {
                i.common.exclusive(|| f(&i));
                i.common.wake();
            });
        };

        crate::ui::section(ui, "Official ForeverWowCraft Helper", |ui| {
            ui.label(
                "The helper is open source Python and runs on Linux as it is. Hearthlink downloads the official release, \
                 keeps it up to date and runs it for you, so what is sent is exactly what the site expects.",
            );
            ui.horizontal_wrapped(|ui| {
                match &version {
                    Some(v) => {
                        ui.label(format!("Version {v}"));
                        if let Some(l) = i.latest.lock().unwrap().clone() {
                            if core::version_cmp(&l, v).is_gt() {
                                ui.colored_label(crate::ui::ACCENT, format!("{l} is available (updating automatically)"));
                            }
                        }
                        ui.weak(helper_dir().display().to_string());
                    }
                    None => {
                        ui.label("Not installed yet.");
                    }
                }
            });
            match &python {
                Some(Ok(v)) => {
                    ui.weak(format!("Runs with Python {v}"));
                }
                Some(Err(e)) => {
                    ui.colored_label(crate::ui::health_color(Health::Error), crate::ui::ib(crate::ui::icon::WARNING, e));
                }
                None => {}
            }
            ui.horizontal(|ui| {
                let label = if version.is_some() { "Reinstall it" } else { "Install it" };
                if ui.add_enabled(!busy && cfg.enabled, egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, label))).clicked() {
                    spawn(Box::new(|i| {
                        if let Err(e) = i.install() {
                            i.common.error(format!("Couldn't install: {}", core::friendly_error(&e)));
                        }
                    }));
                }
                ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "Its code and releases"), META.upstream_url);
            });
        });

        crate::ui::section(ui, "Sender", |ui| {
            match &cfg.name {
                Some(n) => {
                    ui.label(format!("Sends as {n}{}", cfg.trust.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()));
                }
                None => {
                    ui.label("No name yet: the helper is given one the first time it talks to the site.");
                }
            }
            ui.weak("The name is random; nothing about you or your characters is sent. A new sender is \"public\": what it sends is used where it agrees with what others saw.");
            if let Some(from) = &cfg.took_over_from {
                ui.weak(format!("Taken over from the Windows helper in {from}"));
            }
        });

        crate::ui::section(ui, "Game and addon", |ui| {
            let game = i.game();
            egui::Grid::new("fwc-status").num_columns(2).striped(true).show(ui, |ui| {
                ui.weak("Game folder");
                ui.label(game.as_ref().map(|g| g.path.display().to_string()).unwrap_or_else(|| "no WoW Forever folder found".into()));
                ui.end_row();
                let addons = game.as_ref().map(|g| g.addons_dir());
                for (label, name) in [("Scanner addon", SCANNER), ("Plans addon", PLANS_ADDON)] {
                    ui.weak(label);
                    ui.label(addons.as_ref().and_then(|a| toc_version(a, name)).unwrap_or_else(|| "not installed".into()));
                    ui.end_row();
                }
                ui.weak("Last sent");
                ui.label(if cfg.last_sent > 0 {
                    format!("{} · {}", core::ago(cfg.last_sent), cfg.last_sent_text.clone().unwrap_or_default())
                } else {
                    "nothing yet: open the auction house and click Save Prices".into()
                });
                ui.end_row();
                ui.weak("Routes");
                ui.label(cfg.routes.clone().unwrap_or_else(|| "–".into()));
                ui.end_row();
            });
            let folders = i.folders.lock().unwrap().clone();
            if folders.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Use:");
                    for f in folders {
                        let on = game.as_ref().is_some_and(|g| g.path == f.path);
                        if ui.add_enabled(!on, egui::Button::new(f.display_name()).small()).on_hover_text(f.path.display().to_string()).clicked() {
                            i.cfg.lock().unwrap().game_folder = Some(f.path.clone());
                            i.save();
                            i.common.wake();
                        }
                    }
                });
            }
            ui.add_space(4.0);
            let can = !busy && ready && game.is_some() && cfg.enabled;
            ui.horizontal_wrapped(|ui| {
                if crate::ui::primary(ui, can, crate::ui::icon::UPLOAD, "Send prices now").clicked() {
                    self.upload_now();
                }
                if ui.add_enabled(can, egui::Button::new(crate::ui::ib(crate::ui::icon::SYNC, "Fetch routes now"))).clicked() {
                    spawn(Box::new(|i| {
                        if let Err(e) = i.run(&["plans", "--again"], true) {
                            i.common.warn(format!("routes not fetched: {e}"));
                        }
                    }));
                }
                if ui.add_enabled(!busy && game.is_some() && cfg.enabled, egui::Button::new(crate::ui::ib(crate::ui::icon::DOWNLOAD, "Install or update the addon"))).clicked() {
                    spawn(Box::new(|i| {
                        i.common.info("Checking GitHub for the newest helper and addon…");
                        if let Err(e) = i.update_addon() {
                            let message = format!("Couldn't update the addon: {}", core::friendly_error(&e));
                            i.common.error(&message);
                            i.common.set_health(Health::Error, message);
                        }
                    }));
                }
            });
            ui.weak("In game: open the auction house and click Save Prices; /fwc shows your routes after /reload.");
        });

        crate::ui::section(ui, "Options", |ui| {
            let mut bg = cfg.background;
            if ui.checkbox(&mut bg, "Send prices after every Save in game and keep the routes current (while Hearthlink runs)").changed() {
                i.cfg.lock().unwrap().background = bg;
                i.save();
                i.common.wake();
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(crate::ui::ib(crate::ui::icon::FOLDER, "Open its state folder")).clicked() {
                    let _ = open::that(state_dir());
                }
                ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "What is sent"), PRIVACY_URL);
                ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "Help"), HELP_URL);
                ui.hyperlink_to(crate::ui::ib(crate::ui::icon::EXTERNAL, "Discord"), DISCORD_URL);
            });
        });
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop_watch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github_responses(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<()>) {
        github_responses_with_bodies(statuses.into_iter().map(|s| (s, "ok")).collect())
    }

    fn github_responses_with_bodies(responses: Vec<(u16, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (status, body) in responses {
                let until = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < until => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("Missing HTTP request: {e}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    request.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (url, server)
    }

    #[test]
    fn github_retries_gateway_timeouts() {
        let (url, server) = github_responses(vec![504, 502, 200]);
        let common = Common::default();
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        assert_eq!(github_get(&http, &url, "application/zip", &common, Duration::ZERO).unwrap(), b"ok");
        server.join().unwrap();
        assert_eq!(common.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn github_stops_after_three_failures() {
        let (url, server) = github_responses(vec![504; 3]);
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        let error = github_get(&http, &url, "application/json", &Common::default(), Duration::ZERO).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.downcast_ref::<reqwest::Error>().unwrap().status().unwrap().as_u16(), 504);
    }

    #[test]
    fn github_does_not_retry_permanent_errors() {
        let (url, server) = github_responses(vec![404]);
        let common = Common::default();
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        let error = github_get(&http, &url, "application/json", &common, Duration::ZERO).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.downcast_ref::<reqwest::Error>().unwrap().status().unwrap().as_u16(), 404);
        assert!(common.log.lock().unwrap().is_empty());
    }

    #[test]
    fn github_falls_back_to_the_stable_release_list() {
        let (url, server) = github_responses_with_bodies(vec![
            (504, ""), (504, ""), (504, ""),
            (200, r#"[{"tag_name":"v3","draft":true,"prerelease":false},
                      {"tag_name":"v2","draft":false,"prerelease":true},
                      {"tag_name":"v1","draft":false,"prerelease":false}]"#),
        ]);
        let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        let release = github_release(&http, &url, &url, &Common::default(), Duration::ZERO).unwrap();
        server.join().unwrap();
        assert_eq!(release["tag_name"], "v1");
    }

    #[test]
    fn strips_the_helpers_time() {
        assert_eq!(strip_stamp("2026-09-30 14:05:09  sent Stonespine (Horde): 12 items"), "sent Stonespine (Horde): 12 items");
        assert_eq!(strip_stamp("sent x"), "sent x");
        assert_eq!(strip_stamp(""), "");
    }

    /// Downloads the real helper from GitHub and runs the commands that don't register with
    /// the pipe, in temporary folders and a stand-in game folder:
    /// `cargo test -- --ignored official_helper_runs`
    #[test]
    #[ignore]
    fn official_helper_runs() {
        let tmp = std::env::temp_dir().join("opencode").join(format!("hearthlink-fwc-test-{}", std::process::id()));
        let game = tmp.join("wow/_forever_");
        std::fs::create_dir_all(game.join("WTF/Account/TEST/SavedVariables")).unwrap();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
            std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        }
        let env = Arc::new(Env {
            config: std::sync::RwLock::new(core::GlobalConfig { wow_root: Some(tmp.join("wow")), ..Default::default() }),
            ctx: Mutex::new(None),
        });
        let inner = Inner {
            env,
            common: Arc::new(Common::default()),
            cfg: Mutex::new(Config { enabled: true, ..Default::default() }),
            watch: Mutex::new(None),
            http: core::http_client("hearthlink-test", 120),
            latest: Mutex::new(None),
            python: Mutex::new(None),
            folders: Mutex::new(Vec::new()),
            told: Mutex::new(HashMap::new()),
        };
        find_python().unwrap();
        inner.install().unwrap();
        assert!(installed_version().is_some());
        *inner.folders.lock().unwrap() = inner.forever_folders();
        assert_eq!(inner.game().unwrap().path, game);
        let wher = inner.run(&["where"], false).unwrap().join("\n");
        assert!(wher.contains(&format!("State folder:   {}", state_dir().display())), "{wher}");
        inner.run(&["install-addon"], true).unwrap();
        let addons = game.join("Interface/AddOns");
        assert!(toc_version(&addons, SCANNER).is_some());
        assert!(addons.join(PLANS_ADDON).join("Plans.lua").is_file());
        // Installation is local: do not contact the pipe or register a sender in this test.
        let log: Vec<String> = inner.common.log.lock().unwrap().iter().map(|l| l.text.clone()).collect();
        eprintln!("{}", log.join("\n"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
