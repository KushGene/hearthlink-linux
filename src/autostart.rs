//! "Start with my desktop session" via an XDG autostart entry.

use std::path::PathBuf;

fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("autostart").join("hearthlink.desktop")
}

pub fn enabled() -> bool {
    path().exists()
}

pub fn set(on: bool) {
    let p = path();
    if on {
        use crate::core::{install_kind, Install, DESKTOP_ID};
        let exec = match install_kind() {
            Install::Flatpak => format!("flatpak run {DESKTOP_ID} --minimized"),
            // The AppImage's own path, not the temporary mount it runs from.
            Install::AppImage => format!("\"{}\" --minimized", std::env::var("APPIMAGE").unwrap_or_default()),
            _ => {
                let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_else(|_| "hearthlink".into());
                format!("\"{exe}\" --minimized")
            }
        };
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Hearthlink\nComment=World of Warcraft data uploaders\nExec={exec}\nIcon={DESKTOP_ID}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&p, entry);
    } else {
        let _ = std::fs::remove_file(&p);
    }
}
