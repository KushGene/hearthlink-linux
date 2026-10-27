//! Looks for a newer Hearthlink release on GitHub (every 6 hours) and tells the user once
//! per version.

use crate::core::{self, Env, Install};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

#[derive(Default)]
pub struct Updates {
    pub latest: Mutex<Option<Release>>,
    pub checked_at: Mutex<i64>,
}

impl Updates {
    /// The newer release, if there is one.
    pub fn newer(&self) -> Option<Release> {
        let r = self.latest.lock().unwrap().clone()?;
        core::version_cmp(&r.version, env!("CARGO_PKG_VERSION")).is_gt().then_some(r)
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

fn fetch() -> anyhow::Result<Option<Release>> {
    let http = core::http_client(&format!("hearthlink/{} (+https://github.com/{})", env!("CARGO_PKG_VERSION"), core::REPO), 30);
    let resp = http
        .get(format!("https://api.github.com/repos/{}/releases/latest", core::REPO))
        .header("Accept", "application/vnd.github+json")
        .send()?;
    // No release published yet (or no repo yet): nothing to report.
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    let r: GhRelease = resp.error_for_status()?.json()?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    Ok(Some(Release { version: r.tag_name.trim_start_matches(['v', 'V']).to_string(), url: r.html_url, notes: r.body }))
}

/// What to do to update, depending on how Hearthlink was installed.
pub fn how_to_update() -> &'static str {
    match core::install_kind() {
        Install::Flatpak => "Download the new .flatpak from the release page and open it, or run: flatpak update",
        Install::AppImage => "Download the new AppImage from the release page and replace the old file.",
        Install::Package => "Download the new .rpm or .deb from the release page and open it (Discover installs it over the old one).",
        Install::Local => "Download the new release and run its install.sh again.",
    }
}

pub fn start(env: Arc<Env>, updates: Arc<Updates>) {
    std::thread::spawn(move || loop {
        match fetch() {
            Ok(r) => {
                *updates.latest.lock().unwrap() = r;
                *updates.checked_at.lock().unwrap() = core::now_unix();
                if let Some(r) = updates.newer() {
                    let told = env.config.read().unwrap().notified_app_version.clone();
                    if told.as_deref() != Some(r.version.as_str()) {
                        env.notify(
                            &format!("Hearthlink {} is available", r.version),
                            &format!("You have {}. {}", env!("CARGO_PKG_VERSION"), how_to_update()),
                        );
                        env.config.write().unwrap().notified_app_version = Some(r.version.clone());
                        env.save();
                    }
                }
                if let Some(c) = env.ctx.lock().unwrap().as_ref() {
                    c.request_repaint();
                }
            }
            Err(e) => eprintln!("update check failed: {e}"),
        }
        std::thread::sleep(Duration::from_secs(6 * 3600));
    });
}
