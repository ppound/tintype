//! Putting copies of photos from several folders together in one, so they
//! can be dragged to a browser (or anywhere else) in one go. The photos
//! themselves are only read.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Shared with the thread doing the copying.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
}

pub struct Gathered {
    pub copied: Vec<PathBuf>,
    pub error: Option<String>,
}

/// Where copies are put: Tintype's own folder in the system's temporary
/// files, so they aren't synced by OneDrive or iCloud like Pictures can be,
/// and are tidied away by the system in the end.
pub fn folder() -> PathBuf {
    std::env::temp_dir().join("Tintype Upload")
}

/// Copies `files` into `dest`, giving clashing names a number
/// (`DSCF1234 (2).JPG`). The copies from last time are removed first: `dest`
/// is Tintype's own folder, and only files directly in it are touched.
pub fn gather(files: &[PathBuf], dest: &Path, progress: &Progress) -> Gathered {
    let mut gathered = Gathered { copied: Vec::new(), error: None };
    if let Err(e) = std::fs::create_dir_all(dest) {
        gathered.error = Some(format!("Could not make the folder {}: {e}", dest.display()));
        return gathered;
    }
    if let Ok(entries) = std::fs::read_dir(dest) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                // A copy still open in another app stays, and the new one gets a number.
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let mut used = HashSet::new();
    for file in files {
        if progress.cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(name) = file.file_name() else { continue };
        let to = free_name(dest, name, &mut used);
        if let Err(e) = std::fs::copy(file, &to) {
            let _ = std::fs::remove_file(&to);
            gathered.error = Some(format!("Could not copy {}: {e}", file.display()));
            break;
        }
        gathered.copied.push(to);
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    gathered
}

/// `name` in `dest`, or `stem (2).ext`, `stem (3).ext`... if that's taken.
/// Names are compared ignoring case, as Windows and macOS do.
fn free_name(dest: &Path, name: &OsStr, used: &mut HashSet<String>) -> PathBuf {
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or(name).to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 1.. {
        let candidate = if n == 1 { name.to_string_lossy().into_owned() } else { format!("{stem} ({n}){ext}") };
        let to = dest.join(&candidate);
        if !to.exists() && used.insert(candidate.to_lowercase()) {
            return to;
        }
    }
    unreachable!("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn copies_from_several_folders_with_clashing_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("card a/DSCF0001.JPG");
        let b = dir.path().join("card b/dscf0001.jpg");
        let c = dir.path().join("card b/DSCF0002.JPG");
        for (p, text) in [(&a, "a"), (&b, "b"), (&c, "c")] {
            write(p, text);
        }
        let dest = dir.path().join("upload");
        let progress = Progress::default();
        let g = gather(&[a.clone(), b, c], &dest, &progress);
        assert!(g.error.is_none());
        let names: Vec<_> = g.copied.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["DSCF0001.JPG", "dscf0001 (2).jpg", "DSCF0002.JPG"]);
        assert_eq!(std::fs::read_to_string(&g.copied[1]).unwrap(), "b");
        assert_eq!(progress.done.load(Ordering::Relaxed), 3);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a", "the original is untouched");
    }

    #[test]
    fn last_times_copies_make_way() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photos/DSCF0001.JPG");
        write(&photo, "new");
        let dest = dir.path().join("upload");
        write(&dest.join("OLD.JPG"), "old");
        write(&dest.join("DSCF0001.JPG"), "old");
        write(&dest.join("sub/KEPT.JPG"), "old");
        let g = gather(std::slice::from_ref(&photo), &dest, &Progress::default());
        assert_eq!(g.copied, [dest.join("DSCF0001.JPG")]);
        assert_eq!(std::fs::read_to_string(&g.copied[0]).unwrap(), "new");
        assert!(!dest.join("OLD.JPG").exists());
        assert!(dest.join("sub/KEPT.JPG").exists(), "only files directly in the folder go");
    }

    #[test]
    fn a_missing_photo_stops_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let there = dir.path().join("a/ONE.JPG");
        write(&there, "1");
        let gone = dir.path().join("b/GONE.JPG");
        let g = gather(&[there, gone], &dir.path().join("upload"), &Progress::default());
        assert_eq!(g.copied.len(), 1);
        assert!(g.error.unwrap().contains("GONE.JPG"));
    }

    #[test]
    fn cancelling_stops_before_the_next_photo() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("a/ONE.JPG");
        write(&photo, "1");
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let g = gather(&[photo], &dir.path().join("upload"), &progress);
        assert!(g.copied.is_empty() && g.error.is_none());
    }
}
