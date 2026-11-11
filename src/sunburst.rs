//! The ring map: the current folder in the middle, its contents on rings around it,
//! each item's angle proportional to its size. Small items merge into one grey sector.

use std::f32::consts::TAU;

use eframe::egui::{self, epaint::Vertex, Color32, Mesh, Pos2, Shape, Stroke};

use crate::scan::Tree;
use crate::theme::Tokens;

pub const RINGS: usize = 5;
/// Items narrower than this (radians) are merged into "smaller items".
const MIN_SPAN: f32 = 0.006;

#[derive(Clone, Copy)]
pub enum Item {
    Node(u32),
    /// Merged small items of this folder: their count and total size.
    Small {
        parent: u32,
        count: usize,
        size: u64,
    },
}

#[derive(Clone, Copy)]
pub struct Seg {
    pub item: Item,
    pub depth: usize,
    pub a0: f32,
    pub a1: f32,
    pub color: Color32,
}

pub struct Layout {
    pub center: Pos2,
    pub r0: f32,
    pub ring: f32,
    pub segs: Vec<Seg>,
}

/// Hues of the top-level sectors, the Tellus tool colours.
fn palette(t: &Tokens) -> [Color32; 9] {
    let h = t.tools;
    [
        h.load, h.soil, h.geogrid, h.mesh, h.view, h.water, h.fixity, h.spring, h.plate,
    ]
}

fn shade(c: Color32, depth: usize, file: bool, dark: bool) -> Color32 {
    // deeper rings fade towards the canvas, files fade more than folders
    let k = (depth as f32 - 1.0) * 0.11 + if file { 0.22 } else { 0.0 };
    let bg = if dark {
        Color32::from_rgb(28, 29, 32)
    } else {
        Color32::from_rgb(238, 240, 243)
    };
    lerp(c, bg, k.min(0.7))
}

pub fn lerp(a: Color32, b: Color32, k: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

pub fn layout(tree: &Tree, root: u32, center: Pos2, radius: f32, t: &Tokens) -> Layout {
    let r0 = radius * 0.27;
    let ring = (radius - r0) / RINGS as f32;
    let mut segs = Vec::new();
    let pal = palette(t);
    let grey = if t.dark {
        Color32::from_rgb(70, 72, 78)
    } else {
        Color32::from_rgb(190, 193, 199)
    };
    let mut stack: Vec<(u32, usize, f32, f32, Color32)> =
        vec![(root, 0, -TAU / 4.0, TAU * 0.75, Color32::GRAY)];
    while let Some((id, depth, a0, a1, col)) = stack.pop() {
        let n = &tree.nodes[id as usize];
        if depth >= RINGS || n.size == 0 {
            continue;
        }
        let span = a1 - a0;
        let mut a = a0;
        let (mut small_n, mut small_size) = (0usize, 0u64);
        for (k, &c) in n.children.iter().enumerate() {
            let cn = &tree.nodes[c as usize];
            let s = span * cn.size as f32 / n.size as f32;
            if s < MIN_SPAN {
                small_n += 1;
                small_size += cn.size;
                continue;
            }
            let hue = if depth == 0 { pal[k % pal.len()] } else { col };
            let color = shade(hue, depth + 1, !cn.is_dir, t.dark);
            segs.push(Seg {
                item: Item::Node(c),
                depth: depth + 1,
                a0: a,
                a1: a + s,
                color,
            });
            if cn.is_dir {
                stack.push((c, depth + 1, a, a + s, hue));
            }
            a += s;
        }
        if small_n > 0 && a1 - a > 0.002 {
            segs.push(Seg {
                item: Item::Small {
                    parent: id,
                    count: small_n,
                    size: small_size,
                },
                depth: depth + 1,
                a0: a,
                a1,
                color: grey,
            });
        }
    }
    Layout {
        center,
        r0,
        ring,
        segs,
    }
}

impl Layout {
    pub fn radii(&self, depth: usize) -> (f32, f32) {
        let ri = self.r0 + (depth - 1) as f32 * self.ring;
        (ri + 1.0, ri + self.ring - 1.0)
    }

    /// The segment under a point; `None` inside the centre circle or off the rings.
    pub fn hit(&self, p: Pos2) -> Option<Seg> {
        let d = p - self.center;
        let r = d.length();
        if r < self.r0 {
            return None;
        }
        let depth = ((r - self.r0) / self.ring) as usize + 1;
        let mut a = d.y.atan2(d.x);
        if a < -TAU / 4.0 {
            a += TAU;
        }
        self.segs
            .iter()
            .copied()
            .find(|s| s.depth == depth && a >= s.a0 && a < s.a1)
    }

    pub fn paint(&self, painter: &egui::Painter, hovered: Option<Seg>, edge: Color32) {
        let mut mesh = Mesh::default();
        for s in &self.segs {
            let (ri, ro) = self.radii(s.depth);
            let lit = hovered.map(|h| same(h, *s)).unwrap_or(false);
            let c = if lit {
                lerp(s.color, Color32::WHITE, 0.35)
            } else {
                s.color
            };
            sector(&mut mesh, self.center, ri, ro, s.a0, s.a1, c);
        }
        painter.add(Shape::mesh(mesh));
        // thin radial separators between sectors
        let stroke = Stroke::new(1.0_f32, edge);
        for s in &self.segs {
            if s.a1 - s.a0 < 0.012 {
                continue;
            }
            let (ri, ro) = self.radii(s.depth);
            let (sn, cs) = s.a0.sin_cos();
            painter.line_segment(
                [
                    self.center + egui::vec2(cs, sn) * ri,
                    self.center + egui::vec2(cs, sn) * ro,
                ],
                stroke,
            );
        }
    }
}

fn same(a: Seg, b: Seg) -> bool {
    match (a.item, b.item) {
        (Item::Node(x), Item::Node(y)) => x == y,
        (Item::Small { parent: x, .. }, Item::Small { parent: y, .. }) => x == y,
        _ => false,
    }
}

fn sector(mesh: &mut Mesh, c: Pos2, ri: f32, ro: f32, a0: f32, a1: f32, col: Color32) {
    let steps = (((a1 - a0) / 0.025).ceil() as usize).max(1);
    let base = mesh.vertices.len() as u32;
    for k in 0..=steps {
        let a = a0 + (a1 - a0) * k as f32 / steps as f32;
        let (s, cs) = a.sin_cos();
        mesh.vertices.push(Vertex {
            pos: c + egui::vec2(cs, s) * ri,
            uv: egui::epaint::WHITE_UV,
            color: col,
        });
        mesh.vertices.push(Vertex {
            pos: c + egui::vec2(cs, s) * ro,
            uv: egui::epaint::WHITE_UV,
            color: col,
        });
    }
    for k in 0..steps as u32 {
        let i = base + 2 * k;
        mesh.indices
            .extend_from_slice(&[i, i + 1, i + 2, i + 1, i + 3, i + 2]);
    }
}
