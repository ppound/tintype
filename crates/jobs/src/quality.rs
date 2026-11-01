//! Quality passes: measure sharpness, and (when turned on) the LAION
//! aesthetic score, for photos that don't have them yet.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;
use tintype_analyze::{AESTHETIC_VERSION, AestheticScorer, QUALITY_EDGE, SHARPNESS_VERSION};
use tintype_catalog::Catalog;

use crate::Stop;

#[derive(Debug, Clone, Default, Serialize)]
pub struct QualityReport {
    pub processed: u64,
    /// Photos that could not be decoded.
    pub failed: u64,
    /// Photos still waiting for a quality pass.
    pub remaining: u64,
    pub cancelled: bool,
    /// Stopped early because other work was waiting; carry on with another batch.
    pub paused: bool,
    pub elapsed_ms: u64,
}

/// Measures up to `limit` photos. With no `scorer`, only sharpness is measured.
pub fn measure_quality(
    catalog: &mut Catalog,
    scorer: Option<&AestheticScorer>,
    limit: usize,
    stop: Stop,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<QualityReport, tintype_catalog::Error> {
    let started = Instant::now();
    let aesthetic_version = scorer.map(|_| AESTHETIC_VERSION);
    let todo = catalog.photos_needing_quality(SHARPNESS_VERSION, aesthetic_version, limit)?;
    let total = todo.len() as u64;
    let mut report = QualityReport::default();

    type Measured = (i64, Option<(Option<f32>, u32)>, Option<(Option<f32>, u32)>, bool);
    let (tx, rx) = std::sync::mpsc::channel::<Measured>();
    let mut write_error = None;
    std::thread::scope(|scope| {
        let todo = &todo;
        scope.spawn(move || {
            crate::pool().install(|| {
                todo.par_iter().for_each_with(tx, |tx, t| {
                    if stop.requested() {
                        return;
                    }
                    let measured = crate::guarded(|| {
                        let img = tintype_codecs::decode_preview(Path::new(&t.path), QUALITY_EDGE).ok();
                        let sharp = t.sharpness.then(|| {
                            (img.as_ref().and_then(|i| tintype_analyze::sharpness(i).ok()), SHARPNESS_VERSION)
                        });
                        let aes = match (t.aesthetic, scorer) {
                            (true, Some(s)) => Some((img.as_ref().and_then(|i| s.score(i).ok()), AESTHETIC_VERSION)),
                            _ => None,
                        };
                        Some((sharp, aes, img.is_none()))
                    });
                    // A photo that crashed the decoder is recorded as measured, without values.
                    let (sharp, aes, failed) = measured.unwrap_or_else(|| {
                        let aes = (t.aesthetic && scorer.is_some()).then_some((None, AESTHETIC_VERSION));
                        (t.sharpness.then_some((None, SHARPNESS_VERSION)), aes, true)
                    });
                    let _ = tx.send((t.id, sharp, aes, failed));
                })
            });
        });
        for (id, sharp, aes, failed) in rx {
            if write_error.is_some() {
                continue; // drain so the workers can finish
            }
            report.failed += u64::from(failed);
            match catalog.set_quality(id, sharp, aes) {
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
    report.remaining = catalog.count_needing_quality(SHARPNESS_VERSION, aesthetic_version)?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn sharpness_pass_without_aesthetic_models() {
        let dir = tempfile::tempdir().unwrap();
        // A real, decodable photo (JpegSpec files carry metadata only).
        image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([(x * 4) as u8, (y * 5) as u8, 128]))
            .save(dir.path().join("a.jpg"))
            .unwrap();
        std::fs::write(dir.path().join("broken.jpg"), b"not a jpeg").unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        crate::scan(&mut cat, &[dir.path().into()], &Default::default(), &AtomicBool::new(false), &mut |_| {}).unwrap();
        let (cancel, pause) = (AtomicBool::new(false), AtomicBool::new(false));
        let stop = Stop { cancel: &cancel, pause: &pause };
        let r = measure_quality(&mut cat, None, 10, stop, &mut |_, _| {}).unwrap();
        assert_eq!((r.processed, r.failed, r.remaining), (2, 1, 0));
        let again = measure_quality(&mut cat, None, 10, stop, &mut |_, _| {}).unwrap();
        assert_eq!(again.processed, 0, "nothing left to measure");
    }

    #[test]
    fn a_waiting_command_pauses_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        image::RgbImage::new(8, 8).save(dir.path().join("a.jpg")).unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        crate::scan(&mut cat, &[dir.path().into()], &Default::default(), &AtomicBool::new(false), &mut |_| {}).unwrap();
        let (cancel, pause) = (AtomicBool::new(false), AtomicBool::new(true));
        let r = measure_quality(&mut cat, None, 10, Stop { cancel: &cancel, pause: &pause }, &mut |_, _| {}).unwrap();
        assert_eq!((r.processed, r.paused, r.cancelled, r.remaining), (0, true, false, 1));
    }
}
