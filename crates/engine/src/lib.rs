//! The engine owns a library (catalog + search index) and executes
//! [`Command`]s against it. The CLI, the GUI and any future automation (MCP,
//! scripts) all go through [`Engine::execute`], so they behave identically.
//! Commands and responses are serde types, so they can travel as JSON.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tintype_analyze::{
    AESTHETIC_VERSION, ANALYSIS_VERSION, AestheticScorer, ImageEncoder, MIN_RATINGS, SHARPNESS_VERSION, TasteModel,
    TextEncoder, taste_features,
};
use tintype_catalog::{Catalog, StackPlace};
use tintype_faces::{FACES_VERSION, FaceAnalyzer};
use tintype_index::PhotoIndex;
use tintype_infer::{
    CLIP_L14_VISION, LAION_AESTHETIC_HEAD, ModelSpec, ModelStore, SFACE, SIGLIP_TEXT, SIGLIP_TOKENIZER, SIGLIP_VISION,
    YUNET,
};
use tintype_jobs::{ScanOptions, SyncMode};

pub use tintype_catalog::{FaceGroup, FaceInfo, Person, PhotoRecord};
pub use tintype_codecs::{self as codecs, Rgba, ThumbCache, decode_preview};
pub use tintype_index::{FACET_FIELDS, FacetCount, Hit, SearchResults, Sort, split_smart};
pub use tintype_jobs::{AnalyzeReport, FacesReport, ScanProgress, ScanReport, SidecarReport, face_crop_path};
pub use tintype_meta::FileKind;

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
    #[error(transparent)]
    Analysis(#[from] tintype_analyze::Error),
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
        /// Include only these kinds of file from these folders (remembered for
        /// rescans). `None` keeps a folder's earlier choice; new folders include all.
        #[serde(default)]
        kinds: Option<Vec<FileKind>>,
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
        /// Match the words by what the photos show (field terms still filter).
        /// Results are ranked by similarity; `sort` is ignored.
        #[serde(default)]
        smart: bool,
        /// Show one photo per stack (copies, versions and bursts of one shot),
        /// with `stack_size` saying how many of its photos matched.
        #[serde(default = "yes")]
        stacks: bool,
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
    /// Embed up to `batch` photos for smart search (downloading the models
    /// first if needed). Repeat until `remaining` is 0, like `FindFaces`.
    Analyze {
        #[serde(default = "default_analyze_batch")]
        batch: usize,
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
    /// Read sidecars changed outside Tintype in every library folder (and, if
    /// writing is on, create missing ones). Scans do this for the folders they scan.
    SyncSidecars,
    /// Read or change library settings; `None` leaves a setting as it is.
    Settings {
        /// Keep ratings, tags and people in `photo.jpg.xmp` sidecars next to the photos.
        #[serde(default)]
        write_sidecars: Option<bool>,
        /// Score photos with the LAION aesthetic predictor (a one-time 1.2 GB
        /// download; `Analyze` then scores every photo).
        #[serde(default)]
        aesthetic: Option<bool>,
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
fn default_analyze_batch() -> usize {
    200
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
    Analyze(AnalyzeReport),
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
    Sidecars(SidecarReport),
    Settings {
        write_sidecars: bool,
        aesthetic: bool,
        /// Bytes still to download for the aesthetic score (0 when ready).
        aesthetic_download_bytes: u64,
        /// What turning sidecar writing on just did, if it was turned on.
        #[serde(skip_serializing_if = "Option::is_none")]
        sidecars: Option<SidecarReport>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub library: String,
    pub photos: u64,
    pub indexed: u64,
    pub folders: Vec<String>,
    /// The kinds of file each folder includes, in `folders` order (`None`: all).
    pub folder_kinds: Vec<Option<Vec<FileKind>>>,
    /// Photos not yet fully analyzed (smart search, sharpness, aesthetic score)...
    pub unanalyzed: u64,
    /// ...and the passes over photos that this takes (a photo may need two):
    /// `Analyze` counts its `remaining` in these.
    pub analysis_steps: u64,
    /// Smart search models are downloaded; if not, `smart_download_bytes` will be.
    pub smart_ready: bool,
    pub smart_download_bytes: u64,
    /// Ratings the user gave in Tintype, and how many the taste model needs.
    pub taste_ratings: u64,
    pub taste_needed: u64,
}

/// Progress from long-running commands.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Scan(ScanProgress),
    Reindex { done: u64, total: u64 },
    Download { model: &'static str, done: u64, total: u64 },
    Faces { done: u64, total: u64 },
    Analyze { done: u64, total: u64 },
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

/// Smart search shows photos at least this likely (SigLIP's calibrated
/// probability) to match the description...
const SMART_MIN_PROBABILITY: f32 = 0.0003;
/// ...and within this similarity of the best match. A fixed floor alone fits
/// rare subjects ("dog": true matches down to 0.046) but lets common ones
/// flood in ("castle": best 0.12, old buildings at 0.044). Tuned on a
/// 4,253-photo library by checking results at increasing depth.
const SMART_WINDOW: f32 = 0.045;

/// Library setting: the stacking rules (`tintype_jobs::STACKS_VERSION`) last applied.
const STACKS_SETTING: &str = "stacks.version";

/// Library setting: write XMP sidecars ("1", the default) or only read them ("0").
const WRITE_SIDECARS: &str = "sidecars.write";

/// Library setting: compute the LAION aesthetic score ("1") or not (the default).
const AESTHETIC_SETTING: &str = "aesthetic.enabled";

/// Models the aesthetic score needs, downloaded when it is turned on and analysis runs.
const AESTHETIC_MODELS: [ModelSpec; 2] = [CLIP_L14_VISION, LAION_AESTHETIC_HEAD];

/// Models smart search needs, downloaded together.
const SMART_MODELS: [ModelSpec; 3] = [SIGLIP_VISION, SIGLIP_TEXT, SIGLIP_TOKENIZER];

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
    image_encoder: Option<Arc<ImageEncoder>>,
    text_encoder: Option<TextEncoder>,
    /// All image embeddings, loaded on the first smart search after a change.
    embeddings: Option<Vec<(i64, Vec<f32>)>>,
    /// Every stacked photo's place, loaded on the first stacked search after a change.
    stack_places: Option<HashMap<i64, StackPlace>>,
    aesthetic_scorer: Option<Arc<AestheticScorer>>,
    /// The user's learned taste: `None` until (re)fitted, `Some(None)` when
    /// there aren't enough ratings yet.
    taste: Option<Option<TasteModel>>,
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
            image_encoder: None,
            text_encoder: None,
            embeddings: None,
            stack_places: None,
            aesthetic_scorer: None,
            taste: None,
        };
        if fresh || engine.index.doc_count() != engine.catalog.count()? {
            tracing::info!(fresh, "rebuilding search index");
            engine.reindex(&mut |_| {})?;
        }
        if engine.catalog.setting(STACKS_SETTING)?.as_deref() != Some(&tintype_jobs::STACKS_VERSION.to_string()) {
            tracing::info!("stacking the library");
            engine.restack()?;
            engine.catalog.set_setting(STACKS_SETTING, &tintype_jobs::STACKS_VERSION.to_string())?;
        }
        {
            // Finish what an interrupted scan or sidecar import left behind.
            let pending = engine.catalog.index_pending()?;
            if !pending.is_empty() {
                tracing::info!(photos = pending.len(), "updating search index");
                engine.reindex_photos(pending)?;
            }
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
            Command::Scan { roots, hash, kinds } => {
                if let Some(kinds) = &kinds {
                    for root in &roots {
                        let root = tintype_jobs::normalize_path(root)
                            .map_err(|source| Error::Library { path: root.clone(), source })?;
                        let all = FileKind::ALL.iter().all(|k| kinds.contains(k));
                        self.catalog.set_root_kinds(&root.display().to_string(), (!all).then_some(kinds.as_slice()))?;
                    }
                }
                self.scan(&roots, hash, events).map(Response::Scan)
            }
            Command::Rescan { hash } => {
                let roots: Vec<PathBuf> =
                    self.catalog.roots()?.into_iter().map(PathBuf::from).filter(|r| r.is_dir()).collect();
                if roots.is_empty() {
                    return Err(Error::NoFolders);
                }
                self.scan(&roots, hash, events).map(Response::Scan)
            }
            Command::Search { query, smart, sort, limit, offset, stacks } => {
                let smart = smart && !tintype_index::split_smart(&query).1.is_empty();
                let by_taste = !smart && sort == Sort::Best && self.taste_model()?.is_some();
                if !smart && !stacks && !by_taste {
                    return Ok(Response::Search(self.index.search(&query, sort, limit, offset)?));
                }
                let ordered = if smart {
                    self.smart_ranked(&query, events)?
                } else if by_taste {
                    self.taste_ranked(&query)?
                } else {
                    self.index.ordered_ids(&query, sort)?
                };
                self.page(ordered, stacks, limit, offset).map(Response::Search)
            }
            Command::Facets { field, query, parent } => {
                Ok(Response::Facets { counts: self.index.facets(&query, &field, &parent)? })
            }
            // No sidecar check here: on a busy network share it would hold up
            // the details panel. Startup and scans pick up outside edits.
            Command::Show { photo } => {
                let rec = self.resolve(&photo)?;
                Ok(Response::Photo(Box::new(self.with_suggestion(rec)?)))
            }
            Command::SyncSidecars => {
                let roots: Vec<PathBuf> =
                    self.catalog.roots()?.into_iter().map(PathBuf::from).filter(|r| r.is_dir()).collect();
                let mode = if self.write_sidecars_on()? { SyncMode::Write } else { SyncMode::ReadOnly };
                let mut report = tintype_jobs::sync_sidecars(&mut self.catalog, &roots, mode)?;
                self.reindex_photos(std::mem::take(&mut report.changed_photos))?;
                Ok(Response::Sidecars(report))
            }
            Command::Rate { photo, rating } => {
                let id = self.resolve(&photo)?.id;
                self.before_edit(&[id])?;
                self.catalog.set_rating(id, rating)?;
                self.taste = None;
                self.after_edit(&[id])?;
                let moved = tintype_jobs::refresh_cover(&mut self.catalog, id)?;
                if !moved.is_empty() {
                    self.stack_places = None;
                    self.reindex_photos(moved)?;
                }
                self.refresh(id)
            }
            Command::Tag { photo, add, remove } => {
                let id = self.resolve(&photo)?.id;
                self.before_edit(&[id])?;
                self.catalog.edit_tags(id, &add, &remove)?;
                self.after_edit(&[id])?;
                self.refresh(id)
            }
            Command::Analyze { batch } => self.analyze(batch, events).map(Response::Analyze),
            Command::Reindex => Ok(Response::Reindexed { photos: self.reindex(events)? }),
            Command::Stats => Ok(Response::Stats(Stats {
                library: self.dir.display().to_string(),
                photos: self.catalog.count()?,
                indexed: self.index.doc_count(),
                folders: self.catalog.roots()?,
                folder_kinds: self.catalog.root_kinds()?.into_iter().map(|(_, k)| k).collect(),
                unanalyzed: self.catalog.count_needing_any_analysis(
                    ANALYSIS_VERSION,
                    SHARPNESS_VERSION,
                    self.aesthetic_version()?,
                )?,
                analysis_steps: self.catalog.count_needing_analysis(ANALYSIS_VERSION)?
                    + self.catalog.count_needing_quality(SHARPNESS_VERSION, self.aesthetic_version()?)?,
                smart_ready: SMART_MODELS.iter().all(|m| self.models.is_installed(m)),
                smart_download_bytes: SMART_MODELS
                    .iter()
                    .filter(|m| !self.models.is_installed(m))
                    .map(|m| m.bytes)
                    .sum(),
                taste_ratings: self.catalog.user_ratings()?.len() as u64,
                taste_needed: MIN_RATINGS as u64,
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
                self.before_edit(&photos)?;
                self.catalog.name_faces(&faces, &name)?;
                self.after_edit(&photos)?;
                self.after_people_edit(photos)
            }
            Command::RejectFace { face } => {
                let photos = self.catalog.photos_of_faces(&[face])?;
                self.before_edit(&photos)?;
                self.catalog.reject_face(face)?;
                self.after_edit(&photos)?;
                self.after_people_edit(photos)
            }
            Command::UngroupFace { face } => {
                self.catalog.ungroup_face(face)?;
                self.after_people_edit(Vec::new())
            }
            Command::RenamePerson { person, name } => {
                let photos = self.catalog.photos_of_person(person)?;
                self.before_edit(&photos)?;
                self.catalog.rename_person(person, &name)?;
                self.after_edit(&photos)?;
                self.after_people_edit(photos)
            }
            Command::DeletePerson { person } => {
                let photos = self.catalog.photos_of_person(person)?;
                self.before_edit(&photos)?;
                self.catalog.delete_person(person)?;
                self.after_edit(&photos)?;
                self.after_people_edit(photos)
            }
            Command::Settings { write_sidecars, aesthetic } => {
                if let Some(on) = aesthetic {
                    self.catalog.set_setting(AESTHETIC_SETTING, if on { "1" } else { "0" })?;
                    if !on {
                        self.aesthetic_scorer = None;
                    }
                }
                let mut sidecars = None;
                if let Some(on) = write_sidecars {
                    let was_on = self.write_sidecars_on()?;
                    self.catalog.set_setting(WRITE_SIDECARS, if on { "1" } else { "0" })?;
                    if on && !was_on {
                        // Catch up: write sidecars for everything edited while it was off.
                        let roots: Vec<PathBuf> = self.catalog.roots()?.into_iter().map(PathBuf::from).collect();
                        let mode = tintype_jobs::SyncMode::CatchUp;
                        let mut report = tintype_jobs::sync_sidecars(&mut self.catalog, &roots, mode)?;
                        self.reindex_photos(std::mem::take(&mut report.changed_photos))?;
                        sidecars = Some(report);
                    }
                }
                Ok(Response::Settings {
                    write_sidecars: self.write_sidecars_on()?,
                    aesthetic: self.aesthetic_version()?.is_some(),
                    aesthetic_download_bytes: AESTHETIC_MODELS
                        .iter()
                        .filter(|m| !self.models.is_installed(m))
                        .map(|m| m.bytes)
                        .sum(),
                    sidecars,
                })
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

    fn write_sidecars_on(&self) -> Result<bool> {
        Ok(self.catalog.setting(WRITE_SIDECARS)?.is_none_or(|v| v != "0"))
    }

    /// Before an edit: picks up sidecars changed by other apps, so the edit
    /// builds on them instead of overwriting them.
    fn before_edit(&mut self, photos: &[i64]) -> Result<()> {
        let report = tintype_jobs::import_changed(&mut self.catalog, photos)?;
        self.reindex_photos(report.changed_photos)?;
        Ok(())
    }

    /// After an edit: writes the photos' sidecars, if sidecars are on.
    fn after_edit(&mut self, photos: &[i64]) -> Result<()> {
        if self.write_sidecars_on()? {
            tintype_jobs::write_sidecars(&mut self.catalog, photos)?;
        }
        Ok(())
    }

    /// Downloads every smart search model, so analysis and search both work offline afterwards.
    fn ensure_smart_models(&self, events: &mut dyn FnMut(Event)) -> Result<()> {
        for spec in SMART_MODELS {
            let mut progress = |done, total| events(Event::Download { model: spec.name, done, total });
            self.models.ensure(&spec, &mut progress)?;
        }
        Ok(())
    }

    fn image_encoder(&mut self, events: &mut dyn FnMut(Event)) -> Result<Arc<ImageEncoder>> {
        if let Some(e) = &self.image_encoder {
            return Ok(e.clone());
        }
        self.ensure_smart_models(events)?;
        let encoder = Arc::new(ImageEncoder::load(&self.models.path(&SIGLIP_VISION))?);
        self.image_encoder = Some(encoder.clone());
        Ok(encoder)
    }

    fn text_encoder(&mut self, events: &mut dyn FnMut(Event)) -> Result<&TextEncoder> {
        if self.text_encoder.is_none() {
            self.ensure_smart_models(events)?;
            let (text, tokenizer) = (self.models.path(&SIGLIP_TEXT), self.models.path(&SIGLIP_TOKENIZER));
            self.text_encoder = Some(TextEncoder::load(&text, &tokenizer)?);
        }
        Ok(self.text_encoder.as_ref().expect("loaded above"))
    }

    /// The aesthetic score's version when it is turned on.
    fn aesthetic_version(&self) -> Result<Option<u32>> {
        Ok((self.catalog.setting(AESTHETIC_SETTING)?.as_deref() == Some("1")).then_some(AESTHETIC_VERSION))
    }

    fn aesthetic_scorer(&mut self, events: &mut dyn FnMut(Event)) -> Result<Arc<AestheticScorer>> {
        if let Some(s) = &self.aesthetic_scorer {
            return Ok(s.clone());
        }
        for spec in AESTHETIC_MODELS {
            let mut progress = |done, total| events(Event::Download { model: spec.name, done, total });
            self.models.ensure(&spec, &mut progress)?;
        }
        let scorer = Arc::new(AestheticScorer::load(
            &self.models.path(&CLIP_L14_VISION),
            &self.models.path(&LAION_AESTHETIC_HEAD),
        )?);
        self.aesthetic_scorer = Some(scorer.clone());
        Ok(scorer)
    }

    /// One batch of analysis: smart search embeddings first, then quality
    /// (sharpness, and the aesthetic score when it is on). `remaining`
    /// counts the work left in both passes.
    fn analyze(&mut self, batch: usize, events: &mut dyn FnMut(Event)) -> Result<AnalyzeReport> {
        let aesthetic = self.aesthetic_version()?;
        let mut report = if self.catalog.count_needing_analysis(ANALYSIS_VERSION)? > 0 {
            let encoder = self.image_encoder(events)?;
            let mut progress = |done, total| events(Event::Analyze { done, total });
            let r = tintype_jobs::analyze(&mut self.catalog, &encoder, batch.max(1), &self.cancel, &mut progress)?;
            if r.processed > 0 {
                self.embeddings = None;
                self.taste = None;
            }
            r
        } else {
            let scorer = match aesthetic {
                Some(_) => Some(self.aesthetic_scorer(events)?),
                None => None,
            };
            let mut progress = |done, total| events(Event::Analyze { done, total });
            let q = tintype_jobs::measure_quality(
                &mut self.catalog,
                scorer.as_deref(),
                batch.max(1),
                &self.cancel,
                &mut progress,
            )?;
            if q.processed > 0 && scorer.is_some() {
                self.taste = None;
            }
            let pending = self.catalog.index_pending()?;
            self.reindex_photos(pending)?;
            AnalyzeReport {
                processed: q.processed,
                failed: q.failed,
                remaining: q.remaining,
                cancelled: q.cancelled,
                elapsed_ms: q.elapsed_ms,
            }
        };
        report.remaining = self.catalog.count_needing_analysis(ANALYSIS_VERSION)?
            + self.catalog.count_needing_quality(SHARPNESS_VERSION, aesthetic)?;
        // Bursts are found by looks, and covers prefer the sharpest frame: stack again once the work is done.
        if report.processed > 0 && (report.remaining == 0 || report.cancelled) {
            self.restack()?;
        }
        Ok(report)
    }

    /// The user's learned taste, fitted on their ratings the first time it is needed after a change.
    fn taste_model(&mut self) -> Result<Option<&TasteModel>> {
        if self.taste.is_none() {
            if self.embeddings.is_none() {
                self.embeddings = Some(self.catalog.embeddings()?);
            }
            let embeddings: HashMap<i64, &Vec<f32>> =
                self.embeddings.iter().flatten().map(|(id, v)| (*id, v)).collect();
            let aesthetic = self.catalog.aesthetic_scores()?;
            let samples: Vec<(Vec<f32>, f32)> = self
                .catalog
                .user_ratings()?
                .into_iter()
                .filter_map(|(id, r)| {
                    embeddings.get(&id).map(|e| (taste_features(e, aesthetic.get(&id).copied()), f32::from(r)))
                })
                .collect();
            self.taste = Some(TasteModel::fit(&samples));
        }
        Ok(self.taste.as_ref().and_then(Option::as_ref))
    }

    /// What the learned taste predicts for each photo with an embedding.
    fn taste_predictions(&mut self) -> Result<HashMap<i64, f32>> {
        let aesthetic = self.catalog.aesthetic_scores()?;
        let Some(model) = self.taste_model()?.cloned() else { return Ok(HashMap::new()) };
        Ok(self
            .embeddings
            .iter()
            .flatten()
            .map(|(id, e)| (*id, model.predict(&taste_features(e, aesthetic.get(id).copied()))))
            .collect())
    }

    /// Photos matching `query`, the ones the user would like most first;
    /// photos the model can't judge (not analyzed) come last, newest first.
    fn taste_ranked(&mut self, query: &str) -> Result<Vec<i64>> {
        let predicted = self.taste_predictions()?;
        let mut ids = self.index.ordered_ids(query, Sort::Newest)?;
        // Stable: unjudged photos keep their newest-first order at the end.
        ids.sort_by(|a, b| {
            let (pa, pb) = (predicted.get(a), predicted.get(b));
            pb.unwrap_or(&f32::MIN).total_cmp(pa.unwrap_or(&f32::MIN))
        });
        Ok(ids)
    }

    /// Adds the learned taste's suggestion to a record shown to the user.
    fn with_suggestion(&mut self, mut rec: PhotoRecord) -> Result<PhotoRecord> {
        if self.taste_model()?.is_some() {
            if self.embeddings.is_none() {
                self.embeddings = Some(self.catalog.embeddings()?);
            }
            let model = self.taste_model()?.cloned().expect("checked above");
            let emb = self.embeddings.iter().flatten().find(|(id, _)| *id == rec.id).map(|(_, e)| e);
            rec.suggested_rating = emb.map(|e| model.predict(&taste_features(e, rec.aesthetic)));
        }
        Ok(rec)
    }

    /// Ranks photos that pass the query's field terms by how well they match its words.
    fn smart_ranked(&mut self, query: &str, events: &mut dyn FnMut(Event)) -> Result<Vec<i64>> {
        let (filters, words) = tintype_index::split_smart(query);
        let target = self.text_encoder(events)?.embed_query(&words)?;
        let allowed = self.index.matching_ids(&filters)?;
        if self.embeddings.is_none() {
            self.embeddings = Some(self.catalog.embeddings()?);
        }
        let min = tintype_analyze::similarity_for_probability(SMART_MIN_PROBABILITY);
        let scored = rank(self.embeddings.as_deref().unwrap_or_default(), &allowed, &target, min, SMART_WINDOW);
        Ok(scored.into_iter().map(|(_, id)| id).collect())
    }

    /// One page of `ordered` results, stacked if asked.
    fn page(&mut self, ordered: Vec<i64>, stacks: bool, limit: usize, offset: usize) -> Result<SearchResults> {
        let entries = if stacks {
            if self.stack_places.is_none() {
                self.stack_places = Some(self.catalog.stack_places()?);
            }
            collapse(&ordered, self.stack_places.as_ref().expect("loaded above"))
        } else {
            ordered.iter().map(|&id| (id, None, 1)).collect()
        };
        let page = &entries[offset.min(entries.len())..(offset.saturating_add(limit)).min(entries.len())];
        let ids: Vec<i64> = page.iter().map(|e| e.0).collect();
        let paths: HashMap<i64, String> = self.catalog.paths(&ids)?.into_iter().collect();
        let hits = page
            .iter()
            .filter_map(|&(id, stack, stack_size)| {
                paths.get(&id).map(|path| Hit { id, path: path.clone(), stack, stack_size })
            })
            .collect();
        Ok(SearchResults { total: entries.len(), hits })
    }

    /// Groups the whole library into stacks again.
    fn restack(&mut self) -> Result<()> {
        let changed = tintype_jobs::restack(&mut self.catalog)?;
        self.stack_places = None;
        self.reindex_photos(changed)?;
        Ok(())
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
        if ids.is_empty() {
            return Ok(0);
        }
        ids.sort_unstable();
        ids.dedup();
        for &id in &ids {
            if let Some(rec) = self.catalog.get(id)? {
                self.index.upsert([&rec])?;
            }
        }
        self.index.commit()?;
        self.catalog.clear_index_pending(Some(&ids))?;
        Ok(ids.len() as u64)
    }

    fn scan(&mut self, roots: &[PathBuf], hash: bool, events: &mut dyn FnMut(Event)) -> Result<ScanReport> {
        let folder_kinds =
            self.catalog.root_kinds()?.into_iter().map(|(path, kinds)| (PathBuf::from(path), kinds)).collect();
        let opts = ScanOptions { hash, folder_kinds, ..ScanOptions::default() };
        let mut report =
            tintype_jobs::scan(&mut self.catalog, roots, &opts, &self.cancel, &mut |p| events(Event::Scan(p)))?;
        self.catalog.add_roots(&report.roots)?;
        if !report.cancelled {
            let scanned: Vec<PathBuf> = report.roots.iter().map(PathBuf::from).collect();
            let mode = if self.write_sidecars_on()? { SyncMode::Write } else { SyncMode::ReadOnly };
            report.sidecars = tintype_jobs::sync_sidecars(&mut self.catalog, &scanned, mode)?;
            report.changed_ids.append(&mut report.sidecars.changed_photos);
            report.changed_ids.sort_unstable();
            report.changed_ids.dedup();
        }
        self.embeddings = None;
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
        self.catalog.clear_index_pending(Some(&report.changed_ids))?;
        if report.added + report.updated + report.removed + report.moved > 0 {
            self.taste = None;
            self.restack()?;
        }
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
        self.catalog.clear_index_pending(None)?;
        events(Event::Reindex { done, total });
        Ok(done)
    }

    /// Re-indexes one photo after a metadata edit and returns it.
    fn refresh(&mut self, id: i64) -> Result<Response> {
        let rec = self.catalog.get(id)?.ok_or_else(|| Error::PhotoNotFound(id.to_string()))?;
        self.index.upsert([&rec])?;
        self.index.commit()?;
        Ok(Response::Photo(Box::new(self.with_suggestion(rec)?)))
    }

    fn resolve(&self, photo: &PhotoRef) -> Result<PhotoRecord> {
        let found = match photo {
            PhotoRef::Id(id) => self.catalog.get(*id)?,
            PhotoRef::Path(p) => {
                // Catalog paths are canonical; accept relative or differently-spelled ones.
                let canonical = tintype_jobs::normalize_path(p).unwrap_or_else(|_| p.clone());
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

/// One entry per stack in `ordered` (at the position of its first match),
/// shown by its cover if the cover matched; unstacked photos pass through.
/// Entries are (photo, stack, matching photos in the stack).
fn collapse(ordered: &[i64], places: &HashMap<i64, StackPlace>) -> Vec<(i64, Option<i64>, u32)> {
    let mut matched: HashMap<i64, (u32, Option<i64>)> = HashMap::new();
    for id in ordered {
        if let Some(p) = places.get(id) {
            let e = matched.entry(p.stack).or_default();
            e.0 += 1;
            if p.cover {
                e.1 = Some(*id);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ordered.len());
    for &id in ordered {
        match places.get(&id) {
            None => out.push((id, None, 1)),
            Some(p) if seen.insert(p.stack) => {
                let (n, cover) = matched[&p.stack];
                out.push((cover.unwrap_or(id), Some(p.stack), n));
            }
            Some(_) => {}
        }
    }
    out
}

/// Photos in `allowed` (sorted ids) whose embedding is at least `min` similar
/// to `target` and within `window` of the best, best first, as (similarity, id).
fn rank(embeddings: &[(i64, Vec<f32>)], allowed: &[i64], target: &[f32], min: f32, window: f32) -> Vec<(f32, i64)> {
    let mut scored: Vec<(f32, i64)> = embeddings
        .iter()
        .filter(|(id, _)| allowed.binary_search(id).is_ok())
        .map(|(id, v)| (tintype_analyze::similarity(v, target), *id))
        .filter(|(s, _)| *s >= min)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    if let Some(&(best, _)) = scored.first() {
        scored.retain(|(s, _)| *s >= best - window);
    }
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_testkit::JpegSpec;

    fn hits(engine: &mut Engine, query: &str) -> Vec<String> {
        let cmd = Command::Search {
            query: query.into(),
            smart: false,
            stacks: false,
            sort: Sort::Newest,
            limit: 50,
            offset: 0,
        };
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
        let scan = Command::Scan { roots: vec![photos.path().into()], hash: true, kinds: None };
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
    fn rank_filters_thresholds_and_orders() {
        let embs = vec![(1, vec![1.0, 0.0]), (2, vec![0.6, 0.8]), (3, vec![0.0, 1.0]), (4, vec![0.8, 0.6])];
        let ranked = rank(&embs, &[1, 2, 3], &[0.8, 0.6], 0.7, 1.0);
        assert_eq!(
            ranked.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            vec![2, 1],
            "best first; 4 filtered out, 3 too far"
        );
        let ranked = rank(&embs, &[1, 2, 3], &[0.8, 0.6], 0.0, 0.2);
        assert_eq!(ranked.len(), 2, "3 (0.6) is more than 0.2 behind the best (0.96)");
    }

    #[test]
    fn smart_search_without_words_is_a_normal_search() {
        let photos = tempfile::tempdir().unwrap();
        JpegSpec::canon().write(&photos.path().join("canon.jpg"));
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        engine
            .execute(Command::Scan { roots: vec![photos.path().into()], hash: false, kinds: None }, &mut |_| {})
            .unwrap();
        // No description words: no models are needed.
        let cmd = Command::Search {
            query: "iso:>=1000".into(),
            smart: true,
            stacks: true,
            sort: Sort::Newest,
            limit: 5,
            offset: 0,
        };
        let Response::Search(r) = engine.execute(cmd, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(r.total, 1);
        let Response::Stats(stats) = engine.execute(Command::Stats, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(stats.unanalyzed, 1);
    }

    #[test]
    fn sidecars_are_imported_and_written() {
        let photos = tempfile::tempdir().unwrap();
        let (a, b) = (photos.path().join("a.jpg"), photos.path().join("b.jpg"));
        JpegSpec::canon().write(&a);
        JpegSpec::nikon().write(&b);
        let sidecar =
            tintype_meta::xmp::Sidecar { rating: Some(4), tags: vec!["Barn".into()], people: vec!["Grace".into()] };
        tintype_meta::xmp::write_sidecar(&a, &sidecar).unwrap();
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        let Response::Scan(r) = engine
            .execute(Command::Scan { roots: vec![photos.path().into()], hash: true, kinds: None }, &mut |_| {})
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(r.sidecars.imported, 1);
        assert_eq!(hits(&mut engine, "tag:barn rating:4 person:grace"), vec!["a.jpg"]);

        // A rating in Tintype lands in b's (new) sidecar.
        engine.execute(Command::Rate { photo: PhotoRef::Path(b.clone()), rating: Some(5) }, &mut |_| {}).unwrap();
        assert_eq!(tintype_meta::xmp::read_sidecar(&b).unwrap().unwrap().rating, Some(5));

        // With writing off, edits stay in the catalog.
        let off = Command::Settings { write_sidecars: Some(false), aesthetic: None };
        let Response::Settings { write_sidecars, .. } = engine.execute(off, &mut |_| {}).unwrap() else { panic!() };
        assert!(!write_sidecars);
        engine.execute(Command::Rate { photo: PhotoRef::Path(b.clone()), rating: Some(1) }, &mut |_| {}).unwrap();
        assert_eq!(tintype_meta::xmp::read_sidecar(&b).unwrap().unwrap().rating, Some(5));

        // Turning it back on catches up.
        let on = Command::Settings { write_sidecars: Some(true), aesthetic: None };
        engine.execute(on, &mut |_| {}).unwrap();
        assert_eq!(tintype_meta::xmp::read_sidecar(&b).unwrap().unwrap().rating, Some(1));
    }

    #[test]
    fn interrupted_index_updates_finish_on_open() {
        let photos = tempfile::tempdir().unwrap();
        let a = photos.path().join("a.jpg");
        JpegSpec::canon().write(&a);
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        engine
            .execute(Command::Scan { roots: vec![photos.path().into()], hash: false, kinds: None }, &mut |_| {})
            .unwrap();
        drop(engine);

        // A sidecar import that reached the catalog but not the index (app closed midway).
        let mut cat = Catalog::open(&lib.path().join("catalog.db")).unwrap();
        let id = cat.get_by_path(&tintype_jobs::normalize_path(&a).unwrap().display().to_string()).unwrap().unwrap().id;
        let sidecar = tintype_meta::xmp::Sidecar { tags: vec!["barn".into()], ..Default::default() };
        cat.apply_sidecar(id, &sidecar, tintype_catalog::SidecarStamp { mtime_ns: 1, size: 1 }).unwrap();
        drop(cat);

        let mut engine = Engine::open(lib.path()).unwrap();
        assert_eq!(hits(&mut engine, "tag:barn"), vec!["a.jpg"]);
    }

    #[test]
    fn collapse_shows_one_per_stack() {
        let places: HashMap<i64, StackPlace> = [
            (2, StackPlace { stack: 2, cover: false }),
            (3, StackPlace { stack: 2, cover: true }),
            (5, StackPlace { stack: 5, cover: true }),
            (6, StackPlace { stack: 5, cover: false }),
        ]
        .into();
        // 6 matched but its cover 5 didn't: 6 stands in for the stack.
        assert_eq!(
            collapse(&[1, 2, 4, 3, 6], &places),
            vec![(1, None, 1), (3, Some(2), 2), (4, None, 1), (6, Some(5), 1)]
        );
    }

    #[test]
    fn raw_and_jpg_of_one_shot_stack() {
        let photos = tempfile::tempdir().unwrap();
        // Same camera and second: a "RAF" (same bytes, different name) and its JPG.
        JpegSpec::canon().write(&photos.path().join("IMG_1.JPG"));
        std::fs::copy(photos.path().join("IMG_1.JPG"), photos.path().join("IMG_1-edit.jpg")).unwrap();
        JpegSpec::nikon().write(&photos.path().join("other.jpg"));
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        engine
            .execute(Command::Scan { roots: vec![photos.path().into()], hash: true, kinds: None }, &mut |_| {})
            .unwrap();

        let stacked = Command::Search {
            query: String::new(),
            smart: false,
            stacks: true,
            sort: Sort::Newest,
            limit: 10,
            offset: 0,
        };
        let Response::Search(r) = engine.execute(stacked, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(r.total, 2);
        let stack = r.hits.iter().find(|h| h.stack_size == 2).expect("a stack of two").stack.unwrap();
        assert_eq!(hits(&mut engine, &format!("stack:{stack}")).len(), 2, "the stack opens with stack:<id>");
        assert_eq!(hits(&mut engine, "").len(), 3, "unstacked shows all");
    }

    #[test]
    fn learned_taste_ranks_best_first_and_suggests_ratings() {
        // 36 rated photos whose "look" (a synthetic embedding at angle a) predicts
        // the rating, plus two unrated ones the user should love and dislike.
        let photos = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..38).map(|i| format!("p{i:02}.jpg")).collect();
        for n in &names {
            JpegSpec { taken: None, ..JpegSpec::canon() }.write(&photos.path().join(n));
        }
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        engine
            .execute(Command::Scan { roots: vec![photos.path().into()], hash: false, kinds: None }, &mut |_| {})
            .unwrap();
        let id = |engine: &Engine, n: &str| {
            let p = tintype_jobs::normalize_path(&photos.path().join(n)).unwrap();
            engine.catalog.get_by_path(&p.display().to_string()).unwrap().unwrap().id
        };
        let quarter = std::f32::consts::FRAC_PI_2;
        let angle = |i: usize| match i {
            36 => 0.03,
            37 => 1.5,
            _ => i as f32 / 36.0 * quarter,
        };
        let ids: Vec<i64> = names.iter().map(|n| id(&engine, n)).collect();
        for (i, &pid) in ids.iter().enumerate() {
            let a = angle(i);
            engine.catalog.set_embedding(pid, Some(&[a.cos(), a.sin(), 0.0]), ANALYSIS_VERSION).unwrap();
        }
        // As an analysis pass would.
        (engine.embeddings, engine.taste) = (None, None);
        for (i, &pid) in ids.iter().enumerate().take(36) {
            let rating = (5.0 - 4.0 * angle(i) / quarter).round() as u8;
            engine.execute(Command::Rate { photo: PhotoRef::Id(pid), rating: Some(rating) }, &mut |_| {}).unwrap();
        }

        let Response::Stats(stats) = engine.execute(Command::Stats, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!((stats.taste_ratings, stats.taste_needed), (36, MIN_RATINGS as u64));
        let (loved, disliked) = (id(&engine, "p36.jpg"), id(&engine, "p37.jpg"));
        let show = |engine: &mut Engine, pid| {
            let Response::Photo(p) = engine.execute(Command::Show { photo: PhotoRef::Id(pid) }, &mut |_| {}).unwrap()
            else {
                panic!()
            };
            p.suggested_rating.unwrap()
        };
        assert!(show(&mut engine, loved) > 4.0 && show(&mut engine, disliked) < 2.5);

        let best = Command::Search {
            query: String::new(),
            smart: false,
            stacks: false,
            sort: Sort::Best,
            limit: 50,
            offset: 0,
        };
        let Response::Search(r) = engine.execute(best, &mut |_| {}).unwrap() else { panic!() };
        let pos = |pid| r.hits.iter().position(|h| h.id == pid).unwrap();
        assert!(pos(loved) < 5 && pos(disliked) > 30, "{} {}", pos(loved), pos(disliked));
    }

    #[test]
    fn a_folder_can_be_jpeg_only() {
        let photos = tempfile::tempdir().unwrap();
        JpegSpec::canon().write(&photos.path().join("a.jpg"));
        std::fs::write(photos.path().join("a.RAF"), JpegSpec::canon().to_raf_bytes()).unwrap();
        let lib = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(lib.path()).unwrap();
        let scan = Command::Scan { roots: vec![photos.path().into()], hash: false, kinds: Some(vec![FileKind::Jpeg]) };
        engine.execute(scan, &mut |_| {}).unwrap();
        assert_eq!(hits(&mut engine, ""), vec!["a.jpg"]);
        // Rescans remember the choice.
        engine.execute(Command::Rescan { hash: false }, &mut |_| {}).unwrap();
        assert_eq!(hits(&mut engine, ""), vec!["a.jpg"]);
        let Response::Stats(s) = engine.execute(Command::Stats, &mut |_| {}).unwrap() else { panic!() };
        assert_eq!(s.folder_kinds, vec![Some(vec![FileKind::Jpeg])]);
        // Adding the folder again with every kind brings the RAF in.
        let all = Command::Scan { roots: vec![photos.path().into()], hash: false, kinds: Some(FileKind::ALL.to_vec()) };
        engine.execute(all, &mut |_| {}).unwrap();
        assert_eq!(hits(&mut engine, "").len(), 2);
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
