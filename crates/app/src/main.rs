use std::cell::RefCell;
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use diskutil_chart::{Layout, Theme as ChartTheme, MAX_RINGS};
use diskutil_core::{scan, Node, NodeKind, ScanOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, ModelRc, VecModel, Image, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode};
use tiny_skia::Pixmap;

slint::include_modules!();

thread_local! {
    // Row -> node map for the inspector list (kept apart from STATE so row
    // callbacks can read it while STATE is mutably borrowed).
    static LIST: RefCell<Vec<*const Node>> = const { RefCell::new(Vec::new()) };
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
    /// Node under the pointer.
    hover: Option<*const Node>,
    /// Node the user clicked; everything unrelated is faded.
    selected: Option<*const Node>,
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

    fn zoom_out(&mut self) {
        self.path.pop();
        self.selected = None;
        self.hover = None;
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

/// Look up an arc by node address; pointers are only compared, never dereferenced.
fn arc_of(layout: &Layout, p: Option<*const Node>) -> Option<usize> {
    let p = p?;
    layout.arcs.iter().position(|a| a.node.is_some_and(|n| ptr::eq(n, p)))
}

fn render(ui: &MainWindow, st: &mut State) {
    let dark = ui.global::<Theme>().get_dark();
    st.last_dark = dark;
    let Some(cur) = st.current() else { return };
    let (px, _) = chart_px(ui);
    let half = px as f32 / 2.0;
    let layout = Layout::new(cur, half - 2.0);
    let theme = if dark { ChartTheme::dark() } else { ChartTheme::light() };
    let hover = arc_of(&layout, st.hover);
    let focus = arc_of(&layout, st.selected);
    let Some(mut pm) = Pixmap::new(px, px) else { return };
    layout.render(&mut pm, &theme, hover, focus);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(px, px);
    buf.make_mut_bytes().copy_from_slice(pm.data());
    ui.set_chart(Image::from_rgba8_premultiplied(buf));
    ui.set_crumb(st.crumb().into());

}

/// Fill the inspector from the selection, or the hovered node when nothing is selected.
fn refresh_info(ui: &MainWindow, st: &State) {
    let Some(cur) = st.current() else { return };
    let (p, title) = match (st.selected, st.hover) {
        (Some(p), _) => (Some(p), "SELECTION"),
        (None, h) => (h, "HOVER"),
    };
    ui.set_sel_title(title.into());
    ui.set_has_selection(st.selected.is_some());
    // Resolve the pointer back to a node by searching the current subtree.
    let node = p.and_then(|p| find_node(cur, p, MAX_RINGS));
    ui.set_can_zoom(st.selected.is_some() && node.is_some_and(|n| n.kind == NodeKind::Dir && !n.children.is_empty()));
    let Some(n) = node else {
        fill_list(ui, st, cur, None);
        ui.set_sel_name(SharedString::new());
        ui.set_sel_size(SharedString::new());
        ui.set_sel_share(SharedString::new());
        ui.set_sel_files(SharedString::new());
        return;
    };
    fill_list(ui, st, cur, Some(n));
    ui.set_sel_name(n.name.to_string().into());
    ui.set_sel_size(human(n.size).into());
    ui.set_sel_share(format!("{:.1}% of {}", n.size as f64 * 100.0 / cur.size.max(1) as f64, cur.name).into());
    ui.set_sel_files(if n.kind == NodeKind::Dir { format!("{} files", n.file_count) } else { "File".into() }.into());
}

/// Contents list: children of the selected folder, or of the current root.
fn fill_list(ui: &MainWindow, st: &State, cur: &Node, node: Option<&Node>) {
    let base = match (st.selected, node) {
        (Some(_), Some(n)) if n.kind == NodeKind::Dir => n,
        _ => cur,
    };
    let ptrs: Vec<*const Node> = base.children.iter().take(200).map(|c| c as *const Node).collect();
    let rows: Vec<Item> = base
        .children
        .iter()
        .take(200)
        .map(|c| Item { name: c.name.to_string().into(), size: human(c.size).into() })
        .collect();
    LIST.with(|l| *l.borrow_mut() = ptrs);
    ui.set_items(ModelRc::new(VecModel::from(rows)));
}

fn find_node(from: &Node, target: *const Node, depth: usize) -> Option<&Node> {
    if ptr::eq(from, target) {
        return Some(from);
    }
    if depth == 0 {
        return None;
    }
    from.children.iter().find_map(|c| find_node(c, target, depth - 1))
}

/// Child-index path from `from` to `target` (within `MAX_RINGS`).
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
    let layout = Layout::new(cur, half - 2.0);
    let (dx, dy) = (x * sf - half, y * sf - half);
    layout.hit(dx, dy, half - 2.0).map(|a| (a.node, a.ring))
}

/// Zoom the chart into `target` (a directory below the current root).
fn zoom_into(st: &mut State, target: *const Node) {
    let Some(cur) = st.current() else { return };
    if let Some(p) = find_path(cur, target, MAX_RINGS) {
        st.path.extend(p);
        st.selected = None;
        st.hover = None;
    }
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

/// Run `f` on the UI state, then refresh the inspector and redraw the chart.
fn update(ui: &MainWindow, f: impl FnOnce(&mut State)) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        f(&mut s);
        refresh_info(ui, &s);
        render(ui, &mut s);
    });
}

fn main() -> Result<(), slint::PlatformError> {
    // Wayland app-id: lets GNOME match the window to orbit.desktop and its icon.
    slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select()?;
    slint::set_xdg_app_id("orbit")?;
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
                    update(&ui, |s| {
                        s.root = Some(Rc::new(r.root));
                        s.path.clear();
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
            update(&ui, |_| {});
        }
    });

    ui.on_chart_resized({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                update(&ui, |_| {});
            }
        }
    });

    ui.on_hover({
        let weak = ui.as_weak();
        move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            let changed = STATE.with(|s| {
                let s = s.borrow();
                let node = s.current().and_then(|cur| hit(&ui, cur, x, y)).and_then(|(n, _)| n);
                node.map(|n| n as *const Node) != s.hover
            });
            if changed {
                update(&ui, |s| {
                    let node = s
                        .current()
                        .and_then(|cur| hit(&ui, cur, x, y))
                        .and_then(|(n, _)| n)
                        .map(|n| n as *const Node);
                    s.hover = node;
                });
            }
        }
    });

    ui.on_clicked({
        let weak = ui.as_weak();
        move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            update(&ui, |s| {
                let h = s.current().and_then(|cur| hit(&ui, cur, x, y)).map(|(n, r)| (n.map(|n| n as *const Node), r));
                match h {
                    // Centre disc: clear the selection first, otherwise go up a level.
                    Some((_, 0)) => {
                        if s.selected.is_some() {
                            s.selected = None;
                        } else {
                            s.zoom_out();
                        }
                    }
                    Some((Some(p), _)) => s.selected = Some(p),
                    // Empty space or the aggregated "Other" arc.
                    _ => s.selected = None,
                }
            });
        }
    });

    ui.on_double_clicked({
        let weak = ui.as_weak();
        move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            update(&ui, |s| {
                let target = s.current().and_then(|cur| hit(&ui, cur, x, y)).and_then(|(n, r)| {
                    n.filter(|n| r > 0 && n.kind == NodeKind::Dir && !n.children.is_empty())
                        .map(|n| n as *const Node)
                });
                if let Some(t) = target {
                    zoom_into(s, t);
                }
            });
        }
    });

    ui.on_zoom_in({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                update(&ui, |s| {
                    if let Some(t) = s.selected {
                        zoom_into(s, t);
                    }
                });
            }
        }
    });

    ui.on_go_up({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                update(&ui, State::zoom_out);
            }
        }
    });

    ui.on_go_root({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                update(&ui, |s| {
                    s.path.clear();
                    s.selected = None;
                    s.hover = None;
                });
            }
        }
    });

    ui.on_item_clicked({
        let weak = ui.as_weak();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(p) = LIST.with(|l| l.borrow().get(i as usize).copied()) {
                update(&ui, |s| s.selected = Some(p));
            }
        }
    });

    ui.on_item_hovered({
        let weak = ui.as_weak();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(p) = LIST.with(|l| l.borrow().get(i as usize).copied()) {
                if STATE.with(|s| s.borrow().hover) != Some(p) {
                    // Only the chart highlight changes; keep the list itself stable.
                    STATE.with(|s| s.borrow_mut().hover = Some(p));
                    STATE.with(|s| render(&ui, &mut s.borrow_mut()));
                }
            }
        }
    });

    ui.on_set_theme({
        let weak = ui.as_weak();
        move |mode| {
            if let Some(ui) = weak.upgrade() {
                apply_theme(&ui, &mode);
                update(&ui, |_| {});
            }
        }
    });

    ui.run()
}
