//! Core library: filesystem scanner and cleanup rule engine.

pub mod cleanup;
pub mod disks;
pub mod scan;
pub mod tree_io;

pub use scan::{real_mounts, scan, Node, NodeKind, Progress, ScanOptions, ScanResult};
