pub mod foreverdb;
pub mod wowaudit;
pub mod wowhead;
pub mod wowutils;

use crate::core::{Common, Env};
use eframe::egui;
use std::sync::Arc;

/// Describes a service and the Windows app it replaces.
pub struct Meta {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub icon: &'static [u8],
    pub website: &'static str,
    /// The Windows app this module was reverse engineered from.
    pub upstream_app: &'static str,
    pub ported_version: &'static str,
    /// Where the user can read what changed upstream.
    pub upstream_url: &'static str,
    /// The service's own Linux app does the work (nothing reverse engineered to drift).
    pub runs_official: bool,
}

pub trait Service: Send + Sync {
    fn meta(&self) -> &'static Meta;
    fn common(&self) -> &Arc<Common>;
    fn enabled(&self) -> bool;
    fn set_enabled(&self, on: bool);
    /// Starts the background worker. Called once.
    fn start(&self);
    /// The service's page.
    fn ui(&self, ui: &mut egui::Ui);
    /// One line for the overview page / tray.
    fn summary(&self) -> String {
        self.common().health().1
    }
    /// "Upload now" from the tray.
    fn upload_now(&self) {}
}

pub fn all(env: &Arc<Env>) -> Vec<Arc<dyn Service>> {
    vec![
        Arc::new(foreverdb::ForeverDb::new(env.clone())),
        Arc::new(wowhead::Wowhead::new(env.clone())),
        Arc::new(wowutils::WowUtils::new(env.clone())),
        Arc::new(wowaudit::WowAudit::new(env.clone())),
    ]
}

/// Starts the shared upstream-version watcher for a service: `check` returns the newest
/// upstream version (and optional notes). It's called at start and every 6 hours.
pub fn watch_upstream(
    env: Arc<Env>,
    common: Arc<Common>,
    meta: &'static Meta,
    check: impl Fn() -> anyhow::Result<(String, Option<String>)> + Send + 'static,
) {
    std::thread::spawn(move || loop {
        let result = check();
        let mut notify = None;
        {
            let mut up = common.upstream.lock().unwrap();
            up.checked_at = crate::core::now_unix();
            match result {
                Ok((latest, notes)) => {
                    let newer = crate::core::version_cmp(&latest, meta.ported_version).is_gt();
                    if newer && up.latest.as_deref() != Some(latest.as_str()) {
                        let seen = env.config.read().unwrap().dismissed_upstream.get(meta.id).cloned();
                        if seen.as_deref() != Some(latest.as_str()) {
                            notify = Some(latest.clone());
                        }
                    }
                    up.latest = Some(latest);
                    up.notes = notes;
                    up.error = None;
                }
                Err(e) => up.error = Some(crate::core::friendly_error(&e)),
            }
        }
        if let Some(v) = notify {
            env.notify(
                &format!("{} {} is out", meta.upstream_app, v),
                &format!(
                    "This Linux app is based on {}. Things that changed may not be in it yet.",
                    meta.ported_version
                ),
            );
        }
        common.repaint();
        std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
    });
}
