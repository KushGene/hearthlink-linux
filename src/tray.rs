//! The tray icon (StatusNotifierItem: KDE Plasma, GNOME with the AppIndicator extension,
//! most other desktops).

use crate::core::{Env, Health, Notify};
use crate::services::Service;
use crate::ui::Goto;
use ksni::blocking::TrayMethods;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Tray {
    services: Vec<Arc<dyn Service>>,
    env: Arc<Env>,
    updates: Arc<crate::update::Updates>,
    show: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    goto: Goto,
    ctx: Option<eframe::egui::Context>,
    /// The app icon: plain, with an amber dot (something to do) and with a red dot (an error).
    icons: [ksni::Icon; 3],
    /// Small service icons with a status dot, as PNG for the menu, by (service, health).
    menu_icons: Mutex<HashMap<(&'static str, Health), Vec<u8>>>,
}

impl Tray {
    fn open(&self, page: Option<&'static str>) {
        if let Some(p) = page {
            *self.goto.lock().unwrap() = Some(p);
        }
        self.show.store(true, Ordering::SeqCst);
        if let Some(c) = &self.ctx {
            c.request_repaint();
        }
    }

    fn health(s: &dyn Service) -> Health {
        if !s.enabled() {
            Health::Off
        } else if s.common().is_busy() {
            Health::Busy
        } else {
            s.common().health().0
        }
    }

    /// The worst state of the services that are on.
    fn worst(&self) -> Health {
        let hs: Vec<Health> = self.services.iter().filter(|s| s.enabled()).map(|s| Self::health(s.as_ref())).collect();
        if hs.contains(&Health::Error) {
            Health::Error
        } else if hs.contains(&Health::Attention) {
            Health::Attention
        } else {
            Health::Ok
        }
    }

    fn menu_icon(&self, s: &dyn Service) -> Vec<u8> {
        let h = Self::health(s);
        let key = (s.meta().id, h);
        let mut cache = self.menu_icons.lock().unwrap();
        cache.entry(key).or_insert_with(|| menu_png(s.meta().icon, h)).clone()
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
        let i = match self.worst() {
            Health::Error => 2,
            Health::Attention => 1,
            _ => 0,
        };
        vec![self.icons[i].clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let mut lines: Vec<String> = self
            .services
            .iter()
            .filter(|s| s.enabled())
            .map(|s| format!("{}: {}", s.meta().name, short(&s.summary(), 48)))
            .collect();
        if lines.is_empty() {
            lines.push("No service is turned on".into());
        }
        if let Some(r) = self.updates.newer() {
            lines.push(format!("Hearthlink {} is available", r.version));
        }
        ksni::ToolTip { title: crate::core::APP_NAME.into(), description: lines.join("\n"), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.open(None);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![StandardItem {
            label: "Open Hearthlink".into(),
            icon_name: "window-new".into(),
            activate: Box::new(|t: &mut Self| t.open(None)),
            ..Default::default()
        }
        .into()];
        if let Some(r) = self.updates.newer() {
            items.push(
                StandardItem {
                    label: format!("Hearthlink {} is available…", r.version),
                    icon_name: "system-software-update".into(),
                    activate: Box::new(|t: &mut Self| t.open(None)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);

        let any = self.services.iter().any(|s| s.enabled() && s.in_upload_all());
        items.push(
            StandardItem {
                label: "Upload everything now".into(),
                icon_name: "go-up".into(),
                enabled: any,
                activate: Box::new(|t: &mut Self| {
                    for s in t.services.iter().filter(|s| s.enabled() && s.in_upload_all() && !s.common().is_busy()) {
                        s.upload_now();
                    }
                }),
                ..Default::default()
            }
            .into(),
        );
        for (i, s) in self.services.iter().enumerate() {
            if !s.enabled() {
                continue;
            }
            let id = s.meta().id;
            let busy = s.common().is_busy();
            items.push(
                SubMenu {
                    label: s.meta().name.into(),
                    icon_data: self.menu_icon(s.as_ref()),
                    submenu: vec![
                        StandardItem { label: short(&s.summary(), 60), enabled: false, ..Default::default() }.into(),
                        MenuItem::Separator,
                        StandardItem {
                            label: s.action_label().into(),
                            enabled: !busy,
                            activate: Box::new(move |t: &mut Self| t.services[i].upload_now()),
                            ..Default::default()
                        }
                        .into(),
                        StandardItem {
                            label: "Show in Hearthlink".into(),
                            activate: Box::new(move |t: &mut Self| t.open(Some(id))),
                            ..Default::default()
                        }
                        .into(),
                    ],
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);

        let level = self.env.notify_level();
        items.push(
            SubMenu {
                label: "Notifications".into(),
                icon_name: "preferences-desktop-notification".into(),
                submenu: vec![RadioGroup {
                    selected: Notify::ALL.iter().position(|n| *n == level).unwrap_or(1),
                    select: Box::new(|t: &mut Self, n| {
                        t.env.config.write().unwrap().notify = Some(Notify::ALL[n]);
                        t.env.save();
                        if let Some(c) = &t.ctx {
                            c.request_repaint();
                        }
                    }),
                    options: Notify::ALL.iter().map(|n| RadioItem { label: n.label().into(), ..Default::default() }).collect(),
                }
                .into()],
                ..Default::default()
            }
            .into(),
        );
        items.push(
            CheckmarkItem {
                label: "Start with my desktop".into(),
                checked: crate::autostart::enabled(),
                activate: Box::new(|_: &mut Self| crate::autostart::set(!crate::autostart::enabled())),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit Hearthlink".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    t.quit.store(true, Ordering::SeqCst);
                    if let Some(c) = &t.ctx {
                        c.request_repaint();
                    }
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

fn status_rgb(h: Health) -> [u8; 3] {
    let c = crate::ui::health_color(h);
    [c.r(), c.g(), c.b()]
}

/// Paints a status dot with a dark ring into the bottom-right corner.
fn badge(img: &mut image::RgbaImage, rgb: [u8; 3]) {
    let (w, h) = img.dimensions();
    let r = w.min(h) as f32 * 0.22;
    let (cx, cy) = (w as f32 - r - 1.0, h as f32 - r - 1.0);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            if d <= r {
                img.put_pixel(x, y, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
            } else if d <= r + w as f32 * 0.06 {
                img.put_pixel(x, y, image::Rgba([0x12, 0x13, 0x16, 255]));
            }
        }
    }
}

fn to_ksni(img: &image::RgbaImage) -> ksni::Icon {
    let (w, h) = img.dimensions();
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for p in img.pixels() {
        let [r, g, b, a] = p.0;
        data.extend_from_slice(&[a, r, g, b]);
    }
    ksni::Icon { width: w as i32, height: h as i32, data }
}

fn tray_icons(png: &[u8]) -> [ksni::Icon; 3] {
    let Ok(base) = image::load_from_memory(png).map(|i| i.resize(64, 64, image::imageops::FilterType::Lanczos3).to_rgba8()) else {
        let empty = ksni::Icon { width: 0, height: 0, data: vec![] };
        return [empty.clone(), empty.clone(), empty];
    };
    let mut warn = base.clone();
    badge(&mut warn, status_rgb(Health::Attention));
    let mut err = base.clone();
    badge(&mut err, status_rgb(Health::Error));
    [to_ksni(&base), to_ksni(&warn), to_ksni(&err)]
}

/// A 22 px service icon with its status dot, as PNG for a menu entry.
fn menu_png(png: &[u8], h: Health) -> Vec<u8> {
    let Ok(img) = image::load_from_memory(png) else { return Vec::new() };
    let mut img = img.resize(22, 22, image::imageops::FilterType::Lanczos3).to_rgba8();
    if !matches!(h, Health::Idle | Health::Off) {
        badge(&mut img, status_rgb(h));
    }
    let mut out = Vec::new();
    let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png);
    out
}

/// Starts the tray; returns a handle to refresh its menu, or None when no tray host runs.
pub fn spawn(
    services: Vec<Arc<dyn Service>>,
    env: Arc<Env>,
    updates: Arc<crate::update::Updates>,
    show: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    goto: Goto,
    ctx: Option<eframe::egui::Context>,
) -> Option<ksni::blocking::Handle<impl ksni::Tray>> {
    let tray = Tray {
        services,
        env,
        updates,
        show,
        quit,
        goto,
        ctx,
        icons: tray_icons(crate::APP_ICON),
        menu_icons: Mutex::new(HashMap::new()),
    };
    match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("no tray available: {e}");
            None
        }
    }
}

/// Shortens a status line for the tray.
fn short(s: &str, max: usize) -> String {
    let s = s.lines().next().unwrap_or_default();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}
