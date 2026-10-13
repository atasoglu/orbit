//! Cleanup engine: finds space that can be reclaimed safely and removes it.
//!
//! Nothing here is applied automatically. [`detect`] only *describes*
//! candidates; [`execute`] acts on one candidate the caller explicitly picked.
//! Items under the user's home are handled directly; system-wide items
//! (apt cache, journal, old snaps/kernels) need admin rights and are
//! reported with the exact command to run instead.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::scan::{scan, Node, NodeKind, ScanOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Risk {
    /// Regenerated automatically; removal has no visible effect besides a slower first use.
    Safe,
    /// Re-creatable with some effort (re-download / rebuild); worth a glance first.
    Review,
    /// Can affect the system or lose data that is hard to get back.
    Careful,
}

#[derive(Clone, Debug)]
pub enum Method {
    /// Paths the current user may remove directly.
    Files,
    /// Needs admin rights. The command is fixed by the rule table (never user
    /// input), shown to the user, and run through polkit by [`run_admin`].
    Command(String),
}

#[derive(Clone, Debug)]
pub struct CleanupItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub risk: Risk,
    pub size: u64,
    pub paths: Vec<PathBuf>,
    pub method: Method,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Move to the freedesktop trash (recoverable; frees space only once emptied).
    Trash,
    /// Remove for good (frees space immediately).
    Delete,
}

struct UserRule {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    risk: Risk,
    /// Paths relative to the home directory.
    paths: &'static [&'static str],
}

const USER_RULES: &[UserRule] = &[
    UserRule { id: "trash", title: "Trash", description: "Files you already deleted.", risk: Risk::Safe, paths: &[".local/share/Trash/files", ".local/share/Trash/info"] },
    UserRule { id: "thumbnails", title: "Thumbnails", description: "Preview images; rebuilt when folders are opened again.", risk: Risk::Safe, paths: &[".cache/thumbnails"] },
    UserRule { id: "browser-cache", title: "Browser caches", description: "Cached web content for Chrome, Firefox, Chromium, Brave and Edge.", risk: Risk::Safe, paths: &[".cache/google-chrome", ".cache/chromium", ".cache/mozilla", ".cache/BraveSoftware", ".cache/microsoft-edge"] },
    UserRule { id: "npm-cache", title: "npm cache", description: "Downloaded packages; re-fetched on the next install.", risk: Risk::Safe, paths: &[".npm/_cacache"] },
    UserRule { id: "pip-cache", title: "pip cache", description: "Downloaded Python wheels and sdists.", risk: Risk::Safe, paths: &[".cache/pip"] },
    UserRule { id: "uv-cache", title: "uv cache", description: "Downloaded and built Python packages.", risk: Risk::Safe, paths: &[".cache/uv"] },
    UserRule { id: "yarn-cache", title: "Yarn cache", description: "Downloaded JavaScript packages.", risk: Risk::Safe, paths: &[".cache/yarn"] },
    UserRule { id: "go-build", title: "Go build cache", description: "Compiled Go packages; rebuilt on demand.", risk: Risk::Safe, paths: &[".cache/go-build"] },
    UserRule { id: "cargo-cache", title: "Cargo registry cache", description: "Downloaded crates and sources; re-fetched when building.", risk: Risk::Safe, paths: &[".cargo/registry/cache", ".cargo/registry/src"] },
    UserRule { id: "huggingface", title: "Hugging Face models", description: "Downloaded models and datasets. Re-downloading can be slow or large.", risk: Risk::Review, paths: &[".cache/huggingface"] },
    UserRule { id: "flatpak-cache", title: "Flatpak app caches", description: "Per-app caches of Flatpak applications.", risk: Risk::Review, paths: &[".var/app/*/cache"] },
];

/// Cache folders under `~/.cache` already covered by a specific rule.
const COVERED_CACHES: &[&str] = &[
    "thumbnails", "google-chrome", "chromium", "mozilla", "BraveSoftware", "microsoft-edge",
    "pip", "uv", "yarn", "go-build", "huggingface",
];

const MIN_ITEM_SIZE: u64 = 1 << 20;
const MIN_GENERIC_CACHE: u64 = 50 << 20;
const MIN_DEV_ARTIFACT: u64 = 20 << 20;
const MAX_DEV_ARTIFACTS: usize = 60;

/// Allocated size of a file or directory tree.
pub fn path_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.blocks() * 512;
    }
    let opts = ScanOptions { one_file_system: true, min_file_size: u64::MAX, threads: 2, ..ScanOptions::default() };
    scan(path, &opts).map_or(0, |r| r.root.size)
}

fn expand(home: &Path, rel: &str) -> Vec<PathBuf> {
    match rel.split_once("/*/") {
        Some((pre, post)) => fs::read_dir(home.join(pre))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join(post))
            .filter(|p| p.exists())
            .collect(),
        None => {
            let p = home.join(rel);
            if p.exists() { vec![p] } else { Vec::new() }
        }
    }
}

/// Describe everything that could be reclaimed. `tree` is a finished scan
/// together with the path it started from; it enables detection of project
/// build artifacts (pass `None` to skip those).
pub fn detect(home: &Path, tree: Option<(&Path, &Node)>) -> Vec<CleanupItem> {
    let mut items = Vec::new();

    for r in USER_RULES {
        let paths: Vec<PathBuf> = r.paths.iter().flat_map(|p| expand(home, p)).collect();
        let size: u64 = paths.iter().map(|p| path_size(p)).sum();
        if size >= MIN_ITEM_SIZE {
            items.push(CleanupItem {
                id: r.id.into(),
                title: r.title.into(),
                description: r.description.into(),
                risk: r.risk,
                size,
                paths,
                method: Method::Files,
            });
        }
    }

    // Remaining app caches under ~/.cache, one item per application.
    if let Ok(rd) = fs::read_dir(home.join(".cache")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if COVERED_CACHES.contains(&name.as_str()) || !p.is_dir() {
                continue;
            }
            let size = path_size(&p);
            if size >= MIN_GENERIC_CACHE {
                items.push(CleanupItem {
                    id: format!("cache:{name}"),
                    title: format!("{name} cache"),
                    description: format!("Cache of {name} in ~/.cache. Applications recreate it as needed."),
                    risk: if ["runtime", "model", "torch", "checkpoint"].iter().any(|k| name.to_lowercase().contains(k)) {
                        Risk::Review
                    } else {
                        Risk::Safe
                    },
                    size,
                    paths: vec![p],
                    method: Method::Files,
                });
            }
        }
    }

    if let Some((base, root)) = tree {
        items.extend(dev_artifacts(home, base, root));
    }
    items.extend(system_items());
    items.sort_by(|a, b| b.size.cmp(&a.size));
    items
}

/// `node_modules`, Rust `target` and Python `.venv` folders inside the home dir.
fn dev_artifacts(home: &Path, base: &Path, root: &Node) -> Vec<CleanupItem> {
    let mut found = Vec::new();
    walk(&mut base.to_path_buf(), root, home, &mut found);
    found.sort_by(|a: &CleanupItem, b| b.size.cmp(&a.size));
    found.truncate(MAX_DEV_ARTIFACTS);
    found
}

fn walk(path: &mut PathBuf, node: &Node, home: &Path, out: &mut Vec<CleanupItem>) {
    for c in node.children.iter().filter(|c| c.kind == NodeKind::Dir) {
        path.push(&*c.name);
        // Hidden folders directly under home (~/.vscode, ~/.nvm, ~/.config, ...)
        // belong to tools, not projects; deleting their dependencies breaks them.
        if path.parent() == Some(home) && c.name.starts_with('.') {
            path.pop();
            continue;
        }
        if path.starts_with(home) && c.size >= MIN_DEV_ARTIFACT {
            if let Some((what, why)) = artifact_kind(path, &c.name) {
                let project = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                out.push(CleanupItem {
                    id: format!("dev:{}", path.display()),
                    title: format!("{what} — {}", project.strip_prefix(&*home.display().to_string()).map_or(project.clone(), |s| format!("~{s}"))),
                    description: why.into(),
                    risk: Risk::Review,
                    size: c.size,
                    paths: vec![path.clone()],
                    method: Method::Files,
                });
                path.pop();
                continue; // do not look for nested artifacts
            }
        }
        walk(path, c, home, out);
        path.pop();
    }
}

fn artifact_kind(path: &Path, name: &str) -> Option<(&'static str, &'static str)> {
    let parent = path.parent()?;
    match name {
        "node_modules" => Some(("node_modules", "Installed JavaScript dependencies; restored by `npm install`.")),
        "target" if parent.join("Cargo.toml").exists() => Some(("Rust target", "Build output; rebuilt by `cargo build`.")),
        ".venv" | "venv" if path.join("pyvenv.cfg").exists() => Some(("Python venv", "Virtual environment; recreate with `uv sync` / `pip install`.")),
        _ => None,
    }
}

/// System-wide candidates. Only measured here; removal needs admin rights.
fn system_items() -> Vec<CleanupItem> {
    let mut v = Vec::new();
    let mut push = |id: &str, title: &str, desc: &str, risk, size: u64, paths: Vec<PathBuf>, cmd: &str| {
        if size >= MIN_ITEM_SIZE {
            v.push(CleanupItem {
                id: id.into(),
                title: title.into(),
                description: desc.into(),
                risk,
                size,
                paths,
                method: Method::Command(cmd.into()),
            });
        }
    };

    let apt = PathBuf::from("/var/cache/apt/archives");
    push("apt-cache", "APT package cache", "Downloaded .deb files of installed updates.", Risk::Safe, path_size(&apt), vec![apt], "sudo apt-get clean");

    let journal = PathBuf::from("/var/log/journal");
    let js = path_size(&journal);
    push("journal", "System journal logs", "Old logs; keeps the most recent 200 MB.", Risk::Safe, js.saturating_sub(200 << 20), vec![journal], "sudo journalctl --vacuum-size=200M");

    let crash = PathBuf::from("/var/crash");
    push("crash", "Crash reports", "Reports of past application crashes.", Risk::Safe, path_size(&crash), vec![crash], "sudo rm -f /var/crash/*");

    let (snap_size, snap_paths) = old_snaps();
    push(
        "snap-old",
        "Old Snap revisions",
        "Disabled previous versions of installed snaps.",
        Risk::Review,
        snap_size,
        snap_paths,
        "snap list --all | awk '/disabled/{print $1, $3}' | while read n r; do sudo snap remove \"$n\" --revision=\"$r\"; done",
    );

    let (k_size, k_paths, k_versions) = old_kernels();
    push(
        "kernels-old",
        "Old Linux kernels",
        "Kernels older than the running one and its newest fallback; only their own packages are purged.",
        Risk::Careful,
        k_size,
        k_paths,
        &purge_kernels_command(&k_versions),
    );
    v
}

fn old_snaps() -> (u64, Vec<PathBuf>) {
    let Ok(out) = Command::new("snap").args(["list", "--all"]).output() else { return (0, Vec::new()) };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut total = 0;
    let mut paths = Vec::new();
    for l in text.lines().filter(|l| l.contains("disabled")) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if let (Some(name), Some(rev)) = (f.first(), f.get(2)) {
            let p = PathBuf::from(format!("/var/lib/snapd/snaps/{name}_{rev}.snap"));
            total += path_size(&p);
            paths.push(p);
        }
    }
    (total, paths)
}

/// Compare dotted/dashed version strings numerically (`6.8.0-45` < `6.8.0-100`).
fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect()
}

/// Kernels that can go: everything except the running one and the newest other
/// kernel (kept as a fallback). Returns (bytes, paths, versions).
fn old_kernels() -> (u64, Vec<PathBuf>, Vec<String>) {
    let running = Command::new("uname").arg("-r").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut versions: Vec<String> = fs::read_dir("/boot")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("vmlinuz-").map(str::to_owned))
        .filter(|v| *v != running)
        .collect();
    versions.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
    // The newest remaining kernel stays as the fallback.
    let removable: Vec<String> = versions.into_iter().skip(1).collect();
    let (mut total, mut paths) = (0, Vec::new());
    for ver in &removable {
        for p in [PathBuf::from(format!("/boot/vmlinuz-{ver}")), PathBuf::from(format!("/boot/initrd.img-{ver}")), PathBuf::from(format!("/lib/modules/{ver}"))] {
            total += path_size(&p);
            paths.push(p);
        }
    }
    (total, paths, removable)
}

/// Purge exactly the packages of the given kernel versions (only those installed).
/// Versions come from `/boot/vmlinuz-*` file names, so they are plain `[0-9a-z.-]`.
fn purge_kernels_command(versions: &[String]) -> String {
    let vers: Vec<&String> = versions.iter().filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).collect();
    format!(
        "sudo sh -c 'pkgs=\"\"; for v in {}; do for p in \"linux-image-$v\" \"linux-modules-$v\" \"linux-modules-extra-$v\" \"linux-headers-$v\"; do dpkg -s \"$p\" >/dev/null 2>&1 && pkgs=\"$pkgs $p\"; done; done; [ -n \"$pkgs\" ] && apt-get -y purge $pkgs'",
        vers.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(" ")
    )
}

/// Strip `sudo`: [`run_admin`] already runs the script as root via polkit.
pub fn admin_script(cmd: &str) -> String {
    cmd.replace("sudo ", "")
}

/// Run the fixed admin commands of the given items in one `pkexec` call, so the
/// user sees a single system password prompt. Every command runs in its own
/// subshell and reports its exit code, so one failure cannot hide behind a later
/// success. Returns `(item id, succeeded)` for each admin item.
///
/// `Err` means nothing ran: the prompt was dismissed (126) or authorisation failed (127).
pub fn run_admin(items: &[&CleanupItem]) -> io::Result<Vec<(String, bool)>> {
    use std::process::Stdio;
    let admin: Vec<(&CleanupItem, &String)> = items
        .iter()
        .filter_map(|i| match &i.method {
            Method::Command(c) => Some((*i, c)),
            Method::Files => None,
        })
        .collect();
    if admin.is_empty() {
        return Ok(Vec::new());
    }
    let mut script = String::new();
    for (n, (_, cmd)) in admin.iter().enumerate() {
        script.push_str(&format!("( {} ) </dev/null; echo \"ORBIT_RC {n} $?\"\n", admin_script(cmd)));
    }
    let out = Command::new("pkexec").args(["sh", "-c", &script]).stdin(Stdio::null()).output()?;
    match out.status.code() {
        Some(126) | Some(127) => {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "authentication was cancelled or denied"))
        }
        _ => {}
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let codes: Vec<(usize, i32)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.strip_prefix("ORBIT_RC ")?.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect();
    if codes.is_empty() {
        return Err(io::Error::other(format!("admin tasks did not run ({})", out.status)));
    }
    Ok(admin
        .iter()
        .enumerate()
        .map(|(n, (item, _))| (item.id.clone(), codes.iter().any(|&(i, rc)| i == n && rc == 0)))
        .collect())
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub freed: u64,
    pub errors: Vec<String>,
}

/// Apply one `Method::Files` item. Refuses anything outside `home`.
pub fn execute(home: &Path, item: &CleanupItem, mode: Mode) -> Outcome {
    let mut out = Outcome::default();
    if !matches!(item.method, Method::Files) {
        out.errors.push("admin items are run with run_admin, not execute".into());
        return out;
    }
    for p in &item.paths {
        if let Err(e) = check_target(home, p) {
            out.errors.push(format!("{}: {e}", p.display()));
            continue;
        }
        let before = path_size(p);
        let res = match mode {
            Mode::Trash => trash(home, p),
            Mode::Delete => delete_contents(p),
        };
        match res {
            Ok(()) => out.freed += if mode == Mode::Delete { before.saturating_sub(path_size(p)) } else { 0 },
            Err(e) => out.errors.push(format!("{}: {e}", p.display())),
        }
        // Trashing moves data rather than freeing it; report what was moved.
        if mode == Mode::Trash && !p.exists() {
            out.freed += before;
        }
    }
    out
}

/// The target must be strictly inside `home` and deeper than the home folder itself.
fn check_target(home: &Path, p: &Path) -> io::Result<()> {
    let ok = p.is_absolute()
        && p.starts_with(home)
        && p != home
        && !p.components().any(|c| matches!(c, std::path::Component::ParentDir));
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "outside the home directory"))
    }
}

/// Remove a file, or everything inside a directory (the directory itself stays).
fn delete_contents(p: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if !meta.is_dir() {
        return fs::remove_file(p);
    }
    let mut first_err = None;
    for e in fs::read_dir(p)? {
        let e = e?;
        let path = e.path();
        // `remove_dir_all` does not follow symlinks.
        let r = if e.file_type()?.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        if let Err(err) = r {
            first_err.get_or_insert(err);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Move `p` into `~/.local/share/Trash` following the freedesktop.org spec.
fn trash(home: &Path, p: &Path) -> io::Result<()> {
    let base = home.join(".local/share/Trash");
    // Never trash the trash itself.
    if p.starts_with(&base) {
        return delete_contents(p);
    }
    fs::create_dir_all(base.join("files"))?;
    fs::create_dir_all(base.join("info"))?;
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into());
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let unique = format!("{name}.{stamp}");
    let info = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode(p), now_iso());
    fs::write(base.join("info").join(format!("{unique}.trashinfo")), info)?;
    if let Err(e) = fs::rename(p, base.join("files").join(&unique)) {
        let _ = fs::remove_file(base.join("info").join(format!("{unique}.trashinfo")));
        return Err(e);
    }
    Ok(())
}

fn encode(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn now_iso() -> String {
    // Local time via libc to avoid a date dependency.
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orbit-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn item(paths: Vec<PathBuf>) -> CleanupItem {
        CleanupItem { id: "t".into(), title: "t".into(), description: String::new(), risk: Risk::Safe, size: 0, paths, method: Method::Files }
    }

    #[test]
    fn delete_keeps_dir_removes_contents() {
        let home = tmp("del");
        let cache = home.join(".cache/app");
        fs::create_dir_all(cache.join("sub")).unwrap();
        fs::write(cache.join("sub/a.bin"), vec![0u8; 8192]).unwrap();
        fs::write(cache.join("b.bin"), vec![0u8; 8192]).unwrap();
        let out = execute(&home, &item(vec![cache.clone()]), Mode::Delete);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(cache.is_dir());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
        assert!(out.freed > 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn trash_moves_and_writes_info() {
        let home = tmp("trash");
        let proj = home.join("proj/node_modules");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("x.js"), "x").unwrap();
        let out = execute(&home, &item(vec![proj.clone()]), Mode::Trash);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(!proj.exists());
        let info: Vec<_> = fs::read_dir(home.join(".local/share/Trash/info")).unwrap().collect();
        assert_eq!(info.len(), 1);
        let text = fs::read_to_string(info[0].as_ref().unwrap().path()).unwrap();
        assert!(text.contains("Path=") && text.contains("node_modules"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn refuses_outside_home() {
        let home = tmp("guard");
        let other = tmp("guard-other");
        fs::write(other.join("keep"), "x").unwrap();
        for target in [other.clone(), home.clone(), home.join("../x"), PathBuf::from("/etc")] {
            let out = execute(&home, &item(vec![target]), Mode::Delete);
            assert!(!out.errors.is_empty());
        }
        assert!(other.join("keep").exists());
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn admin_script_drops_sudo() {
        assert_eq!(admin_script("sudo apt clean"), "apt clean");
        assert_eq!(
            admin_script("snap list --all | while read n r; do sudo snap remove \"$n\"; done"),
            "snap list --all | while read n r; do snap remove \"$n\"; done"
        );
    }

    #[test]
    fn kernel_versions_sort_numerically() {
        let mut v = vec!["6.8.0-9-generic", "6.8.0-100-generic", "6.8.0-45-generic"];
        v.sort_by_key(|x| std::cmp::Reverse(version_key(x)));
        assert_eq!(v, ["6.8.0-100-generic", "6.8.0-45-generic", "6.8.0-9-generic"]);
    }

    #[test]
    fn kernel_command_only_names_given_versions() {
        let c = purge_kernels_command(&["6.8.0-45-generic".into(), "evil; rm -rf /".into()]);
        assert!(c.contains("6.8.0-45-generic") && !c.contains("rm -rf"));
    }

    #[test]
    fn command_items_are_never_executed() {
        let home = tmp("cmd");
        let mut it = item(vec![home.join("x")]);
        it.method = Method::Command("sudo apt clean".into());
        assert!(!execute(&home, &it, Mode::Delete).errors.is_empty());
        fs::remove_dir_all(home).unwrap();
    }
}
