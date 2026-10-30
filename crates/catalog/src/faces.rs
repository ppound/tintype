//! Faces and people.
//!
//! A face's `person_id` is either confirmed (the user named it) or a
//! suggestion from matching. Suggestions are recomputed freely; confirmed
//! names only change when the user changes them.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Catalog, Result};

/// A face from detection, before it has an id.
#[derive(Debug, Clone)]
pub struct NewFace {
    /// x, y, w, h as fractions of the upright image.
    pub bbox: [f32; 4],
    pub score: f32,
    pub embedding: Vec<f32>,
}

/// What matching and grouping need about a face.
#[derive(Debug, Clone)]
pub struct FaceRow {
    pub id: i64,
    pub photo_id: i64,
    pub score: f32,
    pub embedding: Vec<f32>,
    pub person_id: Option<i64>,
    pub confirmed: bool,
    pub rejected_person_id: Option<i64>,
    pub ungrouped: bool,
    pub group: Option<i64>,
    pub suggest_score: Option<f32>,
}

/// A matching result for an unconfirmed face.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Suggestion {
    pub face: i64,
    pub person: Option<i64>,
    pub group: Option<i64>,
    /// How well the face matched `person`.
    pub score: Option<f32>,
}

/// A face's naming state, carried over when its photo is re-analyzed.
struct OldFace {
    id: i64,
    bbox: [f32; 4],
    person: Option<i64>,
    confirmed: bool,
    rejected: Option<i64>,
}

/// A face as the UI shows it.
#[derive(Debug, Clone, Serialize)]
pub struct FaceInfo {
    pub id: i64,
    pub photo_id: i64,
    pub bbox: [f32; 4],
    pub person_id: Option<i64>,
    pub person: Option<String>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Person {
    pub id: i64,
    pub name: String,
    /// Faces the user confirmed.
    pub faces: u64,
    /// Faces suggested as this person, awaiting review.
    pub suggested: u64,
    pub photos: u64,
    /// A face to show for this person.
    pub cover_face: Option<i64>,
}

/// Unnamed faces that look like the same person.
#[derive(Debug, Clone, Serialize)]
pub struct FaceGroup {
    pub group: i64,
    pub size: u64,
    /// The clearest faces first, at most the requested number.
    pub faces: Vec<i64>,
}

const FACE_INFO: &str = "SELECT f.id, f.photo_id, f.x, f.y, f.w, f.h, f.person_id, p.name, f.confirmed
     FROM faces f LEFT JOIN people p ON p.id = f.person_id";

fn read_info(r: &rusqlite::Row) -> rusqlite::Result<FaceInfo> {
    Ok(FaceInfo {
        id: r.get(0)?,
        photo_id: r.get(1)?,
        bbox: [r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
        person_id: r.get(6)?,
        person: r.get(7)?,
        confirmed: r.get(8)?,
    })
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = ((a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0])).max(0.0);
    let h = ((a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1])).max(0.0);
    let union = a[2] * a[3] + b[2] * b[3] - w * h;
    if union > 0.0 { w * h / union } else { 0.0 }
}

impl Catalog {
    /// Photos not yet analyzed by face detection `version`, at most `limit`.
    pub fn photos_needing_faces(&self, version: u32, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT id, path FROM photos WHERE faces_version < ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(params![version, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_needing_faces(&self, version: u32) -> Result<u64> {
        let n: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM photos WHERE faces_version < ?1", [version], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Replaces a photo's faces with a fresh detection and records `version`.
    /// Names carry over to new faces that overlap an old one. Returns the new
    /// face ids (in input order) and the ids that were deleted.
    pub fn replace_faces(&mut self, photo_id: i64, faces: &[NewFace], version: u32) -> Result<(Vec<i64>, Vec<i64>)> {
        let tx = self.conn.transaction()?;
        let old: Vec<OldFace> = {
            let mut stmt = tx.prepare_cached(
                "SELECT id, x, y, w, h, person_id, confirmed, rejected_person_id FROM faces WHERE photo_id = ?1",
            )?;
            stmt.query_map([photo_id], |r| {
                Ok(OldFace {
                    id: r.get(0)?,
                    bbox: [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?],
                    person: r.get(5)?,
                    confirmed: r.get(6)?,
                    rejected: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        tx.execute("DELETE FROM faces WHERE photo_id = ?1", [photo_id])?;
        let mut ids = Vec::with_capacity(faces.len());
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO faces (photo_id, x, y, w, h, score, embedding, person_id, confirmed, rejected_person_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for f in faces {
                let carried = old
                    .iter()
                    .filter(|o| o.confirmed && iou(o.bbox, f.bbox) > 0.5)
                    .max_by(|a, b| iou(a.bbox, f.bbox).total_cmp(&iou(b.bbox, f.bbox)));
                let (person, confirmed, rejected) =
                    carried.map_or((None, false, None), |o| (o.person, true, o.rejected));
                let [x, y, w, h] = f.bbox;
                insert.execute(params![
                    photo_id,
                    x,
                    y,
                    w,
                    h,
                    f.score,
                    to_blob(&f.embedding),
                    person,
                    confirmed,
                    rejected
                ])?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.execute("UPDATE photos SET faces_version = ?2 WHERE id = ?1", params![photo_id, version])?;
        tx.commit()?;
        Ok((ids, old.into_iter().map(|o| o.id).collect()))
    }

    pub fn face_rows(&self) -> Result<Vec<FaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, score, embedding, person_id, confirmed, rejected_person_id, ungrouped, grp, suggest_score
             FROM faces",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FaceRow {
                id: r.get(0)?,
                photo_id: r.get(1)?,
                score: r.get(2)?,
                embedding: from_blob(&r.get::<_, Vec<u8>>(3)?),
                person_id: r.get(4)?,
                confirmed: r.get(5)?,
                rejected_person_id: r.get(6)?,
                ungrouped: r.get(7)?,
                group: r.get(8)?,
                suggest_score: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores suggestions and groups for unconfirmed faces.
    pub fn set_suggestions(&mut self, updates: &[Suggestion]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE faces SET person_id = ?2, grp = ?3, suggest_score = ?4 WHERE id = ?1 AND confirmed = 0",
            )?;
            for u in updates {
                stmt.execute(params![u.face, u.person, u.group, u.score])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The id of the person called `name`, creating them if needed.
    pub fn person_id(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        self.conn.execute("INSERT OR IGNORE INTO people (name) VALUES (?1)", [name])?;
        Ok(self.conn.query_row("SELECT id FROM people WHERE name = ?1", [name], |r| r.get(0))?)
    }

    /// Confirms faces as the person called `name` (created if new). Returns the person id.
    pub fn name_faces(&mut self, faces: &[i64], name: &str) -> Result<i64> {
        let person = self.person_id(name)?;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE faces SET person_id = ?2, confirmed = 1, grp = NULL, ungrouped = 0,
                    rejected_person_id = CASE WHEN rejected_person_id = ?2 THEN NULL ELSE rejected_person_id END
                 WHERE id = ?1",
            )?;
            for f in faces {
                stmt.execute(params![f, person])?;
            }
        }
        tx.commit()?;
        Ok(person)
    }

    /// "Not this person": clears the face's person and stops suggesting them for it.
    pub fn reject_face(&self, face: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE faces SET rejected_person_id = COALESCE(person_id, rejected_person_id),
                person_id = NULL, confirmed = 0 WHERE id = ?1",
            [face],
        )?;
        Ok(())
    }

    /// Takes a face out of its unnamed group for good.
    pub fn ungroup_face(&self, face: i64) -> Result<()> {
        self.conn.execute("UPDATE faces SET ungrouped = 1, grp = NULL WHERE id = ?1", [face])?;
        Ok(())
    }

    /// Renames a person. Renaming to another person's name merges them.
    pub fn rename_person(&mut self, id: i64, name: &str) -> Result<i64> {
        let name = name.trim();
        let existing: Option<i64> =
            self.conn.query_row("SELECT id FROM people WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        match existing {
            Some(other) if other != id => {
                let tx = self.conn.transaction()?;
                tx.execute("UPDATE faces SET person_id = ?2 WHERE person_id = ?1", params![id, other])?;
                tx.execute(
                    "UPDATE faces SET rejected_person_id = ?2 WHERE rejected_person_id = ?1",
                    params![id, other],
                )?;
                tx.execute("DELETE FROM people WHERE id = ?1", [id])?;
                tx.commit()?;
                Ok(other)
            }
            _ => {
                self.conn.execute("UPDATE people SET name = ?2 WHERE id = ?1", params![id, name])?;
                Ok(id)
            }
        }
    }

    /// Forgets a person; their faces become unnamed.
    pub fn delete_person(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE faces SET confirmed = 0 WHERE person_id = ?1", [id])?;
        self.conn.execute("DELETE FROM people WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn people(&self) -> Result<Vec<Person>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.name,
                COUNT(f.id) FILTER (WHERE f.confirmed = 1),
                COUNT(f.id) FILTER (WHERE f.confirmed = 0),
                COUNT(DISTINCT f.photo_id),
                (SELECT id FROM faces WHERE person_id = p.id ORDER BY confirmed DESC, score DESC, w * h DESC LIMIT 1)
             FROM people p LEFT JOIN faces f ON f.person_id = p.id
             GROUP BY p.id ORDER BY COUNT(f.id) DESC, p.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Person {
                id: r.get(0)?,
                name: r.get(1)?,
                faces: r.get::<_, i64>(2)? as u64,
                suggested: r.get::<_, i64>(3)? as u64,
                photos: r.get::<_, i64>(4)? as u64,
                cover_face: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Unnamed groups of two or more faces, largest first, with up to `per_group` faces each.
    pub fn face_groups(&self, per_group: usize) -> Result<Vec<FaceGroup>> {
        let mut groups: Vec<FaceGroup> = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT grp, id FROM faces WHERE grp IS NOT NULL AND person_id IS NULL
             ORDER BY grp, score DESC, w * h DESC",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (grp, id) = row?;
            match groups.last_mut() {
                Some(g) if g.group == grp => {
                    g.size += 1;
                    if g.faces.len() < per_group {
                        g.faces.push(id);
                    }
                }
                _ => groups.push(FaceGroup { group: grp, size: 1, faces: vec![id] }),
            }
        }
        groups.retain(|g| g.size >= 2);
        groups.sort_by(|a, b| b.size.cmp(&a.size).then(a.group.cmp(&b.group)));
        Ok(groups)
    }

    pub fn group_face_ids(&self, group: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM faces WHERE grp = ?1 AND person_id IS NULL")?;
        let ids = stmt.query_map([group], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    pub fn photo_faces(&self, photo_id: i64) -> Result<Vec<FaceInfo>> {
        let mut stmt = self.conn.prepare_cached(&format!("{FACE_INFO} WHERE f.photo_id = ?1 ORDER BY f.x"))?;
        let rows = stmt.query_map([photo_id], read_info)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A person's faces: suggestions to review first (most likely first), then confirmed.
    pub fn person_faces(&self, person_id: i64) -> Result<Vec<FaceInfo>> {
        let mut stmt = self.conn.prepare(&format!(
            "{FACE_INFO} WHERE f.person_id = ?1 ORDER BY f.confirmed, f.suggest_score DESC, f.score DESC, f.w * f.h DESC"
        ))?;
        let rows = stmt.query_map([person_id], read_info)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn face(&self, id: i64) -> Result<Option<FaceInfo>> {
        let mut stmt = self.conn.prepare_cached(&format!("{FACE_INFO} WHERE f.id = ?1"))?;
        Ok(stmt.query_row([id], read_info).optional()?)
    }

    /// Distinct photos containing any of `faces`.
    pub fn photos_of_faces(&self, faces: &[i64]) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT photo_id FROM faces WHERE id = ?1")?;
        let mut ids = Vec::new();
        for f in faces {
            if let Some(p) = stmt.query_row([f], |r| r.get(0)).optional()? {
                ids.push(p);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Face ids in the given photos (e.g. before deleting the photos).
    pub fn faces_of_photos(&self, photos: &[i64]) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached("SELECT id FROM faces WHERE photo_id = ?1")?;
        let mut ids = Vec::new();
        for p in photos {
            for id in stmt.query_map([p], |r| r.get(0))? {
                ids.push(id?);
            }
        }
        Ok(ids)
    }

    /// Photos with at least one face of `person`.
    pub fn photos_of_person(&self, person: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT photo_id FROM faces WHERE person_id = ?1")?;
        let ids = stmt.query_map([person], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    pub(crate) fn photo_people(&self, photo_id: i64) -> Result<(Vec<String>, u32)> {
        let faces = self.photo_faces(photo_id)?;
        let mut names: Vec<String> = faces.iter().filter_map(|f| f.person.clone()).collect();
        names.sort();
        names.dedup();
        Ok((names, faces.len() as u32))
    }

    pub(crate) fn people_by_photo(&self) -> Result<HashMap<i64, (Vec<String>, u32)>> {
        let mut out: HashMap<i64, (Vec<String>, u32)> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT f.photo_id, p.name FROM faces f LEFT JOIN people p ON p.id = f.person_id ORDER BY p.name",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (photo, name) = row?;
            let entry = out.entry(photo).or_default();
            entry.1 += 1;
            if let Some(n) = name
                && entry.0.last() != Some(&n)
            {
                entry.0.push(n);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewPhoto;

    fn catalog_with_photo() -> (Catalog, i64) {
        let mut cat = Catalog::open_in_memory().unwrap();
        let p = NewPhoto {
            path: "/a.jpg".into(),
            size: 1,
            mtime_ns: 1,
            hash: None,
            meta: Default::default(),
            meta_version: 1,
            error: None,
        };
        let id = cat.upsert_many(&[p]).unwrap()[0];
        (cat, id)
    }

    fn face(x: f32) -> NewFace {
        NewFace { bbox: [x, 0.1, 0.2, 0.2], score: 0.9, embedding: vec![x, 1.0, 0.5] }
    }

    #[test]
    fn faces_round_trip_and_photo_record() {
        let (mut cat, photo) = catalog_with_photo();
        assert_eq!(cat.photos_needing_faces(1, 10).unwrap().len(), 1);
        let (ids, removed) = cat.replace_faces(photo, &[face(0.1), face(0.6)], 1).unwrap();
        assert!(removed.is_empty());
        assert_eq!(cat.photos_needing_faces(1, 10).unwrap().len(), 0);

        let rows = cat.face_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].embedding, vec![0.1, 1.0, 0.5]);

        cat.name_faces(&ids[..1], "Alice").unwrap();
        let rec = cat.get(photo).unwrap().unwrap();
        assert_eq!((rec.people, rec.faces), (vec!["Alice".to_string()], 2));
        let mut seen = Vec::new();
        cat.for_each(|r| seen.push((r.people, r.faces))).unwrap();
        assert_eq!(seen, vec![(vec!["Alice".to_string()], 2)]);
    }

    #[test]
    fn redetection_keeps_confirmed_names() {
        let (mut cat, photo) = catalog_with_photo();
        let (ids, _) = cat.replace_faces(photo, &[face(0.1)], 1).unwrap();
        cat.name_faces(&ids, "Bob").unwrap();
        let (new_ids, removed) = cat.replace_faces(photo, &[face(0.11), face(0.7)], 2).unwrap();
        assert_eq!(removed, ids);
        let faces = cat.photo_faces(photo).unwrap();
        assert_eq!(faces[0].id, new_ids[0]);
        assert_eq!(faces[0].person.as_deref(), Some("Bob"));
        assert!(faces[0].confirmed);
        assert_eq!(faces[1].person, None);
    }

    #[test]
    fn suggestions_reject_and_groups() {
        let (mut cat, photo) = catalog_with_photo();
        let (ids, _) = cat.replace_faces(photo, &[face(0.1), face(0.4), face(0.7)], 1).unwrap();
        let alice = cat.person_id("Alice").unwrap();
        cat.set_suggestions(&[
            Suggestion { face: ids[0], person: Some(alice), group: None, score: Some(0.7) },
            Suggestion { face: ids[1], person: None, group: Some(0), score: None },
            Suggestion { face: ids[2], person: None, group: Some(0), score: None },
        ])
        .unwrap();

        let people = cat.people().unwrap();
        assert_eq!((people[0].faces, people[0].suggested), (0, 1));
        let groups = cat.face_groups(10).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].size, 2);

        cat.reject_face(ids[0]).unwrap();
        let row = cat.face_rows().unwrap().into_iter().find(|r| r.id == ids[0]).unwrap();
        assert_eq!((row.person_id, row.rejected_person_id), (None, Some(alice)));

        cat.ungroup_face(ids[2]).unwrap();
        assert!(cat.face_groups(10).unwrap().is_empty(), "a group of one is not shown");
        assert_eq!(cat.group_face_ids(0).unwrap(), vec![ids[1]]);
    }

    #[test]
    fn rename_to_existing_name_merges() {
        let (mut cat, photo) = catalog_with_photo();
        let (ids, _) = cat.replace_faces(photo, &[face(0.1), face(0.6)], 1).unwrap();
        let a = cat.name_faces(&ids[..1], "Al").unwrap();
        let b = cat.name_faces(&ids[1..], "Alice").unwrap();
        assert_eq!(cat.rename_person(a, "alice").unwrap(), b, "names match case-insensitively");
        let people = cat.people().unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].faces, 2);
    }

    #[test]
    fn changed_file_needs_new_face_pass() {
        let (mut cat, photo) = catalog_with_photo();
        cat.replace_faces(photo, &[], 1).unwrap();
        let changed = NewPhoto {
            path: "/a.jpg".into(),
            size: 2,
            mtime_ns: 1,
            hash: None,
            meta: Default::default(),
            meta_version: 1,
            error: None,
        };
        cat.upsert_many(&[changed]).unwrap();
        assert_eq!(cat.count_needing_faces(1).unwrap(), 1);
    }
}
