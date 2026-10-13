use std::path::PathBuf;
use std::time::Instant;

use diskutil_core::{scan, Node, ScanOptions};

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn print(n: &Node, depth: usize, max_depth: usize, total: u64) {
    let pct = if total > 0 { n.size as f64 * 100.0 / total as f64 } else { 0.0 };
    println!("{:indent$}{:>10} {:5.1}%  {}", "", human(n.size), pct, n.name, indent = depth * 2);
    if depth < max_depth {
        for c in n.children.iter().take(8) {
            print(c, depth + 1, max_depth, total);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "/".into()));
    let depth = args.next().and_then(|d| d.parse().ok()).unwrap_or(2);
    let t = Instant::now();
    match scan(&path, &ScanOptions::default()) {
        Ok(r) => {
            print(&r.root, 0, depth, r.root.size);
            for (p, e) in r.unreadable.iter().take(200) {
                eprintln!("unreadable (errno {e}): {}", p.display());
            }
            eprintln!(
                "\n{} files, {} errors, {:.2}s",
                r.root.file_count,
                r.errors,
                t.elapsed().as_secs_f64()
            );
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
