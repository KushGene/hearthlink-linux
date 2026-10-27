//! Settings files in ~/.config/hearthlink and data in ~/.local/share/hearthlink.

use serde::{de::DeserializeOwned, Serialize};
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    let d = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join(super::APP_ID);
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn data_dir() -> PathBuf {
    let d = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(super::APP_ID);
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn load<T: DeserializeOwned + Default>(name: &str) -> T {
    let path = config_dir().join(format!("{name}.json"));
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            eprintln!("couldn't read {}: {e}; using defaults", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

pub fn save<T: Serialize>(name: &str, value: &T) {
    let path = config_dir().join(format!("{name}.json"));
    match serde_json::to_vec_pretty(value) {
        Ok(b) => {
            if let Err(e) = write_private(&path, &b) {
                eprintln!("couldn't save {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("couldn't encode {name}: {e}"),
    }
}

/// Like `write_atomic`, but the file is only ever readable by the user (it holds tokens).
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}
