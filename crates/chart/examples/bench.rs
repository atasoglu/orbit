use std::time::Instant;

use diskutil_chart::{Layout, Theme};
use diskutil_core::{scan, ScanOptions};
use tiny_skia::Pixmap;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let size: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(700);
    let r = scan(path.as_ref(), &ScanOptions::default()).unwrap();
    let radius = size as f32 / 2.0;

    let t = Instant::now();
    let min_angle = 1.5 / radius; // ~1.5 px at the inner edge scale
    let layout = Layout::new(&r.root, min_angle);
    println!("layout: {:?}, {} arcs, {} rings", t.elapsed(), layout.arcs.len(), layout.rings_used());

    let mut pm = Pixmap::new(size, size).unwrap();
    for (name, theme) in [("light", Theme::light()), ("dark", Theme::dark())] {
        let mut best = f64::MAX;
        for _ in 0..20 {
            let t = Instant::now();
            layout.render(&mut pm, &theme, None);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("render {name} {size}x{size}: {best:.2} ms (best of 20)");
        pm.save_png(format!("/tmp/claude-1000/-home-ahmet-workspaces-diskutil/b5d19efc-d73d-4497-a176-fdfe7745a198/scratchpad/sunburst-{name}.png")).unwrap();
    }

    let t = Instant::now();
    let mut hits = 0;
    for i in 0..100_000 {
        let a = i as f32 * 0.01;
        let d = (i % 330) as f32;
        if layout.hit(d * a.cos(), d * a.sin(), radius - 2.0).is_some() {
            hits += 1;
        }
    }
    println!("hit-test: {:.0} ns/call ({hits} hits)", t.elapsed().as_nanos() as f64 / 100_000.0);
}
