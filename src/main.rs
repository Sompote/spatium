//! Spatium: a disk space map in the style of DaisyDisk, with the Tellus3D look.
//! It reads the disk and never deletes on its own: the user collects items, reviews
//! the list and confirms, and the items go to the Trash.

mod app;
mod scan;
mod security;
mod sunburst;
mod theme;
mod trash;
mod volumes;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Sense, Vec2};

use theme::{medium_font, Tokens};

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Spatium")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Spatium",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}

/// A count with thousands separators: 17,640.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Segmented control, as in Tellus3D's panels.
pub fn segmented(ui: &mut egui::Ui, labels: &[&str], current: &mut usize) {
    let t = Tokens::get(ui.ctx());
    egui::Frame::NONE
        .fill(t.field)
        .stroke(egui::Stroke::new(1.0_f32, t.separator))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(3)
        .show(ui, |ui| {
            let w = (ui.available_width() - 2.0 * (labels.len() - 1) as f32) / labels.len() as f32;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, l) in labels.iter().enumerate() {
                    let (r, resp) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::click());
                    let on = *current == i;
                    let fill = if on {
                        t.accent_soft
                    } else if resp.hovered() {
                        t.hover
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(r, CornerRadius::same(6), fill);
                    let font = if on {
                        medium_font(13.0)
                    } else {
                        FontId::proportional(13.0)
                    };
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        *l,
                        font,
                        if on { t.text } else { t.text_dim },
                    );
                    if resp.clicked() {
                        *current = i;
                    }
                }
            });
        });
}

/// A small rounded label in a colour, on a tint of itself (Tellus3D's chip).
pub fn chip(ui: &mut egui::Ui, text: &str, colour: Color32) {
    let t = Tokens::get(ui.ctx());
    let font = FontId::proportional(11.0);
    let w = ui.fonts(|f| {
        f.layout_no_wrap(text.to_string(), font.clone(), colour)
            .size()
            .x
    }) + 14.0;
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
    ui.painter().rect_filled(
        r,
        CornerRadius::same(10),
        colour.gamma_multiply(if t.dark { 0.18 } else { 0.14 }),
    );
    ui.painter()
        .text(r.center(), Align2::CENTER_CENTER, text, font, colour);
}
