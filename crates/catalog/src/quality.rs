//! Quality measurements (sharpness, the LAION aesthetic score) and the
//! ratings that teach Tintype the user's taste.

use std::collections::HashMap;

use rusqlite::params;

use crate::{Catalog, Result};

/// A photo that needs a quality pass, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityTodo {
    pub id: i64,
    pub path: String,
    pub sharpness: bool,
    pub aesthetic: bool,
}

impl Catalog {
    /// Photos whose sharpness predates `sharpness_version`, or (when
    /// `aesthetic_version` is given) whose aesthetic score predates it.
    pub fn photos_needing_quality(
        &self,
        sharpness_version: u32,
        aesthetic_version: Option<u32>,
        limit: usize,
    ) -> Result<Vec<QualityTodo>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, sharpness_version < ?1, ?2 IS NOT NULL AND aesthetic_version < ?2 FROM photos
             WHERE sharpness_version < ?1 OR (?2 IS NOT NULL AND aesthetic_version < ?2) ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![sharpness_version, aesthetic_version, limit as i64], |r| {
            Ok(QualityTodo { id: r.get(0)?, path: r.get(1)?, sharpness: r.get(2)?, aesthetic: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_needing_quality(&self, sharpness_version: u32, aesthetic_version: Option<u32>) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE sharpness_version < ?1 OR (?2 IS NOT NULL AND aesthetic_version < ?2)",
            params![sharpness_version, aesthetic_version],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Photos needing any analysis: embeddings, auto tags, sharpness, or (when
    /// given) the aesthetic score.
    pub fn count_needing_any_analysis(
        &self,
        analysis_version: u32,
        subjects_version: u32,
        sharpness_version: u32,
        aesthetic_version: Option<u32>,
    ) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE analysis_version < ?1 OR subjects_version < ?2
                 OR sharpness_version < ?3 OR (?4 IS NOT NULL AND aesthetic_version < ?4)",
            params![analysis_version, subjects_version, sharpness_version, aesthetic_version],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Records a quality pass. `None` leaves a measurement as it was; `Some(None)`
    /// records that it could not be taken (an undecodable photo) at that version.
    pub fn set_quality(
        &mut self,
        photo_id: i64,
        sharpness: Option<(Option<f32>, u32)>,
        aesthetic: Option<(Option<f32>, u32)>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        if let Some((value, version)) = sharpness {
            tx.execute(
                "UPDATE photos SET sharpness = ?2, sharpness_version = ?3 WHERE id = ?1",
                params![photo_id, value, version],
            )?;
        }
        if let Some((value, version)) = aesthetic {
            tx.execute(
                "UPDATE photos SET aesthetic = ?2, aesthetic_version = ?3 WHERE id = ?1",
                params![photo_id, value, version],
            )?;
            crate::mark_index_pending(&tx, &[photo_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every photo's aesthetic score.
    pub fn aesthetic_scores(&self) -> Result<HashMap<i64, f32>> {
        let mut stmt = self.conn.prepare("SELECT id, aesthetic FROM photos WHERE aesthetic IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ratings the user gave in Tintype (not ones read from sidecars).
    pub fn user_ratings(&self) -> Result<Vec<(i64, u8)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, rating FROM photos WHERE rating IS NOT NULL AND rating_source = 'user' ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Catalog, NewPhoto};

    fn photo(path: &str, size: u64) -> NewPhoto {
        NewPhoto {
            path: path.into(),
            size,
            mtime_ns: 1,
            hash: None,
            meta: Default::default(),
            meta_version: 1,
            error: None,
        }
    }

    #[test]
    fn quality_bookkeeping() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 1)]).unwrap();
        assert_eq!(cat.count_needing_quality(1, None).unwrap(), 2);
        let todo = cat.photos_needing_quality(1, Some(1), 10).unwrap();
        assert!(todo[0].sharpness && todo[0].aesthetic);

        cat.set_quality(ids[0], Some((Some(120.0), 1)), None).unwrap();
        cat.set_quality(ids[1], Some((None, 1)), None).unwrap();
        assert_eq!(cat.count_needing_quality(1, None).unwrap(), 0, "aesthetic off: done");
        let todo = cat.photos_needing_quality(1, Some(1), 10).unwrap();
        assert_eq!(
            (todo.len(), todo[0].sharpness, todo[0].aesthetic),
            (2, false, true),
            "aesthetic on: still to score"
        );

        cat.set_quality(ids[0], None, Some((Some(6.1), 1))).unwrap();
        assert_eq!(cat.aesthetic_scores().unwrap()[&ids[0]], 6.1);
        assert_eq!(cat.get(ids[0]).unwrap().unwrap().aesthetic, Some(6.1));

        // Changed pixels need measuring again; unchanged files keep their scores.
        cat.upsert_many(&[photo("/a.jpg", 9), photo("/b.jpg", 1)]).unwrap();
        let todo = cat.photos_needing_quality(1, Some(1), 10).unwrap();
        assert_eq!(todo.iter().map(|t| (t.id, t.sharpness)).collect::<Vec<_>>(), vec![(ids[0], true), (ids[1], false)]);
    }

    #[test]
    fn only_ratings_made_in_tintype_teach_taste() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 1)]).unwrap();
        cat.set_rating(ids[0], Some(5)).unwrap();
        let sidecar = tintype_meta::xmp::Sidecar { rating: Some(4), ..Default::default() };
        cat.apply_sidecar(ids[1], &sidecar, crate::SidecarStamp { mtime_ns: 1, size: 1 }).unwrap();
        assert_eq!(cat.user_ratings().unwrap(), vec![(ids[0], 5)]);
        cat.set_rating(ids[0], None).unwrap();
        assert!(cat.user_ratings().unwrap().is_empty());
    }
}
