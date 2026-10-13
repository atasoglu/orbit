use std::cell::RefCell;
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use diskutil_chart::{Layout, Theme as ChartTheme, MAX_RINGS};
use diskutil_core::{scan, Node, NodeKind, ScanOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode};
use tiny_skia::Pixmap;

slint::include_modules!();

thread_local! {
    // UI-thread-only state (Rc nodes are not Send).
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Logical padding between the chart panel edge and the chart itself.
const CHART_PAD: f32 = 24.0;

#[derive(Default)]
struct State {
    root: Option<Rc<Node>>,
    /// Child indices from `root` down to the node shown in the chart.
    path: Vec<usize>,
    highlight: Option<*const Node>,
    last_dark: bool,
}

impl State {
    fn current(&self) -> Option<&Node> {
        let mut n: &Node = self.root.as_deref()?;
        for &i in &self.path {
            n = n.children.get(i)?;
        }
        Some(n)
    }

    fn crumb(&self) -> String {
        let Some(mut n) = self.root.as_deref() else { return String::new() };
        let mut s = n.name.to_string();
        for &i in &self.path {
            let Some(c) = n.children.get(i) else { break };
            if !s.ends_with('/') {
                s.push('/');
            }
            s.push_str(&c.name);
            n = c;
        }
        s
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Chart geometry in physical pixels for the current window.
fn chart_px(ui: &MainWindow) -> (u32, f32) {
    let logical = (ui.get_chart_size() - CHART_PAD).max(0.0);
    let sf = ui.window().scale_factor();
    (((logical * sf) as u32).max(64), sf)
}

fn render(ui: &MainWindow, st: &mut State) {
    let dark = ui.global::<Theme>().get_dark();
    st.last_dark = dark;
    let Some(cur) = st.current() else { return };
    let (px, _) = chart_px(ui);
    let radius = px as f32 / 2.0;
    let layout = Layout::new(cur, 1.5 / radius);
    let theme = if dark { ChartTheme::dark() } else { ChartTheme::light() };
    let hl = st
        .highlight
        .and_then(|p| layout.arcs.iter().position(|a| a.node.is_some_and(|n| ptr::eq(n, p))));
    let Some(mut pm) = Pixmap::new(px, px) else { return };
    layout.render(&mut pm, &theme, hl);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(px, px);
    buf.make_mut_bytes().copy_from_slice(pm.data());
    ui.set_chart(Image::from_rgba8_premultiplied(buf));
    ui.set_crumb(st.crumb().into());
}

fn show_selection(ui: &MainWindow, st: &State, node: Option<&Node>) {
    let (Some(cur), Some(n)) = (st.current(), node) else {
        ui.set_sel_name(SharedString::new());
        ui.set_sel_size(SharedString::new());
        ui.set_sel_share(SharedString::new());
        ui.set_sel_files(SharedString::new());
        return;
    };
    ui.set_sel_name(n.name.to_string().into());
    ui.set_sel_size(human(n.size).into());
    ui.set_sel_share(format!("{:.1}% of {}", n.size as f64 * 100.0 / cur.size.max(1) as f64, cur.name).into());
    ui.set_sel_files(
        if n.kind == NodeKind::Dir { format!("{} files", n.file_count) } else { "File".into() }.into(),
    );
}

/// Find the child-index path from `from` to `target` (within `MAX_RINGS`).
fn find_path(from: &Node, target: *const Node, depth: usize) -> Option<Vec<usize>> {
    if depth == 0 {
        return None;
    }
    for (i, c) in from.children.iter().enumerate() {
        if ptr::eq(c, target) {
            return Some(vec![i]);
        }
        if let Some(mut p) = find_path(c, target, depth - 1) {
            p.insert(0, i);
            return Some(p);
        }
    }
    None
}

fn hit<'a>(ui: &MainWindow, cur: &'a Node, x: f32, y: f32) -> Option<(Option<&'a Node>, usize)> {
    let (px, sf) = chart_px(ui);
    let half = px as f32 / 2.0;
    let radius = half - 2.0;
    let layout = Layout::new(cur, 1.5 / half);
    let (dx, dy) = (x * sf - half, y * sf - half);
    layout.hit(dx, dy, radius).map(|a| (a.node, a.ring))
}

fn apply_theme(ui: &MainWindow, mode: &str) {
    let scheme = match mode {
        "light" => ColorScheme::Light,
        "dark" => ColorScheme::Dark,
        _ => ColorScheme::Unknown,
    };
    ui.global::<Palette>().set_color_scheme(scheme);
    ui.set_theme_mode(mode.into());
}

fn rerender(ui: &MainWindow) {
    STATE.with(|s| render(ui, &mut s.borrow_mut()));
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    apply_theme(&ui, "auto");

    // Scan "/" in the background; poll shared counters for live progress.
    let opts = ScanOptions::default();
    let progress = opts.progress.clone();
    let started = Instant::now();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let res = scan(Path::new("/"), &opts);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match res {
                Ok(r) => {
                    let skipped = if r.errors > 0 {
                        format!(" · {} paths need elevated access", r.errors)
                    } else {
                        String::new()
                    };
                    ui.set_status(
                        format!("{} files · {:.1}s{skipped}", r.root.file_count, started.elapsed().as_secs_f32())
                            .into(),
                    );
                    ui.set_total(format!("Total used: {}", human(r.root.size)).into());
                    STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        s.root = Some(Rc::new(r.root));
                        s.path.clear();
                        render(&ui, &mut s);
                    });
                }
                Err(e) => ui.set_status(format!("Scan failed: {e}").into()),
            }
        });
    });

    // Live progress while scanning, plus follow-the-system theme changes (Auto).
    let tick = Timer::default();
    let weak = ui.as_weak();
    tick.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(ui) = weak.upgrade() else { return };
        let have_root = STATE.with(|s| s.borrow().root.is_some());
        if !have_root {
            ui.set_status(
                format!(
                    "Scanning… {} files · {}",
                    progress.files.load(Ordering::Relaxed),
                    human(progress.bytes.load(Ordering::Relaxed))
                )
                .into(),
            );
        } else if STATE.with(|s| s.borrow().last_dark) != ui.global::<Theme>().get_dark() {
            rerender(&ui);
        }
    });

    ui.on_chart_resized({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                rerender(&ui);
            }
        }
    });

    ui.on_hover({
        let weak = ui.as_weak();
        move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(cur) = s.current() else { return };
                let node = hit(&ui, cur, x, y).and_then(|(n, _)| n);
                let ptr = node.map(|n| n as *const Node);
                if ptr == s.highlight {
                    return;
                }
                show_selection(&ui, &s, node);
                s.highlight = ptr;
                render(&ui, &mut s);
            });
        }
    });

    ui.on_clicked({
        let weak = ui.as_weak();
        move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(cur) = s.current() else { return };
                let target = match hit(&ui, cur, x, y) {
                    Some((_, 0)) => {
                        s.path.pop();
                        s.highlight = None;
                        render(&ui, &mut s);
                        return;
                    }
                    Some((Some(n), _)) if n.kind == NodeKind::Dir && !n.children.is_empty() => n as *const Node,
                    _ => return,
                };
                if let Some(p) = find_path(cur, target, MAX_RINGS) {
                    s.path.extend(p);
                    s.highlight = None;
                    render(&ui, &mut s);
                }
            });
        }
    });

    ui.on_go_up({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.path.pop();
                s.highlight = None;
                render(&ui, &mut s);
            });
        }
    });

    ui.on_set_theme({
        let weak = ui.as_weak();
        move |mode| {
            if let Some(ui) = weak.upgrade() {
                apply_theme(&ui, &mode);
                rerender(&ui);
            }
        }
    });

    ui.run()
}
