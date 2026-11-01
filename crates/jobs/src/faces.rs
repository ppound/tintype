//! Face passes: find faces in photos not yet analyzed, then match them to
//! named people and group the rest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;
use tintype_catalog::{Catalog, NewFace, Suggestion};
use tintype_faces::{FACES_VERSION, Face, FaceAnalyzer};

use crate::Stop;

/// An unconfirmed face is suggested as a person when it matches their
/// confirmed faces this well (mean of its best `MATCH_K` similarities)...
/// Measured on a real library: above 0.6 suggestions were nearly all right,
/// 0.5-0.6 mixed in look-alikes. Faces below stay in unnamed groups.
const SUGGEST: f32 = 0.6;
/// ...and beats the runner-up by this much.
const MARGIN: f32 = 0.08;
const MATCH_K: usize = 3;
/// Unnamed faces are linked into groups at this pairwise similarity. SFace's
/// published same-person threshold is 0.363; stricter here because a wrong
/// merge is worse than a split (naming two groups the same merges them).
const GROUP: f32 = 0.5;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Catalog(#[from] tintype_catalog::Error),
    #[error("saving face crop: {0}")]
    Crop(String),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FacesReport {
    /// Photos analyzed in this pass.
    pub processed: u64,
    pub faces: u64,
    /// Photos that could not be decoded (recorded as having no faces).
    pub failed: u64,
    /// Photos still waiting for a face pass.
    pub remaining: u64,
    pub cancelled: bool,
    /// Stopped early because other work was waiting; carry on with another batch.
    pub paused: bool,
    pub elapsed_ms: u64,
    /// Photos whose faces changed; the caller reindexes these.
    #[serde(skip)]
    pub changed_photos: Vec<i64>,
}

/// Where the display crop of face `id` lives under `dir`.
pub fn face_crop_path(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("{:02x}", id & 0xff)).join(format!("{id}.jpg"))
}

/// Analyzes up to `limit` photos that need a face pass. `progress` gets (done, total).
pub fn find_faces(
    catalog: &mut Catalog,
    analyzer: &FaceAnalyzer,
    crops: &Path,
    limit: usize,
    stop: Stop,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<FacesReport, Error> {
    let started = Instant::now();
    let photos = catalog.photos_needing_faces(FACES_VERSION, limit)?;
    let total = photos.len() as u64;
    let mut report = FacesReport::default();

    // Workers decode and analyze; this thread writes each result as it arrives,
    // so a photo full of faces never holds up the rest.
    let (tx, rx) = std::sync::mpsc::channel::<(i64, Option<Vec<Face>>)>();
    let mut write_error = None;
    std::thread::scope(|scope| {
        let photos = &photos;
        scope.spawn(move || {
            crate::pool().install(|| {
                photos.par_iter().for_each_with(tx, |tx, (id, path)| {
                    if stop.requested() {
                        return;
                    }
                    let faces = crate::guarded(|| {
                        tintype_codecs::decode_preview(Path::new(path), tintype_faces::ANALYSIS_EDGE)
                            .ok()
                            .and_then(|img| analyzer.analyze(&img).ok())
                    });
                    let _ = tx.send((*id, faces));
                })
            });
        });
        for (photo, faces) in rx {
            if write_error.is_some() {
                continue; // drain so the workers can finish
            }
            if let Err(e) = store_faces(catalog, crops, photo, faces, &mut report) {
                write_error = Some(e);
                stop.cancel.store(true, Ordering::Relaxed);
            }
            progress(report.processed, total);
        }
    });
    if let Some(e) = write_error {
        return Err(e);
    }
    report.cancelled = stop.cancelled() && report.processed < total;
    report.paused = !report.cancelled && stop.paused() && report.processed < total;

    report.remaining = catalog.count_needing_faces(FACES_VERSION)?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Records one photo's faces (and crops) in the catalog.
fn store_faces(
    catalog: &mut Catalog,
    crops: &Path,
    photo: i64,
    faces: Option<Vec<Face>>,
    report: &mut FacesReport,
) -> Result<(), Error> {
    let faces = faces.unwrap_or_else(|| {
        report.failed += 1;
        Vec::new()
    });
    let new: Vec<NewFace> = faces
        .iter()
        .map(|f| NewFace { bbox: f.rel_bbox, score: f.detection.score, embedding: f.embedding.clone() })
        .collect();
    let (ids, removed) = catalog.replace_faces(photo, &new, FACES_VERSION)?;
    for id in &removed {
        let _ = std::fs::remove_file(face_crop_path(crops, *id));
    }
    for (id, face) in ids.iter().zip(&faces) {
        save_crop(&face_crop_path(crops, *id), &face.crop)?;
    }
    if !ids.is_empty() || !removed.is_empty() {
        report.changed_photos.push(photo);
    }
    report.faces += ids.len() as u64;
    report.processed += 1;
    Ok(())
}

fn save_crop(path: &Path, crop: &tintype_codecs::Rgba) -> Result<(), Error> {
    let err = |e: &dyn std::fmt::Display| Error::Crop(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(path.parent().expect("crop path has a parent")).map_err(|e| err(&e))?;
    let rgb: Vec<u8> = crop.pixels.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
    image::save_buffer_with_format(
        path,
        &rgb,
        crop.width,
        crop.height,
        image::ExtendedColorType::Rgb8,
        image::ImageFormat::Jpeg,
    )
    .map_err(|e| err(&e))
}

/// Suggests people for unconfirmed faces and groups the faces nobody matched.
/// Returns the photos whose people changed, for reindexing.
pub fn recognize(catalog: &mut Catalog) -> Result<Vec<i64>, Error> {
    let rows = catalog.face_rows()?;

    let mut people: HashMap<i64, Vec<&[f32]>> = HashMap::new();
    for r in rows.iter().filter(|r| r.confirmed) {
        if let Some(p) = r.person_id {
            people.entry(p).or_default().push(&r.embedding);
        }
    }

    // Suggestions.
    let suggested: HashMap<i64, Option<(i64, f32)>> = rows
        .par_iter()
        .filter(|r| !r.confirmed)
        .map(|r| {
            let mut scores: Vec<(i64, f32)> = people
                .iter()
                .filter(|(p, _)| Some(**p) != r.rejected_person_id)
                .map(|(p, known)| (*p, tintype_faces::match_score(&r.embedding, known, MATCH_K)))
                .collect();
            scores.sort_by(|a, b| b.1.total_cmp(&a.1));
            let best = match scores.as_slice() {
                [(p, s), rest @ ..] if *s >= SUGGEST && rest.first().is_none_or(|(_, s2)| s - s2 >= MARGIN) => {
                    Some((*p, *s))
                }
                _ => None,
            };
            (r.id, best)
        })
        .collect();

    // Groups of the faces left over.
    let mut loose: Vec<&tintype_catalog::FaceRow> =
        rows.iter().filter(|r| !r.confirmed && !r.ungrouped && suggested[&r.id].is_none()).collect();
    loose.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    let embeddings: Vec<&[f32]> = loose.iter().map(|r| r.embedding.as_slice()).collect();
    let labels = tintype_faces::cluster(&embeddings, GROUP);
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for &l in &labels {
        *sizes.entry(l).or_default() += 1;
    }
    let groups: HashMap<i64, i64> =
        loose.iter().zip(&labels).filter(|(_, l)| sizes[l] >= 2).map(|(r, l)| (r.id, *l as i64)).collect();

    let mut updates = Vec::new();
    let mut changed_photos = Vec::new();
    for r in rows.iter().filter(|r| !r.confirmed) {
        let (person, score) = suggested[&r.id].map_or((None, None), |(p, s)| (Some(p), Some(s)));
        let group = groups.get(&r.id).copied();
        let score_moved = match (score, r.suggest_score) {
            (Some(a), Some(b)) => (a - b).abs() > 0.01,
            (a, b) => a.is_some() != b.is_some(),
        };
        if person != r.person_id || group != r.group || score_moved {
            updates.push(Suggestion { face: r.id, person, group, score });
        }
        if person != r.person_id {
            changed_photos.push(r.photo_id);
        }
    }
    catalog.set_suggestions(&updates)?;
    changed_photos.sort_unstable();
    changed_photos.dedup();
    Ok(changed_photos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_catalog::NewPhoto;

    fn emb(v: &[f32]) -> Vec<f32> {
        let mut e = v.to_vec();
        tintype_faces::normalize(&mut e);
        e
    }

    /// One photo per face, embeddings given directly (no models needed).
    fn catalog_with_faces(embs: &[Vec<f32>]) -> (Catalog, Vec<i64>) {
        let mut cat = Catalog::open_in_memory().unwrap();
        let mut faces = Vec::new();
        for (i, e) in embs.iter().enumerate() {
            let p = NewPhoto {
                path: format!("/{i}.jpg"),
                size: 1,
                mtime_ns: 1,
                hash: None,
                meta: Default::default(),
                meta_version: 1,
                error: None,
            };
            let photo = cat.upsert_many(&[p]).unwrap()[0];
            let nf = NewFace { bbox: [0.1, 0.1, 0.2, 0.2], score: 0.9, embedding: e.clone() };
            faces.extend(cat.replace_faces(photo, &[nf], FACES_VERSION).unwrap().0);
        }
        (cat, faces)
    }

    #[test]
    fn groups_then_suggests_after_naming() {
        let a = [emb(&[1.0, 0.1, 0.0]), emb(&[1.0, -0.1, 0.05]), emb(&[0.95, 0.0, 0.1])];
        let b = [emb(&[0.0, 1.0, 0.1]), emb(&[0.1, 1.0, 0.0])];
        let loner = emb(&[0.0, 0.0, 1.0]);
        let all: Vec<Vec<f32>> = a.iter().chain(&b).chain([&loner]).cloned().collect();
        let (mut cat, ids) = catalog_with_faces(&all);

        recognize(&mut cat).unwrap();
        let groups = cat.face_groups(10).unwrap();
        assert_eq!(groups.iter().map(|g| g.size).collect::<Vec<_>>(), vec![3, 2], "loner is not a group");

        // Name one of A's faces: the other two become suggestions, not a group.
        cat.name_faces(&ids[..1], "Ann").unwrap();
        let changed = recognize(&mut cat).unwrap();
        assert_eq!(changed.len(), 2);
        let ann = cat.people().unwrap().into_iter().find(|p| p.name == "Ann").unwrap();
        assert_eq!((ann.faces, ann.suggested), (1, 2));
        assert_eq!(cat.face_groups(10).unwrap().len(), 1, "only B's group is left");

        // Rejecting a suggestion sticks.
        cat.reject_face(ids[1]).unwrap();
        recognize(&mut cat).unwrap();
        let row = cat.face_rows().unwrap().into_iter().find(|r| r.id == ids[1]).unwrap();
        assert_eq!(row.person_id, None);
    }

    #[test]
    fn crop_paths_spread_over_folders() {
        let dir = Path::new("/lib/faces");
        assert_eq!(face_crop_path(dir, 0x1234), dir.join("34").join("4660.jpg"));
    }
}
