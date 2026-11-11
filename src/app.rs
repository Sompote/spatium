//! The window: disks on the left, the ring map in the middle, the folder's contents on
//! the right and the Collector at the bottom. Nothing is deleted until the user reviews
//! the Collector and confirms; then the items go to the Trash, one by one.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense,
    Stroke, Vec2,
};

use crate::scan::{self, human, Progress, Tree, NONE};
use crate::security::{self, Agent, Review, Scope};
use crate::sunburst::{self, Item, Seg};
use crate::theme::{self, medium_font, Tokens, RADIUS, TAB_STRIP_H};
use crate::trash;
use crate::volumes::{self, Volume};

struct Scanning {
    root: PathBuf,
    progress: Arc<Progress>,
    rx: Receiver<Option<Tree>>,
    started: Instant,
    /// Bytes the scan is expected to find, when known: a whole volume's used space, or
    /// the size of the same folder at its last scan.
    expected: Option<u64>,
}

pub struct App {
    t: Tokens,
    volumes: Vec<Volume>,
    tree: Option<Tree>,
    /// Folder the map is centred on.
    view: u32,
    tab: usize,
    scanning: Option<Scanning>,
    scan_secs: f64,
    large: Vec<u32>,
    large_min_mb: f64,
    filter: String,
    collected: Vec<u32>,
    confirm: bool,
    /// Messages from the last Trash run.
    report: Vec<(bool, String)>,
    /// Item under the pointer when the context menu opened.
    menu_item: Option<u32>,
    /// Item being dragged towards the Collector.
    drag: Option<u32>,
    collector_rect: Rect,
    selected: Option<u32>,
    shots: Option<Shots>,
    sec_scope: Scope,
    agent: Agent,
    review: Option<Review>,
    review_err: String,
}

/// SPATIUM_SHOT=<file.png>: save the window to a PNG once the scan is shown (for checks).
struct Shots {
    file: PathBuf,
    frames: u32,
    requested: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let dark = std::env::var("SPATIUM_THEME")
            .map(|v| v != "light")
            .unwrap_or(true);
        let t = if dark { Tokens::DARK } else { Tokens::LIGHT };
        theme::apply(&cc.egui_ctx, t);
        let mut app = App {
            t,
            volumes: volumes::list(),
            tree: None,
            view: 0,
            tab: std::env::var("SPATIUM_TAB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            scanning: None,
            scan_secs: 0.0,
            large: Vec::new(),
            large_min_mb: 100.0,
            filter: String::new(),
            collected: Vec::new(),
            confirm: false,
            report: Vec::new(),
            menu_item: None,
            drag: None,
            collector_rect: Rect::NOTHING,
            selected: None,
            sec_scope: Scope::Mac,
            agent: Agent::Claude,
            review: None,
            review_err: String::new(),
            shots: std::env::var_os("SPATIUM_SHOT").map(|f| Shots {
                file: f.into(),
                frames: 0,
                requested: false,
            }),
        };
        if let Some(p) = std::env::var_os("SPATIUM_SCAN") {
            app.start_scan(PathBuf::from(p));
        }
        app
    }

    fn start_scan(&mut self, root: PathBuf) {
        if let Some(s) = &self.scanning {
            s.progress.cancel.store(true, Ordering::Relaxed);
        }
        let expected = self
            .volumes
            .iter()
            .find(|v| v.mount == root)
            .map(|v| v.used())
            .or_else(|| {
                self.tree
                    .as_ref()
                    .filter(|t| t.root_path == root)
                    .map(|t| t.nodes[0].size)
            });
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p, r) = (progress.clone(), root.clone());
        std::thread::spawn(move || {
            let _ = tx.send(scan::scan(&r, p));
        });
        self.scanning = Some(Scanning {
            root,
            progress,
            rx,
            started: Instant::now(),
            expected,
        });
        self.collected.clear();
        self.report.clear();
    }

    fn poll_scan(&mut self) {
        let Some(s) = &self.scanning else { return };
        if let Ok(res) = s.rx.try_recv() {
            self.scan_secs = s.started.elapsed().as_secs_f64();
            if let Some(tree) = res {
                self.large = tree.largest_files(5000);
                self.tree = Some(tree);
                self.view = 0;
                self.selected = None;
            }
            self.scanning = None;
            self.volumes = volumes::list();
        }
    }

    fn collect(&mut self, i: u32) {
        let Some(tree) = &self.tree else { return };
        if i == 0 || self.collected.iter().any(|&c| tree.is_ancestor(c, i)) {
            return; // the root itself, or already inside a collected folder
        }
        self.collected.retain(|&c| !tree.is_ancestor(i, c));
        self.collected.push(i);
    }

    /// Takes an item (or the collected folder holding it) out of the Collector.
    fn uncollect(&mut self, i: u32) {
        let Some(tree) = &self.tree else { return };
        self.collected.retain(|&c| !tree.is_ancestor(c, i));
    }

    fn collected_size(&self) -> u64 {
        self.tree
            .as_ref()
            .map(|t| {
                self.collected
                    .iter()
                    .map(|&i| t.nodes[i as usize].size)
                    .sum()
            })
            .unwrap_or(0)
    }

    fn move_to_trash(&mut self) {
        let Some(tree) = &mut self.tree else { return };
        self.report.clear();
        for &i in &self.collected {
            let path = tree.path(i);
            match trash::to_trash(&path) {
                Ok(()) => {
                    self.report.push((
                        true,
                        format!("{}  {}", human(tree.nodes[i as usize].size), path.display()),
                    ));
                    tree.remove(i);
                }
                Err(e) => self
                    .report
                    .push((false, format!("{}: {e}", path.display()))),
            }
        }
        // keep failed ones in the Collector so the user sees them
        self.collected.retain(|&i| !tree.nodes[i as usize].removed);
        if tree.nodes[self.view as usize].removed {
            self.view = 0;
        }
        self.large.retain(|&i| !tree.nodes[i as usize].removed);
        self.volumes = volumes::list();
    }
}

fn reveal(p: &Path) {
    let _ = std::process::Command::new("open").arg("-R").arg(p).spawn();
}

fn open_item(p: &Path) {
    let _ = std::process::Command::new("open").arg(p).spawn();
}

/// Cuts a label to `w` points with an ellipsis.
fn elide(ui: &egui::Ui, s: &str, font: &FontId, w: f32) -> String {
    let width = |s: &str| {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if width(s) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let t: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&t) <= w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// Cuts the middle of a path, so the file name and the top folder both stay.
fn elide_mid(ui: &egui::Ui, s: &str, font: &FontId, w: f32) -> String {
    let width = |s: &str| {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if width(s) <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut keep = chars.len();
    while keep > 4 {
        keep = keep * 9 / 10;
        let a: String = chars[..keep / 3].iter().collect();
        let b: String = chars[chars.len() - (keep - keep / 3)..].iter().collect();
        let t = format!("{a}…{b}");
        if width(&t) <= w {
            return t;
        }
    }
    "…".into()
}

fn date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

fn header(ui: &mut egui::Ui, title: &str, context: &str) {
    let t = Tokens::get(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), TAB_STRIP_H + 2.0),
        Sense::hover(),
    );
    let corners = CornerRadius {
        nw: RADIUS,
        ne: RADIUS,
        sw: 0,
        se: 0,
    };
    let p = ui.painter();
    p.rect_filled(rect, corners, t.header);
    p.rect_stroke(
        rect,
        corners,
        Stroke::new(1.0_f32, t.header_edge),
        egui::StrokeKind::Inside,
    );
    let tr = p.text(
        rect.left_center() + Vec2::new(10.0, 0.0),
        Align2::LEFT_CENTER,
        title,
        medium_font(12.5),
        t.text,
    );
    if !context.is_empty() {
        let font = FontId::proportional(12.0);
        let s = elide(ui, context, &font, rect.right() - tr.right() - 18.0);
        ui.painter().text(
            Pos2::new(tr.right() + 7.0, rect.center().y),
            Align2::LEFT_CENTER,
            s,
            font,
            t.accent_text,
        );
    }
    ui.add_space(-ui.spacing().item_spacing.y - 1.0);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let t = Tokens::get(ui.ctx());
    let corners = CornerRadius {
        nw: 0,
        ne: 0,
        sw: RADIUS,
        se: RADIUS,
    };
    egui::Frame::NONE
        .fill(t.card)
        .stroke(Stroke::new(1.0_f32, t.separator))
        .corner_radius(corners)
        .inner_margin(8)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        });
}

/// Usage bar: used part on the accent, free part on the field colour.
fn bar(ui: &mut egui::Ui, frac: f32, h: f32, color: Color32) {
    let t = Tokens::get(ui.ctx());
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    ui.painter()
        .rect_filled(r, CornerRadius::same((h / 2.0) as u8), t.field);
    let mut u = r;
    u.set_width(r.width() * frac.clamp(0.0, 1.0));
    ui.painter()
        .rect_filled(u, CornerRadius::same((h / 2.0) as u8), color);
}

fn button(ui: &mut egui::Ui, label: &str, primary: bool) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    if primary {
        ui.add(egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(t.accent))
    } else {
        ui.button(label)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();
        if self.scanning.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.top_bar(ctx);
        self.left_panel(ctx);
        if self.tree.is_some() && self.scanning.is_none() {
            self.collector(ctx);
            self.right_panel(ctx);
        }
        self.centre(ctx);
        if self.confirm {
            self.confirm_dialog(ctx);
        }
        self.drag_ghost(ctx);
        self.shot(ctx);
    }
}

impl App {
    fn shot(&mut self, ctx: &egui::Context) {
        if self.shots.is_none() {
            return;
        }
        // SPATIUM_SHOT_DURING=<seconds>: capture the progress screen while scanning
        let during = std::env::var("SPATIUM_SHOT_DURING")
            .ok()
            .and_then(|v| v.parse::<f64>().ok());
        // SPATIUM_TAB=2 alone: capture the idle Security tab without starting any scan
        let ready = (self.tab == 2 && self.scanning.is_none())
            || match during {
                Some(sec) => self
                    .scanning
                    .as_ref()
                    .map(|sc| sc.started.elapsed().as_secs_f64() > sec)
                    .unwrap_or(false),
                None => self.tree.is_some() && self.scanning.is_none(),
            };
        let s = self.shots.as_mut().unwrap();
        if ready && !s.requested {
            s.frames += 1;
            if s.frames > 8 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                s.requested = true;
            }
            ctx.request_repaint();
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| {
                if let egui::Event::Screenshot { image, .. } = e {
                    Some(image.clone())
                } else {
                    None
                }
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let _ = image::save_buffer(
                &s.file,
                &bytes,
                w as u32,
                h as u32,
                image::ExtendedColorType::Rgba8,
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        let t = self.t;
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::NONE
                    .fill(t.dock)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .stroke(Stroke::new(1.0_f32, t.separator)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Spatium")
                            .font(medium_font(15.0))
                            .color(t.text),
                    );
                    ui.label(RichText::new("disk space").color(t.text_faint));
                    ui.add_space(14.0);
                    if let Some(tree) = &self.tree {
                        let up = tree.nodes[self.view as usize].parent;
                        if ui
                            .add_enabled(up != NONE, egui::Button::new("‹  Up"))
                            .clicked()
                        {
                            self.view = up;
                        }
                        // breadcrumb from the scanned folder down to the map's centre
                        let chain = tree.chain(self.view);
                        let mut jump = None;
                        for (k, &id) in chain.iter().enumerate() {
                            if k > 0 {
                                ui.label(RichText::new("›").color(t.text_faint));
                            }
                            let name = if id == 0 {
                                tree.root_path.to_string_lossy().into_owned()
                            } else {
                                tree.nodes[id as usize].name.to_string()
                            };
                            let on = id == self.view;
                            let txt =
                                RichText::new(name).color(if on { t.text } else { t.text_dim });
                            if ui
                                .add(
                                    egui::Button::new(if on { txt.strong() } else { txt })
                                        .frame(false),
                                )
                                .clicked()
                            {
                                jump = Some(id);
                            }
                        }
                        if let Some(j) = jump {
                            self.view = j;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if t.dark { "Light" } else { "Dark" };
                        if ui.button(label).clicked() {
                            self.t = if t.dark { Tokens::LIGHT } else { Tokens::DARK };
                            theme::apply(ctx, self.t);
                        }
                        if ui.button("Choose folder…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                self.start_scan(p);
                            }
                        }
                        if let Some(tree) = &self.tree {
                            if self.scanning.is_none() && ui.button("Rescan").clicked() {
                                let p = tree.root_path.clone();
                                self.start_scan(p);
                            }
                        }
                    });
                });
            });
    }

    fn left_panel(&mut self, ctx: &egui::Context) {
        let t = self.t;
        egui::SidePanel::left("disks").resizable(false).exact_width(260.0).frame(egui::Frame::NONE.fill(t.chrome).inner_margin(10)).show(ctx, |ui| {
            header(ui, "Disks", "");
            let mut scan_this = None;
            card(ui, |ui| {
                if ui.small_button("Refresh").clicked() {
                    self.volumes = volumes::list();
                }
                for v in &self.volumes {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&v.name).font(medium_font(13.0)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(self.scanning.is_none(), egui::Button::new("Scan")).clicked() {
                                scan_this = Some(v.mount.clone());
                            }
                        });
                    });
                    let frac = v.used() as f32 / v.total.max(1) as f32;
                    let col = if frac > 0.9 { t.error } else if frac > 0.75 { t.warn } else { t.accent };
                    bar(ui, frac, 8.0, col);
                    ui.label(RichText::new(format!("{} used of {}", human(v.used()), human(v.total))).small().color(t.text_dim));
                    ui.label(RichText::new(format!("{} free ({:.0} %)", human(v.free), 100.0 * (1.0 - frac))).small().color(t.ok));
                }
            });
            ui.add_space(10.0);
            header(ui, "Folders", "");
            card(ui, |ui| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let places = [
                    ("Home", home.clone()),
                    ("Downloads", format!("{home}/Downloads")),
                    ("Documents", format!("{home}/Documents")),
                    ("Library (user)", format!("{home}/Library")),
                    ("Applications", "/Applications".into()),
                ];
                for (name, p) in places {
                    if ui.add_enabled(self.scanning.is_none(), egui::Button::new(name).frame(false)).on_hover_text(&p).clicked() {
                        scan_this = Some(PathBuf::from(p));
                    }
                }
            });
            if let Some(p) = scan_this {
                self.start_scan(p);
            }
            if let Some(tree) = &self.tree {
                ui.add_space(10.0);
                header(ui, "Last scan", "");
                card(ui, |ui| {
                    let r = &tree.nodes[0];
                    ui.label(RichText::new(tree.root_path.to_string_lossy()).color(t.accent_text));
                    ui.label(format!("{}   ·   {} files", human(r.size), crate::thousands(r.files)));
                    ui.label(RichText::new(format!("scanned in {:.1} s", self.scan_secs)).small().color(t.text_faint));
                    if tree.denied > 0 {
                        ui.label(RichText::new(format!("{} folders could not be read", crate::thousands(tree.denied as usize as u64)))
                            .small()
                            .color(t.warn))
                            .on_hover_text("Give Spatium Full Disk Access in System Settings › Privacy & Security to see them.");
                    }
                    // what the scan could not see on a whole volume
                    if let Some(v) = self.volumes.iter().find(|v| v.mount == tree.root_path) {
                        let hidden = v.used().saturating_sub(r.size);
                        ui.label(RichText::new(format!("{} not visible (system, snapshots, no access)", human(hidden))).small().color(t.text_dim));
                    }
                });
            }
        });
    }

    fn right_panel(&mut self, ctx: &egui::Context) {
        let t = self.t;
        let mut go = None;
        let mut add = None;
        let mut untick = None;
        egui::SidePanel::right("contents")
            .resizable(true)
            .default_width(340.0)
            .min_width(260.0)
            .frame(egui::Frame::NONE.fill(t.chrome).inner_margin(10))
            .show(ctx, |ui| {
                let Some(tree) = &self.tree else { return };
                let n = &tree.nodes[self.view as usize];
                let title = if self.view == 0 {
                    tree.root_path.to_string_lossy().into_owned()
                } else {
                    n.name.to_string()
                };
                header(ui, "Contents", &title);
                card(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{}  ·  {} items", human(n.size), n.children.len()))
                            .color(t.text_dim),
                    );
                    ui.add_space(2.0);
                    let row_h = 30.0;
                    let total = n.size.max(1);
                    egui::ScrollArea::vertical().auto_shrink(false).show_rows(
                        ui,
                        row_h,
                        n.children.len(),
                        |ui, range| {
                            for &c in &n.children[range] {
                                let cn = &tree.nodes[c as usize];
                                let (r, resp) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), row_h),
                                    Sense::click_and_drag(),
                                );
                                let sel = self.selected == Some(c);
                                let p = ui.painter();
                                if sel {
                                    p.rect_filled(r, CornerRadius::same(5), t.accent_soft);
                                } else if resp.hovered() {
                                    p.rect_filled(r, CornerRadius::same(5), t.hover);
                                }
                                let in_bin = self.collected.iter().any(|&k| tree.is_ancestor(k, c));
                                if cn.is_dir {
                                    // folder marker: a small painted chevron (the fonts have no ▸)
                                    let c = Pos2::new(r.left() + 10.0, r.center().y - 5.0);
                                    p.add(egui::Shape::convex_polygon(
                                        vec![
                                            c + Vec2::new(-2.5, -4.0),
                                            c + Vec2::new(3.0, 0.0),
                                            c + Vec2::new(-2.5, 4.0),
                                        ],
                                        t.text_faint,
                                        Stroke::NONE,
                                    ));
                                }
                                let font = FontId::proportional(13.0);
                                let size_txt = human(cn.size);
                                let name = elide(ui, &cn.name, &font, r.width() - 120.0);
                                let p = ui.painter();
                                p.text(
                                    Pos2::new(r.left() + 20.0, r.center().y - 5.0),
                                    Align2::LEFT_CENTER,
                                    name,
                                    font,
                                    if in_bin { t.text_faint } else { t.text },
                                );
                                p.text(
                                    Pos2::new(r.right() - 30.0, r.center().y - 5.0),
                                    Align2::RIGHT_CENTER,
                                    size_txt,
                                    FontId::monospace(11.5),
                                    t.text_dim,
                                );
                                // share of the folder
                                let br = Rect::from_min_size(
                                    Pos2::new(r.left() + 20.0, r.bottom() - 7.0),
                                    Vec2::new(r.width() - 52.0, 3.0),
                                );
                                p.rect_filled(br, 1.5, t.field);
                                let mut f = br;
                                f.set_width(br.width() * (cn.size as f32 / total as f32));
                                p.rect_filled(
                                    f,
                                    1.5,
                                    if cn.is_dir { t.accent } else { t.text_faint },
                                );
                                // collect button on the right
                                let cr = Rect::from_center_size(
                                    Pos2::new(r.right() - 13.0, r.center().y),
                                    Vec2::splat(20.0),
                                );
                                let cresp =
                                    ui.interact(cr, ui.id().with(("collect", c)), Sense::click());
                                let (sym, col) = if in_bin {
                                    ("✓", t.ok)
                                } else {
                                    (
                                        "+",
                                        if cresp.hovered() {
                                            t.accent_text
                                        } else {
                                            t.text_faint
                                        },
                                    )
                                };
                                if cresp.hovered() {
                                    ui.painter()
                                        .rect_filled(cr, CornerRadius::same(4), t.pressed);
                                }
                                ui.painter().text(
                                    cr.center(),
                                    Align2::CENTER_CENTER,
                                    sym,
                                    FontId::proportional(15.0),
                                    col,
                                );
                                let tip = if in_bin {
                                    "In the Collector: click to take it out"
                                } else {
                                    "Add to the Collector"
                                };
                                if cresp.on_hover_text(tip).clicked() {
                                    if in_bin {
                                        untick = Some(c);
                                    } else {
                                        add = Some(c);
                                    }
                                } else if resp.double_clicked() {
                                    if cn.is_dir {
                                        go = Some(c);
                                    } else {
                                        open_item(&tree.path(c));
                                    }
                                } else if resp.clicked() {
                                    self.selected = Some(c);
                                }
                                if resp.drag_started() {
                                    self.drag = Some(c);
                                }
                                resp.context_menu(|ui| {
                                    if cn.is_dir && ui.button("Open in map").clicked() {
                                        go = Some(c);
                                        ui.close_menu();
                                    }
                                    if ui.button("Add to Collector").clicked() {
                                        add = Some(c);
                                        ui.close_menu();
                                    }
                                    if ui.button("Reveal in Finder").clicked() {
                                        reveal(&tree.path(c));
                                        ui.close_menu();
                                    }
                                });
                            }
                        },
                    );
                });
            });
        if let Some(g) = go {
            self.view = g;
        }
        if let Some(a) = add {
            self.collect(a);
        }
        if let Some(u) = untick {
            self.uncollect(u);
        }
    }

    fn collector(&mut self, ctx: &egui::Context) {
        let t = self.t;
        let mut remove = None;
        let resp = egui::TopBottomPanel::bottom("collector").resizable(false).exact_height(150.0).frame(egui::Frame::NONE.fill(t.chrome).inner_margin(10)).show(ctx, |ui| {
            let Some(tree) = &self.tree else { return };
            let total = self.collected_size();
            let ctxt = if self.collected.is_empty() { "drag items here, or press +".to_string() } else { format!("{} items  ·  {}", self.collected.len(), human(total)) };
            header(ui, "Collector", &ctxt);
            card(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let list_w = ui.available_width() - 230.0;
                    ui.allocate_ui(Vec2::new(list_w, 88.0), |ui| {
                        egui::ScrollArea::vertical().id_salt("bin").auto_shrink(false).max_height(88.0).show(ui, |ui| {
                            if self.collected.is_empty() && self.report.is_empty() {
                                ui.label(RichText::new("Nothing collected. Items you add stay on disk until you confirm.").color(t.text_faint));
                            }
                            for &i in &self.collected {
                                ui.horizontal(|ui| {
                                    if ui.small_button("×").on_hover_text("Take out of the Collector (the file stays on disk)").clicked() {
                                        remove = Some(i);
                                    }
                                    ui.label(RichText::new(human(tree.nodes[i as usize].size)).monospace().color(t.text_dim));
                                    let font = FontId::proportional(13.0);
                                    let p = tree.path(i).to_string_lossy().into_owned();
                                    let w = ui.available_width();
                                    ui.label(elide_mid(ui, &p, &font, w));
                                });
                            }
                            for (ok, m) in &self.report {
                                let font = FontId::proportional(12.0);
                                let w = ui.available_width() - 20.0;
                                let s = elide_mid(ui, m, &font, w);
                                ui.label(RichText::new(format!("{} {s}", if *ok { "Moved to Trash:" } else { "Failed:" })).small().color(if *ok { t.ok } else { t.error }));
                            }
                        });
                    });
                    ui.vertical(|ui| {
                        ui.set_width(220.0);
                        ui.label(RichText::new(human(total)).font(medium_font(20.0)).color(if total > 0 { t.text } else { t.text_faint }));
                        ui.label(RichText::new("to free").small().color(t.text_faint));
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!self.collected.is_empty(), egui::Button::new(RichText::new("Delete…").color(Color32::WHITE)).fill(t.accent)).clicked() {
                                self.confirm = true;
                            }
                            if ui.add_enabled(!self.collected.is_empty() || !self.report.is_empty(), egui::Button::new("Clear")).clicked() {
                                self.collected.clear();
                                self.report.clear();
                            }
                        });
                    });
                });
            });
        });
        self.collector_rect = resp.response.rect;
        if let Some(r) = remove {
            self.collected.retain(|&c| c != r);
        }
    }

    fn drag_ghost(&mut self, ctx: &egui::Context) {
        let Some(d) = self.drag else { return };
        let Some(tree) = &self.tree else { return };
        let (pos, down) = ctx.input(|i| (i.pointer.hover_pos(), i.pointer.primary_down()));
        let t = self.t;
        if down {
            if let Some(p) = pos {
                let n = &tree.nodes[d as usize];
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("ghost"),
                ));
                let txt = format!("{}  {}", n.name, human(n.size));
                let g = painter.layout_no_wrap(txt, FontId::proportional(13.0), t.text);
                let r = Rect::from_min_size(
                    p + Vec2::new(14.0, 10.0),
                    g.size() + Vec2::new(16.0, 10.0),
                );
                let over = self.collector_rect.contains(p);
                painter.rect_filled(
                    r,
                    CornerRadius::same(6),
                    if over { t.accent } else { t.card },
                );
                painter.rect_stroke(
                    r,
                    CornerRadius::same(6),
                    Stroke::new(1.0_f32, t.accent),
                    egui::StrokeKind::Inside,
                );
                painter.galley(r.min + Vec2::new(8.0, 5.0), g, t.text);
                if over {
                    painter.rect_stroke(
                        self.collector_rect.shrink(4.0),
                        CornerRadius::same(8),
                        Stroke::new(2.0_f32, t.accent),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        } else {
            if pos
                .map(|p| self.collector_rect.contains(p))
                .unwrap_or(false)
            {
                self.collect(d);
            }
            self.drag = None;
        }
    }

    fn centre(&mut self, ctx: &egui::Context) {
        let t = self.t;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(t.canvas).inner_margin(10))
            .show(ctx, |ui| {
                if let Some(s) = &self.scanning {
                    let mut cancel = false;
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.3);
                        ui.add(egui::Spinner::new().size(36.0).color(t.accent));
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(format!("Scanning {}", s.root.display()))
                                .font(medium_font(16.0)),
                        );
                        let f = s.progress.files.load(Ordering::Relaxed);
                        let b = s.progress.bytes.load(Ordering::Relaxed);
                        // the further of the two estimates, held below 100 % until the scan ends
                        // bytes found over bytes expected when the total is known (a whole volume, a
                        // rescan); otherwise the share of folders finished, which runs ahead early
                        let frac = match s.expected.filter(|&e| e > 0) {
                            Some(e) => b as f64 / e as f64,
                            None => s.progress.tree_fraction(),
                        }
                        .min(0.99) as f32;
                        let approx = if s.expected.is_some() { "" } else { "≈ " };
                        ui.label(
                            RichText::new(format!("{approx}{:.0} %", 100.0 * frac))
                                .font(medium_font(28.0))
                                .color(t.accent_text),
                        );
                        ui.add_space(4.0);
                        ui.allocate_ui(Vec2::new(420.0, 12.0), |ui| bar(ui, frac, 10.0, t.accent));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!(
                                "{} files  ·  {}  ·  {:.0} s",
                                crate::thousands(f),
                                human(b),
                                s.started.elapsed().as_secs_f64()
                            ))
                            .color(t.text_dim),
                        );
                        let cur = s
                            .progress
                            .current
                            .lock()
                            .map(|c| c.clone())
                            .unwrap_or_default();
                        let font = FontId::proportional(11.0);
                        let w = ui.available_width() * 0.8;
                        ui.label(
                            RichText::new(elide_mid(ui, &cur, &font, w))
                                .small()
                                .color(t.text_faint),
                        );
                        ui.add_space(10.0);
                        cancel = ui.button("Stop").clicked();
                    });
                    if cancel {
                        s.progress.cancel.store(true, Ordering::Relaxed);
                    }
                    return;
                }
                ui.horizontal(|ui| {
                    ui.set_width(390.0);
                    crate::segmented(ui, &["Map", "Largest files", "Security"], &mut self.tab);
                });
                ui.add_space(6.0);
                if self.tab == 2 {
                    self.security(ui);
                } else if self.tree.is_none() {
                    self.start_page(ui);
                } else if self.tab == 0 {
                    self.map(ui);
                } else {
                    self.large_files(ui);
                }
            });
    }

    fn start_page(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.label(RichText::new("See where your disk space goes").font(medium_font(22.0)));
            ui.add_space(6.0);
            ui.label(RichText::new("Scan a disk or a folder on the left. Add what you no longer need to the Collector,\nreview it, and move it to the Trash. Nothing is deleted without your confirmation.").color(t.text_dim));
            ui.add_space(16.0);
            if let Some(v) = self.volumes.first().cloned() {
                if button(ui, &format!("Scan {}", v.name), true).clicked() {
                    self.start_scan(v.mount);
                }
            }
            ui.add_space(6.0);
            if ui.button("Scan home folder").clicked() {
                if let Ok(h) = std::env::var("HOME") {
                    self.start_scan(h.into());
                }
            }
        });
    }

    fn map(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        let Some(tree) = &self.tree else { return };
        let avail = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(avail, Sense::click_and_drag());
        let radius = (avail.width().min(avail.height()) * 0.5 - 8.0).max(60.0);
        let lay = sunburst::layout(tree, self.view, avail.center(), radius, &t);
        let pointer = resp.hover_pos();
        let hovered: Option<Seg> = pointer.and_then(|p| lay.hit(p));
        let painter = ui.painter_at(avail);
        lay.paint(
            &painter,
            if self.drag.is_none() { hovered } else { None },
            t.canvas,
        );
        // centre: the folder, or what the pointer is over
        let in_centre = pointer
            .map(|p| (p - lay.center).length() < lay.r0)
            .unwrap_or(false);
        let up = tree.nodes[self.view as usize].parent;
        painter.circle_filled(
            lay.center,
            lay.r0 - 2.0,
            if in_centre && up != NONE {
                t.hover
            } else {
                t.card
            },
        );
        painter.circle_stroke(lay.center, lay.r0 - 2.0, Stroke::new(1.0_f32, t.separator));
        let (title, size, sub) = match hovered.map(|h| h.item) {
            Some(Item::Node(i)) => {
                let n = &tree.nodes[i as usize];
                let sub = if n.is_dir {
                    format!("{} files", crate::thousands(n.files))
                } else {
                    format!("file · {}", date(n.mtime))
                };
                (n.name.to_string(), n.size, sub)
            }
            Some(Item::Small { count, size, .. }) => {
                (format!("{count} smaller items"), size, String::new())
            }
            None => {
                let n = &tree.nodes[self.view as usize];
                let name = if self.view == 0 {
                    tree.root_path
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| tree.root_path.to_string_lossy().into_owned())
                } else {
                    n.name.to_string()
                };
                let sub = if up != NONE && in_centre {
                    "click to go up".to_string()
                } else {
                    format!("{} files", crate::thousands(n.files))
                };
                (name, n.size, sub)
            }
        };
        let font = medium_font(14.0);
        let tt = elide(ui, &title, &font, lay.r0 * 1.7);
        painter.text(
            lay.center - Vec2::new(0.0, 18.0),
            Align2::CENTER_CENTER,
            tt,
            font,
            t.text,
        );
        painter.text(
            lay.center + Vec2::new(0.0, 4.0),
            Align2::CENTER_CENTER,
            human(size),
            medium_font(18.0),
            t.accent_text,
        );
        painter.text(
            lay.center + Vec2::new(0.0, 26.0),
            Align2::CENTER_CENTER,
            sub,
            FontId::proportional(11.5),
            t.text_faint,
        );
        // labels on the first ring where they fit
        for s in lay.segs.iter().filter(|s| s.depth == 1) {
            let Item::Node(i) = s.item else { continue };
            let (ri, ro) = lay.radii(1);
            let mid = 0.5 * (s.a0 + s.a1);
            let arc = (s.a1 - s.a0) * (ri + ro) * 0.5;
            if arc < 60.0 {
                continue;
            }
            let pos = lay.center + Vec2::angled(mid) * (ri + ro) * 0.5;
            let f = FontId::proportional(11.0);
            // horizontal text: it has to fit the ring's width as well as the arc
            let fit = arc.min(ro - ri) * 0.85;
            let name = elide(ui, &tree.nodes[i as usize].name, &f, fit);
            painter.text(
                pos,
                Align2::CENTER_CENTER,
                name,
                f,
                Color32::from_black_alpha(220),
            );
        }
        if resp.hovered() && hovered.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.drag_started() {
            if let Some(Seg {
                item: Item::Node(i),
                ..
            }) = hovered
            {
                self.drag = Some(i);
            }
        }
        if resp.clicked() {
            match hovered.map(|h| h.item) {
                Some(Item::Node(i)) if tree.nodes[i as usize].is_dir => self.view = i,
                Some(Item::Node(i)) => self.selected = Some(i),
                Some(Item::Small { .. }) => {}
                None if in_centre && up != NONE => self.view = up,
                None => {}
            }
        }
        if resp.secondary_clicked() {
            self.menu_item = match hovered.map(|h| h.item) {
                Some(Item::Node(i)) => Some(i),
                _ => None,
            };
        }
        let mut add = None;
        let mut go = None;
        if let Some(i) = self.menu_item {
            let path = tree.path(i);
            let is_dir = tree.nodes[i as usize].is_dir;
            resp.context_menu(|ui| {
                ui.label(RichText::new(tree.nodes[i as usize].name.to_string()).strong());
                ui.separator();
                if is_dir && ui.button("Open in map").clicked() {
                    go = Some(i);
                    ui.close_menu();
                }
                if ui.button("Add to Collector").clicked() {
                    add = Some(i);
                    ui.close_menu();
                }
                if ui.button("Reveal in Finder").clicked() {
                    reveal(&path);
                    ui.close_menu();
                }
            });
        }
        let hint = "Click a folder to open it  ·  click the centre to go up  ·  drag an item to the Collector  ·  right-click for more";
        ui.painter().text(
            avail.left_bottom() + Vec2::new(4.0, -4.0),
            Align2::LEFT_BOTTOM,
            hint,
            FontId::proportional(11.0),
            t.text_faint,
        );
        if let Some(g) = go {
            self.view = g;
        }
        if let Some(a) = add {
            self.collect(a);
        }
    }

    fn large_files(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        ui.horizontal(|ui| {
            ui.label("At least");
            ui.add(
                egui::DragValue::new(&mut self.large_min_mb)
                    .range(0.0..=1.0e6)
                    .speed(10.0)
                    .suffix(" MB"),
            );
            ui.add_space(12.0);
            ui.label("Name contains");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(180.0)
                    .hint_text("e.g. .mov"),
            );
        });
        let Some(tree) = &self.tree else { return };
        let min = (self.large_min_mb * 1.0e6) as u64;
        let f = self.filter.to_lowercase();
        let rows: Vec<u32> = self
            .large
            .iter()
            .copied()
            .filter(|&i| {
                let n = &tree.nodes[i as usize];
                n.size >= min && (f.is_empty() || n.name.to_lowercase().contains(&f))
            })
            .collect();
        let total: u64 = rows.iter().map(|&i| tree.nodes[i as usize].size).sum();
        ui.label(
            RichText::new(format!(
                "{} files  ·  {} (the largest {} of the scan are listed)",
                rows.len(),
                human(total),
                crate::thousands(self.large.len() as u64)
            ))
            .color(t.text_dim),
        );
        ui.add_space(4.0);
        let mut add = None;
        let mut untick = None;
        let mut show = None;
        egui::Frame::NONE
            .fill(t.card)
            .stroke(Stroke::new(1.0_f32, t.separator))
            .corner_radius(CornerRadius::same(RADIUS))
            .inner_margin(6)
            .show(ui, |ui| {
                let row_h = 34.0;
                egui::ScrollArea::vertical().auto_shrink(false).show_rows(
                    ui,
                    row_h,
                    rows.len(),
                    |ui, range| {
                        for &i in &rows[range] {
                            let n = &tree.nodes[i as usize];
                            let (r, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), row_h),
                                Sense::click_and_drag(),
                            );
                            let in_bin = self.collected.iter().any(|&k| tree.is_ancestor(k, i));
                            if resp.hovered() {
                                ui.painter().rect_filled(r, CornerRadius::same(5), t.hover);
                            }
                            let font = FontId::proportional(13.0);
                            let small = FontId::proportional(11.0);
                            let name = elide(ui, &n.name, &font, r.width() - 260.0);
                            let folder = tree.path(n.parent).to_string_lossy().into_owned();
                            let folder = elide_mid(ui, &folder, &small, r.width() - 260.0);
                            let p = ui.painter();
                            p.text(
                                Pos2::new(r.left() + 8.0, r.top() + 10.0),
                                Align2::LEFT_CENTER,
                                name,
                                font,
                                if in_bin { t.text_faint } else { t.text },
                            );
                            p.text(
                                Pos2::new(r.left() + 8.0, r.bottom() - 9.0),
                                Align2::LEFT_CENTER,
                                folder,
                                small,
                                t.text_faint,
                            );
                            p.text(
                                Pos2::new(r.right() - 130.0, r.center().y),
                                Align2::RIGHT_CENTER,
                                human(n.size),
                                FontId::monospace(12.0),
                                t.text,
                            );
                            p.text(
                                Pos2::new(r.right() - 40.0, r.center().y),
                                Align2::RIGHT_CENTER,
                                date(n.mtime),
                                FontId::proportional(11.5),
                                t.text_dim,
                            );
                            let cr = Rect::from_center_size(
                                Pos2::new(r.right() - 16.0, r.center().y),
                                Vec2::splat(22.0),
                            );
                            let cresp =
                                ui.interact(cr, ui.id().with(("lcollect", i)), Sense::click());
                            if cresp.hovered() {
                                ui.painter()
                                    .rect_filled(cr, CornerRadius::same(4), t.pressed);
                            }
                            let (sym, col) = if in_bin {
                                ("✓", t.ok)
                            } else {
                                (
                                    "+",
                                    if cresp.hovered() {
                                        t.accent_text
                                    } else {
                                        t.text_faint
                                    },
                                )
                            };
                            ui.painter().text(
                                cr.center(),
                                Align2::CENTER_CENTER,
                                sym,
                                FontId::proportional(15.0),
                                col,
                            );
                            let tip = if in_bin {
                                "In the Collector: click to take it out"
                            } else {
                                "Add to the Collector"
                            };
                            if cresp.on_hover_text(tip).clicked() {
                                if in_bin {
                                    untick = Some(i);
                                } else {
                                    add = Some(i);
                                }
                            } else if resp.double_clicked() {
                                reveal(&tree.path(i));
                            }
                            if resp.drag_started() {
                                self.drag = Some(i);
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Add to Collector").clicked() {
                                    add = Some(i);
                                    ui.close_menu();
                                }
                                if ui.button("Show folder in map").clicked() {
                                    show = Some(n.parent);
                                    ui.close_menu();
                                }
                                if ui.button("Reveal in Finder").clicked() {
                                    reveal(&tree.path(i));
                                    ui.close_menu();
                                }
                            });
                        }
                    },
                );
            });
        if let Some(a) = add {
            self.collect(a);
        }
        if let Some(u) = untick {
            self.uncollect(u);
        }
        if let Some(s) = show {
            self.view = s;
            self.tab = 0;
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let t = self.t;
        let Some(tree) = &self.tree else { return };
        let mut go = false;
        let mut cancel = false;
        let m = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(RichText::new("Move these items to the Trash?").font(medium_font(16.0)));
            ui.label(RichText::new("They go to the Trash, not deleted. You can put them back from the Trash in Finder until you empty it.").color(t.text_dim));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                for &i in &self.collected {
                    let p = tree.path(i);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:>10}", human(tree.nodes[i as usize].size))).monospace().color(t.text_dim));
                        let font = FontId::proportional(13.0);
                        let w = ui.available_width();
                        let s = elide_mid(ui, &p.to_string_lossy(), &font, w);
                        if let Some(r) = trash::refuse_reason(&p) {
                            ui.label(RichText::new(format!("{s}  (will be skipped: {r})")).color(t.warn));
                        } else {
                            ui.label(s);
                        }
                    });
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} items  ·  {}", self.collected.len(), human(self.collected_size()))).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Move to Trash").color(Color32::WHITE)).fill(t.error)).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        });
        if go {
            self.confirm = false;
            self.move_to_trash();
        } else if cancel || m.should_close() {
            self.confirm = false;
        }
    }
}

impl App {
    fn security(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        let running = self.review.as_ref().map(|r| !r.done.load(Ordering::Relaxed)).unwrap_or(false);
        if running {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        ui.label(
            RichText::new(
                "Claude Code or Codex scans for malware, trojans, adware and cracked software by reading start-up items, \
code signatures, downloads and scripts. The agent works read-only: it cannot delete, move or change anything. \
It is a careful second opinion, not an antivirus.",
            )
            .color(t.text_dim),
        );
        ui.add_space(8.0);
        egui::Grid::new("sec_opts").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
            ui.label("Scan");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.sec_scope, Scope::Mac, "This Mac (start-up items, apps, downloads)");
                let folder = match &self.sec_scope {
                    Scope::Folder(p) => p.display().to_string(),
                    Scope::Mac => "a folder".into(),
                };
                let on = matches!(self.sec_scope, Scope::Folder(_));
                if ui.radio(on, format!("Folder: {folder}")).clicked() && !on {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.sec_scope = Scope::Folder(p);
                    }
                }
                if on && ui.small_button("Change…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.sec_scope = Scope::Folder(p);
                    }
                }
            });
            ui.end_row();
            ui.label("Agent");
            ui.horizontal(|ui| {
                for a in [Agent::Claude, Agent::Codex] {
                    let found = a.find().is_some();
                    let label = if found { a.name().to_string() } else { format!("{} (not installed)", a.name()) };
                    ui.add_enabled_ui(found, |ui| ui.radio_value(&mut self.agent, a, label));
                }
            });
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let label = format!("Scan with {}", self.agent.name());
            if ui.add_enabled(!running, egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(t.accent)).clicked() {
                match security::start(self.agent, self.sec_scope.clone()) {
                    Ok(r) => {
                        self.review = Some(r);
                        self.review_err.clear();
                    }
                    Err(e) => self.review_err = e,
                }
            }
            if running && ui.button("Stop").clicked() {
                if let Some(r) = &self.review {
                    r.stop();
                }
            }
            if let Some(r) = &self.review {
                let state = if running { "scanning…" } else { "finished" };
                if running {
                    ui.add(egui::Spinner::new().color(t.accent));
                }
                let what = match &r.scope {
                    Scope::Mac => "this Mac".to_string(),
                    Scope::Folder(p) => p.display().to_string(),
                };
                ui.label(RichText::new(format!("{} on {what}  ·  {state}", r.agent.name())).color(t.text_dim));
                if !running && ui.small_button("Open report").on_hover_text(security::report_path().display().to_string()).clicked() {
                    open_item(&security::report_path());
                }
            }
        });
        if !self.review_err.is_empty() {
            ui.label(RichText::new(&self.review_err).color(t.error));
        }
        ui.add_space(6.0);
        let Some(r) = &self.review else {
            ui.label(RichText::new("A scan of this Mac takes a few minutes. Nothing runs until you press Scan.").color(t.text_faint));
            return;
        };
        egui::Frame::NONE.fill(t.card).stroke(Stroke::new(1.0_f32, t.separator)).corner_radius(CornerRadius::same(RADIUS)).inner_margin(12).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("review").auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                let text = r.text.lock().unwrap().clone();
                if text.is_empty() {
                    ui.label(RichText::new("Waiting for the agent…").color(t.text_faint));
                }
                for l in text.lines() {
                    if l.starts_with("  ›") {
                        ui.label(RichText::new(l).small().monospace().color(t.text_faint));
                    } else if l.starts_with('#') {
                        ui.add_space(4.0);
                        ui.label(RichText::new(l.trim_start_matches('#').trim()).font(medium_font(15.0)));
                    } else {
                        ui.label(RichText::new(l.replace("**", "").replace('`', "")).color(t.text));
                    }
                }
            });
        });
    }
}
