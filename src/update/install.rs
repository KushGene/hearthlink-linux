use super::{Asset, Release, Status};
use crate::core::{self, Install};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) const MAX_DOWNLOAD: u64 = 1024 * 1024 * 1024;
const MAX_BINARY: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    AppImage,
    Tar,
    Deb,
    Rpm,
    Flatpak,
}

#[derive(Debug, Clone)]
pub struct Restart {
    pub executable: Option<PathBuf>, // None = Flatpak
    pub backup: Option<PathBuf>,
}

impl Restart {
    pub(super) fn command(&self) -> Command {
        let mut command = match &self.executable {
            Some(path) => Command::new(path),
            None => {
                let mut command = core::host_command(Path::new("flatpak"), None);
                command.args(["run", core::DESKTOP_ID]);
                command
            }
        };
        command.arg("--restart-wait").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // Do not leak an old AppImage's mount/runtime environment into its replacement.
        for key in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            command.env_remove(key);
        }
        if let Some(display) = core::SESSION_WAYLAND_DISPLAY.get() {
            command.env("WAYLAND_DISPLAY", display);
        }
        command
    }

    pub fn spawn(&self) -> anyhow::Result<()> {
        self.command().spawn().context("Could not start the updated app; the current app is still running")?;
        Ok(())
    }
}

pub(super) fn asset_name(version: &str, format: Format, arch: &str) -> anyhow::Result<String> {
    // CI currently publishes x86_64 only. Never install a binary for another CPU.
    anyhow::ensure!(arch == "x86_64", "No update packages are published for architecture {arch}");
    Ok(match format {
        Format::AppImage => format!("Hearthlink-{version}-x86_64.AppImage"),
        Format::Tar => format!("hearthlink-{version}-x86_64.tar.gz"),
        Format::Deb => "Hearthlink.amd64.deb".into(),
        Format::Rpm => "Hearthlink.x86_64.rpm".into(),
        Format::Flatpak => format!("Hearthlink-{version}-x86_64.flatpak"),
    })
}

fn package_format(executable: &Path) -> anyhow::Result<Format> {
    if Command::new("dpkg-query").arg("-S").arg(executable).output().is_ok_and(|o| o.status.success()) {
        return Ok(Format::Deb);
    }
    if Command::new("rpm").arg("-qf").arg(executable).output().is_ok_and(|o| o.status.success()) {
        return Ok(Format::Rpm);
    }
    if crate::update::pacman_owned(executable) {
        anyhow::bail!("Installed with pacman (AUR): update Hearthlink with your AUR helper, for example yay -Syu or paru -Syu");
    }
    anyhow::bail!("Could not identify the package owning {}; update it through your system's package manager", executable.display())
}

pub(super) fn install(release: &Release, progress: impl Fn(String)) -> anyhow::Result<Status> {
    let kind = core::install_kind();
    let executable =
        if kind == Install::AppImage { std::fs::canonicalize(std::env::var_os("APPIMAGE").context("APPIMAGE is not set")?)? } else { std::env::current_exe()? };
    let format = match kind {
        Install::AppImage => Format::AppImage,
        Install::Local => Format::Tar,
        Install::Package => package_format(&executable)?,
        Install::Flatpak => Format::Flatpak,
    };
    let name = asset_name(&release.version, format, std::env::consts::ARCH)?;
    let mut matching = release.assets.iter().filter(|a| a.name == name);
    let asset = matching.next().with_context(|| format!("Release {} has no verified {name}", release.version))?;
    anyhow::ensure!(matching.next().is_none(), "Release lists the update package more than once");
    if matches!(format, Format::AppImage | Format::Tar) {
        check_target(&executable)?;
    }
    // Keep the download on disk, not in RAM. TempDir is private (0700).
    let cache = dirs::cache_dir().context("No cache directory")?.join(core::APP_ID).join("updates");
    std::fs::create_dir_all(&cache)?;
    let dir = tempfile::Builder::new().prefix("download-").tempdir_in(cache)?;
    let package = dir.path().join(&name);
    progress(format!("Downloading Hearthlink {}…", release.version));
    let url = format!("https://github.com/{}/releases/download/v{}/{name}", core::REPO, release.version);
    download(&super::http_client(), &url, asset, &package)?;
    progress("Download verified (SHA-256). Installing…".into());
    match format {
        Format::AppImage | Format::Tar => {
            let backup = replace(&executable, &package, format)?;
            Ok(Status::Ready(Restart { executable: Some(executable), backup: Some(backup) }))
        }
        Format::Flatpak => {
            let info = std::fs::read_to_string("/.flatpak-info")?;
            let scope = flatpak_scope(&info, &core::host_data_dir())?;
            let mut command = core::host_command(Path::new("flatpak"), None);
            command.args(["install", "--noninteractive", "--or-update", "--bundle", scope]).arg(&package);
            run_installer(&mut command)?;
            Ok(Status::Ready(Restart { executable: None, backup: None }))
        }
        Format::Deb | Format::Rpm => {
            progress("Installing through your package manager; an authorization dialog may appear…".into());
            let available = Command::new("pkcon").arg("--version").output().is_ok_and(|o| o.status.success());
            if available {
                run_installer(Command::new("pkcon").args(["--noninteractive", "install-local"]).arg(&package))?;
                Ok(Status::Ready(Restart { executable: Some(executable), backup: None }))
            } else {
                // Opening the package isn't proof it was installed. Do not show "installed".
                open::that(&package).context("No package installer available; install the release package manually")?;
                let kept = dir.keep();
                Ok(Status::InstallerOpened(format!(
                    "Finish the update in your software installer, then close and reopen Hearthlink. Verified package: {}",
                    kept.join(name).display()
                )))
            }
        }
    }
}

pub(super) fn flatpak_scope(info: &str, host_data: &Path) -> anyhow::Result<&'static str> {
    let app_path = info.lines().find_map(|l| l.strip_prefix("app-path=")).context("Flatpak deployment path is missing")?;
    let path = Path::new(app_path);
    if path.starts_with(host_data.join("flatpak")) {
        return Ok("--user");
    }
    if path.starts_with("/var/lib/flatpak") {
        return Ok("--system");
    }
    anyhow::bail!("Custom Flatpak installation: update this bundle through your software manager")
}

pub(super) fn run_installer(command: &mut Command) -> anyhow::Result<()> {
    let output = command.stdin(Stdio::null()).output().context("Could not start the system installer")?;
    anyhow::ensure!(
        output.status.success(),
        "System installer failed or was cancelled: {} {}",
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}

pub(super) fn download(http: &reqwest::blocking::Client, url: &str, asset: &Asset, path: &Path) -> anyhow::Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        let result = download_once(http, url, asset, path);
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = e
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|e| e.is_timeout() || e.is_connect() || e.is_body() || e.status().is_some_and(|s| s.is_server_error()));
                last = Some(e);
                if !transient || attempt == 2 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(attempt + 1));
            }
        }
    }
    Err(last.unwrap())
}

fn download_once(http: &reqwest::blocking::Client, url: &str, asset: &Asset, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(asset.size > 0 && asset.size <= MAX_DOWNLOAD, "Update download is too large");
    let mut response = http.get(url).send()?.error_for_status()?;
    if let Some(size) = response.content_length() {
        anyhow::ensure!(size == asset.size, "Unexpected update size");
    }
    let mut file = File::create(path)?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        anyhow::ensure!(total <= asset.size, "Update exceeds expected size");
        digest.update(&buffer[..n]);
        file.write_all(&buffer[..n])?;
    }
    anyhow::ensure!(total == asset.size, "Incomplete update download");
    anyhow::ensure!(hex::encode(digest.finalize()).eq_ignore_ascii_case(&asset.sha256), "Update checksum mismatch; nothing was installed");
    file.sync_all()?;
    Ok(())
}

fn check_target(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(meta.is_file(), "Update target is not a regular file");
    anyhow::ensure!(meta.uid() == unsafe { libc::geteuid() }, "The executable is owned by another user; update it with your system installer");
    anyhow::ensure!(meta.permissions().mode() & 0o6000 == 0, "Refusing to replace a privileged executable");
    Ok(())
}

pub(super) fn replace(target: &Path, package: &Path, format: Format) -> anyhow::Result<PathBuf> {
    check_target(target)?;
    let parent = target.parent().context("Executable has no parent folder")?;
    // Same-filesystem staging makes the final replacement a single atomic rename.
    let mut fresh = tempfile::Builder::new()
        .prefix(".hearthlink-update-")
        .tempfile_in(parent)
        .context("The app's folder is not writable; move it to a writable folder or use your system installer")?;
    if format == Format::Tar {
        extract_binary(File::open(package)?, &mut fresh)?;
    } else {
        anyhow::ensure!(format == Format::AppImage, "Not a directly replaceable package");
        std::io::copy(&mut File::open(package)?, &mut fresh)?;
    }
    // Reject damaged/mispackaged payloads before touching the old executable.
    fresh.rewind()?;
    let mut header = [0u8; 20];
    fresh.read_exact(&mut header).context("Update executable is truncated")?;
    anyhow::ensure!(&header[..4] == b"\x7fELF" && header[4] == 2 && header[5] == 1 && header[18..20] == [62, 0], "Update is not a Linux x86_64 executable");
    if format == Format::AppImage {
        anyhow::ensure!(&header[8..11] == b"AI\x02", "Update is not a type-2 AppImage");
    }
    fresh.as_file().set_permissions(std::fs::Permissions::from_mode(0o755))?;
    fresh.as_file().sync_all()?;
    let mut backup = tempfile::Builder::new().prefix(".hearthlink-backup-").tempfile_in(parent)?;
    std::io::copy(&mut File::open(target)?, &mut backup)?;
    backup.as_file().set_permissions(std::fs::Permissions::from_mode(0o755))?;
    backup.as_file().sync_all()?;
    let (_, backup_path) = backup.keep()?;
    fresh.persist(target).map_err(|e| e.error).context("Could not replace the app; the previous executable is unchanged")?;
    File::open(parent)?.sync_all()?;
    Ok(backup_path)
}

pub(super) fn extract_binary(reader: impl Read, writer: &mut impl Write) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    let mut found = false;
    let mut expanded = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        expanded = expanded.checked_add(entry.size()).context("Archive size overflow")?;
        anyhow::ensure!(expanded <= MAX_BINARY, "Update archive exceeds extraction limit");
        let path = entry.path()?.into_owned();
        let parts: Vec<_> = path.components().collect();
        if path.file_name().is_some_and(|n| n == "hearthlink") {
            anyhow::ensure!(parts.len() == 2 && parts.iter().all(|c| matches!(c, std::path::Component::Normal(_))), "Unsafe executable path in update archive");
            anyhow::ensure!(entry.header().entry_type().is_file() && !found, "Invalid or duplicate executable in update archive");
            anyhow::ensure!(entry.size() > 0, "Empty update executable");
            std::io::copy(&mut entry, writer)?;
            found = true;
        }
    }
    anyhow::ensure!(found, "Update archive contains no Hearthlink executable");
    Ok(())
}
