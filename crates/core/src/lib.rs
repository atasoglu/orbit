//! Core library: filesystem scanner and (later) cleanup rule engine.

pub mod scan;

pub use scan::{real_mounts, scan, Node, NodeKind, Progress, ScanOptions, ScanResult};
