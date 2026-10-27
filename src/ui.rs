use crate::core::{self, Env, Health, Level};
use crate::services::{Meta, Service};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const ACCENT: Color32 = Color32::from_rgb(0xF2, 0xB8, 0x3B);
const ACCENT_TEXT: Color32 = Color32::from_rgb(0x1A, 0x14, 0x06);
const BG: Color32 = Color32::from_rgb(0x12, 0x13, 0x16);
const SIDEBAR: Color32 = Color32::from_rgb(0x0E, 0x0F, 0x11);
const CARD: Color32 = Color32::from_rgb(0x1A, 0x1C, 0x20);
const CARD_STROKE: Color32 = Color32::from_rgb(0x2A, 0x2D, 0x33);
const MUTED: Color32 = Color32::from_rgb(0x8B, 0x90, 0x99);

/// Icons from the bundled Phosphor font (assets/fonts, MIT).
pub mod icon {
    pub const HOUSE: &str = "\u{E2C2}";
    pub const GEAR: &str = "\u{E272}";
    pub const SYNC: &str = "\u{E094}";
    pub const UPLOAD: &str = "\u{E4C0}";
    pub const DOWNLOAD: &str = "\u{E20C}";
    pub const COPY: &str = "\u{E1CA}";
    pub const TRASH: &str = "\u{E4A6}";
    pub const SIGN_IN: &str = "\u{E428}";
    pub const SIGN_OUT: &str = "\u{E42A}";
    pub const EXTERNAL: &str = "\u{E5DE}";
    pub const LINK: &str = "\u{E2E2}";
    pub const UNLINK: &str = "\u{E2E4}";
    pub const FOLDER: &str = "\u{E256}";
    pub const FILE: &str = "\u{E23A}";
    pub const POWER: &str = "\u{E3DA}";
    pub const WARNING: &str = "\u{E4E2}";
    pub const CHECK: &str = "\u{E184}";
    pub const X: &str = "\u{E4F6}";
    pub const INFO: &str = "\u{E2CE}";
    pub const ARROW_RIGHT: &str = "\u{E06C}";
}

/// Button text with a leading icon.
pub fn ib(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

/// The main action of a card, filled with the accent colour.
pub fn primary(ui: &mut egui::Ui, enabled: bool, icon: &str, text: &str) -> egui::Response {
    let fill = if enabled { ACCENT } else { ACCENT.gamma_multiply(0.35) };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(ib(icon, text)).color(ACCENT_TEXT).strong()).fill(fill).stroke(Stroke::NONE),
    )
}

pub fn health_color(h: Health) -> Color32 {
    match h {
        Health::Ok => Color32::from_rgb(0x4C, 0xC3, 0x6B),
        Health::Busy => Color32::from_rgb(0x4A, 0x9E, 0xF5),
        Health::Attention => Color32::from_rgb(0xF2, 0xB8, 0x3B),
        Health::Error => Color32::from_rgb(0xE5, 0x4B, 0x4B),
        Health::Idle | Health::Off => Color32::from_gray(110),
    }
}

fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(12.0)
        .inner_margin(Margin::same(16))
}

/// A titled card.
pub fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    section_with(ui, title, |_| {}, add);
}

/// A titled card with small actions at the right of the title.
pub fn section_with(ui: &mut egui::Ui, title: &str, actions: impl FnOnce(&mut egui::Ui), add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(10.0);
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong().size(15.0).color(Color32::from_gray(235)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions);
        });
        ui.add_space(6.0);
        add(ui);
    });
}

/// A rounded status label with a coloured dot.
fn pill(ui: &mut egui::Ui, h: Health, text: &str, busy: bool) {
    let c = health_color(h);
    egui::Frame::new()
        .fill(c.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, c.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if busy {
                    ui.add(egui::Spinner::new().size(12.0).color(c));
                } else {
                    dot(ui, c);
                }
                ui.add(egui::Label::new(RichText::new(text).color(Color32::from_gray(225))).truncate()).on_hover_text(text);
            });
        });
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

#[derive(PartialEq, Clone, Copy)]
enum Page {
    Overview,
    Service(usize),
    Settings,
}

/// What the Settings page shows about the disk; looked up in the background because
/// it reads game executables.
#[derive(Default)]
struct DiskInfo {
    at: i64,
    root: Option<PathBuf>,
    found: Vec<PathBuf>,
    games: Vec<(String, String, String)>, // (name, version, path)
}

pub struct App {
    env: Arc<Env>,
    services: Vec<Arc<dyn Service>>,
    updates: Arc<crate::update::Updates>,
    page: Page,
    icons: HashMap<&'static str, egui::TextureHandle>,
    pub show_window: Arc<AtomicBool>,
    pub quit: Arc<AtomicBool>,
    root_text: String,
    wayland: bool,
    disk: Arc<Mutex<DiskInfo>>,
    disk_loading: Arc<AtomicBool>,
    /// Narrow window: the sidebar shows icons only.
    compact: bool,
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Phosphor.ttf")).tweak(egui::FontTweak {
            y_offset_factor: 0.08,
            ..Default::default()
        })),
    );
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("phosphor".into());
    }
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x0C, 0x0D, 0x0F);
    v.faint_bg_color = Color32::from_rgb(0x20, 0x22, 0x27);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(0x6E, 0x50, 0x12);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(12);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, CARD_STROKE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_gray(200));
    for (state, fill, stroke) in [
        (&mut w.inactive, Color32::from_rgb(0x26, 0x29, 0x2F), Color32::from_rgb(0x33, 0x37, 0x3E)),
        (&mut w.hovered, Color32::from_rgb(0x30, 0x34, 0x3C), Color32::from_rgb(0x4A, 0x4F, 0x58)),
        (&mut w.active, Color32::from_rgb(0x3A, 0x3F, 0x48), ACCENT.gamma_multiply(0.7)),
        (&mut w.open, Color32::from_rgb(0x30, 0x34, 0x3C), Color32::from_rgb(0x4A, 0x4F, 0x58)),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.corner_radius = CornerRadius::same(8);
        state.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(220));
    w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        use egui::{FontFamily::*, FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Heading, FontId::new(24.0, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Small, FontId::new(11.5, Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.scroll.bar_width = 8.0;
        s.visuals.weak_text_color = Some(MUTED);
    });
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        env: Arc<Env>,
        services: Vec<Arc<dyn Service>>,
        updates: Arc<crate::update::Updates>,
        show_window: Arc<AtomicBool>,
        quit: Arc<AtomicBool>,
        wayland: bool,
    ) -> Self {
        *env.ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
        for s in &services {
            *s.common().repaint.lock().unwrap() = Some(cc.egui_ctx.clone());
        }
        setup_style(&cc.egui_ctx);
        let mut icons = HashMap::new();
        for s in &services {
            let m = s.meta();
            if let Some(t) = load_texture(&cc.egui_ctx, m.id, m.icon) {
                icons.insert(m.id, t);
            }
        }
        if let Some(t) = load_texture(&cc.egui_ctx, "app", crate::APP_ICON) {
            icons.insert("app", t);
        }
        let root_text = env.wow_root().map(|p| p.display().to_string()).unwrap_or_default();
        let first = !env.config.read().unwrap().first_run_done;
        Self {
            env,
            services,
            updates,
            page: if first { Page::Settings } else { Page::Overview },
            icons,
            show_window,
            quit,
            root_text,
            wayland,
            disk: Default::default(),
            disk_loading: Default::default(),
            compact: false,
        }
    }

    fn upstream_newer(&self, s: &dyn Service) -> Option<String> {
        let up = s.common().upstream.lock().unwrap();
        let latest = up.latest.clone()?;
        core::version_cmp(&latest, s.meta().ported_version).is_gt().then_some(latest)
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let compact = self.compact;
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(if compact { 4.0 } else { 6.0 });
            if let Some(t) = self.icons.get("app") {
                ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(32.0, 32.0)).corner_radius(8.0))
                    .on_hover_text(core::APP_NAME);
            }
            if !compact {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(core::APP_NAME).strong().size(17.0).color(Color32::WHITE));
                    ui.label(RichText::new("WoW uploaders for Linux").small().color(MUTED));
                });
            }
        });
        ui.add_space(18.0);
        let nav = |ui: &mut egui::Ui, selected: bool, tip: &str, add: &mut dyn FnMut(&mut egui::Ui)| -> bool {
            // Own id: the frame below takes the next auto id itself.
            let id = ui.scope_id().with(("nav", tip));
            let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
            let fill = if selected {
                Color32::from_rgb(0x22, 0x24, 0x29)
            } else if hovered {
                Color32::from_rgb(0x1A, 0x1B, 0x1F)
            } else {
                Color32::TRANSPARENT
            };
            let r = egui::Frame::new().fill(fill).corner_radius(8.0).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| add(ui));
            });
            if selected {
                let rect = r.response.rect;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 7.0), egui::vec2(3.0, rect.height() - 14.0)),
                    2.0,
                    ACCENT,
                );
            }
            let mut resp = ui.interact(r.response.rect, id, egui::Sense::click());
            if compact {
                resp = resp.on_hover_text(tip);
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            resp.clicked()
        };
        let nav_icon = |ui: &mut egui::Ui, icon: &str, selected: bool| {
            ui.add_sized(
                egui::vec2(22.0, 22.0),
                egui::Label::new(RichText::new(icon).size(17.0).color(if selected { ACCENT } else { MUTED })),
            );
        };
        let overview = self.page == Page::Overview;
        if nav(ui, overview, "Overview", &mut |ui| {
            nav_icon(ui, icon::HOUSE, overview);
            if !compact {
                ui.label("Overview");
            }
        }) {
            self.page = Page::Overview;
        }
        ui.add_space(10.0);
        if compact {
            ui.separator();
        } else {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("SERVICES").small().strong().color(MUTED));
            });
        }
        ui.add_space(2.0);
        for (i, s) in self.services.clone().iter().enumerate() {
            let m = s.meta();
            let (h, _) = s.common().health();
            let newer = self.upstream_newer(s.as_ref());
            let icon = self.icons.get(m.id).cloned();
            let enabled = s.enabled();
            if nav(ui, self.page == Page::Service(i), m.name, &mut |ui| {
                if let Some(t) = &icon {
                    let img = egui::Image::new(t).fit_to_exact_size(egui::vec2(22.0, 22.0)).corner_radius(5.0);
                    ui.add(if enabled { img } else { img.tint(Color32::from_gray(120)) });
                }
                if compact {
                    // The health dot sits on the icon's corner.
                    let r = ui.min_rect();
                    let c = egui::pos2(r.left() + 22.0, r.top() + 2.0);
                    ui.painter().circle_filled(c, 4.5, SIDEBAR);
                    ui.painter().circle_filled(c, 3.5, health_color(if enabled { h } else { Health::Off }));
                    return;
                }
                ui.label(if enabled { RichText::new(m.name) } else { RichText::new(m.name).color(MUTED) });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dot(ui, health_color(if enabled { h } else { Health::Off }));
                    if newer.is_some() {
                        ui.label(RichText::new(icon::WARNING).color(ACCENT)).on_hover_text("The Windows app has a newer version");
                    }
                });
            }) {
                self.page = Page::Service(i);
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(if compact { 4.0 } else { 10.0 });
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).small().color(MUTED));
                if let Some(r) = self.updates.newer() {
                    if ui
                        .add(egui::Label::new(RichText::new(icon::DOWNLOAD).color(ACCENT)).sense(egui::Sense::click()))
                        .on_hover_text(format!("Hearthlink {} is available", r.version))
                        .clicked()
                    {
                        self.page = Page::Overview;
                    }
                }
            });
            let settings = self.page == Page::Settings;
            if nav(ui, settings, "Settings", &mut |ui| {
                nav_icon(ui, icon::GEAR, settings);
                if !compact {
                    ui.label("Settings");
                }
            }) {
                self.page = Page::Settings;
            }
        });
    }

    fn page_header(&self, ui: &mut egui::Ui, title: &str, subtitle: &str) {
        ui.label(RichText::new(title).heading().strong().color(Color32::WHITE));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).color(MUTED));
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        self.page_header(ui, "Overview", "Each service runs on its own. Turn off the ones you don't use.");
        if let Some(r) = self.updates.newer() {
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(Color32::from_rgb(0x2E, 0x24, 0x0C))
                .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
                .corner_radius(12.0)
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(ib(icon::DOWNLOAD, &format!("Hearthlink {} is available (you have {})", r.version, env!("CARGO_PKG_VERSION"))))
                            .strong()
                            .color(ACCENT),
                    );
                    ui.label(crate::update::how_to_update());
                    if !r.notes.trim().is_empty() {
                        ui.collapsing("What's new", |ui| {
                            ui.label(r.notes.trim());
                        });
                    }
                    if primary(ui, true, icon::EXTERNAL, "Open the release page").clicked() {
                        let _ = open::that(&r.url);
                    }
                });
        }
        if wow_running_cached() {
            ui.add_space(6.0);
            ui.label(RichText::new(ib(icon::INFO, "WoW is running. Uploads that need the game closed wait until you quit.")).color(ACCENT));
        }
        ui.add_space(8.0);
        let width = ui.available_width();
        let cols = if width > 720.0 { 2 } else { 1 };
        let gap = 12.0;
        let card_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
        let services = self.services.clone();
        for row in services.chunks(cols).enumerate() {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (j, s) in row.1.iter().enumerate() {
                    let i = row.0 * cols + j;
                    let m = s.meta();
                    let (h, text) = s.common().health();
                    let enabled = s.enabled();
                    // Top-down inside: the row around the cards is horizontal.
                    ui.allocate_ui_with_layout(egui::vec2(card_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        card_frame().show(ui, |ui| {
                            ui.set_width(card_w - 34.0);
                            ui.horizontal(|ui| {
                                if let Some(t) = self.icons.get(m.id) {
                                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(42.0, 42.0)).corner_radius(10.0));
                                }
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(m.name).strong().size(16.0).color(Color32::WHITE));
                                    ui.label(RichText::new(m.tagline).color(MUTED));
                                });
                            });
                            ui.add_space(6.0);
                            pill(ui, if enabled { h } else { Health::Off }, &if enabled { text } else { "Turned off".into() }, s.common().is_busy());
                            if let Some(v) = self.upstream_newer(s.as_ref()) {
                                ui.label(
                                    RichText::new(ib(icon::WARNING, &format!("{} {v} is out (this app: {})", m.upstream_app, m.ported_version)))
                                        .color(ACCENT)
                                        .small(),
                                );
                            }
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui.button(ib(icon::ARROW_RIGHT, "Open")).clicked() {
                                    self.page = Page::Service(i);
                                }
                                if enabled && ui.add_enabled(!s.common().is_busy(), egui::Button::new(ib(icon::UPLOAD, "Upload now"))).clicked() {
                                    s.upload_now();
                                }
                            });
                        });
                    });
                }
            });
            ui.add_space(gap - 7.0);
        }
    }

    fn service_page(&mut self, ui: &mut egui::Ui, i: usize) {
        let s = self.services[i].clone();
        let m: &Meta = s.meta();
        ui.horizontal(|ui| {
            if let Some(t) = self.icons.get(m.id) {
                ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(48.0, 48.0)).corner_radius(12.0));
            }
            ui.add_space(4.0);
            ui.label(RichText::new(m.name).heading().strong().color(Color32::WHITE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = s.enabled();
                if toggle(ui, &mut on).changed() {
                    s.set_enabled(on);
                }
                ui.label(RichText::new(if on { "On" } else { "Off" }).color(MUTED));
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(m.tagline).color(MUTED));
            ui.hyperlink_to(ib(icon::EXTERNAL, "Website"), m.website);
        });
        self.upstream_banner(ui, s.as_ref());
        self.upstream_line(ui, s.as_ref());
        let (h, text) = s.common().health();
        ui.add_space(8.0);
        pill(ui, if s.enabled() { h } else { Health::Off }, &text, s.common().is_busy());
        if !s.enabled() {
            ui.add_space(10.0);
            ui.label(RichText::new("This service is turned off. Turn it on with the switch at the top right.").color(MUTED));
            return;
        }
        s.ui(ui);
        let common = s.common().clone();
        section_with(
            ui,
            "Activity",
            |ui| {
                if ui.small_button(ib(icon::TRASH, "Clear")).on_hover_text("Clear the list (the services keep running)").clicked() {
                    common.log.lock().unwrap().clear();
                }
                if ui.small_button(ib(icon::COPY, "Copy")).on_hover_text("Copy the whole activity log").clicked() {
                    let text = common
                        .log
                        .lock()
                        .unwrap()
                        .iter()
                        .rev()
                        .map(|l| format!("{} [{:?}] {}", log_time(l.at), l.level, l.text))
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui.ctx().copy_text(format!("{} {}\n{text}\n", m.name, env!("CARGO_PKG_VERSION")));
                    common.info("Activity log copied to the clipboard.");
                }
            },
            |ui| {
                let log = common.log.lock().unwrap().clone();
                if log.is_empty() {
                    ui.label(RichText::new("Nothing yet.").color(MUTED));
                }
                egui::ScrollArea::vertical().max_height(260.0).id_salt(m.id).auto_shrink([false, true]).show(ui, |ui| {
                    use egui::text::{LayoutJob, TextFormat};
                    for l in log.iter().take(200) {
                        let (glyph, color) = match l.level {
                            Level::Info => (icon::INFO, MUTED),
                            Level::Good => (icon::CHECK, health_color(Health::Ok)),
                            Level::Warn => (icon::WARNING, health_color(Health::Attention)),
                            Level::Error => (icon::X, health_color(Health::Error)),
                        };
                        let text_color = if l.level == Level::Info { Color32::from_gray(215) } else { color };
                        // One wrapped text per line, so narrow windows wrap instead of cutting off.
                        let mut job = LayoutJob::default();
                        job.wrap.max_width = ui.available_width();
                        let fmt = |font: egui::FontId, color| TextFormat { font_id: font, color, ..Default::default() };
                        job.append(&core::fmt_time(l.at), 0.0, fmt(egui::FontId::monospace(12.5), MUTED));
                        job.append(glyph, 8.0, fmt(egui::FontId::proportional(14.0), color));
                        job.append(&l.text, 8.0, fmt(egui::FontId::proportional(14.0), text_color));
                        ui.add(egui::Label::new(job).wrap());
                    }
                });
            },
        );
    }

    /// Always visible: which Windows app version this module follows, and the newest one.
    fn upstream_line(&self, ui: &mut egui::Ui, s: &dyn Service) {
        let m = s.meta();
        if m.runs_official {
            ui.add_space(4.0);
            ui.label(
                RichText::new(ib(icon::CHECK, &format!("Runs the official {} for Linux, kept up to date automatically", m.upstream_app)))
                    .small()
                    .color(health_color(Health::Ok)),
            );
            return;
        }
        let up = s.common().upstream.lock().unwrap().clone();
        let (glyph, color, text) = match (&up.latest, &up.error) {
            (Some(l), _) if core::version_cmp(l, m.ported_version).is_gt() => {
                (icon::WARNING, ACCENT, format!("Based on {} {} · {l} is out", m.upstream_app, m.ported_version))
            }
            (Some(_), _) => (icon::CHECK, health_color(Health::Ok), format!("Matches {} {} (the newest version)", m.upstream_app, m.ported_version)),
            (None, Some(e)) => (icon::WARNING, MUTED, format!("Based on {} {} · couldn't check for a newer one: {e}", m.upstream_app, m.ported_version)),
            (None, None) => (icon::INFO, MUTED, format!("Based on {} {} · checking for a newer one…", m.upstream_app, m.ported_version)),
        };
        let checked = if up.checked_at > 0 { format!(" · checked {}", core::ago(up.checked_at)) } else { String::new() };
        ui.add_space(4.0);
        ui.label(RichText::new(format!("{glyph}  {text}{checked}")).small().color(color));
    }

    fn upstream_banner(&mut self, ui: &mut egui::Ui, s: &dyn Service) {
        let m = s.meta();
        let up = s.common().upstream.lock().unwrap().clone();
        let Some(latest) = up.latest.clone() else { return };
        if !core::version_cmp(&latest, m.ported_version).is_gt() {
            return;
        }
        let dismissed = self.env.config.read().unwrap().dismissed_upstream.get(m.id).cloned();
        ui.add_space(10.0);
        egui::Frame::new()
            .fill(Color32::from_rgb(0x2E, 0x24, 0x0C))
            .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
            .corner_radius(12.0)
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(ib(icon::WARNING, &format!("{} {} is out. This Linux version is based on {}.", m.upstream_app, latest, m.ported_version)))
                        .strong()
                        .color(ACCENT),
                );
                ui.label("The developers changed their Windows app. New features or changed behaviour may not be in this app yet, so some data might not be sent the way the new version does.");
                if let Some(n) = &up.notes {
                    ui.collapsing("What's new upstream", |ui| {
                        ui.label(n);
                    });
                }
                ui.horizontal(|ui| {
                    ui.hyperlink_to(ib(icon::EXTERNAL, "See the official app"), m.upstream_url);
                    ui.hyperlink_to(ib(icon::EXTERNAL, "Check for a newer Hearthlink"), "https://github.com/KushGene/hearthlink-linux/releases");
                    if dismissed.as_deref() != Some(latest.as_str()) && ui.small_button(ib(icon::CHECK, "Got it")).clicked() {
                        self.env.config.write().unwrap().dismissed_upstream.insert(m.id.to_string(), latest.clone());
                        self.env.save();
                    }
                });
            });
    }

    /// Refreshes the Settings page's disk facts in the background (at most every 15 s).
    fn disk_info(&self, ctx: &egui::Context) {
        let root = self.env.wow_root();
        let stale = {
            let d = self.disk.lock().unwrap();
            d.at == 0 || d.root != root || core::now_unix() - d.at > 15
        };
        if stale && !self.disk_loading.swap(true, Ordering::SeqCst) {
            let (env, disk, loading, ctx) = (self.env.clone(), self.disk.clone(), self.disk_loading.clone(), ctx.clone());
            std::thread::spawn(move || {
                let found = crate::core::wow::detect_installs();
                let games = env
                    .flavors()
                    .iter()
                    .map(|f| {
                        let v = f.executable().and_then(|e| crate::core::wow::exe_file_version(&e)).unwrap_or_default();
                        (f.display_name(), v, f.path.display().to_string())
                    })
                    .collect();
                *disk.lock().unwrap() = DiskInfo { at: core::now_unix(), root: env.wow_root(), found, games };
                loading.store(false, Ordering::SeqCst);
                ctx.request_repaint();
            });
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        self.disk_info(ui.ctx());
        self.page_header(ui, "Settings", "");
        let first = !self.env.config.read().unwrap().first_run_done;
        if first {
            ui.label(RichText::new("Welcome! Check your World of Warcraft folder below, then turn on the services you use.").color(ACCENT));
        }
        let mut use_root: Option<PathBuf> = None;
        section(ui, "World of Warcraft folder", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.root_text)
                    .desired_width(ui.available_width())
                    .hint_text("/home/you/Games/World of Warcraft")
                    .margin(Margin::symmetric(8, 6)),
            );
            ui.horizontal(|ui| {
                if primary(ui, true, icon::FOLDER, "Use this folder").clicked() {
                    match crate::core::wow::normalize_root(std::path::Path::new(self.root_text.trim())) {
                        Some(root) => use_root = Some(root),
                        None => self.root_text = format!("{} (no WoW game folders in there)", self.root_text.trim()),
                    }
                }
            });
            let d = self.disk.lock().unwrap();
            if !d.found.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Found on this PC").small().strong().color(MUTED));
                for f in &d.found {
                    ui.horizontal(|ui| {
                        if ui.small_button("Use").clicked() {
                            use_root = Some(f.clone());
                        }
                        ui.label(f.display().to_string());
                    });
                }
            }
            ui.add_space(4.0);
            if d.at == 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Looking at the game folders…").color(MUTED));
                });
            } else if d.games.is_empty() {
                ui.colored_label(health_color(Health::Error), ib(icon::WARNING, "No game folders found here yet."));
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Games").small().strong().color(MUTED));
                    for (name, v, path) in &d.games {
                        egui::Frame::new()
                            .fill(Color32::from_rgb(0x24, 0x27, 0x2D))
                            .corner_radius(6.0)
                            .inner_margin(Margin::symmetric(8, 3))
                            .show(ui, |ui| {
                                ui.label(format!("{name}  {v}"));
                            })
                            .response
                            .on_hover_text(path);
                    }
                });
            }
        });
        if let Some(root) = use_root {
            self.root_text = root.display().to_string();
            self.env.config.write().unwrap().wow_root = Some(root);
            self.env.save();
            self.disk.lock().unwrap().at = 0;
            for s in &self.services {
                s.common().wake();
            }
        }
        section(ui, "Services", |ui| {
            for s in &self.services {
                let mut on = s.enabled();
                ui.horizontal(|ui| {
                    if let Some(t) = self.icons.get(s.meta().id) {
                        ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(22.0, 22.0)).corner_radius(5.0));
                    }
                    ui.label(RichText::new(s.meta().name).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if toggle(ui, &mut on).changed() {
                            s.set_enabled(on);
                        }
                        ui.add(egui::Label::new(RichText::new(s.meta().tagline).color(MUTED)).truncate());
                    });
                });
            }
        });
        section(ui, "App", |ui| {
            let mut cfg = self.env.config.read().unwrap().clone();
            let before = (cfg.start_minimized, cfg.close_to_tray, cfg.notifications);
            let mut autostart = crate::autostart::enabled();
            if ui.checkbox(&mut autostart, "Start with my desktop session (in the tray)").changed() {
                crate::autostart::set(autostart);
            }
            ui.checkbox(&mut cfg.start_minimized, "Start hidden in the tray");
            ui.checkbox(&mut cfg.close_to_tray, "Closing the window keeps the uploaders running in the tray");
            if self.wayland && cfg.close_to_tray {
                ui.label(RichText::new("On Wayland the window is minimized instead of hidden.").small().color(MUTED));
            }
            ui.checkbox(&mut cfg.notifications, "Show desktop notifications");
            if (cfg.start_minimized, cfg.close_to_tray, cfg.notifications) != before {
                *self.env.config.write().unwrap() = cfg;
                self.env.save();
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button(ib(icon::FOLDER, "Open the settings folder")).clicked() {
                    let _ = open::that(crate::core::store::config_dir());
                }
                if ui.button(ib(icon::POWER, "Quit Hearthlink")).clicked() {
                    self.quit.store(true, Ordering::SeqCst);
                }
            });
            ui.label(
                RichText::new(format!(
                    "Hearthlink {} · not made by or affiliated with ForeverDB, Wowhead, WowUtils or wowaudit.",
                    env!("CARGO_PKG_VERSION")
                ))
                .small()
                .color(MUTED),
            );
        });
        if first {
            ui.add_space(10.0);
            if primary(ui, true, icon::CHECK, "Done, go to the overview").clicked() {
                self.env.config.write().unwrap().first_run_done = true;
                self.env.save();
                self.page = Page::Overview;
            }
        }
    }
}

fn log_time(t: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(t, 0).single().map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

fn wow_running_cached() -> bool {
    static CACHE: Mutex<(i64, bool)> = Mutex::new((0, false));
    let mut c = CACHE.lock().unwrap();
    if core::now_unix() - c.0 > 5 {
        *c = (core::now_unix(), crate::core::wow::game_running());
    }
    c.1
}

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(42.0, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off = Color32::from_rgb(0x3A, 0x3E, 0x46);
    let bg = off.lerp_to_gamma(health_color(Health::Ok), t);
    ui.painter().rect_filled(rect, 12.0, bg);
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 9.0, Color32::WHITE);
    resp
}

fn load_texture(ctx: &egui::Context, name: &str, png: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    // Mipmaps keep large icons smooth when drawn small.
    let opts = egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
    Some(ctx.load_texture(name, ci, opts))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.quit.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            std::process::exit(0);
        }
        if self.show_window.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.env.config.read().unwrap().close_to_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.wayland {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        self.compact = ui.max_rect().width() < 860.0;
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(if self.compact { 64.0 } else { 220.0 })
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::symmetric(10, 0)).stroke(Stroke::new(1.0, Color32::from_rgb(0x1E, 0x20, 0x24))))
            .show(ui, |ui| self.sidebar(ui));
        let pad = if self.compact { 14 } else { 24 };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(pad, 0))).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(20.0);
                match self.page {
                    Page::Overview => self.overview(ui),
                    Page::Service(i) => self.service_page(ui, i),
                    Page::Settings => self.settings(ui),
                }
                ui.add_space(20.0);
            });
        });
        // Status changes come from worker threads; keep the clock-like texts fresh too.
        ctx.request_repaint_after(std::time::Duration::from_secs(5));
    }
}
