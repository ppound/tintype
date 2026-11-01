//! Auto tags ("subjects"), found from each photo's embedding.

use rusqlite::params;

use crate::{Catalog, Result, from_blob, mark_index_pending};

impl Catalog {
    /// Analyzed photos without auto tags of `version`, with their embeddings
    /// (`None`: the photo could not be analyzed). At most `limit`, in id order.
    pub fn photos_needing_subjects(
        &self,
        analysis_version: u32,
        version: u32,
        limit: usize,
    ) -> Result<Vec<(i64, Option<Vec<f32>>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.id, e.vector FROM photos p LEFT JOIN embeddings e ON e.photo_id = p.id
             WHERE p.analysis_version >= ?1 AND p.subjects_version < ?2 ORDER BY p.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![analysis_version, version, limit as i64], |r| {
            Ok((r.get(0)?, r.get::<_, Option<Vec<u8>>>(1)?.map(|b| from_blob(&b))))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_needing_subjects(&self, analysis_version: u32, version: u32) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE analysis_version >= ?1 AND subjects_version < ?2",
            params![analysis_version, version],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Records the auto tags of several photos at `version`; their index entries need updating.
    pub fn set_subjects(&mut self, subjects: &[(i64, Vec<String>)], version: u32) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("UPDATE photos SET subjects = ?2, subjects_version = ?3 WHERE id = ?1")?;
            for (id, tags) in subjects {
                let joined = (!tags.is_empty()).then(|| tags.join("\n"));
                stmt.execute(params![id, joined, version])?;
            }
        }
        let ids: Vec<i64> = subjects.iter().map(|(id, _)| *id).collect();
        mark_index_pending(&tx, &ids)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Catalog, NewPhoto};

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

    #[test]
    fn subjects_follow_the_embedding() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg"), photo("/b.jpg"), photo("/c.jpg")]).unwrap();
        cat.set_embedding(ids[0], Some(&[0.6, 0.8]), 1).unwrap();
        cat.set_embedding(ids[1], None, 1).unwrap();
        assert_eq!(cat.count_needing_subjects(1, 1).unwrap(), 2, "only analyzed photos");
        assert_eq!(
            cat.photos_needing_subjects(1, 1, 10).unwrap(),
            vec![(ids[0], Some(vec![0.6, 0.8])), (ids[1], None)]
        );

        cat.clear_index_pending(None).unwrap();
        let dog = vec!["Animals/Dog".to_string(), "Nature/Beach".to_string()];
        cat.set_subjects(&[(ids[0], dog.clone()), (ids[1], vec![])], 1).unwrap();
        assert_eq!(cat.count_needing_subjects(1, 1).unwrap(), 0);
        assert_eq!(cat.get(ids[0]).unwrap().unwrap().subjects, dog);
        assert!(cat.get(ids[1]).unwrap().unwrap().subjects.is_empty());
        assert_eq!(cat.index_pending().unwrap(), ids[..2]);
        assert_eq!(cat.count_needing_subjects(1, 2).unwrap(), 2, "a new version tags again");

        // A new embedding (the file changed) needs new tags.
        cat.set_embedding(ids[0], Some(&[0.8, 0.6]), 1).unwrap();
        assert_eq!(cat.photos_needing_subjects(1, 1, 10).unwrap().len(), 1);
    }
}
