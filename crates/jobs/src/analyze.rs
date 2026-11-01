//! Analysis passes: embed photos not yet analyzed, for semantic search.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;
use tintype_analyze::{ANALYSIS_EDGE, ANALYSIS_VERSION, ImageEncoder};
use tintype_catalog::Catalog;

use crate::Stop;

#[derive(Debug, Clone, Default, Serialize)]
pub struct AnalyzeReport {
    /// Photos analyzed in this pass.
    pub processed: u64,
    /// Photos that could not be decoded (recorded as analyzed, without an embedding).
    pub failed: u64,
    /// Photos still waiting for analysis.
    pub remaining: u64,
    pub cancelled: bool,
    /// Stopped early because other work was waiting; carry on with another batch.
    pub paused: bool,
    pub elapsed_ms: u64,
}

/// Embeds up to `limit` photos that need analysis. `progress` gets (done, total).
pub fn analyze(
    catalog: &mut Catalog,
    encoder: &ImageEncoder,
    limit: usize,
    stop: Stop,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<AnalyzeReport, tintype_catalog::Error> {
    let started = Instant::now();
    let photos = catalog.photos_needing_analysis(ANALYSIS_VERSION, limit)?;
    let total = photos.len() as u64;
    let mut report = AnalyzeReport::default();

    // Workers decode and embed; this thread writes each result as it arrives.
    let (tx, rx) = std::sync::mpsc::channel::<(i64, Option<Vec<f32>>)>();
    let mut write_error = None;
    std::thread::scope(|scope| {
        let photos = &photos;
        scope.spawn(move || {
            crate::pool().install(|| {
                photos.par_iter().for_each_with(tx, |tx, (id, path)| {
                    if stop.requested() {
                        return;
                    }
                    let embedding = crate::guarded(|| {
                        tintype_codecs::decode_preview(Path::new(path), ANALYSIS_EDGE)
                            .ok()
                            .and_then(|img| encoder.embed(&img).ok())
                    });
                    let _ = tx.send((*id, embedding));
                })
            });
        });
        for (photo, embedding) in rx {
            if write_error.is_some() {
                continue; // drain so the workers can finish
            }
            report.failed += u64::from(embedding.is_none());
            match catalog.set_embedding(photo, embedding.as_deref(), ANALYSIS_VERSION) {
                Ok(()) => report.processed += 1,
                Err(e) => {
                    write_error = Some(e);
                    stop.cancel.store(true, Ordering::Relaxed);
                }
            }
            progress(report.processed, total);
        }
    });
    if let Some(e) = write_error {
        return Err(e);
    }
    report.cancelled = stop.cancelled() && report.processed < total;
    report.paused = !report.cancelled && stop.paused() && report.processed < total;
    report.remaining = catalog.count_needing_analysis(ANALYSIS_VERSION)?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}
