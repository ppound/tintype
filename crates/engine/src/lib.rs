//! The engine owns a library (catalog + search index) and executes
//! [`Command`]s against it. The CLI, the GUI and any future automation (MCP,
//! scripts) all go through [`Engine::execute`], so they behave identically.
//! Commands and responses are serde types, so they can travel as JSON.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tintype_catalog::Catalog;
use tintype_faces::{FACES_VERSION, FaceAnalyzer};
use tintype_index::PhotoIndex;
use tintype_infer::{ModelStore, SFACE, YUNET};
use tintype_jobs::ScanOptions;

pub use tintype_catalog::{FaceGroup, FaceInfo, Person, PhotoRecord};
pub use tintype_codecs::{self as codecs, Rgba, ThumbCache, decode_preview};
pub use tintype_index::{FACET_FIELDS, FacetCount, Hit, SearchResults, Sort};
pub use tintype_jobs::{FacesReport, ScanProgress, ScanReport, face_crop_path};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Catalog(#[from] tintype_catalog::Error),
    #[error(transparent)]
    Index(#[from] tintype_index::Error),
    #[error(transparent)]
    Scan(#[from] tintype_jobs::scan::Error),
    #[error(transparent)]
    Faces(#[from] tintype_jobs::faces::Error),
    #[error(transparent)]
    Model(#[from] tintype_infer::Error),
    #[error(transparent)]
    Analyzer(#[from] tintype_faces::Error),
    #[error("{path}: {source}")]
    Library { path: PathBuf, source: std::io::Error },
    #[error("no photo {0} in the library")]
    PhotoNotFound(String),
    #[error("no library folders to rescan; add one first")]
    NoFolders,
}

pub type Result<T> = std::result::Result<T, Error>;

/// A photo, by catalog id or by file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PhotoRef {
    Id(i64),
    Path(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Add folders to the library, or refresh them.
    Scan {
        roots: Vec<PathBuf>,
        #[serde(default = "yes")]
        hash: bool,
    },
    /// Refresh every folder previously added with `Scan`. Folders that are
    /// currently missing (an unplugged drive) are skipped, not emptied.
    Rescan {
        #[serde(default = "yes")]
        hash: bool,
    },
    Search {
        #[serde(default)]
        query: String,
        #[serde(default)]
        sort: Sort,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    /// Counts per child of `parent` in a facet, for photos matching `query`.
    Facets {
        field: String,
        #[serde(default)]
        query: String,
        #[serde(default = "facet_root")]
        parent: String,
    },
    Show {
        photo: PhotoRef,
    },
    /// Set (1-5) or clear (`None`) a rating.
    Rate {
        photo: PhotoRef,
        rating: Option<u8>,
    },
    Tag {
        photo: PhotoRef,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    },
    /// Rebuild the search index from the catalog.
    Reindex,
    Stats,

    /// Find faces in up to `batch` photos that haven't been analyzed (downloading
    /// the face models first if needed). Repeat until `remaining` is 0; other
    /// commands can run between batches.
    FindFaces {
        #[serde(default = "default_face_batch")]
        batch: usize,
    },
    /// Named people and unnamed groups of similar faces.
    People,
    PhotoFaces {
        photo: PhotoRef,
    },
    /// A person's faces, suggestions first.
    PersonFaces {
        person: i64,
    },
    /// Confirm faces as `name` (a new or existing person).
    NameFaces {
        faces: Vec<i64>,
        name: String,
    },
    /// "Not this person": unname a face and stop suggesting that person for it.
    RejectFace {
        face: i64,
    },
    /// Take a face out of its unnamed group.
    UngroupFace {
        face: i64,
    },
    /// Rename a person; renaming to another person's name merges the two.
    RenamePerson {
        person: i64,
        name: String,
    },
    /// Forget a person; their faces become unnamed.
    DeletePerson {
        person: i64,
    },
}

fn yes() -> bool {
    true
}
fn default_limit() -> usize {
    50
}
fn facet_root() -> String {
    "/".into()
}
fn default_face_batch() -> usize {
    150
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Scan(ScanReport),
    Search(SearchResults),
    Facets {
        counts: Vec<FacetCount>,
    },
    Photo(Box<PhotoRecord>),
    Reindexed {
        photos: u64,
    },
    Stats(Stats),
    Faces(FacesReport),
    People {
        people: Vec<Person>,
        groups: Vec<FaceGroup>,
        /// Photos not yet analyzed for faces.
        unscanned: u64,
        /// Face models are downloaded.
        models_ready: bool,
    },
    FaceList {
        faces: Vec<FaceInfo>,
    },
    /// A people edit; `photos` were reindexed.
    Changed {
        photos: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub library: String,
    pub photos: u64,
    pub indexed: u64,
    pub folders: Vec<String>,
}

/// Progress from long-running commands.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Scan(ScanProgress),
    Reindex { done: u64, total: u64 },
    Download { model: &'static str, done: u64, total: u64 },
    Faces { done: u64, total: u64 },
}

/// Where the library lives when the user doesn't choose: the OS data dir.
pub fn default_library_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "Tintype").map(|d| d.data_dir().join("library"))
}

/// Where downloaded models live: shared by all libraries.
fn models_dir(library: &Path) -> PathBuf {
    directories::ProjectDirs::from("", "", "Tintype")
        .map_or_else(|| library.join("models"), |d| d.data_dir().join("models"))
}

/// Face passes between full recognition runs while a long face scan is going.
const RECOGNIZE_EVERY: u32 = 4;

/// Long edge, in pixels, of cached grid thumbnails.
pub const THUMB_EDGE: u32 = 400;

pub struct Engine {
    dir: PathBuf,
    catalog: Catalog,
    index: PhotoIndex,
    thumbs: Arc<ThumbCache>,
    cancel: Arc<AtomicBool>,
    models: ModelStore,
    analyzer: Option<Arc<FaceAnalyzer>>,
    batches_since_recognize: u32,
}

impl Engine {
    /// Opens (or creates) the library in `dir`. Rebuilds the index if it is new,
    /// from an older schema, or out of step with the catalog.
    pub fn open(dir: &Path) -> Result<Self> {
        let lib_err = |source| Error::Library { path: dir.to_path_buf(), source };
        std::fs::create_dir_all(dir).map_err(lib_err)?;
        let catalog = Catalog::open(&dir.join("catalog.db"))?;
        let (index, fresh) = PhotoIndex::open(&dir.join("index"))?;
        let thumbs = Arc::new(ThumbCache::new(dir.join("thumbs"), THUMB_EDGE));
        let mut engine = Self {
            dir: dir.to_path_buf(),
            catalog,
            index,
            thumbs,
            cancel: Arc::default(),
            models: ModelStore::new(models_dir(dir)),
            analyzer: None,
            batches_since_recognize: 0,
        };
        if fresh || engine.index.doc_count() != engine.catalog.count()? {
            tracing::info!(fresh, "rebuilding search index");
            engine.reindex(&mut |_| {})?;
        }
        Ok(engine)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The thumbnail cache. Thread-safe, so a UI can decode off its main thread
    /// while the engine is busy with a command.
    pub fn thumbnails(&self) -> Arc<ThumbCache> {
        self.thumbs.clone()
    }

    /// Folder holding face crops; see [`face_crop_path`].
    pub fn faces_dir(&self) -> PathBuf {
        self.dir.join("faces")
    }

    /// Set this flag from another thread to stop the running command early.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn execute(&mut self, cmd: Command, events: &mut dyn FnMut(Event)) -> Result<Response> {
        self.cancel.store(false, Ordering::Relaxed);
        match cmd {
            Command::Scan { roots, hash } => self.scan(&roots, hash, events).map(Response::Scan),
            Command::Rescan { hash } => {
                let roots: Vec<PathBuf> =
                    self.catalog.roots()?.into_iter().map(PathBuf::from).filter(|r| r.is_dir()).collect();
                if roots.is_empty() {
                    return Err(Error::NoFolders);
                }
                self.scan(&roots, hash, events).map(Response::Scan)
            }
            Command::Search { query, sort, limit, offset } => {
                Ok(Response::Search(self.index.search(&query, sort, limit, offset)?))
            }
            Command::Facets { field, query, parent } => {
                Ok(Response::Facets { counts: self.index.facets(&query, &field, &parent)? })
            }
            Command::Show { photo } => Ok(Response::Photo(Box::new(self.resolve(&photo)?))),
            Command::Rate { photo, rating } => {
                let id = self.resolve(&photo)?.id;
                self.catalog.set_rating(id, rating)?;
                self.refresh(id)
            }
            Command::Tag { photo, add, remove } => {
                let id = self.resolve(&photo)?.id;
                self.catalog.edit_tags(id, &add, &remove)?;
                self.refresh(id)
            }
            Command::Reindex => Ok(Response::Reindexed { photos: self.reindex(events)? }),
            Command::Stats => Ok(Response::Stats(Stats {
                library: self.dir.display().to_string(),
                photos: self.catalog.count()?,
                indexed: self.index.doc_count(),
                folders: self.catalog.roots()?,
            })),

            Command::FindFaces { batch } => self.find_faces(batch, events).map(Response::Faces),
            Command::People => Ok(Response::People {
                people: self.catalog.people()?,
                groups: self.catalog.face_groups(usize::MAX)?,
                unscanned: self.catalog.count_needing_faces(FACES_VERSION)?,
                models_ready: self.models.is_installed(&YUNET) && self.models.is_installed(&SFACE),
            }),
            Command::PhotoFaces { photo } => {
                let id = self.resolve(&photo)?.id;
                Ok(Response::FaceList { faces: self.catalog.photo_faces(id)? })
            }
            Command::PersonFaces { person } => Ok(Response::FaceList { faces: self.catalog.person_faces(person)? }),
            Command::NameFaces { faces, name } => {
                let photos = self.catalog.photos_of_faces(&faces)?;
                self.catalog.name_faces(&faces, &name)?;
                self.after_people_edit(photos)
            }
            Command::RejectFace { face } => {
                let photos = self.catalog.photos_of_faces(&[face])?;
                self.catalog.reject_face(face)?;
                self.after_people_edit(photos)
            }
            Command::UngroupFace { face } => {
                self.catalog.ungroup_face(face)?;
                self.after_people_edit(Vec::new())
            }
            Command::RenamePerson { person, name } => {
                let photos = self.catalog.photos_of_person(person)?;
                self.catalog.rename_person(person, &name)?;
                self.after_people_edit(photos)
            }
            Command::DeletePerson { person } => {
                let photos = self.catalog.photos_of_person(person)?;
                self.catalog.delete_person(person)?;
                self.after_people_edit(photos)
            }
        }
    }

    fn analyzer(&mut self, events: &mut dyn FnMut(Event)) -> Result<Arc<FaceAnalyzer>> {
        if let Some(a) = &self.analyzer {
            return Ok(a.clone());
        }
        let mut paths = Vec::new();
        for spec in [YUNET, SFACE] {
            let mut progress = |done, total| events(Event::Download { model: spec.name, done, total });
            paths.push(self.models.ensure(&spec, &mut progress)?);
        }
        let analyzer = Arc::new(FaceAnalyzer::load(&paths[0], &paths[1])?);
        self.analyzer = Some(analyzer.clone());
        Ok(analyzer)
    }

    fn find_faces(&mut self, batch: usize, events: &mut dyn FnMut(Event)) -> Result<FacesReport> {
        let analyzer = self.analyzer(events)?;
        let crops = self.faces_dir();
        let mut progress = |done, total| events(Event::Faces { done, total });
        let mut report =
            tintype_jobs::find_faces(&mut self.catalog, &analyzer, &crops, batch.max(1), &self.cancel, &mut progress)?;
        let mut changed = std::mem::take(&mut report.changed_photos);
        // Matching looks at every face, so during a long scan it runs every few batches, and at the end.
        self.batches_since_recognize += 1;
        if report.remaining == 0 || report.cancelled || self.batches_since_recognize >= RECOGNIZE_EVERY {
            self.batches_since_recognize = 0;
            changed.extend(tintype_jobs::recognize(&mut self.catalog)?);
        }
        self.reindex_photos(changed)?;
        Ok(report)
    }

    /// After naming changes: re-match everyone, then reindex affected photos.
    fn after_people_edit(&mut self, mut photos: Vec<i64>) -> Result<Response> {
        photos.extend(tintype_jobs::recognize(&mut self.catalog)?);
        let n = self.reindex_photos(photos)?;
        Ok(Response::Changed { photos: n })
    }

    fn reindex_photos(&mut self, mut ids: Vec<i64>) -> Result<u64> {
        ids.sort_unstable();
        ids.dedup();
        for &id in &ids {
            if let Some(rec) = self.catalog.get(id)? {
                self.index.upsert([&rec])?;
            }
        }
        self.index.commit()?;
        Ok(ids.len() as u64)
    }

    fn scan(&mut self, roots: &[PathBuf], hash: bool, events: &mut dyn FnMut(Event)) -> Result<ScanReport> {
        let opts = ScanOptions { hash, ..ScanOptions::default() };
        let report =
            tintype_jobs::scan(&mut self.catalog, roots, &opts, &self.cancel, &mut |p| events(Event::Scan(p)))?;
        self.catalog.add_roots(&report.roots)?;
        for &face in &report.removed_face_ids {
            let _ = std::fs::remove_file(face_crop_path(&self.faces_dir(), face));
        }

        self.index.remove(&report.removed_ids)?;
        for &id in &report.changed_ids {
            if let Some(rec) = self.catalog.get(id)? {
                self.index.upsert([&rec])?;
            }
        }
        self.index.commit()?;
        Ok(report)
    }

    fn reindex(&mut self, events: &mut dyn FnMut(Event)) -> Result<u64> {
        const BATCH: usize = 1000;
        let total = self.catalog.count()?;
        self.index.clear()?;

        let index = &mut self.index;
        let mut batch = Vec::with_capacity(BATCH);
        let mut done = 0;
        let mut failed = None;
        self.catalog.for_each(|rec| {
            if failed.is_some() {
                return;
            }
            batch.push(rec);
            if batch.len() == BATCH {
                failed = index.upsert(&batch).err();
                done += batch.len() as u64;
                batch.clear();
                events(Event::Reindex { done, total });
            }
        })?;
        if let Some(e) = failed {
            return Err(e.into());
        }
        index.upsert(&batch)?;
        done += batch.len() as u64;
        index.commit()?;
        events(Event::Reindex { done, total });
        Ok(done)
    }

    /// Re-indexes one photo after a metadata edit and returns it.
    fn refresh(&mut self, id: i64) -> Result<Response> {
        let rec = self.catalog.get(id)?.ok_or_else(|| Error::PhotoNotFound(id.to_string()))?;
        self.index.upsert([&rec])?;
        self.index.commit()?;
        Ok(Response::Photo(Box::new(rec)))
    }

    fn resolve(&self, photo: &PhotoRef) -> Result<PhotoRecord> {
        let found = match photo {
            PhotoRef::Id(id) => self.catalog.get(*id)?,
            PhotoRef::Path(p) => {
                // Catalog paths are canonical; accept relative or differently-spelled ones.
                let canonical = dunce::canonicalize(p).unwrap_or_else(|_| p.clone());
                self.catalog.get_by_path(&canonical.display().to_string())?
            }
        };
        found.ok_or_else(|| {
            Error::PhotoNotFound(match photo {
                PhotoRef::Id(id) => format!("#{id}"),
                PhotoRef::Path(p) => p.display().to_string(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_testkit::JpegSpec;

    fn hits(engine: &mut Engine, query: &str) -> Vec<String> {
        let cmd = Command::Search { query: query.into(), sort: Sort::Newest, limit: 50, offset: 0 };
        let Response::Search(r) = engine.execute(cmd, &mut |_| {}).unwrap() else { panic!() };
        r.hits.into_iter().map(|h| Path::new(&h.path).file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn scan_search_tag_and_reopen() {
        let photos = tempfile::tempdir().unwrap();
        JpegSpec::canon().write(&photos.path().join("trip/canon.jpg"));
        JpegSpec::nikon().write(&photos.path().join("trip/nikon.jpg"));
        let lib = tempfile::tempdir().unwrap();

        let mut engine = Engine::open(lib.path()).unwrap();
        let scan = Command::Scan { roots: vec![photos.path().into()], hash: true };
        let Response::Scan(report) = engine.execute(scan, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(report.added, 2);

        assert_eq!(hits(&mut engine, ""), vec!["canon.jpg", "nikon.jpg"], "newest first");
        assert_eq!(hits(&mut engine, "iso:>=1000"), vec!["canon.jpg"]);
        let Response::Scan(report) = engine.execute(Command::Rescan { hash: true }, &mut |_| {}).unwrap() else {
            panic!()
        };
        assert_eq!(report.unchanged, 2, "rescan covers the added folder");

        let photo = PhotoRef::Path(photos.path().join("trip/nikon.jpg"));
        let tag = Command::Tag { photo: photo.clone(), add: vec!["sunset".into()], remove: vec![] };
        engine.execute(tag, &mut |_| {}).unwrap();
        engine.execute(Command::Rate { photo, rating: Some(5) }, &mut |_| {}).unwrap();
        assert_eq!(hits(&mut engine, "sunset rating:5"), vec!["nikon.jpg"]);
        drop(engine);

        // Deleting the index forces a rebuild from the catalog, keeping user metadata.
        std::fs::remove_dir_all(lib.path().join("index")).unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        assert_eq!(hits(&mut engine, "tag:sunset"), vec!["nikon.jpg"]);

        let facets = Command::Facets { field: "camera".into(), query: String::new(), parent: "/".into() };
        let Response::Facets { counts } = engine.execute(facets, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn commands_round_trip_as_json() {
        let cmd: Command = serde_json::from_str(r#"{"command":"search","query":"iso:>800"}"#).unwrap();
        assert!(matches!(cmd, Command::Search { limit: 50, sort: Sort::Relevance, .. }));
        let cmd: Command = serde_json::from_str(r#"{"command":"rate","photo":12,"rating":null}"#).unwrap();
        assert!(matches!(cmd, Command::Rate { photo: PhotoRef::Id(12), rating: None }));
    }

    #[test]
    fn rescan_needs_folders() {
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        let err = engine.execute(Command::Rescan { hash: false }, &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::NoFolders));
    }

    #[test]
    fn unknown_photo() {
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        let err = engine.execute(Command::Show { photo: PhotoRef::Id(42) }, &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::PhotoNotFound(_)));
    }
}
