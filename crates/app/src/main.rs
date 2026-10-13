use std::cell::RefCell;
use std::path::Path;
use std::ptr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskutil_chart::{Layout, Theme as ChartTheme, MAX_RINGS};
use diskutil_core::cleanup::{self, CleanupItem, Method, Mode, Risk};
use diskutil_core::{scan, Node, NodeKind, Progress, ScanOptions};
use slint::language::ColorScheme;
use slint::{ComponentHandle, Model, ModelRc, VecModel, Image, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode};
use tiny_skia::Pixmap;

slint::include_modules!();

#[derive(Default)]
struct Clean {
    items: Vec<CleanupItem>,
    checked: Vec<bool>,
    delete: bool,
    busy: bool,
}

thread_local! {
    static LARGE: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static LARGE_MODEL: Rc<VecModel<LargeItem>> = Rc::new(VecModel::default());
    static CLEAN: RefCell<Clean> = RefCell::new(Clean::default());
    static CLEAN_MODEL: Rc<VecModel<CleanItem>> = Rc::new(VecModel::default());
    static PROGRESS: RefCell<Option<Arc<Progress>>> = const { RefCell::new(None) };
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
    root: Option<Arc<Node>>,
    scanning: bool,
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

const LARGE_LIMIT: usize = 150;

fn collect_files(path: &mut PathBuf, node: &Node, out: &mut Vec<(PathBuf, u64)>) {
    for c in &node.children {
        path.push(&*c.name);
        match c.kind {
            NodeKind::File => out.push((path.clone(), c.size)),
            NodeKind::Dir => collect_files(path, c, out),
        }
        path.pop();
    }
}

fn age(path: &Path) -> String {
    let Ok(m) = std::fs::symlink_metadata(path).and_then(|m| m.modified()) else { return "—".into() };
    let days = m.elapsed().map_or(0, |d| d.as_secs() / 86_400);
    match days {
        0 => "today".into(),
        1..=30 => format!("{days} d ago"),
        31..=364 => format!("{} mo ago", days / 30),
        _ => format!("{:.1} y ago", days as f64 / 365.0),
    }
}

/// Rebuild the "Large files" list from a finished scan.
fn refresh_large(root: &Node) {
    let mut files = Vec::new();
    collect_files(&mut PathBuf::from("/"), root, &mut files);
    let home = home_dir();
    // Trashed files still sit on disk; listing them would let a second "Trash"
    // click delete them for good. The Cleanup page owns the Trash.
    let trash = home.join(".local/share/Trash");
    files.retain(|(p, _)| !p.starts_with(&trash));
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.truncate(LARGE_LIMIT);
    let rows: Vec<LargeItem> = files
        .iter()
        .map(|(p, size)| LargeItem {
            name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into(),
            dir: p.parent().map(|d| d.display().to_string()).unwrap_or_default().into(),
            size: human(*size).into(),
            age: age(p).into(),
            can_trash: p.starts_with(&home),
        })
        .collect();
    LARGE.with(|l| *l.borrow_mut() = files.into_iter().map(|(p, _)| p).collect());
    LARGE_MODEL.with(|m| m.set_vec(rows));
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// Scan `/` in the background; progress is read from shared counters by the UI timer.
fn start_scan(ui: &MainWindow) {
    let opts = ScanOptions::default();
    PROGRESS.with(|p| *p.borrow_mut() = Some(opts.progress.clone()));
    STATE.with(|s| s.borrow_mut().scanning = true);
    let started = Instant::now();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let res = scan(Path::new("/"), &opts);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            STATE.with(|s| s.borrow_mut().scanning = false);
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
                    let root = Arc::new(r.root);
                    update(&ui, |s| {
                        s.root = Some(root.clone());
                        s.path.clear();
                        s.selected = None;
                        s.hover = None;
                    });
                    refresh_large(&root);
                    start_detect(&ui, root);
                }
                Err(e) => ui.set_status(format!("Scan failed: {e}").into()),
            }
        });
    });
}

/// Look for reclaimable space (needs the finished scan for project artifacts).
fn start_detect(ui: &MainWindow, root: Arc<Node>) {
    ui.set_clean_total("Analysing…".into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let items = cleanup::detect(&home_dir(), Some((Path::new("/"), &root)));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            CLEAN.with(|c| {
                let mut c = c.borrow_mut();
                c.checked = vec![false; items.len()];
                c.items = items;
            });
            refresh_clean(&ui, true);
        });
    });
}

fn clean_row(it: &CleanupItem, checked: bool) -> CleanItem {
    let (risk, command) = (
        match it.risk {
            Risk::Safe => "safe",
            Risk::Review => "review",
            Risk::Careful => "careful",
        },
        match &it.method {
            Method::Command(c) => c.clone(),
            Method::Files => String::new(),
        },
    );
    CleanItem {
        title: it.title.clone().into(),
        desc: it.description.clone().into(),
        size: human(it.size).into(),
        risk: risk.into(),
        checked,
        admin: matches!(it.method, Method::Command(_)),
        command: command.into(),
    }
}

/// Push the cleanup state to the UI. `rebuild` replaces all rows (resets scrolling).
fn refresh_clean(ui: &MainWindow, rebuild: bool) {
    CLEAN.with(|c| {
        let c = c.borrow();
        if rebuild {
            let rows: Vec<CleanItem> = c.items.iter().zip(&c.checked).map(|(i, &k)| clean_row(i, k)).collect();
            CLEAN_MODEL.with(|m| m.set_vec(rows));
        }
        let total: u64 = c.items.iter().map(|i| i.size).sum();
        let direct: u64 = c.items.iter().filter(|i| matches!(i.method, Method::Files)).map(|i| i.size).sum();
        let (count, sel) = c
            .items
            .iter()
            .zip(&c.checked)
            .filter(|(_, &k)| k)
            .fold((0, 0u64), |(n, b), (i, _)| (n + 1, b + i.size));
        ui.set_clean_total(format!("{} can be reclaimed · {} without admin rights", human(total), human(direct)).into());
        ui.set_clean_count(count);
        ui.set_clean_selected(format!("{count} selected · {}", human(sel)).into());
        ui.set_clean_mode(if c.delete { "delete" } else { "trash" }.into());
        ui.set_clean_busy(c.busy);
    });
}

fn run_cleanup(ui: &MainWindow) {
    let (picked, delete) = CLEAN.with(|c| {
        let mut c = c.borrow_mut();
        if c.busy {
            return (Vec::new(), c.delete);
        }
        let picked: Vec<CleanupItem> = c
            .items
            .iter()
            .zip(&c.checked)
            .filter(|(_, &k)| k)
            .map(|(i, _)| i.clone())
            .collect();
        c.busy = !picked.is_empty();
        (picked, c.delete)
    });
    if picked.is_empty() {
        return;
    }
    ui.set_clean_msg("Cleaning…".into());
    refresh_clean(ui, false);
    let mode = if delete { Mode::Delete } else { Mode::Trash };
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let home = home_dir();
        let (mut freed, mut errors) = (0u64, Vec::new());
        for it in picked.iter().filter(|i| matches!(i.method, Method::Files)) {
            let out = cleanup::execute(&home, it, mode);
            freed += out.freed;
            errors.extend(out.errors);
        }
        // All admin tasks share one polkit password prompt.
        let admin: Vec<&CleanupItem> = picked.iter().filter(|i| matches!(i.method, Method::Command(_))).collect();
        if !admin.is_empty() {
            match cleanup::run_admin(&admin) {
                Ok(results) => {
                    for (id, ok) in results {
                        let item = admin.iter().find(|i| i.id == id);
                        if ok {
                            freed += item.map_or(0, |i| i.size);
                        } else {
                            errors.push(format!("{} failed", item.map_or(id.as_str(), |i| i.title.as_str())));
                        }
                    }
                }
                Err(e) => errors.push(format!("admin tasks: {e}")),
            }
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            CLEAN.with(|c| c.borrow_mut().busy = false);
            let verb = if mode == Mode::Delete { "Freed" } else { "Moved to Trash:" };
            let tail = if errors.is_empty() { String::new() } else { format!(" · {} items failed ({})", errors.len(), errors[0]) };
            ui.set_clean_msg(format!("{verb} {}{tail}", human(freed)).into());
            refresh_clean(&ui, false);
            start_scan(&ui); // refresh chart and candidates
        });
    });
}

fn main() -> Result<(), slint::PlatformError> {
    // Wayland app-id: lets GNOME match the window to orbit.desktop and its icon.
    slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select()?;
    slint::set_xdg_app_id("orbit")?;
    let ui = MainWindow::new()?;
    apply_theme(&ui, "auto");

    CLEAN_MODEL.with(|m| ui.set_clean_items(ModelRc::from(m.clone())));
    LARGE_MODEL.with(|m| ui.set_large_items(ModelRc::from(m.clone())));
    start_scan(&ui);

    // Live progress while scanning, plus follow-the-system theme changes (Auto).
    let tick = Timer::default();
    let weak = ui.as_weak();
    tick.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(ui) = weak.upgrade() else { return };
        if STATE.with(|s| s.borrow().scanning) {
            if let Some(p) = PROGRESS.with(|p| p.borrow().clone()) {
                ui.set_status(
                    format!(
                        "Scanning… {} files · {}",
                        p.files.load(Ordering::Relaxed),
                        human(p.bytes.load(Ordering::Relaxed))
                    )
                    .into(),
                );
            }
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

    ui.on_clean_toggle({
        let weak = ui.as_weak();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let i = i as usize;
            CLEAN.with(|c| {
                let mut c = c.borrow_mut();
                if let Some(k) = c.checked.get_mut(i) {
                    *k = !*k;
                    let row = clean_row(&c.items[i], c.checked[i]);
                    CLEAN_MODEL.with(|m| m.set_row_data(i, row));
                }
            });
            refresh_clean(&ui, false);
        }
    });

    ui.on_clean_set_mode({
        let weak = ui.as_weak();
        move |m| {
            let Some(ui) = weak.upgrade() else { return };
            CLEAN.with(|c| c.borrow_mut().delete = m == "delete");
            refresh_clean(&ui, false);
        }
    });

    ui.on_clean_select_safe({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            CLEAN.with(|c| {
                let mut c = c.borrow_mut();
                let flags: Vec<bool> =
                    c.items.iter().map(|i| i.risk == Risk::Safe).collect();
                c.checked = flags;
            });
            refresh_clean(&ui, true);
        }
    });

    ui.on_clean_run({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                run_cleanup(&ui);
            }
        }
    });

    ui.on_large_show(|i| {
        if let Some(p) = LARGE.with(|l| l.borrow().get(i as usize).cloned()) {
            if let Some(dir) = p.parent() {
                let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
            }
        }
    });

    ui.on_large_trash({
        let weak = ui.as_weak();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(p) = LARGE.with(|l| l.borrow().get(i as usize).cloned()) else { return };
            let item = CleanupItem {
                id: "large".into(),
                title: String::new(),
                description: String::new(),
                risk: Risk::Review,
                size: 0,
                paths: vec![p.clone()],
                method: Method::Files,
            };
            let out = cleanup::execute(&home_dir(), &item, Mode::Trash);
            if out.errors.is_empty() {
                ui.set_large_msg(format!("Moved {} to Trash", p.file_name().unwrap_or_default().to_string_lossy()).into());
                LARGE_MODEL.with(|m| m.remove(i as usize));
                LARGE.with(|l| {
                    l.borrow_mut().remove(i as usize);
                });
                start_scan(&ui); // refresh chart and totals (the list is rebuilt after the scan)
            } else {
                ui.set_large_msg(out.errors[0].clone().into());
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
