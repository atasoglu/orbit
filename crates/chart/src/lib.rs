//! CPU sunburst chart: layout, software rasterisation (tiny-skia), hit-testing.

use std::f32::consts::TAU;

use diskutil_core::Node;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub const MAX_RINGS: usize = 5;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub background: Color,
    pub gap: Color,
    pub dark: bool,
}

impl Theme {
    pub fn light() -> Self {
        Self { background: Color::from_rgba8(255, 255, 255, 0), gap: Color::from_rgba8(255, 255, 255, 255), dark: false }
    }
    pub fn dark() -> Self {
        Self { background: Color::from_rgba8(28, 28, 30, 0), gap: Color::from_rgba8(28, 28, 30, 255), dark: true }
    }
}

/// One drawable arc. `ring` 0 is the root disc.
#[derive(Clone, Copy, Debug)]
pub struct Arc<'a> {
    pub node: &'a Node,
    pub ring: usize,
    pub start: f32,
    pub end: f32,
    /// Hue slot inherited from the top-level ancestor.
    pub hue: f32,
}

pub struct Layout<'a> {
    pub arcs: Vec<Arc<'a>>,
    /// Index range of arcs per ring, sorted by `start` (for binary search).
    ring_ranges: [(usize, usize); MAX_RINGS + 1],
}

impl<'a> Layout<'a> {
    /// `min_angle`: arcs narrower than this (radians) are merged away.
    pub fn new(root: &'a Node, min_angle: f32) -> Self {
        let mut arcs = vec![Arc { node: root, ring: 0, start: 0.0, end: TAU, hue: 0.0 }];
        let mut frontier = vec![0usize];
        let mut ring_ranges = [(0, 0); MAX_RINGS + 1];
        ring_ranges[0] = (0, 1);
        for ring in 1..=MAX_RINGS {
            let begin = arcs.len();
            let mut next = Vec::new();
            for &pi in &frontier {
                let p = arcs[pi];
                if p.node.size == 0 {
                    continue;
                }
                let span = p.end - p.start;
                let mut a = p.start;
                let n = p.node.children.len().max(1) as f32;
                for (i, c) in p.node.children.iter().enumerate() {
                    let w = span * c.size as f32 / p.node.size as f32;
                    if w < min_angle {
                        break; // children are sorted by size, the rest is smaller
                    }
                    let hue = if ring == 1 { (i as f32 / n.max(8.0)) * 0.9 } else { p.hue };
                    arcs.push(Arc { node: c, ring, start: a, end: a + w, hue });
                    if !c.children.is_empty() {
                        next.push(arcs.len() - 1);
                    }
                    a += w;
                }
            }
            ring_ranges[ring] = (begin, arcs.len());
            frontier = next;
            if frontier.is_empty() {
                for r in ring_ranges.iter_mut().skip(ring + 1) {
                    *r = (arcs.len(), arcs.len());
                }
                break;
            }
        }
        Self { arcs, ring_ranges }
    }

    pub fn rings_used(&self) -> usize {
        self.ring_ranges.iter().rposition(|(a, b)| b > a).unwrap_or(0)
    }

    /// Geometry for ring `r` inside a chart of `radius` pixels.
    fn radii(&self, r: usize, radius: f32) -> (f32, f32) {
        let hub = radius * 0.22;
        let ring_w = (radius - hub) / MAX_RINGS as f32;
        if r == 0 {
            (0.0, hub)
        } else {
            (hub + ring_w * (r - 1) as f32, hub + ring_w * r as f32)
        }
    }

    /// Find the arc under pixel offset `(dx, dy)` from the chart centre.
    pub fn hit(&self, dx: f32, dy: f32, radius: f32) -> Option<&Arc<'a>> {
        let dist = (dx * dx + dy * dy).sqrt();
        let mut angle = dy.atan2(dx) + std::f32::consts::FRAC_PI_2;
        if angle < 0.0 {
            angle += TAU;
        }
        for r in 0..=MAX_RINGS {
            let (r0, r1) = self.radii(r, radius);
            if dist >= r0 && dist < r1 {
                let (s, e) = self.ring_ranges[r];
                let slice = &self.arcs[s..e];
                let i = slice.partition_point(|a| a.end <= angle);
                return slice.get(i).filter(|a| a.start <= angle);
            }
        }
        None
    }

    /// Render into `pixmap` (premultiplied RGBA). `highlight` is an arc index.
    pub fn render(&self, pixmap: &mut Pixmap, theme: &Theme, highlight: Option<usize>) {
        pixmap.fill(theme.background);
        let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
        let (cx, cy) = (w / 2.0, h / 2.0);
        let radius = w.min(h) / 2.0 - 2.0;
        let mut paint = Paint { anti_alias: true, ..Paint::default() };
        let stroke = Stroke { width: 1.5, ..Stroke::default() };
        let mut gap = Paint { anti_alias: true, ..Paint::default() };
        gap.set_color(theme.gap);

        for (i, a) in self.arcs.iter().enumerate() {
            let (r0, r1) = self.radii(a.ring, radius);
            let Some(path) = sector(cx, cy, r0, r1, a.start, a.end) else { continue };
            let (hue, sat, light) = if a.ring == 0 {
                (0.0, 0.0, if theme.dark { 0.30 } else { 0.90 })
            } else {
                let d = a.ring as f32 - 1.0;
                if theme.dark {
                    (a.hue, 0.55, 0.42 + 0.04 * d)
                } else {
                    (a.hue, 0.60, 0.58 + 0.05 * d)
                }
            };
            let boost = if highlight == Some(i) { 0.12 } else { 0.0 };
            paint.set_color(hsl(hue, sat, (light + boost).min(0.9)));
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            pixmap.stroke_path(&path, &gap, &stroke, Transform::identity(), None);
        }
    }
}

/// Annular sector as a polygon; step scales with radius so edges stay smooth.
fn sector(cx: f32, cy: f32, r0: f32, r1: f32, a0: f32, a1: f32) -> Option<tiny_skia::Path> {
    let off = -std::f32::consts::FRAC_PI_2;
    let (a0, a1) = (a0 + off, a1 + off);
    let steps = (((a1 - a0) * r1) / 4.0).ceil().clamp(1.0, 256.0) as usize;
    let mut pb = PathBuilder::new();
    if r0 <= 0.5 && a1 - a0 > TAU - 1e-3 {
        pb.push_circle(cx, cy, r1);
        return pb.finish();
    }
    for i in 0..=steps {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        let (x, y) = (cx + r1 * a.cos(), cy + r1 * a.sin());
        if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
    }
    for i in (0..=steps).rev() {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        pb.line_to(cx + r0 * a.cos(), cy + r0 * a.sin());
    }
    pb.close();
    pb.finish()
}

fn hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.fract() * 6.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Color::from_rgba(r + m, g + m, b + m, 1.0).unwrap_or(Color::BLACK)
}
