//! Mounted disks and their usage, via `/proc/self/mountinfo` + `statvfs`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::scan::unescape;

#[derive(Clone, Debug)]
pub struct Disk {
    pub device: String,
    pub mount: PathBuf,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

impl Disk {
    /// Used share in percent, the way `df` reports it (used / (used + available to users)).
    pub fn percent(&self) -> f64 {
        let denom = self.used + self.free;
        if denom == 0 { 0.0 } else { self.used as f64 * 100.0 / denom as f64 }
    }
}

const REAL: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "zfs", "jfs",
    "reiserfs", "hfsplus", "iso9660", "udf",
];

/// Disk-backed filesystems currently mounted. Bind mounts and repeated
/// devices (e.g. btrfs subvolumes) are listed once.
pub fn disks() -> Vec<Disk> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Disk> = text
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let mut f = pre.split(' ');
            let dev_id = f.nth(2)?;
            let (root, mnt) = (f.next()?, f.next()?);
            let mut p = post.split(' ');
            let fstype = p.next()?;
            let source = p.next()?;
            if !REAL.contains(&fstype) || root != "/" || !seen.insert(dev_id.to_owned()) {
                return None;
            }
            let mount = PathBuf::from(unescape(mnt));
            let (total, used, free) = usage(&mount)?;
            Some(Disk { device: unescape(source), mount, fstype: fstype.to_owned(), total, used, free })
        })
        .collect();
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
}

/// (total, used, available-to-unprivileged) bytes of the filesystem at `path`.
fn usage(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_frsize as u64;
    let total = s.f_blocks as u64 * bs;
    let used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * bs;
    Some((total, used, s.f_bavail as u64 * bs))
}
