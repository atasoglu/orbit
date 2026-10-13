//! CPU sunburst chart: layout, software rasterisation (tiny-skia), hit-testing.

use std::f32::consts::TAU;

use diskutil_core::Node;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub const MAX_RINGS: usize = 5;

/// Extra pointer tolerance (px) around arcs when hit-testing.
const HIT_SLACK_PX: f32 = 3.0;

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
    /// `None` marks the aggregated "Other" remainder of the parent.
    pub node: Option<&'a Node>,
    pub ring: usize,
    pub start: f32,
    pub end: f32,
    /// Hue slot inherited from the top-level ancestor.
    pub hue: f32,
    /// Index of the parent arc (`usize::MAX` for the root).
    pub parent: usize,
}

pub struct Layout<'a> {
    pub arcs: Vec<Arc<'a>>,
    /// Index range of arcs per ring, sorted by `start` (for binary search).
    ring_ranges: [(usize, usize); MAX_RINGS + 1],
}

impl<'a> Layout<'a> {
    /// `min_angle`: arcs narrower than this (radians) are merged away.
    pub fn new(root: &'a Node, min_angle: f32) -> Self {
        let mut arcs = vec![Arc { node: Some(root), ring: 0, start: 0.0, end: TAU, hue: 0.0, parent: usize::MAX }];
        let mut frontier = vec![0usize];
        let mut ring_ranges = [(0, 0); MAX_RINGS + 1];
        ring_ranges[0] = (0, 1);
        for ring in 1..=MAX_RINGS {
            let begin = arcs.len();
            let mut next = Vec::new();
            for &pi in &frontier {
                let p = arcs[pi];
                let Some(pn) = p.node else { continue };
                if pn.size == 0 {
                    continue;
                }
                let span = p.end - p.start;
                let mut a = p.start;
                for (i, c) in pn.children.iter().enumerate() {
                    let w = span * c.size as f32 / pn.size as f32;
                    if w < min_angle {
                        break; // children are sorted by size, the rest is smaller
                    }
                    let hue = if ring == 1 { (i as f32 * 0.618_034).fract() } else { p.hue };
                    arcs.push(Arc { node: Some(c), ring, start: a, end: a + w, hue, parent: pi });
                    if !c.children.is_empty() {
                        next.push(arcs.len() - 1);
                    }
                    a += w;
                }
                if p.end - a >= min_angle {
                    arcs.push(Arc { node: None, ring, start: a, end: p.end, hue: p.hue, parent: pi });
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

    /// Centre of arc `i` as a pixel offset from the chart centre.
    pub fn centroid(&self, i: usize, radius: f32) -> (f32, f32) {
        let a = &self.arcs[i];
        let (r0, r1) = self.radii(a.ring, radius);
        let (rm, am) = ((r0 + r1) / 2.0, (a.start + a.end) / 2.0 - std::f32::consts::FRAC_PI_2);
        (rm * am.cos(), rm * am.sin())
    }

    /// True when `anc` is `i` itself or one of its ancestors.
    fn within(&self, mut i: usize, anc: usize) -> bool {
        while i != usize::MAX {
            if i == anc {
                return true;
            }
            i = self.arcs[i].parent;
        }
        false
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
                if r == 0 {
                    return slice.first();
                }
                // Thin slivers are impossible to hit exactly, so allow a few
                // pixels of slack and prefer the thinnest arc within reach.
                let tol = HIT_SLACK_PX / dist.max(1.0);
                let i = slice.partition_point(|a| a.end <= angle);
                let lo = i.saturating_sub(2);
                let hi = (i + 3).min(slice.len());
                return slice[lo..hi]
                    .iter()
                    .filter(|a| a.start - tol <= angle && angle <= a.end + tol)
                    .min_by(|a, b| (a.end - a.start).total_cmp(&(b.end - b.start)));
            }
        }
        None
    }

    /// Render into `pixmap` (premultiplied RGBA). `highlight` (hover) and `focus`
    /// (selection) are arc indices; with a focus, unrelated arcs are faded.
    pub fn render(&self, pixmap: &mut Pixmap, theme: &Theme, highlight: Option<usize>, focus: Option<usize>) {
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
            let (hue, sat, light) = if a.node.is_none() {
                (0.0, 0.0, if theme.dark { 0.38 } else { 0.80 })
            } else if a.ring == 0 {
                (0.0, 0.0, if theme.dark { 0.30 } else { 0.90 })
            } else {
                let d = a.ring as f32 - 1.0;
                if theme.dark {
                    (a.hue, 0.42, 0.40 + 0.045 * d)
                } else {
                    (a.hue, 0.55, 0.60 + 0.05 * d)
                }
            };
            let boost = if highlight == Some(i) { 0.10 } else { 0.0 };
            let alpha = match focus {
                None => 1.0,
                Some(f) if self.within(i, f) || self.within(f, i) => 1.0,
                Some(_) => 0.16,
            };
            let mut c = hsl(hue, sat, (light + boost).min(0.9));
            c.set_alpha(alpha);
            paint.set_color(c);
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            if (a.end - a.start) * r1 > 3.0 {
                pixmap.stroke_path(&path, &gap, &stroke, Transform::identity(), None);
            }
        }

        // Selection border, drawn last so neighbours never cover it.
        if let Some(f) = focus {
            let a = &self.arcs[f];
            let (r0, r1) = self.radii(a.ring, radius);
            if let Some(path) = sector(cx, cy, r0, r1, a.start, a.end) {
                // Neutral two-tone outline (ink + halo): stays visible on every hue.
                let (ink, halo) = if theme.dark {
                    (Color::from_rgba8(255, 255, 255, 255), Color::from_rgba8(28, 28, 30, 255))
                } else {
                    (Color::from_rgba8(29, 29, 31, 255), Color::from_rgba8(255, 255, 255, 255))
                };
                for (color, width) in [(halo, 6.0), (ink, 3.0)] {
                    let mut paint = Paint { anti_alias: true, ..Paint::default() };
                    paint.set_color(color);
                    let stroke = Stroke { width, ..Stroke::default() };
                    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
        }
    }
}

/// Annular sector as a polygon; step scales with radius so edges stay smooth.
fn sector(cx: f32, cy: f32, r0: f32, r1: f32, a0: f32, a1: f32) -> Option<tiny_skia::Path> {
    let off = -std::f32::consts::FRAC_PI_2;
    let (a0, a1) = (a0 + off, a1 + off);
    let steps = (((a1 - a0) * r1) / 6.0).ceil().clamp(1.0, 128.0) as usize;
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
