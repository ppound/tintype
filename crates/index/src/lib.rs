//! Search over the catalog, built on tantivy.
//!
//! The index is derived data: it can be deleted and rebuilt from the catalog
//! at any time. Bumping [`SCHEMA_VERSION`] forces that rebuild on next open.
//!
//! Query syntax (tantivy's, all terms must match by default):
//!
//! | Example                     | Meaning                                    |
//! |-----------------------------|--------------------------------------------|
//! | `beach sunset`              | full text over paths, EXIF values and tags |
//! | `make:canon lens:70-200`    | a specific text field                      |
//! | `iso:>1600 aperture:<=2.8`  | numeric ranges                             |
//! | `year:[2019 TO 2021]`       | inclusive range                            |
//! | `camera:"/Canon"`           | facet (and everything below it)            |
//! | `rating:>=4 tag:family`     | the user's own metadata                    |
//! | `person:alice` `faces:>=3`  | people (named faces) and face count         |
//! | `stack:12`                  | the photos of one stack                    |
//! | `aesthetic:>=6`             | LAION aesthetic score (when computed)      |
//! | `gps:true`                  | has location                               |
//!
//! Smart (semantic) search is done by the engine: [`split_smart`] separates
//! field terms, which still filter here via [`PhotoIndex::matching_ids`], from
//! the description, which is matched against image embeddings.

use std::path::{Component, Path, Prefix};

use chrono::Datelike;
use serde::Serialize;
use tantivy::collector::{Count, DocSetCollector, FacetCollector, TopDocs};
use tantivy::query::{AllQuery, Query, QueryParser};
use tantivy::schema::{FAST, Facet, FacetOptions, Field, INDEXED, STORED, Schema, TEXT, TantivyDocument, Value};
use tantivy::{DateTime, DocAddress, Index, IndexReader, IndexWriter, Order, ReloadPolicy, Searcher, Term};
use tintype_catalog::PhotoRecord;

/// Bump whenever the schema or the way documents are built changes.
pub const SCHEMA_VERSION: u32 = 5;
const VERSION_FILE: &str = "tintype-schema-version";
const WRITER_HEAP_BYTES: usize = 128 * 1024 * 1024;

/// Facet fields that [`PhotoIndex::facets`] accepts.
pub const FACET_FIELDS: &[&str] = &["camera", "lenses", "date", "ext", "tags", "people", "folder"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("search index error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("invalid query: {0}")]
    Query(#[from] tantivy::query::QueryParserError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown facet field {0:?}; expected one of {FACET_FIELDS:?}")]
    UnknownFacet(String),
    #[error("invalid facet path {0:?}")]
    BadFacetPath(String),
    #[error("{0} exists but is not a Tintype search index; refusing to replace it")]
    NotAnIndex(std::path::PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Relevance,
    Newest,
    Oldest,
    /// Highest aesthetic score first (the engine uses the user's learned taste when it can).
    Best,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: i64,
    pub path: String,
    /// The stack this hit stands for, when results are stacked...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<i64>,
    /// ...and how many of its photos matched (1 when not stacked).
    pub stack_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    /// Total matches, not just the returned page.
    pub total: usize,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FacetCount {
    pub path: String,
    pub count: u64,
}

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    path: Field,
    text: Field,
    filename: Field,
    make: Field,
    model: Field,
    lens: Field,
    tag: Field,
    person: Field,
    camera: Field,
    lenses: Field,
    date: Field,
    ext: Field,
    tags: Field,
    people: Field,
    folder: Field,
    taken: Field,
    year: Field,
    iso: Field,
    aperture: Field,
    shutter: Field,
    focal: Field,
    focal35: Field,
    rating: Field,
    width: Field,
    height: Field,
    size: Field,
    gps: Field,
    faces: Field,
    stack: Field,
    aesthetic: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let num = INDEXED | FAST;
    let facet = FacetOptions::default();
    let fields = Fields {
        id: b.add_u64_field("id", INDEXED | STORED | FAST),
        path: b.add_text_field("path", TEXT | STORED),
        text: b.add_text_field("text", TEXT),
        filename: b.add_text_field("filename", TEXT),
        make: b.add_text_field("make", TEXT),
        model: b.add_text_field("model", TEXT),
        lens: b.add_text_field("lens", TEXT),
        tag: b.add_text_field("tag", TEXT),
        person: b.add_text_field("person", TEXT),
        camera: b.add_facet_field("camera", facet.clone()),
        lenses: b.add_facet_field("lenses", facet.clone()),
        date: b.add_facet_field("date", facet.clone()),
        ext: b.add_facet_field("ext", facet.clone()),
        tags: b.add_facet_field("tags", facet.clone()),
        people: b.add_facet_field("people", facet.clone()),
        folder: b.add_facet_field("folder", facet),
        taken: b.add_date_field("taken", num.clone()),
        year: b.add_u64_field("year", num.clone()),
        iso: b.add_u64_field("iso", num.clone()),
        aperture: b.add_f64_field("aperture", num.clone()),
        shutter: b.add_f64_field("shutter", num.clone()),
        focal: b.add_f64_field("focal", num.clone()),
        focal35: b.add_u64_field("focal35", num.clone()),
        rating: b.add_u64_field("rating", num.clone()),
        width: b.add_u64_field("width", num.clone()),
        height: b.add_u64_field("height", num.clone()),
        size: b.add_u64_field("size", num.clone()),
        gps: b.add_bool_field("gps", num.clone()),
        faces: b.add_u64_field("faces", num.clone()),
        stack: b.add_u64_field("stack", num.clone()),
        aesthetic: b.add_f64_field("aesthetic", num),
    };
    (b.build(), fields)
}

pub struct PhotoIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    fields: Fields,
    parser: QueryParser,
}

impl PhotoIndex {
    /// Opens the index in `dir`, creating it if needed. The flag is true when the
    /// index was (re)created and must be filled from the catalog.
    pub fn open(dir: &Path) -> Result<(Self, bool)> {
        let (schema, fields) = schema();
        let version_path = dir.join(VERSION_FILE);
        let current = std::fs::read_to_string(&version_path).ok().and_then(|v| v.trim().parse::<u32>().ok());

        let (index, fresh) = if current == Some(SCHEMA_VERSION) {
            (Index::open_in_dir(dir)?, false)
        } else {
            if dir.exists() {
                // Only ever delete our own index: the marker shows we created it.
                let ours = version_path.exists() || std::fs::read_dir(dir)?.next().is_none();
                if !ours {
                    return Err(Error::NotAnIndex(dir.to_path_buf()));
                }
                std::fs::remove_dir_all(dir)?;
            }
            std::fs::create_dir_all(dir)?;
            let index = Index::create_in_dir(dir, schema)?;
            std::fs::write(&version_path, SCHEMA_VERSION.to_string())?;
            (index, true)
        };
        Ok((Self::from_index(index, fields)?, fresh))
    }

    pub fn in_memory() -> Result<Self> {
        let (schema, fields) = schema();
        Self::from_index(Index::create_in_ram(schema), fields)
    }

    fn from_index(index: Index, fields: Fields) -> Result<Self> {
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        let writer = index.writer(WRITER_HEAP_BYTES)?;
        let mut parser = QueryParser::for_index(&index, vec![fields.text]);
        parser.set_conjunction_by_default();
        Ok(Self { index, reader, writer, fields, parser })
    }

    /// Adds or replaces photos. Not visible to searches until [`commit`](Self::commit).
    pub fn upsert<'a>(&mut self, records: impl IntoIterator<Item = &'a PhotoRecord>) -> Result<()> {
        for rec in records {
            self.writer.delete_term(Term::from_field_u64(self.fields.id, rec.id as u64));
            self.writer.add_document(self.document(rec))?;
        }
        Ok(())
    }

    pub fn remove(&mut self, ids: &[i64]) -> Result<()> {
        for &id in ids {
            self.writer.delete_term(Term::from_field_u64(self.fields.id, id as u64));
        }
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.writer.delete_all_documents()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    fn parse(&self, query: &str) -> Result<Box<dyn Query>> {
        if query.trim().is_empty() { Ok(Box::new(AllQuery)) } else { Ok(self.parser.parse_query(query)?) }
    }

    pub fn search(&self, query: &str, sort: Sort, limit: usize, offset: usize) -> Result<SearchResults> {
        let query = self.parse(query)?;
        let searcher = self.reader.searcher();
        let total = searcher.search(&query, &Count)?;
        if limit == 0 || total == 0 {
            return Ok(SearchResults { total, hits: Vec::new() });
        }

        let top = TopDocs::with_limit(limit).and_offset(offset);
        let addrs: Vec<DocAddress> = match sort {
            Sort::Relevance => searcher.search(&query, &top.order_by_score())?.into_iter().map(|(_, a)| a).collect(),
            Sort::Newest | Sort::Oldest => {
                let order = if sort == Sort::Newest { Order::Desc } else { Order::Asc };
                let collector = top.order_by_fast_field::<DateTime>("taken", order);
                searcher.search(&query, &collector)?.into_iter().map(|(_, a)| a).collect()
            }
            Sort::Best => {
                let collector = top.order_by_fast_field::<f64>("aesthetic", Order::Desc);
                searcher.search(&query, &collector)?.into_iter().map(|(_, a)| a).collect()
            }
        };
        let hits = addrs.into_iter().map(|a| self.hit(&searcher, a)).collect::<Result<_>>()?;
        Ok(SearchResults { total, hits })
    }

    /// Ids of every photo matching `query`, in `sort` order.
    pub fn ordered_ids(&self, query: &str, sort: Sort) -> Result<Vec<i64>> {
        let query = self.parse(query)?;
        let searcher = self.reader.searcher();
        let total = searcher.search(&query, &Count)?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let top = TopDocs::with_limit(total);
        let addrs: Vec<DocAddress> = match sort {
            Sort::Relevance => searcher.search(&query, &top.order_by_score())?.into_iter().map(|(_, a)| a).collect(),
            Sort::Newest | Sort::Oldest => {
                let order = if sort == Sort::Newest { Order::Desc } else { Order::Asc };
                let collector = top.order_by_fast_field::<DateTime>("taken", order);
                searcher.search(&query, &collector)?.into_iter().map(|(_, a)| a).collect()
            }
            Sort::Best => {
                let collector = top.order_by_fast_field::<f64>("aesthetic", Order::Desc);
                searcher.search(&query, &collector)?.into_iter().map(|(_, a)| a).collect()
            }
        };
        let mut columns = Vec::new();
        for segment in searcher.segment_readers() {
            columns.push(segment.fast_fields().u64("id")?);
        }
        Ok(addrs
            .into_iter()
            .filter_map(|a| columns[a.segment_ord as usize].first(a.doc_id))
            .map(|id| id as i64)
            .collect())
    }

    /// Ids of every photo matching `query`.
    pub fn matching_ids(&self, query: &str) -> Result<Vec<i64>> {
        let searcher = self.reader.searcher();
        let docs = searcher.search(&self.parse(query)?, &DocSetCollector)?;
        let mut columns = Vec::new();
        for segment in searcher.segment_readers() {
            columns.push(segment.fast_fields().u64("id")?);
        }
        let mut ids: Vec<i64> = docs
            .into_iter()
            .filter_map(|a| columns[a.segment_ord as usize].first(a.doc_id))
            .map(|id| id as i64)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// Counts matches of `query` under each child of `parent` in facet `field`.
    /// Sorted by count, highest first.
    pub fn facets(&self, query: &str, field: &str, parent: &str) -> Result<Vec<FacetCount>> {
        if !FACET_FIELDS.contains(&field) {
            return Err(Error::UnknownFacet(field.into()));
        }
        // Accept "Canon" for "/Canon": shells like Git Bash mangle arguments starting with '/'.
        let parent = if parent.starts_with('/') { parent.to_string() } else { format!("/{parent}") };
        let parent = Facet::from_text(&parent).map_err(|_| Error::BadFacetPath(parent.clone()))?;
        let mut collector = FacetCollector::for_field(field);
        collector.add_facet(parent.clone());
        let counts = self.reader.searcher().search(&self.parse(query)?, &collector)?;
        let mut out: Vec<FacetCount> =
            counts.get(parent).map(|(facet, count)| FacetCount { path: facet.to_string(), count }).collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        Ok(out)
    }

    fn hit(&self, searcher: &Searcher, addr: DocAddress) -> Result<Hit> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let id = doc.get_first(self.fields.id).and_then(|v| v.as_u64()).unwrap_or_default() as i64;
        let path = doc.get_first(self.fields.path).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        Ok(Hit { id, path, stack: None, stack_size: 1 })
    }

    fn document(&self, rec: &PhotoRecord) -> TantivyDocument {
        let f = &self.fields;
        let m = &rec.meta;
        let path = Path::new(&rec.path);
        let mut doc = TantivyDocument::default();
        let mut text: Vec<&str> = vec![&rec.path];

        doc.add_u64(f.id, rec.id as u64);
        doc.add_text(f.path, &rec.path);
        doc.add_u64(f.size, rec.size);

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            doc.add_text(f.filename, name);
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            doc.add_facet(f.ext, Facet::from_path([ext.to_ascii_lowercase()]));
        }
        let folders = folder_segments(path);
        if !folders.is_empty() {
            doc.add_facet(f.folder, Facet::from_path(folders));
        }

        for (field, value) in [(f.make, &m.make), (f.model, &m.model), (f.lens, &m.lens)] {
            if let Some(v) = value {
                doc.add_text(field, v);
            }
        }
        if m.make.is_some() || m.model.is_some() {
            let make = m.make.as_deref().unwrap_or("Unknown");
            let path: Vec<&str> = std::iter::once(make).chain(m.model.as_deref()).collect();
            doc.add_facet(f.camera, Facet::from_path(path));
        }
        if let Some(lens) = &m.lens {
            doc.add_facet(f.lenses, Facet::from_path([lens]));
        }
        if let Some(taken) = m.taken {
            doc.add_date(f.taken, DateTime::from_timestamp_secs(taken.and_utc().timestamp()));
            doc.add_u64(f.year, taken.year().max(0) as u64);
            doc.add_facet(f.date, Facet::from_path([taken.format("%Y").to_string(), taken.format("%m").to_string()]));
        }
        if let Some(v) = m.iso {
            doc.add_u64(f.iso, v.into());
        }
        if let Some(v) = m.f_number {
            doc.add_f64(f.aperture, v.into());
        }
        if let Some(v) = m.exposure_s {
            doc.add_f64(f.shutter, v);
        }
        if let Some(v) = m.focal_mm {
            doc.add_f64(f.focal, v.into());
        }
        if let Some(v) = m.focal_35mm {
            doc.add_u64(f.focal35, v.into());
        }
        if let Some(v) = m.width {
            doc.add_u64(f.width, v.into());
        }
        if let Some(v) = m.height {
            doc.add_u64(f.height, v.into());
        }
        doc.add_bool(f.gps, m.gps.is_some());

        if let Some(r) = rec.rating {
            doc.add_u64(f.rating, r.into());
        }
        for tag in &rec.tags {
            doc.add_text(f.tag, tag);
            doc.add_facet(f.tags, Facet::from_path([tag]));
            text.push(tag);
        }

        for name in &rec.people {
            doc.add_text(f.person, name);
            doc.add_facet(f.people, Facet::from_path([name]));
            text.push(name);
        }
        doc.add_u64(f.faces, rec.faces.into());
        if let Some(a) = rec.aesthetic {
            doc.add_f64(f.aesthetic, a.into());
        }
        if let Some(stack) = rec.stack {
            doc.add_u64(f.stack, stack as u64);
        }

        text.extend(m.fields.values().map(String::as_str));
        for s in
            [&m.make, &m.model, &m.lens, &m.software, &m.artist, &m.copyright, &m.description].into_iter().flatten()
        {
            text.push(s);
        }
        doc.add_text(f.text, text.join(" \n"));
        doc
    }

    /// The underlying tantivy index, for diagnostics.
    pub fn tantivy(&self) -> &Index {
        &self.index
    }
}

/// The folder facet path of a photo: one segment per folder, a drive (`C:`),
/// or a network server and share (`\\nas\photos` gives `nas`, `photos`), so
/// no segment holds a path separator.
fn folder_segments(photo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for c in photo.parent().into_iter().flat_map(Path::components) {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out.push(server.to_string_lossy().into_owned());
                    out.push(share.to_string_lossy().into_owned());
                }
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => out.push(format!("{}:", d as char)),
                _ => out.push(p.as_os_str().to_string_lossy().replace(['\\', '/'], "")),
            },
            Component::Normal(n) => out.push(n.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    out
}

/// Splits a smart search into (field terms, description): `dog on a beach
/// year:2024 camera:"/FUJIFILM"` gives `year:2024 camera:"/FUJIFILM"` and
/// `dog on a beach`. Quotes and `[a TO b]` ranges stay with their field.
pub fn split_smart(query: &str) -> (String, String) {
    let mut terms: Vec<String> = Vec::new();
    let (mut cur, mut quoted, mut depth) = (String::new(), false, 0u32);
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' | '{' if !quoted => depth += 1,
            ']' | '}' if !quoted => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && !quoted && depth == 0 => {
                if !cur.is_empty() {
                    terms.push(std::mem::take(&mut cur));
                }
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    let is_field = |t: &str| {
        let t = t.trim_start_matches(['+', '-']);
        t.split_once(':')
            .is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    };
    let (fields, words): (Vec<String>, Vec<String>) = terms.into_iter().partition(|t| is_field(t));
    (fields.join(" "), words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_meta::{Gps, PhotoMeta};

    fn rec(id: i64, path: &str, meta: PhotoMeta) -> PhotoRecord {
        PhotoRecord {
            id,
            path: path.into(),
            size: 1000,
            mtime_ns: 0,
            hash: None,
            meta,
            error: None,
            rating: None,
            tags: Vec::new(),
            people: Vec::new(),
            faces: 0,
            stack: None,
            stack_cover: false,
            aesthetic: None,
            suggested_rating: None,
            indexed_at: 0,
        }
    }

    fn sample() -> PhotoIndex {
        let canon = PhotoMeta {
            make: Some("Canon".into()),
            model: Some("Canon EOS R5".into()),
            lens: Some("RF70-200mm F2.8".into()),
            iso: Some(3200),
            f_number: Some(2.8),
            taken: chrono::NaiveDate::from_ymd_opt(2023, 5, 14).unwrap().and_hms_opt(10, 0, 0),
            gps: Some(Gps { lat: 1.0, lon: 2.0, alt: None }),
            ..PhotoMeta::default()
        };
        let nikon = PhotoMeta {
            make: Some("NIKON CORPORATION".into()),
            model: Some("NIKON Z 6".into()),
            iso: Some(200),
            f_number: Some(8.0),
            taken: chrono::NaiveDate::from_ymd_opt(2021, 8, 2).unwrap().and_hms_opt(17, 0, 0),
            ..PhotoMeta::default()
        };
        let mut beach = rec(2, "/photos/2021/beach/DSC_0002.NEF", nikon);
        beach.rating = Some(5);
        beach.people = vec!["Ada Lovelace".into()];
        beach.faces = 3;
        beach.tags = vec!["family".into(), "beach".into()];

        let mut idx = PhotoIndex::in_memory().unwrap();
        idx.upsert(&[
            rec(1, "/photos/2023/zoo/IMG_0001.JPG", canon),
            beach,
            rec(3, "/photos/scans/old.png", PhotoMeta::default()),
        ])
        .unwrap();
        idx.commit().unwrap();
        idx
    }

    fn ids(idx: &PhotoIndex, q: &str) -> Vec<i64> {
        let mut ids: Vec<_> = idx.search(q, Sort::Relevance, 100, 0).unwrap().hits.into_iter().map(|h| h.id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn queries() {
        let idx = sample();
        assert_eq!(ids(&idx, ""), vec![1, 2, 3]);
        assert_eq!(ids(&idx, "canon"), vec![1]);
        assert_eq!(ids(&idx, "make:nikon"), vec![2]);
        assert_eq!(ids(&idx, "iso:>1600"), vec![1]);
        assert_eq!(ids(&idx, "aperture:<=2.8"), vec![1]);
        assert_eq!(ids(&idx, "year:[2020 TO 2022]"), vec![2]);
        assert_eq!(ids(&idx, "rating:>=4 tag:family"), vec![2]);
        assert_eq!(ids(&idx, "beach"), vec![2], "tag and folder both match, one hit");
        assert_eq!(ids(&idx, "gps:true"), vec![1]);
        assert_eq!(ids(&idx, "person:ada"), vec![2]);
        assert_eq!(ids(&idx, "lovelace faces:>=2"), vec![2]);
        assert_eq!(ids(&idx, "people:\"/Ada Lovelace\""), vec![2]);
        assert_eq!(ids(&idx, "zoo"), vec![1], "path components are searchable");
        assert_eq!(ids(&idx, "camera:\"/Canon\""), vec![1]);
        assert_eq!(ids(&idx, "canon nikon"), Vec::<i64>::new(), "terms are ANDed");
        assert!(matches!(idx.search("iso:>>", Sort::Relevance, 10, 0), Err(Error::Query(_))));
    }

    #[cfg(windows)]
    #[test]
    fn folder_segments_have_no_separators() {
        let seg = |p: &str| folder_segments(Path::new(p));
        assert_eq!(seg(r"C:\Users\me\a.jpg"), ["C:", "Users", "me"]);
        assert_eq!(seg(r"\\nas\photos 2\2026\a.jpg"), ["nas", "photos 2", "2026"]);
        assert_eq!(seg(r"\\?\UNC\nas\share\a.jpg"), ["nas", "share"]);
    }

    #[test]
    fn smart_split() {
        let split = |q: &str| split_smart(q);
        assert_eq!(split("dog on a beach"), ("".into(), "dog on a beach".into()));
        assert_eq!(
            split(r#"dog  year:2024 on camera:"/FUJIFILM/X-T5" a beach focal:[70 TO 200] -tag:x"#),
            (r#"year:2024 camera:"/FUJIFILM/X-T5" focal:[70 TO 200] -tag:x"#.into(), "dog on a beach".into())
        );
        assert_eq!(split(":) smile"), ("".into(), ":) smile".into()));
    }

    #[test]
    fn best_first_and_aesthetic_filter() {
        let mut idx = PhotoIndex::in_memory().unwrap();
        let mut recs: Vec<PhotoRecord> = (1..=3).map(|i| rec(i, &format!("/{i}.jpg"), PhotoMeta::default())).collect();
        recs[0].aesthetic = Some(4.5);
        recs[1].aesthetic = Some(6.2);
        idx.upsert(&recs).unwrap();
        idx.commit().unwrap();
        let best = idx.ordered_ids("", Sort::Best).unwrap();
        assert_eq!(best[..2], [2, 1], "scored photos best first");
        assert_eq!(best.len(), 3, "unscored photos are still listed");
        assert_eq!(ids(&idx, "aesthetic:>=6"), vec![2]);
    }

    #[test]
    fn ordered_ids_follow_sort() {
        let idx = sample();
        assert_eq!(idx.ordered_ids("", Sort::Newest).unwrap()[..2], [1, 2]);
        assert_eq!(idx.ordered_ids("", Sort::Oldest).unwrap()[0], 2);
        assert_eq!(idx.ordered_ids("canon", Sort::Relevance).unwrap(), vec![1]);
    }

    #[test]
    fn matching_ids_follows_filters() {
        let idx = sample();
        assert_eq!(idx.matching_ids("").unwrap(), vec![1, 2, 3]);
        assert_eq!(idx.matching_ids("year:[2020 TO 2024] iso:<1000").unwrap(), vec![2]);
    }

    #[test]
    fn sort_and_paging() {
        let idx = sample();
        let newest = idx.search("", Sort::Newest, 2, 0).unwrap();
        assert_eq!(newest.total, 3);
        assert_eq!(newest.hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
        let oldest = idx.search("", Sort::Oldest, 1, 0).unwrap();
        assert_eq!(oldest.hits[0].id, 2);
        let page2 = idx.search("", Sort::Newest, 2, 2).unwrap();
        assert_eq!(page2.hits.len(), 1);
    }

    #[test]
    fn facet_counts() {
        let idx = sample();
        let cams = idx.facets("", "camera", "/").unwrap();
        assert_eq!(cams.len(), 2);
        let ext = idx.facets("", "ext", "/").unwrap();
        assert!(ext.contains(&FacetCount { path: "/jpg".into(), count: 1 }));
        let months = idx.facets("", "date", "/2023").unwrap();
        assert_eq!(months, vec![FacetCount { path: "/2023/05".into(), count: 1 }]);
        assert_eq!(idx.facets("", "date", "2023").unwrap(), months, "leading slash optional");
        assert!(matches!(idx.facets("", "nope", "/"), Err(Error::UnknownFacet(_))));
    }

    #[test]
    fn upsert_replaces_and_remove_deletes() {
        let mut idx = sample();
        let mut r = rec(1, "/photos/renamed.jpg", PhotoMeta::default());
        r.tags = vec!["moved".into()];
        idx.upsert([&r]).unwrap();
        idx.remove(&[3]).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.doc_count(), 2);
        assert_eq!(ids(&idx, "moved"), vec![1]);
        assert_eq!(ids(&idx, "canon"), Vec::<i64>::new());
    }

    #[test]
    fn reopen_and_version_bump() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        let (mut idx, fresh) = PhotoIndex::open(&path).unwrap();
        assert!(fresh);
        idx.upsert([&rec(1, "/a.jpg", PhotoMeta::default())]).unwrap();
        idx.commit().unwrap();
        drop(idx);

        let (idx, fresh) = PhotoIndex::open(&path).unwrap();
        assert!(!fresh);
        assert_eq!(idx.doc_count(), 1);
        drop(idx);

        std::fs::write(path.join(VERSION_FILE), "0").unwrap();
        let (idx, fresh) = PhotoIndex::open(&path).unwrap();
        assert!(fresh);
        assert_eq!(idx.doc_count(), 0);
    }

    #[test]
    fn never_replaces_a_folder_it_did_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("holiday.jpg"), b"precious").unwrap();
        assert!(matches!(PhotoIndex::open(&path), Err(Error::NotAnIndex(_))));
        assert_eq!(std::fs::read(path.join("holiday.jpg")).unwrap(), b"precious");
    }
}
