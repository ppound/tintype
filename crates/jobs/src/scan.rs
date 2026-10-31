//! Incremental folder scan: find photos, read metadata for new or changed
//! files, and drop catalog entries for files that no longer exist.
//!
//! A new file with the same content as a file that is gone (same hash, or
//! same name, size and modification time when hashing is off) is the same
//! photo moved or renamed: its catalog entry follows it, keeping ratings,
//! tags, faces and analysis.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Serialize;
use tintype_catalog::{Catalog, NewPhoto};
use walkdir::WalkDir;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: not a folder")]
    NotADirectory(PathBuf),
    #[error("{path}: {source}")]
    Root { path: PathBuf, source: std::io::Error },
    #[error(transparent)]
    Catalog(#[from] tintype_catalog::Error),
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Hash file contents (BLAKE3) for exact-duplicate detection. Costs a full read of each new file.
    pub hash: bool,
    /// Photos analyzed in parallel and written per catalog transaction.
    pub batch_size: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { hash: true, batch_size: 256 }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ScanProgress {
    /// Still walking folders; `found` photos so far.
    Walking { found: u64 },
    /// Reading metadata for new or changed photos.
    Analyzing { done: u64, total: u64 },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanReport {
    pub roots: Vec<String>,
    pub found: u64,
    pub unchanged: u64,
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    /// Photos found at a new path (moved or renamed); not counted as added or removed.
    pub moved: u64,
    /// Photos cataloged without metadata because it could not be read.
    pub unreadable: u64,
    /// Folders or files the walk could not access, and non-UTF-8 paths.
    pub skipped: u64,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    /// Filled in by the caller after the scan.
    pub sidecars: crate::SidecarReport,
    /// Catalog ids that were added or updated; the caller reindexes these.
    #[serde(skip)]
    pub changed_ids: Vec<i64>,
    #[serde(skip)]
    pub removed_ids: Vec<i64>,
    /// Faces deleted along with removed photos; the caller deletes their crops.
    #[serde(skip)]
    pub removed_face_ids: Vec<i64>,
}

struct Candidate {
    path: String,
    size: u64,
    mtime_ns: i64,
    /// Known file, only cataloged by an older extractor: the catalog keeps its hash.
    content_unchanged: bool,
    /// Not in the catalog: a new file, or one that moved here.
    new: bool,
}

/// How a moved file is recognized.
#[derive(Hash, PartialEq, Eq)]
enum MoveKey {
    Hash([u8; 32]),
    NameSizeTime(String, u64, i64),
}

fn name_key(path: &str, size: u64, mtime_ns: i64) -> MoveKey {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    MoveKey::NameSizeTime(name, size, mtime_ns)
}

/// Scans `roots` into `catalog`.
pub fn scan(
    catalog: &mut Catalog,
    roots: &[PathBuf],
    opts: &ScanOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<ScanReport, Error> {
    let started = Instant::now();
    let roots = roots
        .iter()
        .map(|r| {
            let canonical = dunce::canonicalize(r).map_err(|source| Error::Root { path: r.clone(), source })?;
            if canonical.is_dir() { Ok(canonical) } else { Err(Error::NotADirectory(r.clone())) }
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut report =
        ScanReport { roots: roots.iter().map(|r| r.display().to_string()).collect(), ..ScanReport::default() };
    let known = catalog.fingerprints()?;
    let mut seen = HashSet::new();
    let mut changed = Vec::new();

    for root in &roots {
        for entry in WalkDir::new(root).into_iter().filter_entry(|e| !is_ignored(e)) {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    report.skipped += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() || !tintype_meta::is_photo(entry.path()) {
                continue;
            }
            let (Some(path), Ok(md)) = (entry.path().to_str(), entry.metadata()) else {
                report.skipped += 1;
                continue;
            };
            let mtime_ns =
                md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64);
            let mut candidate =
                Candidate { path: path.to_string(), size: md.len(), mtime_ns, content_unchanged: false, new: false };

            report.found += 1;
            if report.found.is_multiple_of(1000) {
                progress(ScanProgress::Walking { found: report.found });
            }
            match known.get(&candidate.path) {
                Some(fp)
                    if fp.size == candidate.size
                        && fp.mtime_ns == candidate.mtime_ns
                        && fp.meta_version >= tintype_meta::EXTRACTOR_VERSION =>
                {
                    report.unchanged += 1
                }
                Some(fp) => {
                    candidate.content_unchanged = fp.size == candidate.size && fp.mtime_ns == candidate.mtime_ns;
                    report.updated += 1;
                    changed.push(candidate);
                }
                None => {
                    report.added += 1;
                    candidate.new = true;
                    changed.push(candidate);
                }
            }
            seen.insert(path.to_string());
        }
    }
    progress(ScanProgress::Walking { found: report.found });

    // Only drop entries whose file is really gone: an unreadable folder is not a deletion.
    // A cancelled walk hasn't seen everything, so it can't tell what is gone.
    let mut gone: HashSet<i64> = HashSet::new();
    let mut gone_by_key: HashMap<MoveKey, Vec<i64>> = HashMap::new();
    if !report.cancelled {
        for (path, fp) in &known {
            if !seen.contains(path) && roots.iter().any(|r| Path::new(path).starts_with(r)) && !Path::new(path).exists()
            {
                gone.insert(fp.id);
                if let Some(h) = fp.hash {
                    gone_by_key.entry(MoveKey::Hash(h)).or_default().push(fp.id);
                }
                gone_by_key.entry(name_key(path, fp.size, fp.mtime_ns)).or_default().push(fp.id);
            }
        }
    }

    let total = changed.len() as u64;
    let mut done = 0;
    for chunk in changed.chunks(opts.batch_size.max(1)) {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let photos: Vec<NewPhoto> = chunk.par_iter().map(|c| analyze(c, opts)).collect();
        for (c, p) in chunk.iter().zip(&photos).filter(|(c, _)| c.new) {
            let by_hash = p.hash.as_deref().and_then(|h| blake3::Hash::from_hex(h).ok());
            let keys = by_hash.map(|h| MoveKey::Hash(*h.as_bytes())).into_iter();
            let mut from = None;
            for key in keys.chain([name_key(&c.path, c.size, c.mtime_ns)]) {
                let ids = gone_by_key.get_mut(&key).map(std::mem::take).unwrap_or_default();
                let mut rest = ids.into_iter().filter(|id| gone.contains(id));
                from = rest.next();
                gone_by_key.insert(key, rest.collect());
                if from.is_some() {
                    break;
                }
            }
            if let Some(id) = from {
                gone.remove(&id);
                catalog.relocate(id, &c.path)?;
                report.moved += 1;
                report.added -= 1;
            }
        }
        report.unreadable += photos.iter().filter(|p| p.error.is_some()).count() as u64;
        report.changed_ids.extend(catalog.upsert_many(&photos)?);
        done += chunk.len() as u64;
        progress(ScanProgress::Analyzing { done, total });
    }

    // A scan cancelled while reading may not have reached a moved file yet; keep its old entry.
    if !report.cancelled {
        report.removed_ids = gone.into_iter().collect();
        report.removed_ids.sort_unstable();
        report.removed_face_ids = catalog.remove_many(&report.removed_ids)?;
        report.removed = report.removed_ids.len() as u64;
    }

    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// Hidden folders and NAS/OS thumbnail caches.
fn is_ignored(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry.file_name().to_str().is_some_and(|n| n.starts_with('.') || n == "@eaDir" || n == "$RECYCLE.BIN")
}

fn analyze(c: &Candidate, opts: &ScanOptions) -> NewPhoto {
    let path = Path::new(&c.path);
    let (meta, error) = match tintype_meta::read_path(path) {
        Ok(m) => (m, None),
        Err(e) => (Default::default(), Some(e.to_string())),
    };
    let hash = (opts.hash && !c.content_unchanged).then(|| hash_file(path).ok()).flatten();
    NewPhoto {
        path: c.path.clone(),
        size: c.size,
        mtime_ns: c.mtime_ns,
        hash,
        meta,
        meta_version: tintype_meta::EXTRACTOR_VERSION,
        error,
    }
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_testkit::JpegSpec;

    fn run(cat: &mut Catalog, root: &Path) -> ScanReport {
        scan(cat, &[root.to_path_buf()], &ScanOptions::default(), &AtomicBool::new(false), &mut |_| {}).unwrap()
    }

    #[test]
    fn incremental_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        JpegSpec::canon().write(&root.join("a/one.jpg"));
        JpegSpec::nikon().write(&root.join("b/two.JPG"));
        JpegSpec::default().write(&root.join(".cache/hidden.jpg"));
        std::fs::write(root.join("notes.txt"), "not a photo").unwrap();
        std::fs::write(root.join("broken.jpg"), "not really a jpeg").unwrap();

        let mut cat = Catalog::open_in_memory().unwrap();
        let r = run(&mut cat, root);
        assert_eq!((r.found, r.added, r.updated, r.removed), (3, 3, 0, 0));
        assert_eq!(r.changed_ids.len(), 3);
        assert_eq!(r.unreadable, 1, "broken.jpg is cataloged with an error");

        let r = run(&mut cat, root);
        assert_eq!((r.unchanged, r.added, r.updated), (3, 0, 0), "nothing changed");

        // Change one file's size (mtime granularity is too coarse to rely on), delete another.
        std::fs::write(root.join("a/one.jpg"), [JpegSpec::canon().to_bytes(), vec![0; 10]].concat()).unwrap();
        std::fs::remove_file(root.join("b/two.JPG")).unwrap();
        let r = run(&mut cat, root);
        assert_eq!((r.unchanged, r.updated, r.removed), (1, 1, 1));
        assert_eq!(cat.count().unwrap(), 2);

        let rec = cat
            .get_by_path(&dunce::canonicalize(root.join("a/one.jpg")).unwrap().display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(rec.meta.make.as_deref(), Some("Canon"));
        assert_eq!(rec.hash.as_ref().map(String::len), Some(64));
    }

    #[test]
    fn scanning_one_root_leaves_others_alone() {
        let dir = tempfile::tempdir().unwrap();
        JpegSpec::canon().write(&dir.path().join("x/a.jpg"));
        JpegSpec::canon().write(&dir.path().join("y/b.jpg"));
        let mut cat = Catalog::open_in_memory().unwrap();
        run(&mut cat, &dir.path().join("x"));
        run(&mut cat, &dir.path().join("y"));
        let r = run(&mut cat, &dir.path().join("x"));
        assert_eq!(r.removed, 0);
        assert_eq!(cat.count().unwrap(), 2);
    }

    #[test]
    fn missing_root_is_an_error() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let err = scan(
            &mut cat,
            &[PathBuf::from("/no/such/dir")],
            &ScanOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(matches!(err, Err(Error::Root { .. })));
    }

    fn path_of(root: &Path, rel: &str) -> String {
        dunce::canonicalize(root.join(rel)).unwrap().display().to_string()
    }

    #[test]
    fn moved_photos_keep_their_entry() {
        for hash in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            JpegSpec::canon().write(&root.join("in/a.jpg"));
            JpegSpec::nikon().write(&root.join("in/b.jpg"));
            let mut cat = Catalog::open_in_memory().unwrap();
            let opts = ScanOptions { hash, ..ScanOptions::default() };
            let run = |cat: &mut Catalog| {
                scan(cat, &[root.to_path_buf()], &opts, &AtomicBool::new(false), &mut |_| {}).unwrap()
            };
            run(&mut cat);
            let id = cat.get_by_path(&path_of(root, "in/a.jpg")).unwrap().unwrap().id;
            cat.set_rating(id, Some(5)).unwrap();

            // Move a.jpg into another folder under a new name; copy b.jpg.
            std::fs::create_dir_all(root.join("out")).unwrap();
            std::fs::rename(root.join("in/a.jpg"), root.join("out/a-renamed.jpg")).unwrap();
            std::fs::copy(root.join("in/b.jpg"), root.join("out/b-copy.jpg")).unwrap();
            let r = run(&mut cat);
            if hash {
                assert_eq!((r.moved, r.added, r.removed), (1, 1, 0), "a moved; the copy is a new photo");
                let rec = cat.get_by_path(&path_of(root, "out/a-renamed.jpg")).unwrap().unwrap();
                assert_eq!((rec.id, rec.rating), (id, Some(5)));
            } else {
                // Without hashes a renamed file can't be recognized: only same-name moves are.
                assert_eq!((r.moved, r.removed), (0, 1));
            }

            // A same-name move into another folder is recognized either way.
            std::fs::rename(root.join("in/b.jpg"), root.join("b.jpg")).unwrap();
            let r = run(&mut cat);
            assert_eq!((r.moved, r.added, r.removed), (1, 0, 0), "hash: {hash}");
            assert_eq!(cat.count().unwrap(), 3);
        }
    }

    #[test]
    fn cancelled_scan_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        JpegSpec::canon().write(&dir.path().join("a.jpg"));
        let mut cat = Catalog::open_in_memory().unwrap();
        run(&mut cat, dir.path());
        let r =
            scan(&mut cat, &[dir.path().to_path_buf()], &ScanOptions::default(), &AtomicBool::new(true), &mut |_| {})
                .unwrap();
        assert!(r.cancelled);
        assert_eq!(cat.count().unwrap(), 1);
    }
}
