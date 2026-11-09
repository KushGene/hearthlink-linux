//! Installs a local DEB/RPM through PackageKit's D-Bus API.
//!
//! Not via `pkcon`: `pkcon --noninteractive` marks the transaction non-interactive, so polkit
//! refuses without showing a password dialog ("Failed to obtain authentication"), and without
//! that flag pkcon asks its questions on /dev/tty, which a desktop app does not have.
use anyhow::Context;
use std::path::Path;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const SERVICE: &str = "org.freedesktop.PackageKit";
const TRANSACTION: &str = "org.freedesktop.PackageKit.Transaction";
// PkExitEnum
const EXIT_SUCCESS: u32 = 1;
const EXIT_CANCELLED: u32 = 3;

/// Opens a PackageKit transaction, or `None` when PackageKit is not available on this system.
pub(super) fn transaction() -> Option<(Connection, OwnedObjectPath)> {
    let connection = Connection::system().ok()?;
    let root = Proxy::new(&connection, SERVICE, "/org/freedesktop/PackageKit", SERVICE).ok()?;
    let path: OwnedObjectPath = root.call("CreateTransaction", &()).ok()?;
    Some((connection, path))
}

/// Runs `method` on the transaction with an interactive hint, so polkit may show its dialog.
pub(super) fn run<B>(connection: &Connection, path: OwnedObjectPath, method: &str, body: &B) -> anyhow::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let transaction = Proxy::new(connection, SERVICE, path, TRANSACTION)?;
    let mut hints = vec!["interactive=true".to_string()];
    if let Some(locale) = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())) {
        hints.push(format!("locale={locale}"));
    }
    transaction.call_method("SetHints", &(hints,)).context("PackageKit rejected the transaction hints")?;
    // Subscribe before starting, or a fast transaction can finish unseen.
    let signals = transaction.receive_all_signals()?;
    transaction.call_method(method, body).with_context(|| format!("PackageKit {method} failed"))?;
    let mut error = None;
    for message in signals {
        let header = message.header();
        match header.member().map(|m| m.as_str()) {
            Some("ErrorCode") => {
                let (_code, details): (u32, String) = message.body().deserialize()?;
                error = Some(details);
            }
            Some("Finished") => {
                let (exit, _runtime): (u32, u32) = message.body().deserialize()?;
                let details = error.unwrap_or_default();
                return match exit {
                    EXIT_SUCCESS => Ok(()),
                    EXIT_CANCELLED => anyhow::bail!("Installation was cancelled. {details}"),
                    _ => anyhow::bail!("System installer failed or was cancelled: {details}"),
                };
            }
            Some("Destroy") => break,
            _ => {}
        }
    }
    anyhow::bail!("PackageKit ended the transaction without a result")
}

/// Installs a verified local package. `Ok(false)` means PackageKit is unavailable.
pub(super) fn install_file(package: &Path) -> anyhow::Result<bool> {
    let Some((connection, path)) = transaction() else { return Ok(false) };
    let file = package.to_str().context("Update path is not valid UTF-8")?;
    // Flags 0, not ONLY_TRUSTED: the package is unsigned, but its SHA-256 was verified against the release.
    run(&connection, path, "InstallFiles", &(0u64, vec![file]))?;
    Ok(true)
}
