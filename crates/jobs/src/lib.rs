//! Long-running library work. Jobs report progress through a callback and stop
//! early when asked ([`Stop`]), so the same code serves the CLI and UI.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Asks a batch over photos to end early: `cancel` when the user stops it,
/// `pause` when other work is waiting (the batch ends after the photos it has
/// started, and the next batch carries on).
#[derive(Clone, Copy)]
pub struct Stop<'a> {
    pub cancel: &'a AtomicBool,
    pub pause: &'a AtomicBool,
}

impl Stop<'_> {
    fn requested(&self) -> bool {
        self.cancelled() || self.paused()
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }
}

/// Threads for passes over photos: most cores, but not all, so the window and
/// its thumbnails stay responsive while a long pass runs.
pub(crate) fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads((cores - (cores / 4).max(1)).max(1))
            .thread_name(|i| format!("tintype-pass-{i}"))
            .build()
            .expect("a thread pool")
    })
}

/// Runs `f` on one photo, turning a panic (a decoder bug on one odd file)
/// into `None`, so one photo can't stop a pass over thousands.
pub(crate) fn guarded<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok().flatten()
}
