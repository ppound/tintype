//! Keeping XMP sidecars and the catalog in step.
//!
//! A sidecar that changed since Tintype last read or wrote it was edited by
//! something else (PhotoStat, another app, a copy from elsewhere): it wins,
//! and its rating, tags and people are imported. When writing is on, a
//! photo's sidecar is rewritten after every edit in Tintype, and created for
//! photos that have metadata but no sidecar yet. Photos are never touched.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;
use tintype_catalog::{Catalog, SidecarStamp};
use tintype_meta::xmp;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SidecarReport {
    /// Sidecars whose metadata was read into the catalog.
    pub imported: u64,
    /// Sidecars created or updated.
    pub written: u64,
    /// Sidecars that could not be read or written (invalid XMP, read-only folder).
    pub failed: u64,
    /// Photos whose metadata changed; the caller reindexes these.
    #[serde(skip)]
    pub changed_photos: Vec<i64>,
}

impl SidecarReport {
    fn add(&mut self, other: SidecarReport) {
        self.imported += other.imported;
        self.written += other.written;
        self.failed += other.failed;
        self.changed_photos.extend(other.changed_photos);
    }
}

/// The sidecar's current stamp, or `None` if it doesn't exist.
fn stamp(photo: &Path) -> Option<SidecarStamp> {
    let md = std::fs::metadata(xmp::sidecar_path(photo)).ok()?;
    let mtime_ns = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i64;
    Some(SidecarStamp { mtime_ns, size: md.len() })
}

/// How [`sync_sidecars`] treats sidecars nobody else changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Only read changed sidecars (writing is off).
    ReadOnly,
    /// Also create missing sidecars for photos with metadata (after a scan).
    Write,
    /// Also rewrite unchanged sidecars that disagree with the catalog (writing
    /// was just turned on, so edits made while it was off are caught up).
    CatchUp,
}

/// Brings sidecars of photos under `roots` in step with the catalog (see the
/// module docs).
pub fn sync_sidecars(
    catalog: &mut Catalog,
    roots: &[PathBuf],
    mode: SyncMode,
) -> Result<SidecarReport, tintype_catalog::Error> {
    let with_metadata = catalog.photos_with_user_metadata()?;
    let mut photos: Vec<(i64, String, Option<SidecarStamp>)> = catalog
        .sidecar_stamps()?
        .into_iter()
        .filter(|(_, (path, _))| roots.iter().any(|r| Path::new(path).starts_with(r)))
        .map(|(id, (path, recorded))| (id, path, recorded))
        .collect();
    photos.sort_by_key(|(id, ..)| *id);
    let mut report = SidecarReport::default();
    for (id, path, recorded) in photos {
        let photo = Path::new(&path);
        let current = stamp(photo);
        if current.is_some() && current != recorded {
            report.add(import(catalog, id, photo)?);
            continue;
        }
        if current.is_none() && recorded.is_some() {
            catalog.set_sidecar_stamp(id, None)?;
        }
        let stale = match (mode, current) {
            (SyncMode::ReadOnly, _) => false,
            (_, None) => with_metadata.contains(&id),
            (SyncMode::Write, Some(_)) => false,
            (SyncMode::CatchUp, Some(_)) => {
                let ours = catalog.sidecar_for(id)?;
                xmp::read_sidecar(photo).ok().flatten().is_none_or(|s| !same(&s, &ours))
            }
        };
        if stale {
            report.add(write_one(catalog, id, photo)?);
        }
    }
    Ok(report)
}

/// Imports the sidecars of `ids` that changed since Tintype last saw them,
/// so an edit about to be made in Tintype starts from them.
pub fn import_changed(catalog: &mut Catalog, ids: &[i64]) -> Result<SidecarReport, tintype_catalog::Error> {
    let stamps = catalog.sidecar_stamps()?;
    let mut report = SidecarReport::default();
    for id in ids {
        if let Some((path, recorded)) = stamps.get(id) {
            let current = stamp(Path::new(path));
            if current.is_some() && current != *recorded {
                report.add(import(catalog, *id, Path::new(path))?);
            }
        }
    }
    Ok(report)
}

/// Writes the sidecars of `ids` from the catalog.
pub fn write_sidecars(catalog: &mut Catalog, ids: &[i64]) -> Result<SidecarReport, tintype_catalog::Error> {
    let paths = catalog.paths(ids)?;
    let mut report = SidecarReport::default();
    for (id, path) in paths {
        report.add(write_one(catalog, id, Path::new(&path))?);
    }
    Ok(report)
}

/// Same rating, tags and people, in any order.
fn same(a: &xmp::Sidecar, b: &xmp::Sidecar) -> bool {
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v
    };
    a.rating == b.rating && sorted(&a.tags) == sorted(&b.tags) && sorted(&a.people) == sorted(&b.people)
}

fn import(catalog: &mut Catalog, id: i64, photo: &Path) -> Result<SidecarReport, tintype_catalog::Error> {
    let mut report = SidecarReport::default();
    let Some(stamp) = stamp(photo) else { return Ok(report) };
    match xmp::read_sidecar(photo) {
        Ok(Some(sidecar)) => {
            catalog.apply_sidecar(id, &sidecar, stamp)?;
            report.imported = 1;
            report.changed_photos.push(id);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(photo = %photo.display(), "unreadable sidecar: {e}");
            // Don't retry until the file changes again.
            catalog.set_sidecar_stamp(id, Some(stamp))?;
            report.failed = 1;
        }
    }
    Ok(report)
}

fn write_one(catalog: &mut Catalog, id: i64, photo: &Path) -> Result<SidecarReport, tintype_catalog::Error> {
    let mut report = SidecarReport::default();
    let sidecar = catalog.sidecar_for(id)?;
    // Nothing to say and no sidecar to correct: don't create an empty one.
    if sidecar.is_empty() && stamp(photo).is_none() {
        return Ok(report);
    }
    match xmp::write_sidecar(photo, &sidecar) {
        Ok(()) => {
            catalog.set_sidecar_stamp(id, stamp(photo))?;
            report.written = 1;
        }
        Err(e) => {
            tracing::warn!(photo = %photo.display(), "could not write sidecar: {e}");
            report.failed = 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tintype_testkit::JpegSpec;
    use xmp::Sidecar;

    fn scan(cat: &mut Catalog, root: &Path) {
        crate::scan(cat, &[root.to_path_buf()], &Default::default(), &AtomicBool::new(false), &mut |_| {}).unwrap();
    }

    fn sc(rating: Option<u8>, tags: &[&str], people: &[&str]) -> Sidecar {
        Sidecar {
            rating,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            people: people.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn import_edit_write_and_reimport() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let (a, b) = (root.join("a.jpg"), root.join("b.jpg"));
        JpegSpec::canon().write(&a);
        JpegSpec::nikon().write(&b);
        xmp::write_sidecar(&a, &sc(Some(4), &["Barn"], &["Grace"])).unwrap();
        let photo_bytes = std::fs::read(&a).unwrap();

        let mut cat = Catalog::open_in_memory().unwrap();
        scan(&mut cat, &root);
        let r = sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::Write).unwrap();
        assert_eq!((r.imported, r.written, r.failed), (1, 0, 0), "b has no metadata, so no sidecar");
        let ida = cat.get_by_path(&a.display().to_string()).unwrap().unwrap().id;
        let idb = cat.get_by_path(&b.display().to_string()).unwrap().unwrap().id;
        let rec = cat.get(ida).unwrap().unwrap();
        assert_eq!(
            (rec.rating, rec.tags.clone(), rec.people.clone()),
            (Some(4), vec!["Barn".into()], vec!["Grace".into()])
        );
        assert!(!xmp::sidecar_path(&b).exists());

        // Nothing changed: nothing to do.
        let r = sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::Write).unwrap();
        assert_eq!((r.imported, r.written), (0, 0));

        // Edits in Tintype are written out; b gets its first sidecar.
        cat.set_rating(ida, Some(2)).unwrap();
        cat.edit_tags(idb, &["new".into()], &[]).unwrap();
        let r = write_sidecars(&mut cat, &[ida, idb]).unwrap();
        assert_eq!(r.written, 2);
        assert_eq!(xmp::read_sidecar(&a).unwrap(), Some(sc(Some(2), &["Barn"], &["Grace"])));
        assert_eq!(xmp::read_sidecar(&b).unwrap(), Some(sc(None, &["new"], &[])));
        assert_eq!(
            sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::Write).unwrap().imported,
            0,
            "our own writes"
        );

        // Another app edits a's sidecar: the next sync imports it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        xmp::write_sidecar(&a, &sc(Some(5), &["Farm", "Barn"], &["Grace"])).unwrap();
        let r = import_changed(&mut cat, &[ida, idb]).unwrap();
        assert_eq!((r.imported, r.changed_photos.clone()), (1, vec![ida]));
        assert_eq!(cat.get(ida).unwrap().unwrap().rating, Some(5));

        assert_eq!(std::fs::read(&a).unwrap(), photo_bytes, "the photo itself is untouched");
    }

    #[test]
    fn writing_off_only_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        JpegSpec::canon().write(&root.join("a.jpg"));
        let mut cat = Catalog::open_in_memory().unwrap();
        scan(&mut cat, &root);
        let id = cat.get_by_path(&root.join("a.jpg").display().to_string()).unwrap().unwrap().id;
        cat.set_rating(id, Some(3)).unwrap();
        let r = sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::ReadOnly).unwrap();
        assert_eq!(r.written, 0);
        assert!(!xmp::sidecar_path(&root.join("a.jpg")).exists());
    }

    #[test]
    fn invalid_sidecar_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        JpegSpec::canon().write(&root.join("a.jpg"));
        std::fs::write(xmp::sidecar_path(&root.join("a.jpg")), "<x:xmpmeta").unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        scan(&mut cat, &root);
        assert_eq!(sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::Write).unwrap().failed, 1);
        assert_eq!(sync_sidecars(&mut cat, std::slice::from_ref(&root), SyncMode::Write).unwrap().failed, 0);
    }
}
