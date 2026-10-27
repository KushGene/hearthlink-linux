//! The tray icon (StatusNotifierItem: KDE Plasma, GNOME with the AppIndicator extension,
//! most other desktops).

use crate::services::Service;
use ksni::blocking::TrayMethods;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Tray {
    services: Vec<Arc<dyn Service>>,
    show: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    ctx: Option<eframe::egui::Context>,
    icon: ksni::Icon,
}

impl Tray {
    fn wake_ui(&self) {
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        crate::core::APP_ID.into()
    }
    fn title(&self) -> String {
        crate::core::APP_NAME.into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let lines: Vec<String> = self
            .services
            .iter()
            .filter(|s| s.enabled())
            .map(|s| format!("{}: {}", s.meta().name, short(&s.summary(), 60)))
            .collect();
        ksni::ToolTip { title: crate::core::APP_NAME.into(), description: lines.join("\n"), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.show.store(true, Ordering::SeqCst);
        self.wake_ui();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![StandardItem {
            label: "Open Hearthlink".into(),
            activate: Box::new(|t: &mut Self| {
                t.show.store(true, Ordering::SeqCst);
                t.wake_ui();
            }),
            ..Default::default()
        }
        .into()];
        items.push(MenuItem::Separator);
        for (i, s) in self.services.iter().enumerate() {
            if !s.enabled() {
                continue;
            }
            items.push(
                StandardItem {
                    label: format!("{}: {}", s.meta().name, short(&s.summary(), 60)),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: format!("    Upload to {} now", s.meta().name),
                    activate: Box::new(move |t: &mut Self| t.services[i].upload_now()),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    t.quit.store(true, Ordering::SeqCst);
                    t.wake_ui();
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(800));
                        std::process::exit(0);
                    });
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

fn icon_from_png(png: &[u8]) -> ksni::Icon {
    let img = image::load_from_memory(png).map(|i| i.resize(64, 64, image::imageops::FilterType::Lanczos3).to_rgba8());
    match img {
        Ok(img) => {
            let (w, h) = img.dimensions();
            let mut data = Vec::with_capacity((w * h * 4) as usize);
            for p in img.pixels() {
                let [r, g, b, a] = p.0;
                data.extend_from_slice(&[a, r, g, b]);
            }
            ksni::Icon { width: w as i32, height: h as i32, data }
        }
        Err(_) => ksni::Icon { width: 0, height: 0, data: vec![] },
    }
}

/// Starts the tray; returns a handle to refresh its menu, or None when no tray host runs.
pub fn spawn(
    services: Vec<Arc<dyn Service>>,
    show: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    ctx: Option<eframe::egui::Context>,
) -> Option<ksni::blocking::Handle<impl ksni::Tray>> {
    let tray = Tray { services, show, quit, ctx, icon: icon_from_png(crate::APP_ICON) };
    match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("no tray available: {e}");
            None
        }
    }
}

/// Shortens a status line for the tray (some, like ForeverDB's last upload, are long).
fn short(s: &str, max: usize) -> String {
    let s = s.lines().next().unwrap_or_default();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}
