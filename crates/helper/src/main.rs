//! Read-only scan helper. Meant to be started through `pkexec` so it can read
//! root-only directories; the GUI itself never runs with elevated rights.
//!
//! Protocol: the scan tree goes to stdout (see `diskutil_core::tree_io`),
//! progress lines `P <files> <bytes>` go to stderr. The helper writes nothing
//! to disk and only accepts an absolute path (default `/`).

use std::io::{self, BufWriter};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use diskutil_core::tree_io::write_result;
use diskutil_core::{scan, ScanOptions};

fn lower_priority() {
    // SAFETY: plain syscalls; failures are harmless (we just run at normal priority).
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        // IOPRIO_WHO_PROCESS = 1, class IDLE = 3 (shifted by 13 bits).
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let path = Path::new(&arg);
    if !path.is_absolute() {
        eprintln!("orbit-scan-helper: path must be absolute");
        std::process::exit(2);
    }
    lower_priority();

    let opts = ScanOptions::default();
    let progress = opts.progress.clone();
    std::thread::spawn(move || loop {
        eprintln!("P {} {}", progress.files.load(Ordering::Relaxed), progress.bytes.load(Ordering::Relaxed));
        std::thread::sleep(Duration::from_millis(250));
    });

    match scan(path, &opts) {
        Ok(result) => {
            let mut out = BufWriter::new(io::stdout().lock());
            if let Err(e) = write_result(&mut out, &result) {
                eprintln!("orbit-scan-helper: write failed: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("orbit-scan-helper: scan failed: {e}");
            std::process::exit(1);
        }
    }
}
