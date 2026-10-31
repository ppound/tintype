//! Image embeddings from the analysis pass.

use rusqlite::{OptionalExtension, params};

use crate::{Catalog, Result, from_blob, to_blob};

impl Catalog {
    /// Photos not yet analyzed by analysis `version`, at most `limit`.
    pub fn photos_needing_analysis(&self, version: u32, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT id, path FROM photos WHERE analysis_version < ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(params![version, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_needing_analysis(&self, version: u32) -> Result<u64> {
        let n: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM photos WHERE analysis_version < ?1", [version], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Records a photo's embedding (`None`: it could not be analyzed) and `version`.
    pub fn set_embedding(&mut self, photo_id: i64, embedding: Option<&[f32]>, version: u32) -> Result<()> {
        let tx = self.conn.transaction()?;
        match embedding {
            Some(v) => tx.execute(
                "INSERT INTO embeddings (photo_id, vector) VALUES (?1, ?2)
                 ON CONFLICT(photo_id) DO UPDATE SET vector = excluded.vector",
                params![photo_id, to_blob(v)],
            )?,
            None => tx.execute("DELETE FROM embeddings WHERE photo_id = ?1", [photo_id])?,
        };
        tx.execute("UPDATE photos SET analysis_version = ?2 WHERE id = ?1", params![photo_id, version])?;
        tx.commit()?;
        Ok(())
    }

    /// Every stored embedding, in photo id order.
    pub fn embeddings(&self) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut stmt = self.conn.prepare("SELECT photo_id, vector FROM embeddings ORDER BY photo_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, from_blob(&r.get::<_, Vec<u8>>(1)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn embedding(&self, photo_id: i64) -> Result<Option<Vec<f32>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT vector FROM embeddings WHERE photo_id = ?1", [photo_id], |r| r.get(0))
            .optional()?;
        Ok(blob.map(|b| from_blob(&b)))
    }

    /// File paths of `ids`, in the same order; unknown ids are skipped.
    pub fn paths(&self, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare_cached("SELECT path FROM photos WHERE id = ?1")?;
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Some(path) = stmt.query_row([id], |r| r.get(0)).optional()? {
                out.push((id, path));
            }
        }
        Ok(out)
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
    fn analysis_pass_bookkeeping() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let ids = cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 1)]).unwrap();
        assert_eq!(cat.count_needing_analysis(1).unwrap(), 2);

        cat.set_embedding(ids[0], Some(&[0.6, 0.8]), 1).unwrap();
        cat.set_embedding(ids[1], None, 1).unwrap();
        assert_eq!(cat.count_needing_analysis(1).unwrap(), 0);
        assert_eq!(cat.embeddings().unwrap(), vec![(ids[0], vec![0.6, 0.8])]);
        assert_eq!(cat.count_needing_analysis(2).unwrap(), 2, "a new version re-analyzes everything");

        // Unchanged files keep their analysis; changed ones need a new pass.
        cat.upsert_many(&[photo("/a.jpg", 1), photo("/b.jpg", 9)]).unwrap();
        assert_eq!(cat.photos_needing_analysis(1, 10).unwrap(), vec![(ids[1], "/b.jpg".to_string())]);

        cat.remove_many(&ids[..1]).unwrap();
        assert!(cat.embedding(ids[0]).unwrap().is_none(), "deleted with its photo");
        assert_eq!(cat.paths(&[ids[1], ids[0]]).unwrap(), vec![(ids[1], "/b.jpg".to_string())]);
    }
}
