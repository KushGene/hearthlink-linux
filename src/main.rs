mod autostart;
mod core;
mod services;
mod tray;
mod ui;
mod update;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub const APP_ICON: &[u8] = include_bytes!("../assets/icons/app.png");

fn single_instance() -> Option<std::os::unix::net::UnixListener> {
    let sock = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("hearthlink.sock");
    if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        return None; // another instance answered: it shows its window
    }
    let _ = std::fs::remove_file(&sock);
    std::os::unix::net::UnixListener::bind(&sock).ok()
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let minimized_arg = args.iter().any(|a| a == "--minimized");
    if args.iter().any(|a| a == "--wowhead-dry-run") {
        services::wowhead::dry_run(core::Env::load());
        return Ok(());
    }
    let Some(listener) = single_instance() else {
        eprintln!("Hearthlink is already running; showing its window.");
        return Ok(());
    };
    // Hiding to the tray needs a window that can be hidden; on Wayland that isn't
    // possible, so the app runs through XWayland unless asked otherwise.
    let wayland = std::env::var_os("HEARTHLINK_WAYLAND").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !wayland && std::env::var_os("DISPLAY").is_some() {
        // SAFETY: single-threaded at this point.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    let env = core::Env::load();
    let services = services::all(&env);
    for s in &services {
        s.start();
    }
    let show = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));
    {
        let show = show.clone();
        let env = env.clone();
        std::thread::spawn(move || {
            for _ in listener.incoming().flatten() {
                show.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(c) = env.ctx.lock().unwrap().as_ref() {
                    c.request_repaint();
                }
            }
        });
    }
    install_icon();
    if autostart::enabled() {
        autostart::set(true); // keeps the entry's command and icon current
    }
    let updates = Arc::new(update::Updates::default());
    update::start(env.clone(), updates.clone());
    let start_hidden = minimized_arg || env.config.read().unwrap().start_minimized;
    let icon = image::load_from_memory(APP_ICON).ok().map(|i| {
        let i = i.to_rgba8();
        eframe::egui::IconData { width: i.width(), height: i.height(), rgba: i.into_raw() }
    });
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(core::APP_NAME)
        .with_app_id(core::DESKTOP_ID)
        .with_inner_size([980.0, 680.0])
        .with_min_inner_size([640.0, 460.0])
        .with_visible(!start_hidden || wayland);
    if let Some(i) = icon {
        viewport = viewport.with_icon(Arc::new(i));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        core::APP_NAME,
        options,
        Box::new(move |cc| {
            let tray = tray::spawn(services.clone(), show.clone(), quit.clone(), Some(cc.egui_ctx.clone()));
            if let Some(handle) = tray {
                // Refresh the tray's status lines now and then.
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(15));
                    handle.update(|_| {});
                });
            }
            if start_hidden && wayland {
                cc.egui_ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(true));
            }
            Ok(Box::new(ui::App::new(cc, env, services, updates, show, quit, wayland)))
        }),
    )
}

/// Puts the logo into the user's icon theme, so notifications and the autostart entry
/// (which only name the icon `hearthlink`) show it.
fn install_icon() {
    if core::install_kind() != core::Install::Local {
        return; // packages and AppImages bring their own icon
    }
    let Some(dir) = dirs::data_dir().map(|d| d.join("icons/hicolor/256x256/apps")) else { return };
    let path = dir.join(format!("{}.png", core::DESKTOP_ID));
    let Ok(img) = image::load_from_memory(APP_ICON) else { return };
    let mut png = Vec::new();
    let small = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
    if small.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).is_err() {
        return;
    }
    if std::fs::read(&path).ok().as_deref() != Some(png.as_slice()) {
        let _ = core::write_atomic(&path, &png);
    }
}
