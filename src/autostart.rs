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
        use crate::core::DESKTOP_ID;
        let exec = format!("{} --minimized", crate::core::self_exec());
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
