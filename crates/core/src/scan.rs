//! Parallel, low-footprint disk usage scanner.
//!
//! Sizes are *allocated* sizes (`st_blocks * 512`), hard links are counted
//! once, and by default the scan stays on disk-backed filesystems only.
//! Small files are folded into their parent directory to keep memory low.
//!
//! Directory entries are read through `openat`/`fdopendir`/`fstatat` relative
//! to the parent directory fd, which avoids re-resolving long paths for every
//! entry (the dominant cost of a naive `lstat(path)` walk).

use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    pub kind: NodeKind,
    /// Total allocated bytes, including all descendants.
    pub size: u64,
    /// Number of files at or below this node.
    pub file_count: u64,
    /// Children kept individually (dirs, plus files above the size threshold).
    pub children: Vec<Node>,
}

/// Live counters, updated by workers and readable from any thread.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Do not cross into other filesystems (except `mounts`).
    pub one_file_system: bool,
    /// Files smaller than this are folded into their parent directory.
    pub min_file_size: u64,
    /// Worker threads; 0 = auto (half the cores, max 4).
    pub threads: usize,
    /// Absolute paths that are never entered.
    pub skip: Vec<PathBuf>,
    /// Mount points of disk-backed filesystems that are entered even when
    /// `one_file_system` is set, so `/boot`, `/boot/efi`, data partitions count.
    pub mounts: HashSet<PathBuf>,
    /// Shared progress counters (poll at a few Hz for a live UI).
    pub progress: Arc<Progress>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            one_file_system: true,
            min_file_size: 1 << 20,
            threads: 0,
            skip: ["/proc", "/sys", "/dev", "/run"].map(PathBuf::from).into(),
            mounts: real_mounts(),
            progress: Arc::default(),
        }
    }
}

/// Mount points of disk-backed filesystems, read from `/proc/self/mountinfo`.
/// Bind mounts (root != "/") and repeated devices are dropped so no data is
/// counted twice.
pub fn real_mounts() -> HashSet<PathBuf> {
    const REAL: &[&str] = &[
        "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "zfs",
        "jfs", "reiserfs", "hfsplus", "iso9660", "udf",
    ];
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return HashSet::new();
    };
    let mut devs = HashSet::new();
    text.lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let mut f = pre.split(' ');
            let dev = f.nth(2)?;
            let (root, mnt) = (f.next()?, f.next()?);
            let fstype = post.split(' ').next()?;
            (REAL.contains(&fstype) && root == "/" && devs.insert(dev.to_owned()))
                .then(|| PathBuf::from(unescape(mnt)))
        })
        .collect()
}

/// mountinfo escapes space, tab, newline and backslash as `\NNN` (octal).
pub(crate) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct ScanResult {
    pub root: Node,
    pub errors: u64,
    /// Up to `MAX_ERROR_PATHS` unreadable paths with their `errno`
    /// (EACCES = needs privileges, EMFILE = fd exhaustion).
    pub unreadable: Vec<(PathBuf, i32)>,
}

const MAX_ERROR_PATHS: usize = 200;

struct Ctx<'a> {
    opts: &'a ScanOptions,
    seen: Mutex<HashSet<(u64, u64)>>,
    errors: AtomicU64,
    unreadable: Mutex<Vec<(PathBuf, i32)>>,
}

impl Ctx<'_> {
    /// Record a failure; must be called right after the failing syscall.
    fn fail(&self, path: &Path) {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut v = self.unreadable.lock().unwrap();
        if v.len() < MAX_ERROR_PATHS {
            v.push((path.to_path_buf(), errno));
        }
    }
}

type Stat = libc::stat;

/// Parallel recursion keeps one fd per in-flight directory; lift the soft limit.
fn raise_nofile_limit() {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain getrlimit/setrlimit on a local struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 && rl.rlim_cur < rl.rlim_max {
            rl.rlim_cur = rl.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
}

fn fstatat(dirfd: libc::c_int, name: &CStr) -> Option<Stat> {
    let mut st = std::mem::MaybeUninit::<Stat>::uninit();
    // SAFETY: `name` is NUL-terminated; `st` is written on success.
    let r = unsafe { libc::fstatat(dirfd, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    // SAFETY: initialised when r == 0.
    (r == 0).then(|| unsafe { st.assume_init() })
}

fn is_dir(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn is_reg(st: &Stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub fn scan(path: &Path, opts: &ScanOptions) -> std::io::Result<ScanResult> {
    raise_nofile_limit();
    let threads = if opts.threads > 0 {
        opts.threads
    } else {
        (std::thread::available_parallelism().map_or(2, |n| n.get()) / 2).clamp(1, 4)
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(std::io::Error::other)?;
    let ctx = Ctx {
        opts,
        seen: Mutex::new(HashSet::new()),
        errors: AtomicU64::new(0),
        unreadable: Mutex::new(Vec::new()),
    };
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let st = fstatat(libc::AT_FDCWD, &cpath).ok_or_else(std::io::Error::last_os_error)?;
    let name: Box<str> = path
        .file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
        .into();
    let root = pool.install(|| scan_dir(libc::AT_FDCWD, &cpath, path.to_path_buf(), name, &st, &ctx));
    Ok(ScanResult {
        root,
        errors: ctx.errors.load(Ordering::Relaxed),
        unreadable: ctx.unreadable.into_inner().unwrap(),
    })
}

struct SubDir {
    cname: CString,
    path: PathBuf,
    name: Box<str>,
    st: Stat,
}

fn scan_dir(parent_fd: libc::c_int, cname: &CStr, path: PathBuf, name: Box<str>, st: &Stat, ctx: &Ctx) -> Node {
    let mut own = st.st_blocks as u64 * 512;
    let mut file_count = 0u64;
    let mut files: Vec<Node> = Vec::new();
    let mut subdirs: Vec<SubDir> = Vec::new();

    // SAFETY: plain syscall with a valid NUL-terminated name.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        ctx.fail(&path);
        return finish(name, own, file_count, files, Vec::new());
    }
    // SAFETY: fd is a valid directory fd; fdopendir takes ownership.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        ctx.fail(&path);
        unsafe { libc::close(fd) };
        return finish(name, own, file_count, files, Vec::new());
    }

    loop {
        // SAFETY: `dir` is a valid DIR*, used from this thread only.
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated inside the dirent.
        let ename = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = ename.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let Some(est) = fstatat(fd, ename) else {
            ctx.fail(&path.join(OsStr::from_bytes(bytes)));
            continue;
        };
        if is_dir(&est) {
            let p = path.join(OsStr::from_bytes(bytes));
            if ctx.opts.one_file_system && est.st_dev != st.st_dev && !ctx.opts.mounts.contains(&p) {
                continue;
            }
            if ctx.opts.skip.iter().any(|s| s == &p) {
                continue;
            }
            subdirs.push(SubDir {
                cname: ename.to_owned(),
                name: String::from_utf8_lossy(bytes).into(),
                path: p,
                st: est,
            });
        } else {
            // Count hard-linked inodes only once.
            if is_reg(&est) && est.st_nlink > 1 {
                let fresh = ctx.seen.lock().unwrap().insert((est.st_dev, est.st_ino));
                if !fresh {
                    continue;
                }
            }
            let size = est.st_blocks as u64 * 512;
            file_count += 1;
            ctx.opts.progress.files.fetch_add(1, Ordering::Relaxed);
            ctx.opts.progress.bytes.fetch_add(size, Ordering::Relaxed);
            if size >= ctx.opts.min_file_size {
                files.push(Node {
                    name: String::from_utf8_lossy(bytes).into(),
                    kind: NodeKind::File,
                    size,
                    file_count: 1,
                    children: Vec::new(),
                });
            } else {
                own += size;
            }
        }
    }

    // The parent fd stays open while children are scanned (they are opened
    // relative to it); the soft fd limit is raised to the hard limit above.
    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|s| scan_dir(fd, &s.cname, s.path, s.name, &s.st, ctx))
        .collect();
    unsafe { libc::closedir(dir) };
    finish(name, own, file_count, files, dirs)
}

fn finish(name: Box<str>, own: u64, mut file_count: u64, files: Vec<Node>, dirs: Vec<Node>) -> Node {
    let mut children = files;
    children.extend(dirs);
    children.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    children.shrink_to_fit();
    let mut size = own;
    for c in &children {
        size += c.size;
        if c.kind == NodeKind::Dir {
            file_count += c.file_count;
        }
    }
    Node { name, kind: NodeKind::Dir, size, file_count, children }
}
