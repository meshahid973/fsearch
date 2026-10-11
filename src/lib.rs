//! Native Windows file search.

pub mod catalog;
mod mapped;
pub mod walk;
pub use catalog::{Catalog, ScanSummary, DEFAULT_ENTRY_LIMIT, default_root, default_roots};

#[cfg(windows)]
pub mod gui;
