//! The catalog: every known photo, its extracted metadata and the user's own
//! metadata (ratings, tags). This is the source of truth; the search index is
//! derived from it and can always be rebuilt.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use tintype_meta::PhotoMeta;

mod embeddings;
mod faces;
mod sidecars;
pub use faces::{FaceGroup, FaceInfo, FaceRow, NewFace, Person, Suggestion};
pub use sidecars::SidecarStamp;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("corrupt metadata for photo {id}: {source}")]
    Json { id: i64, source: serde_json::Error },
    #[error("rating must be 1-5, got {0}")]
    BadRating(u8),
    #[error("catalog schema version {0} is newer than this build supports")]
    TooNew(i64),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Schema migrations; entry `i` upgrades version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE photos (
        id         INTEGER PRIMARY KEY,
        path       TEXT    NOT NULL UNIQUE,
        size       INTEGER NOT NULL,
        mtime_ns   INTEGER NOT NULL,
        hash       TEXT,
        meta       TEXT    NOT NULL,
        error      TEXT,
        rating     INTEGER,
        indexed_at INTEGER NOT NULL
    );
    CREATE INDEX photos_hash ON photos(hash);
    CREATE TABLE photo_tags (
        photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
        tag      TEXT    NOT NULL,
        PRIMARY KEY (photo_id, tag)
    );
"#,
    r#"
    ALTER TABLE photos ADD COLUMN meta_version INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
    CREATE TABLE roots (path TEXT PRIMARY KEY);
"#,
    r#"
    CREATE TABLE people (
        id   INTEGER PRIMARY KEY,
        name TEXT    NOT NULL UNIQUE COLLATE NOCASE
    );
    CREATE TABLE faces (
        id        INTEGER PRIMARY KEY,
        photo_id  INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
        x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
        score     REAL    NOT NULL,
        embedding BLOB    NOT NULL,
        person_id INTEGER REFERENCES people(id) ON DELETE SET NULL,
        -- 1: the user named this face; 0: person_id (if any) is a suggestion.
        confirmed INTEGER NOT NULL DEFAULT 0,
        rejected_person_id INTEGER REFERENCES people(id) ON DELETE SET NULL,
        -- 1: the user took this face out of its group; don't group it again.
        ungrouped INTEGER NOT NULL DEFAULT 0,
        grp       INTEGER
    );
    CREATE INDEX faces_photo ON faces(photo_id);
    CREATE INDEX faces_person ON faces(person_id);
    CREATE INDEX faces_grp ON faces(grp);
    ALTER TABLE photos ADD COLUMN faces_version INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
    -- How well a suggested face matched its person (higher is more likely).
    ALTER TABLE faces ADD COLUMN suggest_score REAL;
"#,
    r#"
    -- Image embeddings for semantic search; one per analyzed photo.
    CREATE TABLE embeddings (
        photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
        vector   BLOB NOT NULL
    );
    ALTER TABLE photos ADD COLUMN analysis_version INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
    -- People named in a photo's XMP sidecar who aren't on a confirmed face.
    CREATE TABLE photo_names (
        photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
        name     TEXT    NOT NULL,
        PRIMARY KEY (photo_id, name)
    );
    -- The sidecar as Tintype last read or wrote it; NULL: none.
    ALTER TABLE photos ADD COLUMN sidecar_mtime_ns INTEGER;
    ALTER TABLE photos ADD COLUMN sidecar_size INTEGER;
    CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#,
    r#"
    -- Network paths were stored as \\?\UNC\server\share\...; store \\server\share\...
    UPDATE OR IGNORE photos SET path = '\\' || substr(path, 9) WHERE path LIKE '\\?\UNC\%';
    UPDATE OR IGNORE roots SET path = '\\' || substr(path, 9) WHERE path LIKE '\\?\UNC\%';
"#,
    r#"
    -- Photos changed in the catalog whose search index entry may not be updated yet.
    CREATE TABLE index_pending (photo_id INTEGER PRIMARY KEY);
"#,
];

/// A photo as produced by a scan, before it has an id.
#[derive(Debug, Clone)]
pub struct NewPhoto {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: Option<String>,
    pub meta: PhotoMeta,
    /// [`tintype_meta::EXTRACTOR_VERSION`] that produced `meta`.
    pub meta_version: u32,
    /// Why metadata could not be read, if it couldn't. The photo is still cataloged.
    pub error: Option<String>,
}

/// What a scan needs to decide whether a file changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub id: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub meta_version: u32,
    /// Content hash, if one was computed; lets a scan recognize moved files.
    pub hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhotoRecord {
    pub id: i64,
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: Option<String>,
    pub meta: PhotoMeta,
    pub error: Option<String>,
    pub rating: Option<u8>,
    pub tags: Vec<String>,
    /// Names of the people in this photo (confirmed or suggested), sorted.
    pub people: Vec<String>,
    /// Faces found in this photo.
    pub faces: u32,
    pub indexed_at: i64,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let target = MIGRATIONS.len() as i64;
        if version > target {
            return Err(Error::TooNew(version));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))?;
        }
        Ok(Self { conn })
    }

    /// Fingerprints of every cataloged photo, keyed by path.
    pub fn fingerprints(&self) -> Result<HashMap<String, Fingerprint>> {
        let mut stmt = self.conn.prepare("SELECT id, path, size, mtime_ns, meta_version, hash FROM photos")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                Fingerprint {
                    id: r.get(0)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime_ns: r.get(3)?,
                    meta_version: r.get(4)?,
                    hash: r.get::<_, Option<String>>(5)?.as_deref().and_then(parse_hash),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Inserts or updates photos by path in one transaction. Ratings and tags of
    /// existing photos are kept, and so is the hash when `hash` is `None` and the
    /// file's size and mtime are unchanged. Returns ids in input order.
    pub fn upsert_many(&mut self, photos: &[NewPhoto]) -> Result<Vec<i64>> {
        let now = unix_now();
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(photos.len());
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO photos (path, size, mtime_ns, hash, meta, meta_version, error, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET
                    -- Same size and mtime means same content: keep a hash we didn't recompute.
                    hash = CASE WHEN excluded.size = photos.size AND excluded.mtime_ns = photos.mtime_ns
                                THEN COALESCE(excluded.hash, photos.hash) ELSE excluded.hash END,
                    -- Changed pixels need new face and image analysis passes.
                    faces_version = CASE WHEN excluded.size = photos.size AND excluded.mtime_ns = photos.mtime_ns
                                THEN photos.faces_version ELSE 0 END,
                    analysis_version = CASE WHEN excluded.size = photos.size AND excluded.mtime_ns = photos.mtime_ns
                                THEN photos.analysis_version ELSE 0 END,
                    size = excluded.size, mtime_ns = excluded.mtime_ns,
                    meta = excluded.meta, meta_version = excluded.meta_version,
                    error = excluded.error, indexed_at = excluded.indexed_at
                 RETURNING id",
            )?;
            for p in photos {
                let meta = serde_json::to_string(&p.meta).expect("PhotoMeta serializes");
                let id = stmt.query_row(
                    params![p.path, p.size as i64, p.mtime_ns, p.hash, meta, p.meta_version, p.error, now],
                    |r| r.get(0),
                )?;
                ids.push(id);
            }
        }
        mark_index_pending(&tx, &ids)?;
        tx.commit()?;
        Ok(ids)
    }

    /// Deletes photos (and their faces). Returns the ids of the deleted faces,
    /// so their crops can be cleaned up.
    pub fn remove_many(&mut self, ids: &[i64]) -> Result<Vec<i64>> {
        let faces = self.faces_of_photos(ids)?;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM photos WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(faces)
    }

    pub fn get(&self, id: i64) -> Result<Option<PhotoRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!("{SELECT_PHOTO} WHERE id = ?1"))?;
        let Some(mut raw) = stmt.query_row([id], read_row).optional()? else {
            return Ok(None);
        };
        raw.rec.tags = self.tags(id)?;
        (raw.rec.people, raw.rec.faces) = self.photo_people(id)?;
        merge_names(&mut raw.rec.people, self.photo_names(id)?);
        Ok(Some(raw.finish()?))
    }

    pub fn get_by_path(&self, path: &str) -> Result<Option<PhotoRecord>> {
        let id: Option<i64> =
            self.conn.query_row("SELECT id FROM photos WHERE path = ?1", [path], |r| r.get(0)).optional()?;
        id.map_or(Ok(None), |id| self.get(id))
    }

    /// Calls `f` with every photo, in id order.
    pub fn for_each(&self, mut f: impl FnMut(PhotoRecord)) -> Result<()> {
        let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT photo_id, tag FROM photo_tags ORDER BY tag")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, tag) = row?;
            tags.entry(id).or_default().push(tag);
        }
        let mut people = self.people_by_photo()?;
        let mut names = self.photo_names_by_photo()?;

        let mut stmt = self.conn.prepare(&format!("{SELECT_PHOTO} ORDER BY id"))?;
        for row in stmt.query_map([], read_row)? {
            let mut raw = row?;
            raw.rec.tags = tags.remove(&raw.rec.id).unwrap_or_default();
            (raw.rec.people, raw.rec.faces) = people.remove(&raw.rec.id).unwrap_or_default();
            merge_names(&mut raw.rec.people, names.remove(&raw.rec.id).unwrap_or_default());
            f(raw.finish()?);
        }
        Ok(())
    }

    pub fn count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Sets (1-5) or clears the rating. Returns false if the photo doesn't exist.
    pub fn set_rating(&self, id: i64, rating: Option<u8>) -> Result<bool> {
        if let Some(r) = rating
            && !(1..=5).contains(&r)
        {
            return Err(Error::BadRating(r));
        }
        Ok(self.conn.execute("UPDATE photos SET rating = ?2 WHERE id = ?1", params![id, rating])? > 0)
    }

    /// Adds and removes tags. Tags are trimmed; empty ones are ignored.
    pub fn edit_tags(&mut self, id: i64, add: &[String], remove: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for tag in remove.iter().map(|t| t.trim()) {
            tx.execute("DELETE FROM photo_tags WHERE photo_id = ?1 AND tag = ?2", params![id, tag])?;
        }
        for tag in add.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            tx.execute("INSERT OR IGNORE INTO photo_tags (photo_id, tag) VALUES (?1, ?2)", params![id, tag])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remembers folders the user added, so they can be rescanned together.
    pub fn add_roots(&self, roots: &[String]) -> Result<()> {
        for root in roots {
            self.conn.execute("INSERT OR IGNORE INTO roots (path) VALUES (?1)", [root])?;
        }
        Ok(())
    }

    pub fn roots(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM roots ORDER BY path")?;
        let roots = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(roots)
    }

    /// Photos changed by batch work (scans, sidecar imports) that the search
    /// index may not have caught up with, e.g. because the app was closed midway.
    pub fn index_pending(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT photo_id FROM index_pending ORDER BY photo_id")?;
        let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Call after the index has committed `ids` (`None`: everything).
    pub fn clear_index_pending(&mut self, ids: Option<&[i64]>) -> Result<()> {
        let tx = self.conn.transaction()?;
        match ids {
            None => {
                tx.execute("DELETE FROM index_pending", [])?;
            }
            Some(ids) => {
                let mut stmt = tx.prepare_cached("DELETE FROM index_pending WHERE photo_id = ?1")?;
                for id in ids {
                    stmt.execute([id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn tags(&self, id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT tag FROM photo_tags WHERE photo_id = ?1 ORDER BY tag")?;
        let tags = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(tags)
    }
}

const SELECT_PHOTO: &str = "SELECT id, path, size, mtime_ns, hash, meta, error, rating, indexed_at FROM photos";

/// A row with its metadata JSON not yet parsed (parsing errors aren't rusqlite errors).
struct RawRecord {
    rec: PhotoRecord,
    meta_json: String,
}

impl RawRecord {
    fn finish(mut self) -> Result<PhotoRecord> {
        self.rec.meta =
            serde_json::from_str(&self.meta_json).map_err(|source| Error::Json { id: self.rec.id, source })?;
        Ok(self.rec)
    }
}

fn read_row(r: &Row) -> rusqlite::Result<RawRecord> {
    Ok(RawRecord {
        rec: PhotoRecord {
            id: r.get(0)?,
            path: r.get(1)?,
            size: r.get::<_, i64>(2)? as u64,
            mtime_ns: r.get(3)?,
            hash: r.get(4)?,
            meta: PhotoMeta::default(),
            error: r.get(6)?,
            rating: r.get(7)?,
            tags: Vec::new(),
            people: Vec::new(),
            faces: 0,
            indexed_at: r.get(8)?,
        },
        meta_json: r.get(5)?,
    })
}

fn mark_index_pending(tx: &rusqlite::Transaction, ids: &[i64]) -> Result<()> {
    let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO index_pending (photo_id) VALUES (?1)")?;
    for id in ids {
        stmt.execute([id])?;
    }
    Ok(())
}

/// A 64-digit hex hash as bytes (kept compact: the scan holds one per photo).
fn parse_hash(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if hex.len() != 64 {
        return None;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Adds sidecar names to face names, sorted, without duplicates.
fn merge_names(people: &mut Vec<String>, names: Vec<String>) {
    if !names.is_empty() {
        people.extend(names);
        people.sort();
        people.dedup();
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(path: &str, iso: u32) -> NewPhoto {
        NewPhoto {
            path: path.into(),
            size: 100,
            mtime_ns: 1,
            hash: Some("abc".into()),
            meta: PhotoMeta { iso: Some(iso), ..PhotoMeta::default() },
            meta_version: 1,
            error: None,
        }
    }

    #[test]
    fn upsert_keeps_user_metadata() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let [id] = cat.upsert_many(&[photo("/a.jpg", 100)]).unwrap()[..] else { panic!() };
        assert!(cat.set_rating(id, Some(4)).unwrap());
        cat.edit_tags(id, &["family".into(), " beach ".into()], &[]).unwrap();

        let ids = cat.upsert_many(&[photo("/a.jpg", 200)]).unwrap();
        assert_eq!(ids, vec![id], "same path keeps its id");

        let rec = cat.get(id).unwrap().unwrap();
        assert_eq!(rec.meta.iso, Some(200));
        assert_eq!(rec.rating, Some(4));
        assert_eq!(rec.tags, vec!["beach", "family"]);
    }

    #[test]
    fn fingerprints_and_removal() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 2)]).unwrap();
        let fps = cat.fingerprints().unwrap();
        assert_eq!(fps["/b.jpg"], Fingerprint { id: ids[1], size: 100, mtime_ns: 1, meta_version: 1, hash: None });

        cat.edit_tags(ids[0], &["x".into()], &[]).unwrap();
        cat.remove_many(&ids[..1]).unwrap();
        assert_eq!(cat.count().unwrap(), 1);
        assert!(cat.get(ids[0]).unwrap().is_none());

        let mut seen = Vec::new();
        cat.for_each(|r| seen.push(r.path)).unwrap();
        assert_eq!(seen, vec!["/b.jpg"]);
    }

    #[test]
    fn hash_kept_only_for_unchanged_content() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let [id] = cat.upsert_many(&[photo("/a.jpg", 1)]).unwrap()[..] else { panic!() };
        cat.upsert_many(&[NewPhoto { hash: None, ..photo("/a.jpg", 1) }]).unwrap();
        assert_eq!(cat.get(id).unwrap().unwrap().hash.as_deref(), Some("abc"));
        cat.upsert_many(&[NewPhoto { hash: None, size: 5, ..photo("/a.jpg", 1) }]).unwrap();
        assert_eq!(cat.get(id).unwrap().unwrap().hash, None);
    }

    #[test]
    fn roots_are_remembered_once() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.add_roots(&["/b".into(), "/a".into()]).unwrap();
        cat.add_roots(&["/a".into()]).unwrap();
        assert_eq!(cat.roots().unwrap(), vec!["/a", "/b"]);
    }

    #[test]
    fn rejects_bad_rating() {
        let cat = Catalog::open_in_memory().unwrap();
        assert!(matches!(cat.set_rating(1, Some(6)), Err(Error::BadRating(6))));
        assert!(!cat.set_rating(1, Some(3)).unwrap(), "missing photo");
    }

    #[test]
    fn index_pending_until_cleared() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 1)]).unwrap();
        assert_eq!(cat.index_pending().unwrap(), ids);
        cat.clear_index_pending(Some(&ids[..1])).unwrap();
        assert_eq!(cat.index_pending().unwrap(), ids[1..]);
        cat.clear_index_pending(None).unwrap();
        assert!(cat.index_pending().unwrap().is_empty());
    }

    #[test]
    fn verbatim_network_paths_are_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let cat = Catalog::open(&path).unwrap();
        let mut cat = cat;
        cat.upsert_many(&[photo(r"\\?\UNC\nas\share\a.jpg", 1), photo(r"C:\b.jpg", 1)]).unwrap();
        cat.add_roots(&[r"\\?\UNC\nas\share".into()]).unwrap();
        // Back to the schema before the path fix (and the later index_pending table).
        cat.conn.execute("DROP TABLE index_pending", []).unwrap();
        cat.conn.pragma_update(None, "user_version", 7).unwrap();
        drop(cat);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.get_by_path(r"\\nas\share\a.jpg").unwrap().is_some());
        assert!(cat.get_by_path(r"C:\b.jpg").unwrap().is_some());
        assert_eq!(cat.roots().unwrap(), vec![r"\\nas\share".to_string()]);
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        Catalog::open(&path).unwrap().upsert_many(&[photo("/a.jpg", 1)]).unwrap();
        assert_eq!(Catalog::open(&path).unwrap().count().unwrap(), 1);
    }
}
