use super::install::{self, Format};
use super::*;
use std::io::Write;

fn temp() -> tempfile::TempDir {
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn release() -> Release {
    Release { version: "99.0.0".into(), notes: String::new(), assets: vec![Asset { name: "Hearthlink.amd64.deb".into(), sha256: "ab".repeat(32), size: 100 }] }
}

#[test]
fn validates_release_paths_checksums_and_sizes() {
    let good = release();
    good.validate().unwrap();
    for version in ["", "v1.4.0", "1.4.0/../evil", "1.4.0-beta", "1.4"] {
        let mut bad = good.clone();
        bad.version = version.into();
        assert!(bad.validate().is_err(), "{version}");
    }
    for name in ["../app", "app/name", "app\\name", "file?x=y", ""] {
        let mut bad = good.clone();
        bad.assets[0].name = name.into();
        assert!(bad.validate().is_err(), "{name}");
    }
    let mut bad = good.clone();
    bad.assets[0].sha256 = "a".repeat(63);
    assert!(bad.validate().is_err());
    bad.assets[0].sha256 = "z".repeat(64);
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.assets[0].size = 0;
    assert!(bad.validate().is_err());
    bad.assets[0].size = install::MAX_DOWNLOAD + 1;
    assert!(bad.validate().is_err());
}

#[test]
fn selects_exact_installation_format_and_architecture() {
    assert_eq!(install::asset_name("1.4.0", Format::AppImage, "x86_64").unwrap(), "Hearthlink-1.4.0-x86_64.AppImage");
    assert_eq!(install::asset_name("1.4.0", Format::Tar, "x86_64").unwrap(), "hearthlink-1.4.0-x86_64.tar.gz");
    assert_eq!(install::asset_name("1.4.0", Format::Flatpak, "x86_64").unwrap(), "Hearthlink-1.4.0-x86_64.flatpak");
    assert_eq!(install::asset_name("1.4.0", Format::Deb, "x86_64").unwrap(), "Hearthlink.amd64.deb");
    assert_eq!(install::asset_name("1.4.0", Format::Rpm, "x86_64").unwrap(), "Hearthlink.x86_64.rpm");
    assert!(install::asset_name("1.4.0", Format::Tar, "aarch64").is_err());
}

#[test]
fn github_metadata_never_trusts_download_urls() {
    let parsed = from_github(serde_json::json!({"tag_name":"v1.4.0", "assets":[{
        "name":"Hearthlink.amd64.deb", "digest":format!("sha256:{}", "ab".repeat(32)), "size":100,
        "browser_download_url":"https://untrusted.invalid/evil"
    }], "html_url":"https://untrusted.invalid/"}))
    .unwrap();
    assert_eq!(parsed.assets.len(), 1);
    assert_eq!(parsed.url(), "https://github.com/KushGene/hearthlink-linux/releases/tag/v1.4.0");
    let missing = from_github(serde_json::json!({"tag_name":"v1.4.0", "assets":[{"name":"x", "size":1}]})).unwrap();
    assert!(missing.assets.is_empty()); // Such an asset cannot be selected for installation.
}

fn archive(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, data, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(data.len() as u64);
        header.set_entry_type(*kind);
        if kind.is_symlink() {
            header.set_link_name("/etc/passwd").unwrap();
        }
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn extracts_only_one_regular_executable_without_unpacking_other_files() {
    let data = archive(&[("release/README.md", b"ignored", tar::EntryType::Regular), ("release/hearthlink", b"new executable", tar::EntryType::Regular)]);
    let mut output = Vec::new();
    install::extract_binary(data.as_slice(), &mut output).unwrap();
    assert_eq!(output, b"new executable");
    for entries in [
        vec![("release/hearthlink", b"".as_slice(), tar::EntryType::Symlink)],
        vec![("release/hearthlink", b"".as_slice(), tar::EntryType::Regular)],
        vec![("release/nested/hearthlink", b"x".as_slice(), tar::EntryType::Regular)],
        vec![("release/hearthlink", b"x".as_slice(), tar::EntryType::Regular), ("release/hearthlink", b"y".as_slice(), tar::EntryType::Regular)],
        vec![("release/not-hearthlink", b"x".as_slice(), tar::EntryType::Regular)],
    ] {
        assert!(install::extract_binary(archive(&entries).as_slice(), &mut Vec::new()).is_err());
    }
}

#[test]
fn replaces_atomically_and_preserves_the_previous_binary() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    let target = dir.path().join("hearthlink");
    let package = dir.path().join("new.tar.gz");
    std::fs::write(&target, b"old executable").unwrap();
    let new = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00new executable";
    std::fs::write(&package, archive(&[("release/hearthlink", new, tar::EntryType::Regular)])).unwrap();
    let backup = install::replace(&target, &package, Format::Tar).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), new);
    assert_eq!(std::fs::read(&backup).unwrap(), b"old executable");
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::write(&package, b"not an archive").unwrap();
    assert!(install::replace(&target, &package, Format::Tar).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), new);
}

#[test]
fn refuses_symlink_replacement_targets() {
    let dir = temp();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::write(&target, b"old").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(install::replace(&link, &target, Format::AppImage).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

#[test]
fn flatpak_updates_the_same_installation_scope() {
    let data = std::path::Path::new("/home/test/.local/share");
    assert_eq!(install::flatpak_scope("[Instance]\napp-path=/home/test/.local/share/flatpak/app/id/x86_64/stable/hash/files", data).unwrap(), "--user");
    assert_eq!(install::flatpak_scope("app-path=/var/lib/flatpak/app/id/x86_64/stable/hash/files", data).unwrap(), "--system");
    assert!(install::flatpak_scope("app-path=/custom/installation", data).is_err());
    assert!(install::flatpak_scope("", data).is_err());
}

fn serve(body: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
        loop {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        socket.write_all(body).unwrap();
    });
    (address, thread)
}

#[test]
fn verifies_download_before_installation() {
    let dir = temp();
    let path = dir.path().join("download");
    let http = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    let mut asset = Asset { name: "test".into(), size: 7, sha256: core::sha256_hex(b"package") };
    let (url, server) = serve(b"package");
    install::download(&http, &url, &asset, &path).unwrap();
    server.join().unwrap();
    asset.sha256 = "0".repeat(64);
    let (url, server) = serve(b"package");
    assert!(install::download(&http, &url, &asset, &path).unwrap_err().to_string().contains("checksum"));
    server.join().unwrap();
    asset.size = 6;
    let (url, server) = serve(b"package");
    assert!(install::download(&http, &url, &asset, &path).is_err());
    server.join().unwrap();
}

#[test]
fn no_downgrade_is_offered() {
    let updates = Updates::default();
    let mut older = release();
    older.version = "0.1.0".into();
    *updates.latest.lock().unwrap() = Some(older);
    assert!(updates.newer().is_none());
    let mut newer = release();
    newer.version = "999.0.0".into();
    *updates.latest.lock().unwrap() = Some(newer);
    assert!(updates.newer().is_some());
}

#[test]
fn installer_failure_is_not_reported_as_success() {
    use std::process::Command;
    install::run_installer(&mut Command::new("true")).unwrap();
    assert!(install::run_installer(&mut Command::new("false")).is_err());
    assert!(install::run_installer(&mut Command::new("/no/such/hearthlink-installer")).is_err());
}

#[test]
fn restart_waits_for_the_old_instance_and_does_not_inherit_an_old_appimage() {
    let restart = install::Restart { executable: Some("/home/test/My Apps/Hearthlink.AppImage".into()), backup: None };
    let command = restart.command();
    assert_eq!(command.get_program(), "/home/test/My Apps/Hearthlink.AppImage");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["--restart-wait"]);
    for name in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        assert!(command.get_envs().any(|(key, value)| key == name && value.is_none()));
    }
}

#[test]
fn appimage_replacement_checks_the_payload_and_keeps_a_backup() {
    let dir = temp();
    let target = dir.path().join("Hearthlink.AppImage");
    let package = dir.path().join("download.AppImage");
    std::fs::write(&target, b"old image").unwrap();
    let image = b"\x7fELF\x02\x01\x01\x00AI\x02\x00\x00\x00\x00\x00\x02\x00\x3e\x00new image";
    std::fs::write(&package, image).unwrap();
    let backup = install::replace(&target, &package, Format::AppImage).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), image);
    assert_eq!(std::fs::read(&backup).unwrap(), b"old image");
    std::fs::write(&package, b"not an AppImage").unwrap();
    assert!(install::replace(&target, &package, Format::AppImage).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), image);
}

#[test]
#[ignore = "needs a running PackageKit daemon"]
fn packagekit_transaction_reports_finished() {
    let (connection, path) = super::packagekit::transaction().expect("PackageKit available");
    super::packagekit::run(&connection, path, "Resolve", &(0u64, vec!["bash"])).unwrap();
}

#[test]
#[ignore = "installs HEARTHLINK_PK_FILE system-wide; shows a polkit dialog"]
fn packagekit_installs_local_file() {
    let file = std::env::var("HEARTHLINK_PK_FILE").expect("HEARTHLINK_PK_FILE");
    assert!(super::packagekit::install_file(std::path::Path::new(&file)).unwrap());
}
