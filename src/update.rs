//! User-confirmed, install-aware updates. No installer runs during background checks.
mod install;
#[cfg(test)]
mod tests;

use crate::core::{self, Env, Install};
use anyhow::Context;
use serde::Deserialize;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_METADATA: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/releases/tag/v{}", core::REPO, self.version)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let parts: Vec<_> = self.version.split('.').collect();
        anyhow::ensure!(parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())), "Invalid stable release version");
        for asset in &self.assets {
            anyhow::ensure!(!asset.name.is_empty() && asset.name.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)), "Unsafe update filename");
            anyhow::ensure!(asset.sha256.len() == 64 && asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()), "Missing or invalid SHA-256 for {}", asset.name);
            anyhow::ensure!(asset.size > 0 && asset.size <= install::MAX_DOWNLOAD, "Invalid update size for {}", asset.name);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Working(String),
    Ready(install::Restart),
    InstallerOpened(String),
    Error(String),
}

#[derive(Default)]
pub struct Updates {
    pub latest: Mutex<Option<Release>>,
    pub checked_at: Mutex<i64>,
    pub status: Mutex<Status>,
    busy: AtomicBool,
}

impl Updates {
    pub fn newer(&self) -> Option<Release> {
        let r = self.latest.lock().unwrap().clone()?;
        core::version_cmp(&r.version, env!("CARGO_PKG_VERSION")).is_gt().then_some(r)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    fn set_status(&self, env: &Env, status: Status) {
        *self.status.lock().unwrap() = status;
        if let Some(ctx) = env.ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    pub fn check_now(self: &Arc<Self>, env: Arc<Env>) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let updates = self.clone();
        std::thread::spawn(move || {
            updates.set_status(&env, Status::Checking);
            updates.check(&env);
            updates.busy.store(false, Ordering::SeqCst);
        });
    }

    fn check(&self, env: &Env) {
        match fetch() {
            Ok(r) => {
                *self.latest.lock().unwrap() = r;
                *self.checked_at.lock().unwrap() = core::now_unix();
                if let Some(r) = self.newer() {
                    let told = env.config.read().unwrap().notified_app_version.clone();
                    if told.as_deref() != Some(&r.version) {
                        env.notify(&format!("Hearthlink {} is available", r.version), "Open Hearthlink to install the update.");
                        env.config.write().unwrap().notified_app_version = Some(r.version);
                        env.save();
                    }
                }
                self.set_status(env, Status::Idle);
            }
            Err(e) => self.set_status(env, Status::Error(format!("Update check failed: {e:#}"))),
        }
    }

    pub fn install_now(self: &Arc<Self>, env: Arc<Env>) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let updates = self.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let release = updates.newer().context("No newer release is available")?;
                release.validate()?;
                install::install(&release, |message| updates.set_status(&env, Status::Working(message)))
            })();
            let status = match result {
                Ok(status) => status,
                Err(e) => Status::Error(format!("Update not installed: {e:#}")),
            };
            updates.set_status(&env, status);
            updates.busy.store(false, Ordering::SeqCst);
        });
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        if let Status::Ready(restart) = &*self.status.lock().unwrap() {
            restart.spawn()?;
        } else {
            anyhow::bail!("No installed update is ready to restart");
        }
        Ok(())
    }
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(format!("hearthlink/{} (+https://github.com/{})", env!("CARGO_PKG_VERSION"), core::REPO))
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .expect("update HTTP client")
}

fn metadata(http: &reqwest::blocking::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..3 {
        let result = http.get(url).timeout(Duration::from_secs(20)).header("Accept", "application/json").send().and_then(|r| r.error_for_status());
        match result {
            Ok(response) => {
                let mut data = Vec::new();
                response.take(MAX_METADATA + 1).read_to_end(&mut data)?;
                anyhow::ensure!(data.len() as u64 <= MAX_METADATA, "Update metadata exceeds size limit");
                return Ok(data);
            }
            Err(e) => {
                let retry = e.is_timeout() || e.is_connect() || e.status().is_some_and(|s| s.is_server_error());
                last = Some(e);
                if !retry || attempt == 2 {
                    break;
                }
                std::thread::sleep(Duration::from_secs(attempt + 1));
            }
        }
    }
    Err(last.unwrap().into())
}

fn fetch() -> anyhow::Result<Option<Release>> {
    let http = http_client();
    // Published alongside packages by our release workflow; works without api.github.com.
    let feed = format!("https://github.com/{}/releases/latest/download/hearthlink-update.json", core::REPO);
    match metadata(&http, &feed) {
        Ok(data) => {
            let release: Release = serde_json::from_slice(&data)?;
            release.validate()?;
            return Ok(Some(release));
        }
        Err(e) => eprintln!("Update feed unavailable; trying GitHub API: {e}"),
    }
    // Compatibility with releases published before the update feed existed.
    let api = format!("https://api.github.com/repos/{}/releases", core::REPO);
    let data = match metadata(&http, &format!("{api}/latest")) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Latest release unavailable: {e}; trying release list");
            let list = metadata(&http, &format!("{api}?per_page=20"))?;
            let releases: Vec<serde_json::Value> = serde_json::from_slice(&list)?;
            let Some(release) = releases.into_iter().find(|r| r["draft"] == false && r["prerelease"] == false) else { return Ok(None) };
            return from_github(release).map(Some);
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&data)?;
    if value["draft"] == true || value["prerelease"] == true {
        return Ok(None);
    }
    from_github(value).map(Some)
}

fn from_github(value: serde_json::Value) -> anyhow::Result<Release> {
    let assets = value["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(Asset { name: a["name"].as_str()?.to_string(), sha256: a["digest"].as_str()?.strip_prefix("sha256:")?.to_string(), size: a["size"].as_u64()? })
        })
        .collect();
    let release = Release {
        version: value["tag_name"].as_str().context("Release has no tag")?.trim_start_matches('v').into(),
        notes: value["body"].as_str().unwrap_or_default().into(),
        assets,
    };
    release.validate()?;
    Ok(release)
}

pub fn how_to_update() -> &'static str {
    match core::install_kind() {
        Install::Flatpak => "Updates the installed Flatpak from the verified release bundle. System installations may request authorization.",
        Install::AppImage => "Replaces this AppImage at its current location and keeps a backup. Restart when ready.",
        Install::Package => "Downloads the matching DEB/RPM and uses PackageKit (may request authorization), or opens your software installer.",
        Install::Local => "Replaces this executable at its current location and keeps a backup. The folder must be writable.",
    }
}

pub fn start(env: Arc<Env>, updates: Arc<Updates>) {
    std::thread::spawn(move || loop {
        // Never discard a pending restart or an active installation on a timer.
        if !updates.busy.swap(true, Ordering::SeqCst) {
            if !matches!(*updates.status.lock().unwrap(), Status::Ready(_) | Status::InstallerOpened(_)) {
                updates.check(&env);
            }
            updates.busy.store(false, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_secs(6 * 3600));
    });
}
