//! Long-running library work. Jobs report progress through a callback and stop
//! early when their cancel flag is set, so the same code serves the CLI and UI.

pub mod analyze;
pub mod faces;
pub mod quality;
pub mod scan;
pub mod sidecars;
pub mod stacks;

pub use analyze::{AnalyzeReport, analyze};
pub use faces::{FacesReport, face_crop_path, find_faces, recognize};
pub use quality::{QualityReport, measure_quality};
pub use scan::{ScanOptions, ScanProgress, ScanReport, normalize_path, scan};
pub use sidecars::{SidecarReport, SyncMode, import_changed, sync_sidecars, write_sidecars};
pub use stacks::{STACKS_VERSION, refresh_cover, restack};
