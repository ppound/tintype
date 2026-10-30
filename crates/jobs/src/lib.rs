//! Long-running library work. Jobs report progress through a callback and stop
//! early when their cancel flag is set, so the same code serves the CLI and UI.

pub mod scan;

pub use scan::{ScanOptions, ScanProgress, ScanReport, scan};
