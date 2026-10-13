//! CPU sunburst chart: layout, software rasterisation (tiny-skia), hit-testing.

use std::f32::consts::TAU;

use diskutil_core::Node;
use tiny_skia::{Color, FillRule, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub const MAX_RINGS: usize = 5;

/// Minimum arc length (px, at the ring's mid radius) for any drawn arc.
const MIN_ARC_PX: f32 = 8.0;

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
    /// `radius`: chart radius in px. Every arc gets at least `MIN_ARC_PX` of
    /// arc length at its ring's mid radius, so small items stay selectable;
    /// items that cannot fit are merged into a grey "Other" arc. Widths are
    /// therefore proportional only for items above that floor.
    pub fn new(root: &'a Node, radius: f32) -> Self {
        let mut arcs = vec![Arc { node: Some(root), ring: 0, start: 0.0, end: TAU, hue: 0.0, parent: usize::MAX }];
        let mut frontier = vec![0usize];
        let mut ring_ranges = [(0, 0); MAX_RINGS + 1];
        ring_ranges[0] = (0, 1);
        for ring in 1..=MAX_RINGS {
            let begin = arcs.len();
            let mut next = Vec::new();
            let (r0, r1) = radii_for(ring, radius);
            let min_w = MIN_ARC_PX / ((r0 + r1) / 2.0).max(1.0);
            for &pi in &frontier {
                let p = arcs[pi];
                let Some(pn) = p.node else { continue };
                if pn.size == 0 {
                    continue;
                }
                let span = p.end - p.start;
                let cap = ((span / min_w) as usize).max(1);

                // Items: children by size, then one aggregated remainder.
                let kids = &pn.children;
                let sum_kids: u64 = kids.iter().map(|c| c.size).sum();
                let mut keep = kids.len();
                let mut other = pn.size.saturating_sub(sum_kids);
                // A bare remainder (folded small files) only earns an arc when
                // it is visibly wide; merged children always do.
                let natural = span * other as f32 / pn.size as f32;
                if natural < min_w * 0.5 {
                    other = 0;
                }
                if keep + usize::from(other > 0) > cap {
                    keep = cap.saturating_sub(1);
                    other += kids[keep..].iter().map(|c| c.size).sum::<u64>();
                }
                let mut sizes: Vec<u64> = kids[..keep].iter().map(|c| c.size).collect();
                if other > 0 {
                    sizes.push(other);
                }
                let widths = fill_widths(&sizes, pn.size, span, min_w);

                let mut a = p.start;
                for (i, w) in widths.iter().enumerate() {
                    let child = kids.get(i).filter(|_| i < keep);
                    let hue = match child {
                        Some(_) if ring == 1 => (i as f32 * 0.618_034).fract(),
                        _ => p.hue,
                    };
                    arcs.push(Arc { node: child, ring, start: a, end: a + w, hue, parent: pi });
                    if child.is_some_and(|c| !c.children.is_empty()) {
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
        radii_for(r, radius)
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
            let (hue, sat, light) = base_color(a, theme);
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

        // Selection: the arc lifts off the chart (slightly larger, brighter,
        // soft shadow underneath, thin light rim) instead of getting an outline.
        if let Some(f) = focus {
            let a = &self.arcs[f];
            let (r0, r1) = self.radii(a.ring, radius);
            let pad = 2.0 / ((r0 + r1) / 2.0).max(1.0);
            let (lo, hi) = ((r0 - 3.0).max(0.0), r1 + 6.0);
            let (s, e) = ((a.start - pad).max(a.start - 0.05), (a.end + pad).min(a.end + 0.05));
            if let Some(path) = sector(cx, cy, lo, hi, s, e) {
                // Layered translucent strokes approximate a blurred drop shadow.
                let shadow_a = if theme.dark { 0.10 } else { 0.07 };
                for (width, k) in [(22.0, 0.35), (16.0, 0.5), (11.0, 0.7), (7.0, 0.9), (3.0, 1.0)] {
                    let mut sp = Paint { anti_alias: true, ..Paint::default() };
                    sp.set_color(Color::from_rgba(0.0, 0.0, 0.0, shadow_a * k).unwrap_or(Color::BLACK));
                    let st = Stroke { width, line_join: LineJoin::Round, ..Stroke::default() };
                    pixmap.stroke_path(&path, &sp, &st, Transform::from_translate(0.0, 3.0), None);
                }
                let (hue, sat, light) = base_color(a, theme);
                let mut fp = Paint { anti_alias: true, ..Paint::default() };
                fp.set_color(hsl(hue, (sat + 0.08).min(1.0), (light + 0.07).min(0.92)));
                pixmap.fill_path(&path, &fp, FillRule::Winding, Transform::identity(), None);
                let mut rim = Paint { anti_alias: true, ..Paint::default() };
                rim.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.85).unwrap_or(Color::WHITE));
                let st = Stroke { width: 1.5, line_join: LineJoin::Round, ..Stroke::default() };
                pixmap.stroke_path(&path, &rim, &st, Transform::identity(), None);
            }
        }
    }
}

/// Base (hue, saturation, lightness) of an arc for the given theme.
fn base_color(a: &Arc, theme: &Theme) -> (f32, f32, f32) {
    if a.node.is_none() {
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
    }
}

fn radii_for(r: usize, radius: f32) -> (f32, f32) {
    let hub = radius * 0.22;
    let ring_w = (radius - hub) / MAX_RINGS as f32;
    if r == 0 {
        (0.0, hub)
    } else {
        (hub + ring_w * (r - 1) as f32, hub + ring_w * r as f32)
    }
}

/// Split `span` proportionally to `sizes`, but never below `min_w` per item
/// (water-filling: items under the floor are pinned, the rest share what is left).
fn fill_widths(sizes: &[u64], total: u64, span: f32, min_w: f32) -> Vec<f32> {
    let mut w: Vec<f32> = sizes.iter().map(|&s| span * s as f32 / total as f32).collect();
    let mut pinned = vec![false; w.len()];
    loop {
        let n_pinned = pinned.iter().filter(|&&p| p).count();
        let free: u64 = sizes.iter().zip(&pinned).filter(|(_, &p)| !p).map(|(&s, _)| s).sum();
        let budget = span - min_w * n_pinned as f32;
        let mut changed = false;
        for i in 0..w.len() {
            if pinned[i] {
                w[i] = min_w;
            } else {
                w[i] = budget * sizes[i] as f32 / free.max(1) as f32;
                if w[i] < min_w {
                    pinned[i] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            return w;
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
