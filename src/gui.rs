//! The window: Overview, Processes, Disk & Space, Cleanup, History, Claude, Settings.

use crate::cleanup::{self, Risk, Rule};
use crate::collect::{human_bytes, Sampler, Snapshot};
use crate::config::{ClaudeMode, Config};
use crate::diagnose::{analyze, health_score, Act, Finding, Level};
use crate::store::{CleanupEntry, Event, Row, Store};
use crate::{claude, daemon, update};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};



// palette: deep space with neon readouts
const BG: Color32 = Color32::from_rgb(4, 7, 13);
const SIDE: Color32 = Color32::from_rgb(6, 11, 19);
const CARD: Color32 = Color32::from_rgba_premultiplied(8, 16, 26, 235);
const CARD_HI: Color32 = Color32::from_rgb(12, 26, 40);
const LINE: Color32 = Color32::from_rgb(22, 48, 66);
const TEXT: Color32 = Color32::from_rgb(208, 240, 255);
const MUTED: Color32 = Color32::from_rgb(98, 132, 152);
const ACCENT: Color32 = Color32::from_rgb(0, 229, 255);
const GOOD: Color32 = Color32::from_rgb(0, 255, 156);
const WARN: Color32 = Color32::from_rgb(255, 176, 32);
const BAD: Color32 = Color32::from_rgb(255, 59, 92);
const VIOLET: Color32 = Color32::from_rgb(255, 43, 214);
const BLUE: Color32 = Color32::from_rgb(77, 140, 255);

fn display(size: f32) -> FontId {
    FontId::new(size, egui::FontFamily::Name("display".into()))
}

fn mono(size: f32) -> FontId {
    FontId::new(size, egui::FontFamily::Monospace)
}

fn level_color(l: Level) -> Color32 {
    match l {
        Level::Good => GOOD,
        Level::Info => ACCENT,
        Level::Warn => WARN,
        Level::Bad => BAD,
    }
}

fn score_color(s: u8) -> Color32 {
    if s >= 80 { GOOD } else if s >= 55 { WARN } else { BAD }
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Overview,
    Processes,
    Disk,
    Cleanup,
    History,
    Claude,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Cpu,
    Mem,
    Disk,
    Swap,
}

#[derive(Default)]
struct Live {
    now: Arc<Snapshot>,
    /// recent snapshots for charts, process lists stripped to keep frames cheap
    ring: VecDeque<Snapshot>,
    /// the same history shared with the window, rebuilt once per sample (never per frame)
    shared_ring: Arc<Vec<Snapshot>>,
    at: Option<Instant>,
}

#[derive(Default)]
struct Shared {
    rules: Option<Vec<Rule>>,
    scanning: bool,
    running: Option<String>,
    last_result: Option<String>,
    space: Option<Vec<(PathBuf, u64)>>,
    space_busy: bool,
    talkers: Vec<(String, usize)>,
    claude_busy: bool,
    claude_reply: Option<Result<String, String>>,
    service_active: Option<bool>,
    update_checking: bool,
    /// None = not checked; Some(Ok(None)) = up to date
    update: Option<Result<Option<(String, String)>, String>>,
}

pub struct App {
    /// how often the sampler thread reads the system, in ms
    sample_ms: Arc<std::sync::atomic::AtomicU64>,
    smooth_now: bool,
    page: Page,
    live: Arc<Mutex<Live>>,
    shared: Arc<Mutex<Shared>>,
    cfg: Config,
    store: Option<Store>,
    hist_range: i64,
    hist: Vec<Row>,
    events: Vec<Event>,
    cleanup_log: Vec<CleanupEntry>,
    hist_loaded: Option<Instant>,
    sort: SortBy,
    filter: String,
    confirm_kill: Option<(i32, String)>,
    confirm_rule: Option<String>,
    toast: Option<(String, Instant)>,
    chat: Vec<(String, String, Vec<Act>)>,
    question: String,
    pending_q: Option<String>,
    show_report: bool,
    report_cache: String,
    calm_text: String,
    key_text: String,
    started: Instant,
    claude_found: bool,
    /// cpu per pid at the last movers refresh, and the board itself
    movers_prev: std::collections::HashMap<i32, f32>,
    movers: Vec<(i32, String, f32, f32)>,
    movers_at: Option<Instant>,
    freezes: Vec<Event>,
    freezes_at: Option<Instant>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(&cc.egui_ctx);
        cc.egui_ctx.add_plugin(CopyOnSelect::default());
        let live = Arc::new(Mutex::new(Live::default()));
        let sample_ms = Arc::new(std::sync::atomic::AtomicU64::new(1000));
        {
            let live = live.clone();
            let ctx = cc.egui_ctx.clone();
            let sample_ms = sample_ms.clone();
            std::thread::spawn(move || {
                let mut s = Sampler::new();
                s.sample();
                std::thread::sleep(Duration::from_millis(500));
                loop {
                    let snap = s.sample();
                    {
                        let mut lite = snap.clone();
                        lite.procs = Vec::new();
                        let mut l = live.lock().unwrap();
                        l.ring.push_back(lite);
                        while l.ring.len() > 300 {
                            l.ring.pop_front();
                        }
                        l.now = Arc::new(snap);
                        l.shared_ring = Arc::new(l.ring.iter().cloned().collect());
                        l.at = Some(Instant::now());
                    }
                    ctx.request_repaint();
                    // sleep in small steps so a new interval applies at once
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_millis(sample_ms.load(std::sync::atomic::Ordering::Relaxed)) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
        }
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            // slow background facts: log talkers + service state
            let shared = shared.clone();
            std::thread::spawn(move || loop {
                let t = claude::journal_talkers(10);
                let active = std::process::Command::new("systemctl")
                    .args(["--user", "is-active", "--quiet", "membli.service"])
                    .status()
                    .map(|s| s.success())
                    .ok();
                {
                    let mut sh = shared.lock().unwrap();
                    sh.talkers = t;
                    sh.service_active = active;
                }
                std::thread::sleep(Duration::from_secs(60));
            });
        }
        let cfg = Config::load();
        // MEMBLI_PAGE opens a given page first (handy for screenshots and tests)
        let page = match std::env::var("MEMBLI_PAGE").unwrap_or_default().as_str() {
            "processes" => Page::Processes,
            "disk" => Page::Disk,
            "cleanup" => Page::Cleanup,
            "history" => Page::History,
            "claude" => Page::Claude,
            "settings" => Page::Settings,
            _ => Page::Overview,
        };
        let mut app = Self {
            sample_ms,
            smooth_now: true,
            page,
            live,
            shared,
            calm_text: cfg.calm_apps.join(", "),
            key_text: cfg.api_key.clone(),
            cfg,
            store: Store::open().ok(),
            hist_range: 86400,
            hist: vec![],
            events: vec![],
            cleanup_log: vec![],
            hist_loaded: None,
            sort: SortBy::Cpu,
            filter: String::new(),
            confirm_kill: None,
            confirm_rule: None,
            toast: None,
            chat: vec![],
            question: String::new(),
            pending_q: None,
            show_report: false,
            report_cache: String::new(),
            started: Instant::now(),
            claude_found: claude::which_claude().is_some(),
            movers_prev: Default::default(),
            movers: vec![],
            movers_at: None,
            freezes: vec![],
            freezes_at: None,
        };
        app.start_scan();
        if app.cfg.check_updates {
            app.check_updates();
        }
        if page == Page::Disk {
            app.start_space();
        }
        if page == Page::History {
            app.load_history(true);
        }
        app
    }

    fn start_scan(&self) {
        let sh = self.shared.clone();
        let days = self.cfg.trash_days;
        {
            let mut s = sh.lock().unwrap();
            if s.scanning {
                return;
            }
            s.scanning = true;
        }
        std::thread::spawn(move || {
            let rules = cleanup::scan(days);
            let mut s = sh.lock().unwrap();
            s.rules = Some(rules);
            s.scanning = false;
        });
    }

    fn check_updates(&self) {
        let sh = self.shared.clone();
        {
            let mut s = sh.lock().unwrap();
            if s.update_checking {
                return;
            }
            s.update_checking = true;
        }
        std::thread::spawn(move || {
            let r = update::check().map(|o| o.map(|r| (r.tag, r.url)));
            let mut s = sh.lock().unwrap();
            s.update = Some(r);
            s.update_checking = false;
        });
    }

    fn start_space(&self) {
        let sh = self.shared.clone();
        {
            let mut s = sh.lock().unwrap();
            if s.space_busy {
                return;
            }
            s.space_busy = true;
        }
        std::thread::spawn(move || {
            let m = cleanup::space_map();
            let mut s = sh.lock().unwrap();
            s.space = Some(m);
            s.space_busy = false;
        });
    }

    fn run_rule(&mut self, rule: Rule) {
        let sh = self.shared.clone();
        let days = self.cfg.trash_days;
        sh.lock().unwrap().running = Some(rule.title.clone());
        std::thread::spawn(move || {
            let o = cleanup::run(&rule, days);
            if let Ok(st) = Store::open() {
                let _ = st.log_cleanup(&rule.title, o.freed, false, &o.note);
            }
            let msg = if o.note == "Cancelled" {
                format!("{}: cancelled", rule.title)
            } else if o.freed > 0 {
                format!("{}: freed {}", rule.title, human_bytes(o.freed))
            } else if o.note.is_empty() {
                format!("{}: done", rule.title)
            } else {
                format!("{}: {}", rule.title, o.note)
            };
            let rules = cleanup::scan(days);
            let mut s = sh.lock().unwrap();
            s.running = None;
            s.last_result = Some(msg);
            s.rules = Some(rules);
        });
    }

    fn load_history(&mut self, force: bool) {
        if !force && self.hist_loaded.map(|t| t.elapsed() < Duration::from_secs(30)).unwrap_or(false) {
            return;
        }
        if let Some(st) = &self.store {
            let now = chrono::Utc::now().timestamp();
            self.hist = st.rows_since(now - self.hist_range);
            self.events = st.events_since(now - 7 * 86400);
            self.cleanup_log = st.cleanup_log(20);
        }
        self.hist_loaded = Some(Instant::now());
    }

    fn report(&mut self) -> String {
        let (now, ring) = {
            let l = self.live.lock().unwrap();
            ((*l.now).clone(), l.ring.iter().rev().take(15).cloned().collect::<Vec<_>>())
        };
        let day = self.store.as_ref().map(|s| s.rows_since(chrono::Utc::now().timestamp() - 86400)).unwrap_or_default();
        let events = self.store.as_ref().map(|s| s.events_since(chrono::Utc::now().timestamp() - 3 * 86400)).unwrap_or_default();
        let sh = self.shared.lock().unwrap();
        let rules = sh.rules.clone().unwrap_or_default();
        claude::build_report(&now, &ring, &day, &events, &rules, &sh.talkers)
    }

    fn ask_claude(&mut self, q: String) {
        if self.shared.lock().unwrap().claude_busy {
            return;
        }
        let report = self.report();
        self.report_cache = report.clone();
        let cfg = self.cfg.clone();
        let hist: Vec<(String, String)> = self.chat.iter().map(|(q, a, _)| (q.clone(), a.clone())).collect();
        self.pending_q = Some(q.clone());
        let sh = self.shared.clone();
        sh.lock().unwrap().claude_busy = true;
        std::thread::spawn(move || {
            let r = claude::ask(&cfg, &report, &hist, &q);
            let mut s = sh.lock().unwrap();
            s.claude_busy = false;
            s.claude_reply = Some(r);
        });
    }

    /// One place where every button action happens, with the same checks.
    fn do_act(&mut self, act: Act) {
        match act {
            Act::Calm(pid, name) => match daemon::calm(pid) {
                Ok(_) => self.toast(format!("{name} now runs at low CPU and disk priority")),
                Err(e) => self.toast(format!("Could not calm {name}: {e}")),
            },
            Act::End(pid, name) => self.confirm_kill = Some((pid, name)),
            Act::Goto(p) => {
                self.page = match p.as_str() {
                    "cleanup" => Page::Cleanup,
                    "processes" => Page::Processes,
                    "disk" => Page::Disk,
                    "history" => Page::History,
                    "settings" => Page::Settings,
                    _ => Page::Overview,
                }
            }
            Act::Ask(q) => {
                self.page = Page::Claude;
                self.ask_claude(q);
            }
            Act::Clean(id) => {
                let (rules, running) = {
                    let s = self.shared.lock().unwrap();
                    (s.rules.clone(), s.running.clone())
                };
                if let Some(r) = running {
                    self.toast(format!("Busy with {r}; try again in a moment"));
                    return;
                }
                let Some(rules) = rules else {
                    self.toast("Still measuring what can be cleaned; try again in a minute");
                    self.start_scan();
                    return;
                };
                if id == "safe" {
                    let safe: Vec<Rule> = rules.iter().filter(|r| r.risk == Risk::Safe && r.size.unwrap_or(0) > 0).cloned().collect();
                    if safe.is_empty() {
                        self.toast("Nothing safe to clean right now");
                    }
                    for r in safe {
                        self.run_rule(r);
                    }
                    return;
                }
                match rules.iter().find(|r| r.id == id) {
                    None => self.toast("That cleanup isn't available on this PC right now"),
                    Some(r) if r.blocked.is_some() => {
                        let why = r.blocked.clone().unwrap_or_default();
                        self.toast(format!("{}: {why}", r.title));
                    }
                    Some(r) if r.risk == Risk::Safe => self.run_rule(r.clone()),
                    Some(r) => self.confirm_rule = Some(r.id.to_string()),
                }
            }
        }
    }

    fn calm_list_now(&mut self, now: &Snapshot) {
        let mut n = 0;
        for p in now.procs.iter().filter(|p| p.user_owned && p.nice < 10 && self.cfg.calm_apps.iter().any(|a| a.eq_ignore_ascii_case(&p.name))) {
            if daemon::calm(p.pid).is_ok() {
                n += 1;
            }
        }
        self.toast(if n > 0 { format!("Calmed {n} processes") } else { "None of your calm-list apps are running".to_string() });
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }
}

/// Copies text to the clipboard as soon as you finish selecting it (and Ctrl+C still works).
#[derive(Default)]
struct CopyOnSelect;

fn copied_id() -> egui::Id {
    egui::Id::unique("membli_copied")
}

impl egui::plugin::Plugin for CopyOnSelect {
    fn debug_name(&self) -> &'static str {
        "CopyOnSelect"
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let settled = ui.ctx().output(|o| {
            o.commands.iter().rev().find_map(|c| match c {
                egui::OutputCommand::TextSelectionSettled(t) => Some(t.clone()),
                _ => None,
            })
        });
        // egui reports a selection once, when the mouse is released
        if let Some(t) = settled {
            if !t.trim().is_empty() {
                ui.ctx().copy_text(t);
                ui.ctx().data_mut(|d| d.insert_temp(copied_id(), Instant::now()));
            }
        }
    }
}

fn style(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily, TextStyle};
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("orbitron".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Orbitron.ttf"))));
    fonts.font_data.insert("rajdhani".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Rajdhani-SemiBold.ttf"))));
    fonts.font_data.insert("techmono".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/ShareTechMono.ttf"))));
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "rajdhani".into());
    fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "techmono".into());
    let mut disp = vec!["orbitron".to_string()];
    disp.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("display".into()), disp);
    ctx.set_fonts(fonts);

    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = Color32::from_rgb(7, 14, 23);
        v.window_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.5));
        v.extreme_bg_color = Color32::from_rgb(3, 10, 17);
        v.faint_bg_color = Color32::from_rgb(7, 17, 27);
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.28);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        let r = CornerRadius::same(2);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgba_premultiplied(0, 30, 42, 120);
        v.widgets.inactive.bg_fill = Color32::from_rgb(10, 24, 36);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.35));
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.weak_bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.16);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.28);
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
        v.window_corner_radius = CornerRadius::same(2);
        s.text_styles.insert(TextStyle::Body, FontId::new(16.0, FontFamily::Proportional));
        s.text_styles.insert(TextStyle::Button, mono(13.5));
        s.text_styles.insert(TextStyle::Small, FontId::new(13.0, FontFamily::Proportional));
        s.text_styles.insert(TextStyle::Heading, display(20.0));
        s.text_styles.insert(TextStyle::Monospace, mono(13.5));
    });
}

// ---------- HUD components ----------

/// Corner brackets around a rect: the signature look of the panels.
fn brackets(p: &egui::Painter, r: Rect, col: Color32, len: f32) {
    let s = Stroke::new(1.6, col);
    let (l, t, rr, b) = (r.left(), r.top(), r.right(), r.bottom());
    for (corner, dx, dy) in [((l, t), 1.0, 1.0), ((rr, t), -1.0, 1.0), ((l, b), 1.0, -1.0), ((rr, b), -1.0, -1.0)] {
        let c = Pos2::new(corner.0, corner.1);
        p.line_segment([c, c + Vec2::new(len * dx, 0.0)], s);
        p.line_segment([c, c + Vec2::new(0.0, len * dy)], s);
    }
}

/// Neon line: a soft glow under a sharp core.
fn glow_line(p: &egui::Painter, pts: Vec<Pos2>, col: Color32, w: f32) {
    p.add(Shape::line(pts.clone(), Stroke::new(w * 4.0, col.gamma_multiply(0.08))));
    p.add(Shape::line(pts.clone(), Stroke::new(w * 2.2, col.gamma_multiply(0.22))));
    p.add(Shape::line(pts, Stroke::new(w, col)));
}

fn hud_frame<R>(ui: &mut Ui, label: Option<&str>, accent: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let resp = Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, accent.gamma_multiply(0.22)))
        .corner_radius(CornerRadius::same(2))
        .inner_margin(Margin { left: 16, right: 16, top: if label.is_some() { 30 } else { 16 }, bottom: 16 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let r = resp.response.rect;
    let p = ui.painter();
    brackets(p, r, accent.gamma_multiply(0.9), 12.0);
    if let Some(l) = label {
        let tl = r.left_top() + Vec2::new(16.0, 10.0);
        p.rect_filled(Rect::from_min_size(tl + Vec2::new(0.0, 2.0), Vec2::new(6.0, 6.0)), CornerRadius::ZERO, accent);
        p.text(tl + Vec2::new(14.0, -2.0), Align2::LEFT_TOP, l.to_uppercase(), mono(12.0), accent.gamma_multiply(0.95));
        let end = r.right() - 16.0;
        let gx = tl.x + 24.0 + l.len() as f32 * 7.6;
        if end > gx + 20.0 {
            p.line_segment([Pos2::new(gx, tl.y + 5.0), Pos2::new(end, tl.y + 5.0)], Stroke::new(1.0, accent.gamma_multiply(0.15)));
        }
    }
    resp.inner
}

/// Fixed-width, left-aligned, truncating text cell.
fn cell(ui: &mut Ui, text: RichText, width: f32) {
    ui.allocate_ui_with_layout(Vec2::new(width, 20.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    hud_frame(ui, None, ACCENT, add)
}

fn panel<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    hud_frame(ui, Some(label), ACCENT, add)
}

fn title(ui: &mut Ui, t: &str, sub: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("//").font(display(22.0)).color(ACCENT.gamma_multiply(0.6)));
        ui.label(RichText::new(t.to_uppercase()).font(display(22.0)).color(TEXT));
    });
    if !sub.is_empty() {
        ui.label(RichText::new(sub).color(MUTED));
    }
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
    let y = r.center().y;
    ui.painter().line_segment([Pos2::new(r.left(), y), Pos2::new(r.left() + 140.0, y)], Stroke::new(2.0, ACCENT));
    ui.painter().line_segment([Pos2::new(r.left() + 146.0, y), Pos2::new(r.right(), y)], Stroke::new(1.0, LINE));
    ui.add_space(6.0);
}

fn section(ui: &mut Ui, t: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("▸").color(ACCENT).font(mono(13.0)));
        ui.label(RichText::new(t.to_uppercase()).font(mono(13.0)).color(ACCENT.gamma_multiply(0.85)));
    });
    ui.add_space(2.0);
}

fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let font = mono(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_uppercase(), font, color);
    let size = galley.size() + Vec2::new(14.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(1), color.gamma_multiply(0.12));
    p.rect_stroke(rect, CornerRadius::same(1), Stroke::new(1.0, color.gamma_multiply(0.55)), egui::StrokeKind::Inside);
    p.galley(rect.center() - galley.size() / 2.0, galley, color);
}

fn hud_button(ui: &mut Ui, text: &str, col: Color32, solid: bool) -> egui::Response {
    let label = RichText::new(text.to_uppercase()).font(mono(13.5)).color(if solid { BG } else { col });
    let b = egui::Button::new(label)
        .fill(if solid { col } else { col.gamma_multiply(0.06) })
        .stroke(Stroke::new(1.0, col.gamma_multiply(if solid { 1.0 } else { 0.7 })))
        .corner_radius(CornerRadius::same(1));
    let r = ui.add(b);
    if r.hovered() {
        brackets(ui.painter(), r.rect.expand(3.0), col, 6.0);
    }
    r
}

fn primary(ui: &mut Ui, text: &str) -> egui::Response {
    hud_button(ui, text, ACCENT, true)
}

fn ghost(ui: &mut Ui, text: &str) -> egui::Response {
    hud_button(ui, text, ACCENT, false)
}

fn danger(ui: &mut Ui, text: &str) -> egui::Response {
    hud_button(ui, text, BAD, true)
}

fn phase_id() -> egui::Id {
    egui::Id::unique("membli_phase")
}

/// Ease a number toward its new value so readouts roll instead of jumping.
fn ease(ui: &Ui, key: &str, target: f32) -> f32 {
    ui.ctx().animate_value_with_time(egui::Id::unique(("ease", key)), target, 0.45)
}

/// Segmented level bar.
fn bar(ui: &mut Ui, frac: f32, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    // ease per bar position so level changes slide instead of snapping
    let key = format!("bar{:.0}x{:.0}", rect.left(), rect.top());
    let frac = ease(ui, &key, frac.clamp(0.0, 1.0));
    let p = ui.painter();
    let n = (width / 7.0) as usize;
    let lit = (frac.clamp(0.0, 1.0) * n as f32).round() as usize;
    for i in 0..n {
        let x = rect.left() + i as f32 * 7.0;
        let seg = Rect::from_min_size(Pos2::new(x, rect.top() + 1.0), Vec2::new(5.0, 8.0));
        p.rect_filled(seg, CornerRadius::ZERO, if i < lit { color } else { LINE.gamma_multiply(0.7) });
    }
}

/// Health reactor: segmented ring, rotating outer arc, readout in the middle.
fn ring(ui: &mut Ui, score: u8, size: f32) {
    let t = ui.input(|i| i.time) as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let col = score_color(score);
    let tau = std::f32::consts::TAU;
    // outer rotating dashes
    let r0 = size / 2.0 - 3.0;
    for k in 0..3 {
        let a0 = t * 0.6 + k as f32 * tau / 3.0;
        let pts: Vec<Pos2> = (0..=20).map(|i| a0 + 0.9 * i as f32 / 20.0).map(|a| c + Vec2::new(a.cos(), a.sin()) * r0).collect();
        p.add(Shape::line(pts, Stroke::new(1.5, ACCENT.gamma_multiply(0.55))));
    }
    // counter-rotating ticks
    let r1 = size / 2.0 - 10.0;
    for i in 0..60 {
        let a = -t * 0.25 + tau * i as f32 / 60.0;
        let len = if i % 5 == 0 { 5.0 } else { 2.0 };
        let d = Vec2::new(a.cos(), a.sin());
        p.line_segment([c + d * r1, c + d * (r1 - len)], Stroke::new(1.0, ACCENT.gamma_multiply(0.3)));
    }
    // segmented score ring
    let r2 = size / 2.0 - 24.0;
    let n = 48;
    let shown = ease(ui, "ring", score as f32);
    let litf = shown / 100.0 * n as f32;
    let lit = litf.floor() as usize;
    for i in 0..n {
        let a0 = -tau / 4.0 + tau * i as f32 / n as f32 + 0.02;
        let a1 = a0 + tau / n as f32 - 0.05;
        let pts: Vec<Pos2> = (0..=4).map(|j| a0 + (a1 - a0) * j as f32 / 4.0).map(|a| c + Vec2::new(a.cos(), a.sin()) * r2).collect();
        if i < lit {
            p.add(Shape::line(pts.clone(), Stroke::new(14.0, col.gamma_multiply(0.15))));
            p.add(Shape::line(pts, Stroke::new(8.0, col)));
        } else if i == lit && litf > lit as f32 {
            p.add(Shape::line(pts, Stroke::new(8.0, col.gamma_multiply(0.25 + 0.75 * (litf - lit as f32)))));
        } else {
            p.add(Shape::line(pts, Stroke::new(8.0, LINE)));
        }
    }
    let pulse = 0.75 + 0.25 * (t * 2.0).sin();
    p.circle_filled(c, size * 0.24, col.gamma_multiply(0.06 * pulse));
    p.text(c - Vec2::new(0.0, 8.0), Align2::CENTER_CENTER, format!("{:.0}", shown), display(size * 0.22), TEXT);
    p.text(c + Vec2::new(0.0, size * 0.13), Align2::CENTER_CENTER, "INTEGRITY", mono(11.0), col);
}

/// Neon line chart with grid and hover readout. `max` None = auto.
/// A live chart that glides between samples (newest point slides in from the right edge).
fn live_chart(ui: &mut Ui, vals: &[f32], max: Option<f32>, color: Color32, height: f32) {
    let phase = ui.ctx().data(|d| d.get_temp::<f32>(phase_id())).unwrap_or(1.0);
    chart_impl(ui, vals, max, color, height, Some(phase));
}

fn chart(ui: &mut Ui, vals: &[f32], max: Option<f32>, color: Color32, height: f32) {
    chart_impl(ui, vals, max, color, height, None);
}

fn chart_impl(ui: &mut Ui, vals: &[f32], max: Option<f32>, color: Color32, height: f32, phase: Option<f32>) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), Sense::hover());
    let p = ui.painter_at(rect.expand(2.0));
    let grid = Stroke::new(1.0, ACCENT.gamma_multiply(0.07));
    for i in 0..=4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        p.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], grid);
    }
    let cols = ((rect.width() / 40.0) as usize).max(1);
    for i in 0..=cols {
        let x = rect.left() + rect.width() * i as f32 / cols as f32;
        p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], grid);
    }
    if vals.len() < 2 {
        p.text(rect.center(), Align2::CENTER_CENTER, "ACQUIRING SIGNAL…", mono(11.0), MUTED);
        return;
    }
    let raw_top = max.unwrap_or_else(|| vals.iter().cloned().fold(0.0f32, f32::max).max(1.0) * 1.15);
    // auto-scaled charts ease their scale too, so the line doesn't jump when the peak changes
    let top = if max.is_none() { ease(ui, &format!("top{:.0}x{:.0}", rect.left(), rect.top()), raw_top).max(0.001) } else { raw_top };
    let n = vals.len();
    let yv = |v: f32| rect.bottom() - rect.height() * (v / top).clamp(0.0, 1.0);
    let line: Vec<Pos2> = match phase {
        // live: draw one sample behind, with the head interpolated toward the newest value
        Some(ph) if n >= 3 => {
            let step = rect.width() / (n - 2) as f32;
            let mut l: Vec<Pos2> = (0..n - 1).map(|i| Pos2::new(rect.right() - ((n - 2 - i) as f32 + ph) * step, yv(vals[i]))).collect();
            let head = vals[n - 2] + (vals[n - 1] - vals[n - 2]) * ph;
            l.push(Pos2::new(rect.right(), yv(head)));
            l.retain(|p| p.x >= rect.left() - step);
            l
        }
        _ => vals.iter().enumerate().map(|(i, v)| Pos2::new(rect.left() + rect.width() * i as f32 / (n - 1) as f32, yv(*v))).collect(),
    };
    let p = ui.painter_at(rect.expand(2.0));
    let n = line.len();
    for i in 0..n - 1 {
        let a = line[i];
        let b = line[i + 1];
        p.add(Shape::convex_polygon(vec![a, b, Pos2::new(b.x, rect.bottom()), Pos2::new(a.x, rect.bottom())], color.gamma_multiply(0.07), Stroke::NONE));
    }
    glow_line(&p, line.clone(), color, 1.6);
    let vals: Vec<f32> = line.iter().map(|q| (rect.bottom() - q.y) / rect.height() * top).collect();
    let last = *line.last().unwrap();
    p.circle_filled(last, 3.0, color);
    p.circle_stroke(last, 6.0, Stroke::new(1.0, color.gamma_multiply(0.5)));
    if let Some(hp) = resp.hover_pos() {
        let i = (((hp.x - rect.left()) / rect.width()) * (n - 1) as f32).round().clamp(0.0, (n - 1) as f32) as usize;
        let at = line[i];
        p.line_segment([Pos2::new(at.x, rect.top()), Pos2::new(at.x, rect.bottom())], Stroke::new(1.0, color.gamma_multiply(0.5)));
        p.circle_filled(at, 4.0, color);
        let anchor = if at.x > rect.center().x { Align2::RIGHT_BOTTOM } else { Align2::LEFT_BOTTOM };
        p.text(at + Vec2::new(if at.x > rect.center().x { -8.0 } else { 8.0 }, -6.0), anchor, format!("{:.1}", vals[i]), mono(12.0), TEXT);
    }
}

fn stat_card(ui: &mut Ui, label: &str, value: String, sub: String, vals: &[f32], max: Option<f32>, color: Color32, change: f32, unit: &str) {
    hud_frame(ui, Some(label), color, |ui| {
        // roll the number toward the newest sample
        let shown = match vals.last() {
            Some(&v) if value.ends_with('%') => format!("{:.0}%", ease(ui, label, v)),
            Some(&v) if value.ends_with("MB/s") => format!("{:.1} MB/s", ease(ui, label, v)),
            _ => value,
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(shown).font(display(26.0)).color(color));
            ui.add_space(4.0);
            delta(ui, change, unit);
        });
        ui.label(RichText::new(sub).color(MUTED).size(14.0));
        ui.add_space(4.0);
        live_chart(ui, vals, max, color, 56.0);
    });
}

fn act_button(ui: &mut Ui, act: &Act, col: Color32) -> bool {
    hud_button(ui, &act.label(), col, false).clicked()
}

/// Turn Claude's suggested actions into real ones, matched against what is running now.
fn acts_from_claude(raw: &[serde_json::Value], now: &Snapshot) -> Vec<Act> {
    let find = |name: &str| {
        now.procs
            .iter()
            .filter(|p| p.user_owned && p.name.eq_ignore_ascii_case(name.trim()))
            .max_by(|a, b| (a.cpu_pct + a.read_mbs + a.write_mbs).partial_cmp(&(b.cpu_pct + b.read_mbs + b.write_mbs)).unwrap())
            .map(|p| (p.pid, p.name.clone()))
    };
    let mut out = Vec::new();
    for v in raw.iter().take(4) {
        if let Some(id) = v.get("clean").and_then(|x| x.as_str()) {
            out.push(Act::Clean(id.to_string()));
        } else if let Some((pid, n)) = v.get("calm").and_then(|x| x.as_str()).and_then(find) {
            out.push(Act::Calm(pid, n));
        } else if let Some((pid, n)) = v.get("end").and_then(|x| x.as_str()).and_then(find) {
            out.push(Act::End(pid, n));
        }
    }
    out
}

fn finding_row(ui: &mut Ui, f: &Finding) -> Option<Act> {
    let mut clicked = None;
    let col = level_color(f.level);
    let tag = match f.level {
        Level::Good => "NOMINAL",
        Level::Info => "NOTICE",
        Level::Warn => "CAUTION",
        Level::Bad => "ALERT",
    };
    hud_frame(ui, None, col, |ui| {
        ui.horizontal(|ui| {
            pill(ui, tag, col);
            ui.label(RichText::new(&f.title).font(display(14.5)).color(TEXT));
        });
        ui.add(egui::Label::new(RichText::new(&f.detail).color(MUTED)).wrap());
        if let Some(fix) = &f.fix {
            ui.add(egui::Label::new(RichText::new(format!("» {fix}")).color(col)).wrap());
        }
        if !f.actions.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for a in &f.actions {
                    if act_button(ui, a, col) {
                        clicked = Some(a.clone());
                    }
                }
            });
        }
    });
    clicked
}

fn ago(ts: i64) -> String {
    let d = chrono::Utc::now().timestamp() - ts;
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{} min ago", d / 60)
    } else if d < 86400 {
        format!("{} h ago", d / 3600)
    } else {
        chrono::DateTime::from_timestamp(ts, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %d %b %H:%M").to_string()).unwrap_or_default()
    }
}

/// Very small markdown: headings, bullets, code blocks.
fn markdown(ui: &mut Ui, text: &str) {
    let mut in_code = false;
    for line in text.lines() {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            ui.label(RichText::new(t).font(mono(13.0)).color(GOOD));
            continue;
        }
        let clean = t.replace("**", "").replace('`', "");
        if let Some(h) = clean.strip_prefix("### ").or_else(|| clean.strip_prefix("## ")).or_else(|| clean.strip_prefix("# ")) {
            ui.add_space(6.0);
            ui.label(RichText::new(h.to_uppercase()).font(display(14.5)).color(ACCENT));
        } else if let Some(b) = clean.trim_start().strip_prefix("- ").or_else(|| clean.trim_start().strip_prefix("* ")) {
            let indent = (clean.len() - clean.trim_start().len()) as f32 * 6.0;
            ui.horizontal_wrapped(|ui| {
                ui.add_space(indent + 4.0);
                ui.label(RichText::new("▹").color(ACCENT));
                ui.add(egui::Label::new(RichText::new(b).color(TEXT)).wrap());
            });
        } else if clean.trim().is_empty() {
            ui.add_space(4.0);
        } else {
            ui.add(egui::Label::new(RichText::new(clean).color(TEXT)).wrap());
        }
    }
}

/// Space backdrop: grid, vignette glow and a slow scanning sweep.
fn backdrop(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    let t = ui.input(|i| i.time) as f32;
    p.rect_filled(rect, CornerRadius::ZERO, BG);
    let step = 44.0;
    let g = Stroke::new(1.0, ACCENT.gamma_multiply(0.035));
    let mut x = rect.left();
    while x < rect.right() {
        p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], g);
        x += step;
    }
    let mut y = rect.top();
    while y < rect.bottom() {
        p.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], g);
        y += step;
    }
    // sweep
    let h = rect.height();
    let sy = rect.top() + (t * 70.0) % (h + 120.0) - 60.0;
    for k in 0..24 {
        let a = 0.05 * (1.0 - k as f32 / 24.0);
        let yy = sy - k as f32 * 2.0;
        if yy > rect.top() && yy < rect.bottom() {
            p.line_segment([Pos2::new(rect.left(), yy), Pos2::new(rect.right(), yy)], Stroke::new(2.0, ACCENT.gamma_multiply(a)));
        }
    }
}

// ---------- live board components ----------

/// Small up/down triangle drawn by hand (no font glyphs needed).
fn tri(p: &egui::Painter, c: Pos2, up: bool, col: Color32, s: f32) {
    let pts = if up {
        vec![c + Vec2::new(0.0, -s * 0.6), c + Vec2::new(s * 0.6, s * 0.45), c + Vec2::new(-s * 0.6, s * 0.45)]
    } else {
        vec![c + Vec2::new(-s * 0.6, -s * 0.45), c + Vec2::new(s * 0.6, -s * 0.45), c + Vec2::new(0.0, s * 0.6)]
    };
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
}

/// Colour for a change in load: rising is hot, falling is cool.
fn delta_color(d: f32) -> Color32 {
    if d > 0.05 { BAD } else if d < -0.05 { GOOD } else { MUTED }
}

/// "▲ 2.4" style change marker as a widget.
fn delta(ui: &mut Ui, d: f32, unit: &str) {
    let col = delta_color(d);
    let txt = format!("{:+.1}{unit}", d);
    let galley = ui.painter().layout_no_wrap(txt, mono(12.5), col);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 16.0, 18.0), Sense::hover());
    let p = ui.painter();
    if d.abs() > 0.05 {
        tri(p, Pos2::new(rect.left() + 5.0, rect.center().y), d > 0.0, col, 8.0);
    } else {
        p.line_segment([Pos2::new(rect.left() + 1.0, rect.center().y), Pos2::new(rect.left() + 9.0, rect.center().y)], Stroke::new(1.5, col));
    }
    p.galley(Pos2::new(rect.left() + 14.0, rect.center().y - galley.size().y / 2.0), galley, col);
}

pub struct TickItem {
    label: String,
    value: String,
    change: f32,
}

/// Scrolling ticker tape, like a market board.
fn ticker(ui: &mut Ui, items: &[TickItem]) {
    let t = ui.input(|i| i.time) as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(3, 9, 15));
    p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, ACCENT.gamma_multiply(0.25)));
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, ACCENT.gamma_multiply(0.25)));
    if items.is_empty() {
        return;
    }
    // lay out once per frame
    let parts: Vec<_> = items
        .iter()
        .map(|it| {
            let l = p.layout_no_wrap(it.label.to_uppercase(), mono(12.5), MUTED);
            let v = p.layout_no_wrap(it.value.clone(), mono(13.5), TEXT);
            let col = delta_color(it.change);
            let d = p.layout_no_wrap(format!("{:+.1}", it.change), mono(12.5), col);
            let w = l.size().x + 8.0 + v.size().x + 8.0 + 12.0 + d.size().x + 34.0;
            (l, v, d, col, it.change, w)
        })
        .collect();
    let total: f32 = parts.iter().map(|x| x.5).sum();
    let offset = (t * 60.0) % total;
    let mut x = rect.left() - offset;
    let cy = rect.center().y;
    let mut i = 0;
    while x < rect.right() {
        let (l, v, d, col, ch, w) = &parts[i % parts.len()];
        let mut cx = x;
        p.galley(Pos2::new(cx, cy - l.size().y / 2.0), l.clone(), MUTED);
        cx += l.size().x + 8.0;
        p.galley(Pos2::new(cx, cy - v.size().y / 2.0), v.clone(), TEXT);
        cx += v.size().x + 8.0;
        if ch.abs() > 0.05 {
            tri(&p, Pos2::new(cx + 4.0, cy), *ch > 0.0, *col, 8.0);
        } else {
            p.line_segment([Pos2::new(cx, cy), Pos2::new(cx + 8.0, cy)], Stroke::new(1.5, *col));
        }
        cx += 12.0;
        p.galley(Pos2::new(cx, cy - d.size().y / 2.0), d.clone(), *col);
        cx += d.size().x + 16.0;
        p.circle_filled(Pos2::new(cx, cy), 1.8, ACCENT.gamma_multiply(0.5));
        x += w;
        i += 1;
    }
    // fade the edges
    for k in 0..24 {
        let a = 1.0 - k as f32 / 24.0;
        let c = Color32::from_rgb(3, 9, 15).gamma_multiply(a);
        p.line_segment([Pos2::new(rect.left() + k as f32, rect.top() + 1.0), Pos2::new(rect.left() + k as f32, rect.bottom() - 1.0)], Stroke::new(1.0, c));
        p.line_segment([Pos2::new(rect.right() - k as f32, rect.top() + 1.0), Pos2::new(rect.right() - k as f32, rect.bottom() - 1.0)], Stroke::new(1.0, c));
    }
}

fn heat(v: f32) -> Color32 {
    // cool cyan → amber → red
    let v = (v / 100.0).clamp(0.0, 1.0);
    let lerp = |a: Color32, b: Color32, t: f32| {
        Color32::from_rgb(
            (a.r() as f32 + (b.r() as f32 - a.r() as f32) * t) as u8,
            (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t) as u8,
            (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t) as u8,
        )
    };
    if v < 0.5 { lerp(Color32::from_rgb(0, 90, 110), ACCENT, v * 2.0) } else if v < 0.8 { lerp(ACCENT, WARN, (v - 0.5) / 0.3) } else { lerp(WARN, BAD, (v - 0.8) / 0.2) }
}

/// Grid of CPU cores as heat tiles.
fn core_matrix(ui: &mut Ui, cores: &[f32]) {
    if cores.is_empty() {
        ui.label(RichText::new("ACQUIRING SIGNAL…").font(mono(11.0)).color(MUTED));
        return;
    }
    let cols = if cores.len() > 8 { 6 } else { 4 };
    let gap = 6.0;
    let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    let h = 46.0;
    let rows = cores.len().div_ceil(cols);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), rows as f32 * (h + gap) - gap), Sense::hover());
    let p = ui.painter();
    for (i, v) in cores.iter().enumerate() {
        let (r, c) = (i / cols, i % cols);
        let tile = Rect::from_min_size(rect.min + Vec2::new(c as f32 * (w + gap), r as f32 * (h + gap)), Vec2::new(w, h));
        let col = heat(*v);
        p.rect_filled(tile, CornerRadius::same(1), col.gamma_multiply(0.10 + 0.30 * (*v / 100.0)));
        p.rect_stroke(tile, CornerRadius::same(1), Stroke::new(1.0, col.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        let mut fill = tile.shrink(3.0);
        fill.set_top(fill.bottom() - 3.0);
        fill.set_width(fill.width() * (*v / 100.0).clamp(0.0, 1.0));
        p.rect_filled(fill, CornerRadius::ZERO, col);
        p.text(tile.left_top() + Vec2::new(6.0, 4.0), Align2::LEFT_TOP, format!("C{:02}", i), mono(10.5), MUTED);
        p.text(tile.right_top() + Vec2::new(-6.0, 15.0), Align2::RIGHT_TOP, format!("{v:.0}%"), mono(15.0), if *v > 50.0 { col } else { TEXT });
    }
}

/// Candlesticks of a series: each candle is `bucket` samples (open, high, low, close).
fn candles(ui: &mut Ui, vals: &[f32], bucket: usize, max: f32, height: f32) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), Sense::hover());
    let p = ui.painter_at(rect.expand(2.0));
    for i in 0..=4 {
        let y = rect.top() + rect.height() * i as f32 / 4.0;
        p.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], Stroke::new(1.0, ACCENT.gamma_multiply(0.07)));
        p.text(Pos2::new(rect.right() - 2.0, y - 1.0), Align2::RIGHT_BOTTOM, format!("{:.0}", max * (1.0 - i as f32 / 4.0)), mono(9.5), MUTED.gamma_multiply(0.6));
    }
    let groups: Vec<&[f32]> = vals.chunks(bucket.max(1)).collect();
    if groups.len() < 2 {
        p.text(rect.center(), Align2::CENTER_CENTER, "ACQUIRING SIGNAL…", mono(11.0), MUTED);
        return;
    }
    let n = groups.len();
    let slots = 30usize;
    let groups: Vec<&[f32]> = groups.into_iter().skip(n.saturating_sub(slots)).collect();
    let n = groups.len();
    let slot = (rect.width() - 30.0) / slots as f32;
    let first = slots - n;
    let y = |v: f32| rect.bottom() - rect.height() * (v / max).clamp(0.0, 1.0);
    let mut hover: Option<(f32, f32, f32, f32)> = None;
    for (i, g) in groups.iter().enumerate() {
        let (o, c) = (g[0], *g.last().unwrap());
        let hi = g.iter().cloned().fold(f32::MIN, f32::max);
        let lo = g.iter().cloned().fold(f32::MAX, f32::min);
        let x = rect.left() + slot * ((first + i) as f32 + 0.5);
        let col = if c >= o { ACCENT } else { VIOLET };
        p.line_segment([Pos2::new(x, y(hi)), Pos2::new(x, y(lo))], Stroke::new(1.0, col));
        let top = y(o.max(c));
        let bot = y(o.min(c)).max(top + 1.5);
        let body = Rect::from_min_max(Pos2::new(x - slot * 0.32, top), Pos2::new(x + slot * 0.32, bot));
        if i == n - 1 {
            p.rect_filled(body.expand(3.0), CornerRadius::ZERO, col.gamma_multiply(0.15));
        }
        p.rect_filled(body, CornerRadius::ZERO, if c >= o { col } else { col.gamma_multiply(0.85) });
        if let Some(hp) = resp.hover_pos() {
            if (hp.x - x).abs() <= slot / 2.0 {
                hover = Some((o, hi, lo, c));
                p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, col.gamma_multiply(0.3)));
            }
        }
    }
    if let Some((o, h, l, c)) = hover {
        p.text(rect.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, format!("O {o:.0}  H {h:.0}  L {l:.0}  C {c:.0}"), mono(11.5), TEXT);
    }
}

// ---------- pages ----------

impl App {
    fn nav(&mut self, ui: &mut Ui, score: u8, now: &Snapshot) {
        ui.add_space(16.0);
        let local = chrono::Local::now();
        hud_frame(ui, Some("Telemetry"), score_color(score), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(local.format("%H:%M").to_string()).font(display(24.0)).color(TEXT));
                ui.label(RichText::new(local.format(":%S").to_string()).font(display(13.0)).color(ACCENT));
            });
            ui.label(RichText::new(format!("{}  ·  UP {}H", local.format("%a %d %b").to_string().to_uppercase(), now.uptime_s / 3600)).font(mono(11.0)).color(MUTED));
            ui.add_space(6.0);
            let temp = now.temp_max_c.unwrap_or(0.0);
            let rows = [
                ("CPU", format!("{:.0}%", now.cpu_pct), now.cpu_pct / 100.0, ACCENT),
                ("MEMORY", format!("{:.0}%", now.mem_used_pct()), now.mem_used_pct() / 100.0, VIOLET),
                ("DISK STALL", format!("{:.0}%", now.psi_io.full10), now.psi_io.full10 / 100.0, if now.psi_io.full10 > 20.0 { BAD } else { GOOD }),
                ("CORE TEMP", if temp > 0.0 { format!("{temp:.0}°C") } else { "–".into() }, temp / 105.0, if temp > 90.0 { BAD } else if temp > 80.0 { WARN } else { GOOD }),
            ];
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.spacing_mut().interact_size.y = 16.0;
            for (name, val, frac, col) in rows {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).font(mono(10.5)).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(val).font(mono(12.0)).color(col));
                    });
                });
                bar(ui, frac, col, ui.available_width());
                ui.add_space(4.0);
            }
            ui.add_space(4.0);
            let ai = match self.cfg.claude_mode {
                ClaudeMode::Cli if self.claude_found => ("AI LINK READY", GOOD),
                ClaudeMode::Api if !self.cfg.api_key.is_empty() || std::env::var("ANTHROPIC_API_KEY").is_ok() => ("AI LINK READY", GOOD),
                _ => ("AI LINK OFFLINE", WARN),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                let pulse = 0.5 + 0.5 * (ui.input(|i| i.time) as f32 * 3.0).sin();
                ui.painter().circle_filled(r.center(), 3.5, ai.1.gamma_multiply(0.4 + 0.6 * pulse));
                ui.label(RichText::new(ai.0).font(mono(11.5)).color(ai.1));
            });
            let active = self.shared.lock().unwrap().service_active;
            let (txt, col) = match active {
                Some(true) => ("SENTINEL ONLINE", GOOD),
                Some(false) => ("SENTINEL OFFLINE", WARN),
                None => ("SENTINEL …", MUTED),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                let blink = ((ui.input(|i| i.time) * 2.0) as i64) % 2 == 0;
                ui.painter().circle_filled(r.center(), 3.5, if blink || active != Some(true) { col } else { col.gamma_multiply(0.3) });
                ui.label(RichText::new(txt).font(mono(11.5)).color(col));
            })
            .response
            .on_hover_text("Background service: history, freeze log, automatic cleanup");
            let (mode_txt, mode_col) = if self.smooth_now {
                ("DISPLAY · SMOOTH 60 FPS".to_string(), ACCENT)
            } else {
                (format!("DISPLAY · EVERY {} S", self.cfg.view_interval.clamp(1, 60)), if self.cfg.view_mode == "auto" { WARN } else { MUTED })
            };
            ui.label(RichText::new(mode_txt).font(mono(10.5)).color(mode_col)).on_hover_text(if self.cfg.view_mode == "auto" {
                "Auto: smooth while the CPU has room, slower while the PC is busy. Change it in Settings."
            } else {
                "Change it in Settings."
            });
            let upd = self.shared.lock().unwrap().update.clone();
            if let Some(Ok(Some((tag, url)))) = upd {
                if hud_button(ui, &format!("Update {tag}"), GOOD, false).clicked() {
                    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                }
            }
        });
        ui.add_space(14.0);
        let items = [
            (Page::Overview, "01", "Overview"),
            (Page::Processes, "02", "Processes"),
            (Page::Disk, "03", "Disk & Space"),
            (Page::Cleanup, "04", "Cleanup"),
            (Page::History, "05", "History"),
            (Page::Claude, "06", "Ask Claude"),
            (Page::Settings, "07", "Settings"),
        ];
        for (p, code, name) in items {
            let sel = self.page == p;
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
            let painter = ui.painter();
            if sel {
                painter.rect_filled(rect, CornerRadius::ZERO, ACCENT.gamma_multiply(0.10));
                painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())), CornerRadius::ZERO, ACCENT);
                brackets(painter, rect, ACCENT.gamma_multiply(0.7), 7.0);
            } else if resp.hovered() {
                painter.rect_filled(rect, CornerRadius::ZERO, ACCENT.gamma_multiply(0.05));
            }
            painter.text(rect.left_center() + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, code, mono(12.0), if sel { ACCENT } else { MUTED.gamma_multiply(0.7) });
            painter.text(rect.left_center() + Vec2::new(40.0, 0.0), Align2::LEFT_CENTER, name.to_uppercase(), mono(14.0), if sel { TEXT } else { MUTED });
            if resp.clicked() {
                self.page = p;
                if p == Page::History {
                    self.load_history(true);
                }
                if p == Page::Disk {
                    let need = self.shared.lock().unwrap().space.is_none();
                    if need {
                        self.start_space();
                    }
                }
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }

    fn overview(&mut self, ui: &mut Ui, now: &Snapshot, ring_v: &[Snapshot]) {
        if self.freezes_at.map(|t| t.elapsed() > Duration::from_secs(15)).unwrap_or(true) {
            self.freezes = self.store.as_ref().map(|s| s.events_since(chrono::Utc::now().timestamp() - 86400)).unwrap_or_default();
            self.freezes_at = Some(Instant::now());
        }
        let score = health_score(now);
        let findings = analyze(now, &ring_v[ring_v.len().saturating_sub(15)..]);
        let headline = findings.first().cloned();
        let hcol = headline.as_ref().map(|h| level_color(h.level)).unwrap_or(ACCENT);
        let mut act: Option<Act> = None;
        hud_frame(ui, Some("Core diagnostics"), hcol, |ui| {
            ui.horizontal(|ui| {
                ring(ui, score, 176.0);
                ui.add_space(18.0);
                ui.vertical(|ui| {
                    ui.add_space(24.0);
                    if let Some(h) = &headline {
                        pill(ui, match h.level { Level::Bad => "Alert", Level::Warn => "Caution", Level::Info => "Notice", Level::Good => "All systems nominal" }, hcol);
                        ui.label(RichText::new(h.title.to_uppercase()).font(display(21.0)).color(TEXT));
                        ui.add(egui::Label::new(RichText::new(&h.detail).color(MUTED)).wrap());
                    }
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        if let Some(h) = &headline {
                            for a in &h.actions {
                                if !matches!(a, Act::Ask(_) | Act::Goto(_)) && hud_button(ui, &a.label(), hcol, true).clicked() {
                                    act = Some(a.clone());
                                }
                            }
                        }
                        if primary(ui, "Ask Claude why").clicked() {
                            self.page = Page::Claude;
                            self.ask_claude("Look at my PC report. What is slowing it down right now, and what should I do first?".into());
                        }
                        if ghost(ui, "Open cleanup").clicked() {
                            self.page = Page::Cleanup;
                        }
                    });
                });
            });
        });
        ui.add_space(10.0);

        // stat cards show the last 90 s so the glide between samples is visible
        let series = |g: &dyn Fn(&Snapshot) -> f32| ring_v[ring_v.len().saturating_sub(90)..].iter().map(|s| g(s)).collect::<Vec<f32>>();
        let prev = ring_v.get(ring_v.len().saturating_sub(11)).cloned().unwrap_or_default();
        let dmb = |s: &Snapshot| s.main_disk().map(|d| d.read_mbs + d.write_mbs).unwrap_or(0.0);
        // movers board refresh every 3 s
        if self.movers_at.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true) {
            let mut m: Vec<(i32, String, f32, f32)> = now
                .procs
                .iter()
                .filter(|p| p.cpu_pct > 0.5 || self.movers_prev.get(&p.pid).copied().unwrap_or(0.0) > 0.5)
                .map(|p| (p.pid, p.name.clone(), p.cpu_pct, p.cpu_pct - self.movers_prev.get(&p.pid).copied().unwrap_or(p.cpu_pct)))
                .collect();
            m.sort_by(|a, b| b.3.abs().partial_cmp(&a.3.abs()).unwrap().then(b.2.partial_cmp(&a.2).unwrap()));
            m.truncate(8);
            self.movers = m;
            self.movers_prev = now.procs.iter().map(|p| (p.pid, p.cpu_pct)).collect();
            self.movers_at = Some(Instant::now());
        }
        // ticker tape
        let mut tape = vec![
            TickItem { label: "CPU".into(), value: format!("{:.1}%", now.cpu_pct), change: now.cpu_pct - prev.cpu_pct },
            TickItem { label: "MEM".into(), value: format!("{:.1}%", now.mem_used_pct()), change: now.mem_used_pct() - prev.mem_used_pct() },
            TickItem { label: "DISK STALL".into(), value: format!("{:.0}%", now.psi_io.full10), change: now.psi_io.full10 - prev.psi_io.full10 },
            TickItem { label: "DISK".into(), value: format!("{:.1} MB/s", dmb(now)), change: dmb(now) - dmb(&prev) },
            TickItem { label: "LATENCY".into(), value: format!("{:.1} ms", now.main_disk().map(|d| d.await_ms).unwrap_or(0.0)), change: now.main_disk().map(|d| d.await_ms).unwrap_or(0.0) - prev.main_disk().map(|d| d.await_ms).unwrap_or(0.0) },
            TickItem { label: "SWAP".into(), value: format!("{:.0}%", now.swap_pct()), change: now.swap_pct() - prev.swap_pct() },
            TickItem { label: "NET IN".into(), value: format!("{:.0} KB/s", now.net_rx_kbs), change: now.net_rx_kbs - prev.net_rx_kbs },
            TickItem { label: "NET OUT".into(), value: format!("{:.0} KB/s", now.net_tx_kbs), change: now.net_tx_kbs - prev.net_tx_kbs },
            TickItem { label: "LOAD".into(), value: format!("{:.2}", now.load1), change: now.load1 - prev.load1 },
        ];
        if let Some(t) = now.temp_max_c {
            tape.push(TickItem { label: "CORE".into(), value: format!("{t:.0}°C"), change: t - prev.temp_max_c.unwrap_or(t) });
        }
        let mut hot: Vec<_> = now.procs.iter().filter(|p| p.cpu_pct > 1.0).collect();
        hot.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap());
        for p in hot.iter().take(8) {
            let before = self.movers_prev.get(&p.pid).copied().unwrap_or(p.cpu_pct);
            tape.push(TickItem { label: p.name.clone(), value: format!("{:.1}%", p.cpu_pct), change: p.cpu_pct - before });
        }
        ui.add_space(8.0);
        ticker(ui, &tape);
        ui.add_space(8.0);
        let disk = now.main_disk().cloned().unwrap_or_default();
        ui.columns(4, |cols| {
            stat_card(
                &mut cols[0],
                "CPU",
                format!("{:.0}%", now.cpu_pct),
                format!("{} cores · load {:.1}{}", now.cores, now.load1, now.temp_max_c.map(|t| format!(" · {t:.0} °C")).unwrap_or_default()),
                &series(&|s| s.cpu_pct),
                Some(100.0),
                ACCENT,
                now.cpu_pct - prev.cpu_pct,
                "%",
            );
            stat_card(
                &mut cols[1],
                "Memory",
                format!("{:.0}%", now.mem_used_pct()),
                format!("{} free · swap {:.0}%", human_bytes(now.mem_avail), now.swap_pct()),
                &series(&|s| s.mem_used_pct()),
                Some(100.0),
                VIOLET,
                now.mem_used_pct() - prev.mem_used_pct(),
                "%",
            );
            stat_card(
                &mut cols[2],
                "Disk stalls",
                format!("{:.0}%", now.psi_io.full10),
                "time everything waited on disk".into(),
                &series(&|s| s.psi_io.full10),
                Some(100.0),
                if now.psi_io.full10 > 20.0 { BAD } else { WARN },
                now.psi_io.full10 - prev.psi_io.full10,
                "%",
            );
            stat_card(
                &mut cols[3],
                "Disk speed",
                format!("{:.1} MB/s", disk.read_mbs + disk.write_mbs),
                format!("{:.0}% busy · {:.1} ms latency", disk.util_pct, disk.await_ms),
                &series(&|s| dmb(s)),
                None,
                BLUE,
                dmb(now) - dmb(&prev),
                "",
            );
        });
        // trading-board row: core matrix, CPU candles, top movers
        ui.add_space(8.0);
        let cpu_series: Vec<f32> = ring_v.iter().map(|s| s.cpu_pct).collect();
        let movers = self.movers.clone();
        let t = ui.input(|i| i.time) as f32;
        ui.columns(3, |c| {
            panel(&mut c[0], "Core matrix", |ui| core_matrix(ui, &now.cores_pct));
            panel(&mut c[1], "CPU candles · 5 s", |ui| {
                let tail = &cpu_series[cpu_series.len().saturating_sub(150)..];
                candles(ui, tail, 5, 100.0, 140.0);
            });
            panel(&mut c[2], "Top movers · 3 s", |ui| {
                if movers.is_empty() {
                    ui.label(RichText::new("ACQUIRING SIGNAL…").font(mono(11.0)).color(MUTED));
                }
                for (_, name, cpu, d) in &movers {
                    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 17.0), Sense::hover());
                    let p = ui.painter();
                    if d.abs() > 8.0 {
                        let pulse = 0.5 + 0.5 * (t * 6.0).sin();
                        p.rect_filled(row, CornerRadius::ZERO, delta_color(*d).gamma_multiply(0.10 + 0.10 * pulse));
                    }
                    let nm: String = name.chars().take(18).collect();
                    p.text(row.left_center() + Vec2::new(4.0, 0.0), Align2::LEFT_CENTER, nm.to_uppercase(), mono(12.0), TEXT);
                    p.text(Pos2::new(row.right() - 70.0, row.center().y), Align2::RIGHT_CENTER, format!("{cpu:.1}%"), mono(12.5), TEXT);
                    let col = delta_color(*d);
                    if d.abs() > 0.05 {
                        tri(p, Pos2::new(row.right() - 60.0, row.center().y), *d > 0.0, col, 7.0);
                    }
                    p.text(Pos2::new(row.right() - 2.0, row.center().y), Align2::RIGHT_CENTER, format!("{:+.1}", d), mono(12.0), col);
                }
            });
        });

        ui.add_space(10.0);
        panel(ui, "Quick actions", |ui| {
            ui.horizontal_wrapped(|ui| {
                let calm_n = self.cfg.calm_apps.len();
                for (a, col) in [
                    (Act::Clean("safe".into()), GOOD),
                    (Act::Clean("trim".into()), ACCENT),
                    (Act::Clean("swap".into()), ACCENT),
                    (Act::Clean("journal".into()), ACCENT),
                    (Act::Clean("browser".into()), ACCENT),
                    (Act::Clean("docker-unused".into()), ACCENT),
                    (Act::Clean("zram".into()), VIOLET),
                    (Act::Clean("ssd-tune".into()), VIOLET),
                    (Act::Clean("tracker".into()), VIOLET),
                ] {
                    // tunes that are already applied disappear from the list
                    if matches!(&a, Act::Clean(id) if ["zram", "ssd-tune", "tracker"].contains(&id.as_str()))
                        && self.shared.lock().unwrap().rules.as_ref().map(|r| !r.iter().any(|x| matches!(&a, Act::Clean(id) if x.id == id))).unwrap_or(false)
                    {
                        continue;
                    }
                    if act_button(ui, &a, col) {
                        act = Some(a);
                    }
                }
                if calm_n > 0 && hud_button(ui, &format!("Calm my {calm_n} background apps"), VIOLET, false).clicked() {
                    self.calm_list_now(now);
                }
            });
        });
        ui.add_space(6.0);
        section(ui, "Threat assessment");
        // the headline already sits in Core diagnostics
        for f in findings.iter().skip(if findings.len() > 1 { 1 } else { 0 }) {
            if let Some(a) = finding_row(ui, f) {
                act = Some(a);
            }
        }

        let (talkers, freezes) = {
            let sh = self.shared.lock().unwrap();
            let fz = self.freezes.clone();
            (sh.talkers.clone(), fz.into_iter().filter(|e| e.kind == "freeze").collect::<Vec<_>>())
        };
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Freeze log · 24 h", |ui| {
                if freezes.is_empty() {
                    ui.label(RichText::new("None recorded. The background service records each one with its cause.").color(MUTED));
                }
                for e in freezes.iter().take(5) {
                    ui.horizontal(|ui| {
                        pill(ui, &ago(e.ts), WARN);
                        ui.label(RichText::new(&e.summary).color(TEXT));
                    });
                    if !e.detail.is_empty() {
                        ui.add(egui::Label::new(RichText::new(&e.detail).color(MUTED).size(12.5)).wrap());
                    }
                }
            });
            panel(&mut cols[1], "Log traffic · 10 min", |ui| {
                if talkers.is_empty() {
                    ui.label(RichText::new("Reading the system log…").color(MUTED));
                }
                let max = talkers.first().map(|t| t.1).unwrap_or(1).max(1);
                for (k, n) in talkers.iter().take(6) {
                    ui.horizontal(|ui| {
                        cell(ui, RichText::new(k).color(TEXT), 150.0);
                        bar(ui, *n as f32 / max as f32, if *n > 3000 { WARN } else { ACCENT }, 120.0);
                        ui.label(RichText::new(format!("{n} lines")).color(MUTED).size(12.5));
                    });
                }
            });
        });
        if let Some(a) = act {
            self.do_act(a);
        }
    }

    fn processes(&mut self, ui: &mut Ui, now: &Snapshot) {
        title(ui, "Processes", "Everything running. Calm lowers a program's CPU and disk priority without closing it.");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter by name").desired_width(220.0));
            ui.add_space(10.0);
            for (s, n) in [(SortBy::Cpu, "CPU"), (SortBy::Mem, "Memory"), (SortBy::Disk, "Disk"), (SortBy::Swap, "Swap")] {
                if ui.add(egui::Button::new(RichText::new(n).color(if self.sort == s { TEXT } else { MUTED })).selected(self.sort == s)).clicked() {
                    self.sort = s;
                }
            }
        });
        ui.add_space(8.0);
        let mut procs: Vec<_> = now.procs.iter().filter(|p| self.filter.is_empty() || p.name.to_lowercase().contains(&self.filter.to_lowercase())).cloned().collect();
        match self.sort {
            SortBy::Cpu => procs.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap()),
            SortBy::Mem => procs.sort_by(|a, b| b.rss.cmp(&a.rss)),
            SortBy::Disk => procs.sort_by(|a, b| (b.read_mbs + b.write_mbs).partial_cmp(&(a.read_mbs + a.write_mbs)).unwrap()),
            SortBy::Swap => procs.sort_by(|a, b| b.swap.cmp(&a.swap)),
        }
        procs.truncate(120);
        card(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("procs").num_columns(8).striped(true).spacing([18.0, 8.0]).min_col_width(40.0).show(ui, |ui| {
                    for h in ["Name", "PID", "CPU", "Memory", "Disk", "Swap", "Priority", ""] {
                        ui.label(RichText::new(h).color(MUTED).size(12.5).strong());
                    }
                    ui.end_row();
                    for p in &procs {
                        ui.horizontal(|ui| {
                            if p.state == 'D' {
                                pill(ui, "waiting on disk", WARN);
                            }
                            cell(ui, RichText::new(&p.name).color(TEXT), 180.0);
                        });
                        ui.label(RichText::new(p.pid.to_string()).color(MUTED));
                        ui.label(RichText::new(format!("{:.1}%", p.cpu_pct)).color(if p.cpu_pct > 50.0 { WARN } else { TEXT }));
                        ui.label(human_bytes(p.rss));
                        let io = p.read_mbs + p.write_mbs;
                        ui.label(RichText::new(if io > 0.01 { format!("{io:.1} MB/s") } else { "–".into() }).color(if io > 5.0 { WARN } else { TEXT }));
                        ui.label(RichText::new(if p.swap > 0 { human_bytes(p.swap) } else { "–".into() }).color(MUTED));
                        ui.label(RichText::new(if p.nice > 0 { format!("calm ({})", p.nice) } else if p.nice < 0 { "high".into() } else { "normal".into() }).color(if p.nice > 0 { ACCENT } else { MUTED }));
                        ui.horizontal(|ui| {
                            if p.user_owned {
                                if p.nice < 10 && ghost(ui, "Calm").clicked() {
                                    match daemon::calm(p.pid) {
                                        Ok(_) => self.toast(format!("{} now runs at low priority", p.name)),
                                        Err(e) => self.toast(format!("Could not calm {}: {e}", p.name)),
                                    }
                                }
                                if ghost(ui, "End").clicked() {
                                    self.confirm_kill = Some((p.pid, p.name.clone()));
                                }
                            } else {
                                ui.label(RichText::new("system").color(MUTED).size(12.0));
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn disk_page(&mut self, ui: &mut Ui, now: &Snapshot, ring_v: &[Snapshot]) {
        title(ui, "Disk & Space", "How fast your drives are and what is filling them.");
        for d in &now.disks {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if d.model.is_empty() { d.name.clone() } else { d.model.clone() }).strong().size(17.0));
                    pill(ui, &d.name, MUTED);
                    if !d.controller.is_empty() {
                        pill(ui, &format!("{} controller", d.controller), if d.model.to_lowercase().contains("samsung") && d.controller != "Samsung" { WARN } else { ACCENT });
                    }
                    if let Some(t) = d.temp_c {
                        pill(ui, &format!("{t:.0} °C"), if t > 70.0 { WARN } else { GOOD });
                    }
                });
                ui.add_space(4.0);
                ui.columns(3, |c| {
                    c[0].label(RichText::new(format!("{:.0}% busy", d.util_pct)).size(20.0).strong());
                    c[0].label(RichText::new("time the drive was working").color(MUTED).size(12.0));
                    c[1].label(RichText::new(format!("{:.1} ms", d.await_ms)).size(20.0).strong().color(if d.await_ms > 20.0 { BAD } else if d.await_ms > 5.0 { WARN } else { TEXT }));
                    c[1].label(RichText::new("wait per request (good SSD: under 1 ms)").color(MUTED).size(12.0));
                    c[2].label(RichText::new(format!("R {:.1} · W {:.1} MB/s", d.read_mbs, d.write_mbs)).size(20.0).strong());
                    c[2].label(RichText::new("read · write").color(MUTED).size(12.0));
                });
                let vals: Vec<f32> = ring_v.iter().filter_map(|s| s.disks.iter().find(|x| x.name == d.name).map(|x| x.util_pct)).collect();
                live_chart(ui, &vals, Some(100.0), BLUE, 60.0);
            });
            ui.add_space(8.0);
        }
        section(ui, "Drives");
        card(ui, |ui| {
            for f in &now.fs {
                ui.horizontal(|ui| {
                    cell(ui, RichText::new(&f.mount).color(TEXT), 140.0);
                    let pct = f.pct();
                    bar(ui, pct / 100.0, if pct > 90.0 { BAD } else if pct > 80.0 { WARN } else { ACCENT }, 280.0);
                    ui.label(RichText::new(format!("{pct:.0}% · {} free of {}", human_bytes(f.total - f.used), human_bytes(f.total))).color(MUTED));
                });
            }
        });
        section(ui, "Biggest folders in your home");
        let (space, busy) = {
            let s = self.shared.lock().unwrap();
            (s.space.clone(), s.space_busy)
        };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                if busy {
                    ui.spinner();
                    ui.label(RichText::new("Measuring folders…").color(MUTED));
                } else if ghost(ui, "Measure again").clicked() {
                    self.start_space();
                }
            });
            if let Some(sp) = space {
                let max = sp.first().map(|x| x.1).unwrap_or(1).max(1);
                for (p, s) in sp {
                    ui.horizontal(|ui| {
                        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        cell(ui, RichText::new(name).color(TEXT), 180.0);
                        bar(ui, s as f32 / max as f32, VIOLET, 260.0);
                        ui.label(RichText::new(human_bytes(s)).color(MUTED));
                        if ghost(ui, "Open").clicked() {
                            let _ = std::process::Command::new("xdg-open").arg(&p).spawn();
                        }
                    });
                }
            }
        });
    }

    fn cleanup_page(&mut self, ui: &mut Ui) {
        title(ui, "Cleanup", "Only caches, old trash and leftovers. Your files and projects are never touched.");
        let (rules, scanning, running, last) = {
            let s = self.shared.lock().unwrap();
            (s.rules.clone(), s.scanning, s.running.clone(), s.last_result.clone())
        };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let total: u64 = rules.as_ref().map(|r| r.iter().filter_map(|x| x.size).sum()).unwrap_or(0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(human_bytes(total)).size(28.0).strong().color(ACCENT));
                    ui.label(RichText::new("can be freed").color(MUTED));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if scanning {
                        ui.spinner();
                        ui.label(RichText::new("Scanning…").color(MUTED));
                    } else if ghost(ui, "Scan again").clicked() {
                        self.start_scan();
                    }
                    if let Some(rules) = &rules {
                        let safe: Vec<Rule> = rules.iter().filter(|r| r.risk == Risk::Safe && r.size.unwrap_or(0) > 0).cloned().collect();
                        if !safe.is_empty() && running.is_none() && primary(ui, "Clean the safe items").clicked() {
                            for r in safe {
                                self.run_rule(r);
                            }
                        }
                    }
                });
            });
            if let Some(r) = &running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("Working on: {r}")).color(MUTED));
                });
            }
            if let Some(l) = &last {
                ui.label(RichText::new(l).color(GOOD));
            }
        });
        let Some(rules) = rules else { return };
        for (risk, head, note) in [
            (Risk::Safe, "Safe — runs on its own", "These rebuild themselves. The background service clears them every few hours if Automatic cleanup is on."),
            (Risk::Ask, "Your call", "Caches and leftovers that are safe to remove but cost a re-download or rebuild later."),
            (Risk::Admin, "System — needs your password", "System-wide items. Ubuntu asks for your password each time."),
        ] {
            section(ui, head);
            ui.label(RichText::new(note).color(MUTED).size(12.5));
            for r in rules.iter().filter(|r| r.risk == risk) {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_max_width(ui.available_width() - 220.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&r.title).strong().size(15.0));
                                if let Some(b) = &r.blocked {
                                    pill(ui, b, WARN);
                                }
                            });
                            ui.add(egui::Label::new(RichText::new(&r.explain).color(MUTED)).wrap());
                            if !r.items.is_empty() {
                                egui::CollapsingHeader::new(RichText::new(format!("{} folders", r.items.len())).color(MUTED).size(12.5)).id_salt(r.id).show(ui, |ui| {
                                    for (p, s) in &r.items {
                                        ui.label(RichText::new(format!("{}  ·  {}", p.display(), human_bytes(*s))).size(12.0).color(MUTED));
                                    }
                                });
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let can = r.blocked.is_none() && running.is_none() && (r.size.unwrap_or(1) > 0);
                            let label = if r.risk == Risk::Admin { "Run…" } else { "Clean" };
                            if ui.add_enabled(can, egui::Button::new(RichText::new(label).color(TEXT)).fill(CARD_HI).stroke(Stroke::new(1.0, LINE))).clicked() {
                                if r.risk == Risk::Safe {
                                    self.run_rule(r.clone());
                                } else {
                                    self.confirm_rule = Some(r.id.to_string());
                                }
                            }
                            ui.label(RichText::new(r.size.map(human_bytes).unwrap_or_else(|| "–".into())).strong().size(16.0).color(if r.size.unwrap_or(0) > 1073741824 { ACCENT } else { TEXT }));
                        });
                    });
                });
            }
        }
    }

    fn history(&mut self, ui: &mut Ui) {
        self.load_history(false);
        title(ui, "History", "Recorded by the background service every 10 seconds, kept for 30 days.");
        ui.horizontal(|ui| {
            for (secs, n) in [(3600, "1 hour"), (6 * 3600, "6 hours"), (86400, "24 hours"), (7 * 86400, "7 days")] {
                if ui.add(egui::Button::new(n).selected(self.hist_range == secs)).clicked() {
                    self.hist_range = secs;
                    self.load_history(true);
                }
            }
        });
        if self.hist.is_empty() {
            card(ui, |ui| {
                ui.label(RichText::new("No history yet").strong());
                ui.label(RichText::new("Turn on the background service in Settings; charts fill in from then on.").color(MUTED));
            });
        } else {
            // thin to at most ~400 points
            let step = (self.hist.len() / 400).max(1);
            let pts: Vec<&Row> = self.hist.iter().step_by(step).collect();
            let s = |g: &dyn Fn(&Row) -> f32| pts.iter().map(|r| g(r)).collect::<Vec<f32>>();
            let from = pts.first().map(|r| ago(r.ts)).unwrap_or_default();
            ui.label(RichText::new(format!("From {from} to now")).color(MUTED).size(12.5));
            ui.columns(2, |c| {
                for (i, (name, vals, max, col)) in [
                    ("Disk stalls %", s(&|r| r.io_full), Some(100.0), BAD),
                    ("CPU %", s(&|r| r.cpu), Some(100.0), ACCENT),
                    ("Memory %", s(&|r| r.mem), Some(100.0), VIOLET),
                    ("Disk latency ms", s(&|r| r.disk_await), None, BLUE),
                ]
                .into_iter()
                .enumerate()
                {
                    let ui = &mut c[i % 2];
                    card(ui, |ui| {
                        ui.label(RichText::new(name).color(MUTED));
                        chart(ui, &vals, max, col, 120.0);
                    });
                    ui.add_space(6.0);
                }
            });
        }
        section(ui, "Events this week");
        card(ui, |ui| {
            if self.events.is_empty() {
                ui.label(RichText::new("Nothing recorded yet.").color(MUTED));
            }
            for e in self.events.iter().take(60) {
                ui.horizontal(|ui| {
                    let col = match e.kind.as_str() {
                        "freeze" => WARN,
                        "cleanup" => GOOD,
                        _ => ACCENT,
                    };
                    pill(ui, &e.kind, col);
                    ui.label(RichText::new(&e.summary).color(TEXT));
                    ui.label(RichText::new(ago(e.ts)).color(MUTED).size(12.0));
                });
                if !e.detail.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&e.detail).color(MUTED).size(12.5)).wrap());
                }
                ui.add_space(2.0);
            }
        });
        section(ui, "Cleanup log");
        card(ui, |ui| {
            if self.cleanup_log.is_empty() {
                ui.label(RichText::new("No cleanups yet.").color(MUTED));
            }
            for c in &self.cleanup_log {
                ui.horizontal(|ui| {
                    pill(ui, if c.auto { "auto" } else { "you" }, if c.auto { ACCENT } else { VIOLET });
                    ui.label(&c.rule);
                    ui.label(RichText::new(human_bytes(c.freed.max(0) as u64)).color(GOOD));
                    ui.label(RichText::new(ago(c.ts)).color(MUTED).size(12.0));
                });
            }
        });
    }

    fn claude_page(&mut self, ui: &mut Ui) {
        title(ui, "Ask Claude", "Claude reads a measured report of this PC and tells you what to fix.");
        let busy = self.shared.lock().unwrap().claude_busy;
        let mode = match self.cfg.claude_mode {
            ClaudeMode::Cli => match self.claude_found {
                true => ("Using your signed-in claude command".to_string(), GOOD),
                false => ("claude command not found: set it up in Settings".to_string(), WARN),
            },
            ClaudeMode::Api => (
                if self.cfg.api_key.is_empty() && std::env::var("ANTHROPIC_API_KEY").is_err() { "API key missing: add it in Settings".to_string() } else { format!("Using API key · {}", self.cfg.model) },
                if self.cfg.api_key.is_empty() && std::env::var("ANTHROPIC_API_KEY").is_err() { WARN } else { GOOD },
            ),
        };
        ui.horizontal(|ui| pill(ui, &mode.0, mode.1));
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for q in [
                "Why does my PC freeze for a few seconds?",
                "What should I clean up first?",
                "Which apps are hurting performance?",
                "Is my SSD healthy and genuine?",
                "Give me a plan to keep this PC at peak speed",
            ] {
                if ui.add_enabled(!busy, egui::Button::new(RichText::new(format!("› {q}")).color(ACCENT)).fill(ACCENT.gamma_multiply(0.05)).stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.4))).corner_radius(CornerRadius::same(1))).clicked() {
                    self.ask_claude(q.to_string());
                }
            }
        });
        ui.add_space(8.0);
        let mut clicked: Option<Act> = None;
        let input_h = 56.0;
        let avail = ui.available_height() - input_h - 40.0;
        card(ui, |ui| {
            egui::ScrollArea::vertical().max_height(avail.max(160.0)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                if self.chat.is_empty() && self.pending_q.is_none() {
                    // empty state
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Pick a question above or type your own.").color(MUTED));
                        ui.label(RichText::new("Each question sends a fresh report: live numbers, the last 24 hours, freezes, log writers and cleanup sizes. No file contents.").color(MUTED).size(12.5));
                    });
                }
                for (q, a, acts) in &self.chat {
                    bubble_user(ui, q);
                    Frame::new().inner_margin(Margin::symmetric(6, 4)).show(ui, |ui| markdown(ui, a));
                    if !acts.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("EXECUTE ›").font(mono(12.0)).color(MUTED));
                            for act in acts {
                                if act_button(ui, act, ACCENT) {
                                    clicked = Some(act.clone());
                                }
                            }
                        });
                    }
                    ui.add_space(10.0);
                }
                if let Some(q) = &self.pending_q {
                    bubble_user(ui, q);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Claude is reading your report…").color(MUTED));
                    });
                }
            });
        });
        if let Some(a) = clicked {
            self.do_act(a);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let te = ui.add(egui::TextEdit::singleline(&mut self.question).hint_text("Ask anything about this PC").desired_width(ui.available_width() - 260.0).margin(Margin::symmetric(10, 8)));
            let send = (te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || primary(ui, "Send").clicked();
            if send && !busy && !self.question.trim().is_empty() {
                let q = std::mem::take(&mut self.question);
                self.ask_claude(q);
            }
            if ghost(ui, if self.show_report { "Hide report" } else { "What gets sent" }).clicked() {
                self.show_report = !self.show_report;
                if self.show_report {
                    self.report_cache = self.report();
                }
            }
            if !self.chat.is_empty() && ghost(ui, "New chat").clicked() {
                self.chat.clear();
            }
        });
        if self.show_report {
            card(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("report").max_height(260.0).show(ui, |ui| {
                    ui.label(RichText::new(&self.report_cache).monospace().size(12.0).color(MUTED));
                });
            });
        }
    }

    fn settings(&mut self, ui: &mut Ui) {
        title(ui, "Settings", "");
        let mut changed = false;
        section(ui, "Background service");
        card(ui, |ui| {
            let active = self.shared.lock().unwrap().service_active;
            ui.label(RichText::new(match active {
                Some(true) => "Running. It records history, catches freezes and runs the safe cleanup.",
                _ => "Off. Without it there is no history, freeze log or automatic cleanup.",
            }).color(MUTED));
            ui.horizontal(|ui| {
                if active != Some(true) && primary(ui, "Install and start").clicked() {
                    match daemon::install_service() {
                        Ok(_) => self.toast("Background service started"),
                        Err(e) => self.toast(format!("Could not install: {e}")),
                    }
                    self.shared.lock().unwrap().service_active = None;
                }
                if active == Some(true) && ghost(ui, "Restart").clicked() {
                    let _ = std::process::Command::new("systemctl").args(["--user", "restart", "membli.service"]).status();
                    self.toast("Restarted");
                }
            });
        });
        section(ui, "Display");
        card(ui, |ui| {
            ui.horizontal(|ui| {
                changed |= ui.radio_value(&mut self.cfg.view_mode, "auto".to_string(), "Auto").changed();
                changed |= ui.radio_value(&mut self.cfg.view_mode, "smooth".to_string(), "Smooth").changed();
                changed |= ui.radio_value(&mut self.cfg.view_mode, "interval".to_string(), "Interval").changed();
            });
            let help = match self.cfg.view_mode.as_str() {
                "smooth" => "Always 60 fps with live animations. Uses the most CPU.",
                "interval" => "Refreshes only at the interval below, no animation. Lightest on a busy PC.",
                _ => "Smooth while the CPU has room; when the PC is busy (CPU over 60%), it refreshes only at the interval below.",
            };
            ui.label(RichText::new(help).color(MUTED));
            ui.horizontal(|ui| {
                ui.label("Refresh every");
                egui::ComboBox::from_id_salt("interval").selected_text(format!("{} s", self.cfg.view_interval)).show_ui(ui, |ui| {
                    for v in [1u64, 2, 5, 10, 15, 30, 60] {
                        changed |= ui.selectable_value(&mut self.cfg.view_interval, v, format!("{v} s")).changed();
                    }
                });
                ui.label(RichText::new(if self.smooth_now { "· now smooth" } else { "· now on interval" }).color(MUTED));
            });
        });
        section(ui, "Guards");
        card(ui, |ui| {
            changed |= ui.checkbox(&mut self.cfg.heat_guard, "Heat guard: switch to the power-saver profile while the CPU stays above 88 °C, and back below 75 °C").changed();
            changed |= ui.checkbox(&mut self.cfg.memory_warn, "Warn me before memory runs out").changed();
            changed |= ui.checkbox(&mut self.cfg.memory_kill, "When RAM and swap are both nearly gone, close the biggest app to prevent a freeze").changed();
            ui.label(RichText::new("It asks the app to close, then forces it after 6 s. Desktop parts (GNOME Shell, Xorg, sound) are never touched.").color(MUTED).size(13.0));
            changed |= ui.checkbox(&mut self.cfg.notify, "Desktop notifications for freezes and guard actions").changed();
        });
        section(ui, "Automatic cleanup");
        card(ui, |ui| {
            changed |= ui.checkbox(&mut self.cfg.auto_cleanup, "Clear the safe items on their own (old thumbnails, old trash, pip cache)").changed();
            changed |= ui.checkbox(&mut self.cfg.auto_browser, "Also clear browser caches, only while the browser is closed").changed();
            ui.horizontal(|ui| {
                ui.label("Every");
                changed |= ui.add(egui::Slider::new(&mut self.cfg.auto_cleanup_hours, 1..=48).suffix(" h")).changed();
                ui.add_space(16.0);
                ui.label("Empty trash older than");
                changed |= ui.add(egui::Slider::new(&mut self.cfg.trash_days, 7..=180).suffix(" days")).changed();
            });
        });
        section(ui, "Calm list");
        card(ui, |ui| {
            ui.add(egui::Label::new(RichText::new("Programs that should give way when the disk is choking. The service lowers their CPU and disk priority (they keep running).").color(MUTED)).wrap());
            let r = ui.add(egui::TextEdit::singleline(&mut self.calm_text).hint_text("e.g. slack, dropbox, steam").desired_width(f32::INFINITY));
            if r.changed() {
                self.cfg.calm_apps = self.calm_text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                changed = true;
            }
            changed |= ui.checkbox(&mut self.cfg.auto_calm, "Calm them automatically during disk stalls").changed();
        });
        section(ui, "Updates");
        card(ui, |ui| {
            let (checking, upd) = {
                let s = self.shared.lock().unwrap();
                (s.update_checking, s.update.clone())
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Membli {}", env!("CARGO_PKG_VERSION"))).font(mono(14.0)));
                if checking {
                    ui.spinner();
                } else if ghost(ui, "Check now").clicked() {
                    self.check_updates();
                }
                match &upd {
                    Some(Ok(Some((tag, url)))) => {
                        ui.label(RichText::new(format!("{tag} is available")).color(GOOD));
                        if primary(ui, "Download").clicked() {
                            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                        }
                    }
                    Some(Ok(None)) => {
                        ui.label(RichText::new("You have the newest version").color(MUTED));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(WARN));
                    }
                    None => {}
                }
            });
            changed |= ui.checkbox(&mut self.cfg.check_updates, "Check for updates when Membli opens").changed();
        });
        section(ui, "Claude");
        card(ui, |ui| {
            ui.horizontal(|ui| {
                changed |= ui.radio_value(&mut self.cfg.claude_mode, ClaudeMode::Cli, "Use the claude command (your Claude login, no key)").changed();
                changed |= ui.radio_value(&mut self.cfg.claude_mode, ClaudeMode::Api, "Use an API key").changed();
            });
            if self.cfg.claude_mode == ClaudeMode::Api {
                ui.horizontal(|ui| {
                    ui.label("API key");
                    let r = ui.add(egui::TextEdit::singleline(&mut self.key_text).password(true).hint_text("sk-ant-…  (or set ANTHROPIC_API_KEY)").desired_width(380.0));
                    if r.changed() {
                        self.cfg.api_key = self.key_text.trim().to_string();
                        changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Model");
                    egui::ComboBox::from_id_salt("model").selected_text(self.cfg.model.clone()).show_ui(ui, |ui| {
                        for m in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"] {
                            changed |= ui.selectable_value(&mut self.cfg.model, m.to_string(), m).changed();
                        }
                    });
                });
                ui.label(RichText::new("The key is stored only in ~/.config/membli/config.json, readable by you alone.").color(MUTED).size(12.0));
            } else if !self.claude_found {
                ui.label(RichText::new("The claude command isn't installed. Install Claude Code, or use an API key.").color(WARN));
            }
        });
        if changed {
            if let Err(e) = self.cfg.save() {
                self.toast(format!("Could not save settings: {e}"));
            }
        }
    }
}

/// Our own window title bar (the system one is turned off): drag to move,
/// double-click to maximize, HUD-drawn window controls.
fn title_bar(ui: &mut Ui) {
    let r = ui.max_rect();
    let ctx = ui.ctx().clone();
    let (maximized, fullscreen) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
    let t = ctx.input(|i| i.time) as f32;
    let drag = ui.interact(r, ui.scope_id().with("titlebar_drag"), Sense::click_and_drag());
    if drag.drag_started() {
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if drag.double_clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgb(3, 6, 11));
    // bottom rail with a light pulse travelling along it
    let y = r.bottom() - 0.5;
    p.line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], Stroke::new(1.0, ACCENT.gamma_multiply(0.18)));
    let x = r.left() + (t * 180.0) % (r.width() + 300.0) - 150.0;
    for k in 0..30 {
        let a = 0.6 * (1.0 - k as f32 / 30.0);
        let xx = x - k as f32 * 5.0;
        if xx > r.left() && xx + 5.0 < r.right() {
            p.line_segment([Pos2::new(xx, y), Pos2::new(xx + 5.0, y)], Stroke::new(1.5, ACCENT.gamma_multiply(a)));
        }
    }
    // left: mark + name + page
    p.text(Pos2::new(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, "MEMBLI", display(13.0), TEXT);

    // right: window controls
    let bw = 40.0;
    let mut x = r.right();
    for kind in ["close"] {
        let br = Rect::from_min_max(Pos2::new(x - bw, r.top()), Pos2::new(x, r.bottom() - 1.0));
        x -= bw;
        let resp = ui.interact(br, ui.scope_id().with(("winbtn", kind)), Sense::click());
        let hot = resp.hovered();
        let col = if hot { if kind == "close" { BAD } else { ACCENT } } else { MUTED };
        let p = ui.painter();
        if hot {
            p.rect_filled(br, CornerRadius::ZERO, col.gamma_multiply(0.14));
            brackets(p, br.shrink(5.0), col, 4.0);
        }
        let cc = br.center();
        let s = Stroke::new(1.4, col);
        match kind {
            "min" => {
                p.line_segment([cc + Vec2::new(-6.0, 4.0), cc + Vec2::new(6.0, 4.0)], s);
            }
            "max" => {
                if maximized {
                    p.rect_stroke(Rect::from_center_size(cc + Vec2::new(2.0, -2.0), Vec2::splat(9.0)), CornerRadius::ZERO, s, egui::StrokeKind::Middle);
                    p.rect_filled(Rect::from_center_size(cc + Vec2::new(-2.0, 2.0), Vec2::splat(9.0)), CornerRadius::ZERO, Color32::from_rgb(3, 6, 11));
                    p.rect_stroke(Rect::from_center_size(cc + Vec2::new(-2.0, 2.0), Vec2::splat(9.0)), CornerRadius::ZERO, s, egui::StrokeKind::Middle);
                } else {
                    p.rect_stroke(Rect::from_center_size(cc, Vec2::splat(11.0)), CornerRadius::ZERO, s, egui::StrokeKind::Middle);
                }
            }
            "full" => {
                let rr = Rect::from_center_size(cc, Vec2::splat(13.0));
                brackets(p, if fullscreen { rr.shrink(2.0) } else { rr }, col, 4.0);
            }
            _ => {
                p.line_segment([cc + Vec2::new(-5.5, -5.5), cc + Vec2::new(5.5, 5.5)], s);
                p.line_segment([cc + Vec2::new(-5.5, 5.5), cc + Vec2::new(5.5, -5.5)], s);
            }
        }
        if resp.clicked() {
            let cmd = match kind {
                "min" => egui::ViewportCommand::Minimized(true),
                "max" => egui::ViewportCommand::Maximized(!maximized),
                "full" => egui::ViewportCommand::Fullscreen(!fullscreen),
                _ => egui::ViewportCommand::Close,
            };
            ctx.send_viewport_cmd(cmd);
        }
        let tip = match kind {
            "min" => "Minimize",
            "max" => "Maximize",
            "full" => "Full screen (F11)",
            _ => "Close",
        };
        resp.on_hover_text(tip);
    }
}

/// Without system decorations the window edges need their own resize handles.
fn resize_edges(ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)) {
        return;
    }
    use egui::{CursorIcon as C, ResizeDirection as D};
    let r = ui.max_rect();
    let e = 5.0;
    let zones = [
        (Rect::from_min_max(Pos2::new(r.right() - 14.0, r.bottom() - 14.0), r.max), D::SouthEast, C::ResizeSouthEast),
        (Rect::from_min_max(Pos2::new(r.left(), r.bottom() - 14.0), Pos2::new(r.left() + 14.0, r.bottom())), D::SouthWest, C::ResizeSouthWest),
        (Rect::from_min_max(Pos2::new(r.right() - e, r.top() + 34.0), Pos2::new(r.right(), r.bottom() - 14.0)), D::East, C::ResizeEast),
        (Rect::from_min_max(Pos2::new(r.left(), r.top() + 34.0), Pos2::new(r.left() + e, r.bottom() - 14.0)), D::West, C::ResizeWest),
        (Rect::from_min_max(Pos2::new(r.left() + 14.0, r.bottom() - e), Pos2::new(r.right() - 14.0, r.bottom())), D::South, C::ResizeSouth),
        (Rect::from_min_max(r.min, Pos2::new(r.right() - 170.0, r.top() + 3.0)), D::North, C::ResizeNorth),
    ];
    for (i, (rect, dir, cur)) in zones.into_iter().enumerate() {
        let resp = ui.interact(rect, ui.scope_id().with(("resize", i)), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ctx.set_cursor_icon(cur);
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }
    // little grip in the corner
    let p = ui.painter();
    for k in 1..=3 {
        let o = k as f32 * 4.0;
        p.line_segment([Pos2::new(r.right() - o, r.bottom() - 2.0), Pos2::new(r.right() - 2.0, r.bottom() - o)], Stroke::new(1.0, ACCENT.gamma_multiply(0.35)));
    }
}

fn bubble_user(ui: &mut Ui, q: &str) {
    ui.with_layout(Layout::top_down(Align::RIGHT), |ui| {
        Frame::new().fill(ACCENT.gamma_multiply(0.18)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_max_width(520.0);
            ui.add(egui::Label::new(RichText::new(q).color(TEXT)).wrap());
        });
    });
    ui.add_space(4.0);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // smooth animation while you look at it, almost nothing in the background
        let focused = ui.input(|i| i.focused);
        // display mode: smooth (60 fps), interval (every N s), or auto (smooth while the CPU has room)
        let busy = {
            let l = self.live.lock().unwrap();
            l.now.cpu_pct > 60.0 || l.now.psi_cpu.some10 > 25.0
        };
        let interval = self.cfg.view_interval.clamp(1, 60);
        self.smooth_now = match self.cfg.view_mode.as_str() {
            "smooth" => true,
            "interval" => false,
            _ => !busy,
        };
        let sample = if self.smooth_now { 1000 } else { interval * 1000 };
        self.sample_ms.store(sample, std::sync::atomic::Ordering::Relaxed);
        let frame_ms = if !focused { 1000.max(sample) } else if self.smooth_now { 16 } else { sample };
        ctx.request_repaint_after(Duration::from_millis(frame_ms));
        let (now_arc, ring_arc, at) = {
            let l = self.live.lock().unwrap();
            (l.now.clone(), l.shared_ring.clone(), l.at)
        };
        let ring_v: &[Snapshot] = &ring_arc;
        // how far we are between two samples: charts glide by this much
        let phase = if self.smooth_now { at.map(|t| (t.elapsed().as_secs_f32() / 1.0).min(1.0)).unwrap_or(1.0) } else { 1.0 };
        ctx.data_mut(|d| d.insert_temp(phase_id(), phase));
        let now: &Snapshot = &now_arc;
        // claude reply arrived
        let reply = self.shared.lock().unwrap().claude_reply.take();
        if let Some(r) = reply {
            let q = self.pending_q.take().unwrap_or_default();
            match r {
                Ok(a) => {
                    let (text, raw) = claude::split_actions(&a);
                    let acts = acts_from_claude(&raw, now);
                    self.chat.push((q, text, acts));
                }
                Err(e) => self.chat.push((q, format!("**Could not reach Claude.** {e}"), vec![])),
            }
        }
        let score = health_score(now);

        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        egui::Panel::top("titlebar").exact_size(34.0).resizable(false).frame(Frame::new()).show(ui, |ui| {
            title_bar(ui);
        });
        egui::Panel::left("nav").exact_size(230.0).resizable(false).frame(Frame::new().fill(SIDE).stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.2))).inner_margin(Margin::symmetric(12, 0))).show(ui, |ui| {
            self.nav(ui, score, now);
        });
        egui::CentralPanel::default().frame(Frame::new().inner_margin(Margin::same(22))).show(ui, |ui| {
            backdrop(ui, ui.max_rect().expand(22.0));
            if ring_v.is_empty() && self.started.elapsed() < Duration::from_secs(3) {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                });
                return;
            }
            match self.page {
                Page::Claude => self.claude_page(ui),
                Page::Processes => self.processes(ui, now),
                p => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match p {
                        Page::Overview => self.overview(ui, now, ring_v),
                        Page::Disk => self.disk_page(ui, now, ring_v),
                        Page::Cleanup => self.cleanup_page(ui),
                        Page::History => self.history(ui),
                        Page::Settings => self.settings(ui),
                        _ => {}
                    });
                }
            }
        });

        // confirmations
        if let Some((pid, name)) = self.confirm_kill.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("kill")).show(&ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new(format!("End {name}?")).strong().size(17.0));
                ui.label(RichText::new("It gets asked to close, like closing its window. Unsaved work in it may be lost.").color(MUTED));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if danger(ui, "End it").clicked() {
                        let r = unsafe { libc::kill(pid, libc::SIGTERM) };
                        self.toast(if r == 0 { format!("Asked {name} to close") } else { format!("Could not end {name}") });
                        close = true;
                    }
                    if ghost(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_kill = None;
            }
        }
        if let Some(id) = self.confirm_rule.clone() {
            let rule = self.shared.lock().unwrap().rules.as_ref().and_then(|r| r.iter().find(|x| x.id == id).cloned());
            let mut close = rule.is_none();
            if let Some(rule) = rule {
                egui::Modal::new(egui::Id::unique("rule")).show(&ctx, |ui| {
                    ui.set_width(420.0);
                    ui.label(RichText::new(&rule.title).strong().size(17.0));
                    ui.add(egui::Label::new(RichText::new(&rule.explain).color(MUTED)).wrap());
                    if let Some(s) = rule.size {
                        ui.label(RichText::new(format!("Frees about {}", human_bytes(s))).color(ACCENT));
                    }
                    if rule.risk == Risk::Admin {
                        ui.label(RichText::new("Ubuntu will ask for your password.").color(WARN));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if primary(ui, "Go ahead").clicked() {
                            self.run_rule(rule.clone());
                            close = true;
                        }
                        if ghost(ui, "Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            if close {
                self.confirm_rule = None;
            }
        }
        resize_edges(ui);
        if let Some(at) = ctx.data(|d| d.get_temp::<Instant>(copied_id())) {
            ctx.data_mut(|d| d.remove::<Instant>(copied_id()));
            if at.elapsed() < Duration::from_secs(2) {
                self.toast("Copied to clipboard");
            }
        }
        // toast
        if let Some((msg, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(4) {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::unique("toast")).anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0)).show(&ctx, |ui| {
                    Frame::new().fill(CARD_HI).stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.5))).corner_radius(CornerRadius::same(10)).inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
                        ui.label(RichText::new(msg).color(TEXT));
                    });
                });
            }
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Membli").with_app_id("membli").with_icon(egui::IconData { rgba: include_bytes!("../assets/icon/membli-256.rgba").to_vec(), width: 256, height: 256 }).with_decorations(false).with_resizable(true).with_inner_size([1280.0, 840.0]).with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native("Membli", opts, Box::new(|cc| Ok(Box::new(App::new(cc))))).map_err(|e| anyhow::anyhow!("{e}"))
}
