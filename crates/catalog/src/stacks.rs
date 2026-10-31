//! Stacks: photos that are copies, versions or burst frames of one shot,
//! shown as one in the grid. A stack's id is its smallest photo id, so it
//! stays put when the cover (the photo shown on top) changes.

use std::collections::HashMap;

use rusqlite::params;
use tintype_meta::PhotoMeta;

use crate::{Catalog, Result};

/// What grouping needs to know about a photo.
#[derive(Debug, Clone, PartialEq)]
pub struct StackCandidate {
    pub id: i64,
    pub hash: Option<String>,
    /// The camera model without its brand, lowercased (`x-m5`): the camera
    /// writes make `FUJIFILM` + model `X-M5`, LightCraft exports write model
    /// `FUJIFILM X-M5` and no make, and both must match.
    pub camera: Option<String>,
    /// The body's serial number, when the file kept it.
    pub serial: Option<String>,
    /// Capture time, seconds since the epoch (as written, no time zone).
    pub taken: Option<i64>,
    pub rating: Option<u8>,
    /// Width x height, 0 if unknown.
    pub pixels: u64,
    pub raw: bool,
    pub mtime_ns: i64,
}

/// A photo's place in a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackPlace {
    pub stack: i64,
    pub cover: bool,
}

const RAW_EXTENSIONS: &[&str] = &["raf", "dng", "nef", "cr2", "cr3", "arw", "orf", "rw2", "pef", "srw"];

fn candidate(
    id: i64,
    path: &str,
    hash: Option<String>,
    meta: &PhotoMeta,
    rating: Option<u8>,
    mtime_ns: i64,
) -> StackCandidate {
    let camera = meta.model.as_deref().and_then(|m| model_key(meta.make.as_deref(), m));
    let serial = meta.fields.get("BodySerialNumber").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    StackCandidate {
        id,
        hash,
        camera,
        serial,
        taken: meta.taken.map(|t| t.and_utc().timestamp()),
        rating,
        pixels: u64::from(meta.width.unwrap_or(0)) * u64::from(meta.height.unwrap_or(0)),
        raw: RAW_EXTENSIONS.contains(&ext.as_str()),
        mtime_ns,
    }
}

/// `X-M5` (make `FUJIFILM`), `FUJIFILM X-M5` (no make) and `RICOH GR III`
/// (make `RICOH IMAGING COMPANY, LTD.`) become `x-m5`, `x-m5`, `gr iii`.
fn model_key(make: Option<&str>, model: &str) -> Option<String> {
    let model = model.trim().to_lowercase();
    let mut words: Vec<&str> = model.split_whitespace().collect();
    let brand = make.and_then(|m| m.split_whitespace().next()).map(str::to_lowercase);
    if words.len() > 1 && brand.as_deref().is_none_or(|b| b == words[0]) {
        words.remove(0);
    }
    (!words.is_empty()).then(|| words.join(" "))
}

impl Catalog {
    /// Grouping inputs for every photo, or only the members of `stack`.
    pub fn stack_candidates(&self, stack: Option<i64>) -> Result<Vec<StackCandidate>> {
        let sql =
            "SELECT id, path, hash, meta, rating, mtime_ns FROM photos WHERE ?1 IS NULL OR stack = ?1 ORDER BY id";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([stack], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<u8>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, path, hash, meta, rating, mtime_ns) = row?;
            let meta: PhotoMeta = serde_json::from_str(&meta).map_err(|source| crate::Error::Json { id, source })?;
            out.push(candidate(id, &path, hash, &meta, rating, mtime_ns));
        }
        Ok(out)
    }

    /// Every stacked photo's place, keyed by photo id.
    pub fn stack_places(&self) -> Result<HashMap<i64, StackPlace>> {
        let mut stmt = self.conn.prepare("SELECT id, stack, stack_cover FROM photos WHERE stack IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, StackPlace { stack: r.get(1)?, cover: r.get(2)? })))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Makes `photos` (every photo when `None`, else just these) stack as
    /// `places` says; photos missing from `places` are unstacked. Returns the
    /// photos whose place changed.
    pub fn set_stacks(&mut self, places: &HashMap<i64, StackPlace>, photos: Option<&[i64]>) -> Result<Vec<i64>> {
        let current = self.stack_places()?;
        let all: Vec<i64>;
        let ids = match photos {
            Some(ids) => ids,
            None => {
                let mut stmt = self.conn.prepare("SELECT id FROM photos")?;
                all = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                &all
            }
        };
        let changed: Vec<i64> = ids.iter().copied().filter(|id| current.get(id) != places.get(id)).collect();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("UPDATE photos SET stack = ?2, stack_cover = ?3 WHERE id = ?1")?;
            for id in &changed {
                let place = places.get(id);
                stmt.execute(params![id, place.map(|p| p.stack), place.is_some_and(|p| p.cover)])?;
            }
        }
        crate::mark_index_pending(&tx, &changed)?;
        tx.commit()?;
        Ok(changed)
    }

    /// The stack `photo` is in, if any.
    pub fn stack_of(&self, photo: i64) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self.conn.query_row("SELECT stack FROM photos WHERE id = ?1", [photo], |r| r.get(0)).optional()?.flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewPhoto;

    #[test]
    fn model_keys_ignore_how_the_brand_is_written() {
        assert_eq!(model_key(Some("FUJIFILM"), "X-M5").as_deref(), Some("x-m5"));
        assert_eq!(model_key(None, "FUJIFILM X-M5").as_deref(), Some("x-m5"));
        assert_eq!(model_key(Some("RICOH IMAGING COMPANY, LTD."), "RICOH GR III").as_deref(), Some("gr iii"));
        assert_eq!(model_key(Some("Canon"), "Canon EOS R5").as_deref(), Some("eos r5"));
        assert_eq!(model_key(Some("NIKON CORPORATION"), "NIKON Z 6").as_deref(), Some("z 6"));
    }

    #[test]
    fn candidates_and_places() {
        let mut cat = Catalog::open_in_memory().unwrap();
        let meta = PhotoMeta {
            make: Some("FUJIFILM".into()),
            model: Some("X-T5".into()),
            width: Some(6000),
            height: Some(4000),
            taken: chrono::NaiveDate::from_ymd_opt(2026, 7, 17).unwrap().and_hms_opt(12, 0, 0),
            fields: [("BodySerialNumber".to_string(), "1A000001".to_string())].into(),
            ..PhotoMeta::default()
        };
        let p = |path: &str| NewPhoto {
            path: path.into(),
            size: 1,
            mtime_ns: 5,
            hash: Some("h".into()),
            meta: meta.clone(),
            meta_version: 1,
            error: None,
        };
        let ids = cat.upsert_many(&[p("/a.RAF"), p("/a.jpg")]).unwrap();
        let c = cat.stack_candidates(None).unwrap();
        assert_eq!((c[0].camera.as_deref(), c[0].serial.as_deref()), (Some("x-t5"), Some("1A000001")));
        assert_eq!((c[0].raw, c[1].raw, c[1].pixels), (true, false, 24_000_000));

        let places: HashMap<i64, StackPlace> =
            [(ids[0], StackPlace { stack: ids[0], cover: false }), (ids[1], StackPlace { stack: ids[0], cover: true })]
                .into();
        cat.clear_index_pending(None).unwrap();
        assert_eq!(cat.set_stacks(&places, None).unwrap(), ids);
        assert_eq!(cat.index_pending().unwrap(), ids, "stack changes reach the index");
        assert_eq!(cat.set_stacks(&places, None).unwrap(), Vec::<i64>::new(), "nothing changed");
        assert_eq!(cat.stack_candidates(Some(ids[0])).unwrap().len(), 2);
        let rec = cat.get(ids[1]).unwrap().unwrap();
        assert_eq!((rec.stack, rec.stack_cover), (Some(ids[0]), true));

        assert_eq!(cat.set_stacks(&HashMap::new(), Some(&ids[..1])).unwrap(), ids[..1], "unstack one");
        assert_eq!(cat.stack_of(ids[0]).unwrap(), None);
    }
}
