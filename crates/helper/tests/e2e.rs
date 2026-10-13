//! Runs the real helper binary (as the current user) and checks that what it
//! streams matches an in-process scan of the same tree.

use std::fs;
use std::process::{Command, Stdio};

use diskutil_core::tree_io::read_result;
use diskutil_core::{scan, ScanOptions};

#[test]
fn helper_output_matches_in_process_scan() {
    let dir = std::env::temp_dir().join(format!("orbit-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("a/b")).unwrap();
    fs::write(dir.join("a/one.bin"), vec![7u8; 3 << 20]).unwrap();
    fs::write(dir.join("a/b/two.bin"), vec![7u8; 2 << 20]).unwrap();
    fs::write(dir.join("small.txt"), "hi").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_orbit-scan-helper"))
        .arg(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let got = read_result(&mut out.stdout.as_slice()).unwrap();
    let want = scan(&dir, &ScanOptions::default()).unwrap();

    assert_eq!(got.root.size, want.root.size);
    assert_eq!(got.root.file_count, want.root.file_count);
    assert_eq!(got.root.children.len(), want.root.children.len());
    assert_eq!(got.errors, want.errors);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn relative_path_is_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_orbit-scan-helper")).arg("relative/path").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
