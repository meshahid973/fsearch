//! FSearch Windows filename index.

pub mod catalog;
mod mapped;
pub mod walk;
pub use catalog::{Catalog, ScanSummary, IndexProgress, ProgressSnapshot, DEFAULT_ENTRY_LIMIT, default_root, default_roots};
