//! Long-running library work. Jobs report progress through a callback and stop
//! early when their cancel flag is set, so the same code serves the CLI and UI.

pub mod faces;
pub mod scan;

pub use faces::{FacesReport, face_crop_path, find_faces, recognize};
pub use scan::{ScanOptions, ScanProgress, ScanReport, scan};
