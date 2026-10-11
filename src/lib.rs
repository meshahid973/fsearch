//! Small, dependency-free file catalog for the first Windows milestone.
//!
//! The catalog is intentionally scoped to one directory tree. Persistent,
//! memory-mapped indexing and filesystem change notifications come later.

pub mod catalog;
pub use catalog::{Catalog, ScanSummary, DEFAULT_ENTRY_LIMIT, default_root};

#[cfg(windows)]
pub mod gui;
