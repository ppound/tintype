//! What XMP sidecars need from the catalog: the metadata to write, applying
//! what a sidecar says, and the sidecar's state when Tintype last read or
//! wrote it (so a scan can tell whether something else changed it).
//!
//! People in a sidecar have no face positions. Names that don't match a
//! confirmed face are kept per photo (`photo_names`) and count as people in
//! that photo for search.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use tintype_meta::xmp::Sidecar;

use crate::{Catalog, Result};

/// A sidecar file's identity: modification time and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarStamp {
    pub mtime_ns: i64,
    pub size: u64,
}

impl Catalog {
    /// Tintype's metadata for a photo, as a sidecar holds it. People are
    /// confirmed faces plus names from earlier sidecars.
    pub fn sidecar_for(&self, photo_id: i64) -> Result<Sidecar> {
        let rating: Option<u8> = self
            .conn
            .query_row("SELECT rating FROM photos WHERE id = ?1", [photo_id], |r| r.get(0))
            .optional()?
            .flatten();
        let mut people: Vec<String> = self.confirmed_names(photo_id)?;
        people.extend(self.photo_names(photo_id)?);
        people.sort();
        people.dedup();
        Ok(Sidecar { rating, tags: self.tags(photo_id)?, people })
    }

    /// Makes the catalog agree with a sidecar: its rating and tags replace
    /// ours, and its people become this photo's names (except those already
    /// on a confirmed face). Records `stamp` as seen.
    pub fn apply_sidecar(&mut self, photo_id: i64, sidecar: &Sidecar, stamp: SidecarStamp) -> Result<()> {
        let confirmed: HashSet<String> =
            self.confirmed_names(photo_id)?.into_iter().map(|n| n.to_lowercase()).collect();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE photos SET rating = ?2, rating_source = CASE WHEN ?2 IS NULL THEN NULL ELSE 'sidecar' END WHERE id = ?1",
            params![photo_id, sidecar.rating],
        )?;
        tx.execute("DELETE FROM photo_tags WHERE photo_id = ?1", [photo_id])?;
        for tag in sidecar.tags.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            tx.execute("INSERT OR IGNORE INTO photo_tags (photo_id, tag) VALUES (?1, ?2)", params![photo_id, tag])?;
        }
        tx.execute("DELETE FROM photo_names WHERE photo_id = ?1", [photo_id])?;
        for name in sidecar.people.iter().map(|n| n.trim()) {
            if !name.is_empty() && !confirmed.contains(&name.to_lowercase()) {
                tx.execute(
                    "INSERT OR IGNORE INTO photo_names (photo_id, name) VALUES (?1, ?2)",
                    params![photo_id, name],
                )?;
            }
        }
        tx.execute(
            "UPDATE photos SET sidecar_mtime_ns = ?2, sidecar_size = ?3 WHERE id = ?1",
            params![photo_id, stamp.mtime_ns, stamp.size as i64],
        )?;
        crate::mark_index_pending(&tx, &[photo_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Records the sidecar as Tintype last saw it (`None`: there is none).
    pub fn set_sidecar_stamp(&self, photo_id: i64, stamp: Option<SidecarStamp>) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET sidecar_mtime_ns = ?2, sidecar_size = ?3 WHERE id = ?1",
            params![photo_id, stamp.map(|s| s.mtime_ns), stamp.map(|s| s.size as i64)],
        )?;
        Ok(())
    }

    /// Every photo with its recorded sidecar stamp, keyed by id: (path, stamp).
    pub fn sidecar_stamps(&self) -> Result<HashMap<i64, (String, Option<SidecarStamp>)>> {
        let mut stmt = self.conn.prepare("SELECT id, path, sidecar_mtime_ns, sidecar_size FROM photos")?;
        let rows = stmt.query_map([], |r| {
            let stamp = match (r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?) {
                (Some(mtime_ns), Some(size)) => Some(SidecarStamp { mtime_ns, size: size as u64 }),
                _ => None,
            };
            Ok((r.get(0)?, (r.get(1)?, stamp)))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Photos with a rating, tags, a confirmed face or a sidecar name.
    pub fn photos_with_user_metadata(&self) -> Result<HashSet<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM photos WHERE rating IS NOT NULL
             UNION SELECT photo_id FROM photo_tags
             UNION SELECT photo_id FROM faces WHERE confirmed = 1
             UNION SELECT photo_id FROM photo_names",
        )?;
        let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// A library setting, or `None` if never set.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Points a photo at its new path after it was moved or renamed, keeping
    /// everything known about it.
    pub fn relocate(&self, photo_id: i64, path: &str) -> Result<()> {
        self.conn.execute("UPDATE photos SET path = ?2 WHERE id = ?1", params![photo_id, path])?;
        Ok(())
    }

    fn confirmed_names(&self, photo_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT p.name FROM faces f JOIN people p ON p.id = f.person_id
             WHERE f.photo_id = ?1 AND f.confirmed = 1 ORDER BY p.name",
        )?;
        let names = stmt.query_map([photo_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(names)
    }

    pub(crate) fn photo_names(&self, photo_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT name FROM photo_names WHERE photo_id = ?1 ORDER BY name")?;
        let names = stmt.query_map([photo_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(names)
    }

    pub(crate) fn photo_names_by_photo(&self) -> Result<HashMap<i64, Vec<String>>> {
        let mut out: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT photo_id, name FROM photo_names ORDER BY name")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (photo, name) = row?;
            out.entry(photo).or_default().push(name);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewFace, NewPhoto};

    fn photo(path: &str) -> NewPhoto {
        NewPhoto {
            path: path.into(),
            size: 1,
            mtime_ns: 1,
            hash: None,
            meta: Default::default(),
            meta_version: 1,
            error: None,
        }
    }

    fn sc(rating: Option<u8>, tags: &[&str], people: &[&str]) -> Sidecar {
        Sidecar {
            rating,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            people: people.iter().map(|s| s.to_string()).collect(),
        }
    }

    const STAMP: SidecarStamp = SidecarStamp { mtime_ns: 7, size: 99 };

    #[test]
    fn sidecar_round_trip_through_the_catalog() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let [id] = cat.upsert_many(&[photo("/a.jpg")]).unwrap()[..] else { panic!() };
        assert!(cat.sidecar_for(id).unwrap().is_empty());
        assert!(cat.photos_with_user_metadata().unwrap().is_empty());

        // A confirmed face named Ada; the sidecar also names Ada and Grace.
        let nf = NewFace { bbox: [0.1, 0.1, 0.2, 0.2], score: 0.9, embedding: vec![1.0] };
        let (faces, _) = cat.replace_faces(id, &[nf], 1).unwrap();
        cat.name_faces(&faces, "Ada").unwrap();
        cat.apply_sidecar(id, &sc(Some(4), &["Barn", "Farm"], &["ada", "Grace"]), STAMP).unwrap();

        assert_eq!(cat.photo_names(id).unwrap(), vec!["Grace"], "Ada is already a confirmed face");
        assert_eq!(cat.sidecar_for(id).unwrap(), sc(Some(4), &["Barn", "Farm"], &["Ada", "Grace"]));
        let rec = cat.get(id).unwrap().unwrap();
        assert_eq!(rec.people, vec!["Ada", "Grace"], "sidecar names count for search");
        assert_eq!(cat.sidecar_stamps().unwrap()[&id], ("/a.jpg".to_string(), Some(STAMP)));
        assert_eq!(cat.photos_with_user_metadata().unwrap(), HashSet::from([id]));

        // A later sidecar replaces rating and tags.
        cat.apply_sidecar(id, &sc(None, &["Farm"], &[]), STAMP).unwrap();
        assert_eq!(cat.sidecar_for(id).unwrap(), sc(None, &["Farm"], &["Ada"]));
    }

    #[test]
    fn relocate_keeps_everything() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let [id] = cat.upsert_many(&[photo("/old/a.jpg")]).unwrap()[..] else { panic!() };
        cat.set_rating(id, Some(5)).unwrap();
        cat.relocate(id, "/new/a.jpg").unwrap();
        assert_eq!(cat.upsert_many(&[photo("/new/a.jpg")]).unwrap(), vec![id], "same row");
        assert_eq!(cat.get(id).unwrap().unwrap().rating, Some(5));
        assert!(cat.get_by_path("/old/a.jpg").unwrap().is_none());
    }

    #[test]
    fn settings() {
        let cat = Catalog::open_in_memory().unwrap();
        assert_eq!(cat.setting("x").unwrap(), None);
        cat.set_setting("x", "1").unwrap();
        cat.set_setting("x", "2").unwrap();
        assert_eq!(cat.setting("x").unwrap().as_deref(), Some("2"));
    }
}
