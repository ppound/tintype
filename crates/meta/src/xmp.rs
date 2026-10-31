//! XMP sidecars: `photo.jpg.xmp` next to the photo, as PhotoStat writes them.
//!
//! Tintype owns three properties: `xmp:Rating`, `dc:subject` (tags) and
//! `Iptc4xmpExt:PersonInImage` (people). Updating a sidecar rewrites only
//! those and streams everything else through unchanged, so other apps' data
//! (PhotoStat's `place`, Lightroom's develop settings) survives.

use std::path::{Path, PathBuf};

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::{NsReader, Writer};

const NS_RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const NS_DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const NS_IPTC_EXT: &[u8] = b"http://iptc.org/std/Iptc4xmpExt/2008-02-29/";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not valid XMP: {0}")]
    Xml(String),
}

impl From<quick_xml::Error> for Error {
    fn from(e: quick_xml::Error) -> Self {
        Error::Xml(e.to_string())
    }
}

impl From<quick_xml::events::attributes::AttrError> for Error {
    fn from(e: quick_xml::events::attributes::AttrError) -> Self {
        Error::Xml(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The metadata Tintype keeps in a sidecar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sidecar {
    /// 1-5; XMP's 0 (unrated) and -1 (rejected) read as `None`.
    pub rating: Option<u8>,
    pub tags: Vec<String>,
    pub people: Vec<String>,
}

impl Sidecar {
    pub fn is_empty(&self) -> bool {
        self.rating.is_none() && self.tags.is_empty() && self.people.is_empty()
    }
}

/// `DSCF1234.JPG` -> `DSCF1234.JPG.xmp`.
pub fn sidecar_path(photo: &Path) -> PathBuf {
    let mut name = photo.as_os_str().to_owned();
    name.push(".xmp");
    PathBuf::from(name)
}

/// The sidecar of `photo`, if it has one.
pub fn read_sidecar(photo: &Path) -> Result<Option<Sidecar>> {
    match std::fs::read_to_string(sidecar_path(photo)) {
        Ok(xml) => parse(&xml).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes `sidecar` for `photo`, keeping whatever else an existing sidecar
/// holds. Written to a temporary file first, then renamed over the old one.
pub fn write_sidecar(photo: &Path, sidecar: &Sidecar) -> Result<()> {
    let path = sidecar_path(photo);
    let existing = match std::fs::read_to_string(&path) {
        Ok(xml) => Some(xml),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let xml = update(existing.as_deref(), sidecar)?;
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tintype-tmp");
    std::fs::write(&tmp, xml)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Which of our properties an element is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owned {
    Rating,
    Tags,
    People,
}

fn owned(ns: &ResolveResult, local: &[u8]) -> Option<Owned> {
    match (ns, local) {
        (ResolveResult::Bound(Namespace(NS_XMP)), b"Rating") => Some(Owned::Rating),
        (ResolveResult::Bound(Namespace(NS_DC)), b"subject") => Some(Owned::Tags),
        (ResolveResult::Bound(Namespace(NS_IPTC_EXT)), b"PersonInImage") => Some(Owned::People),
        _ => None,
    }
}

fn is_rdf(ns: &ResolveResult, local: &[u8], name: &[u8]) -> bool {
    matches!(ns, ResolveResult::Bound(Namespace(NS_RDF))) && local == name
}

fn parse_rating(v: &str) -> Option<u8> {
    v.trim().parse::<f32>().ok().filter(|r| (1.0..=5.0).contains(r)).map(|r| r.round() as u8)
}

fn reader(xml: &str) -> NsReader<&[u8]> {
    NsReader::from_str(xml.trim_start_matches('\u{feff}'))
}

/// Reads our properties from sidecar XML, as attributes or elements.
pub fn parse(xml: &str) -> Result<Sidecar> {
    let mut r = reader(xml);
    let mut out = Sidecar::default();
    // The property being read, whether we're inside one of its `rdf:li`, and its text so far.
    let (mut current, mut in_li, mut text) = (None::<Owned>, false, String::new());
    loop {
        let (ns, event) = r.read_resolved_event()?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name();
                if is_rdf(&ns, local.as_ref(), b"Description") {
                    for a in e.attributes() {
                        let a = a?;
                        let (ans, alocal) = r.resolver().resolve_attribute(a.key);
                        if owned(&ans, alocal.as_ref()) == Some(Owned::Rating) {
                            out.rating = parse_rating(&String::from_utf8_lossy(&a.value));
                        }
                    }
                } else if let Some(o) = owned(&ns, local.as_ref()) {
                    current = matches!(event, Event::Start(_)).then_some(o);
                    text.clear();
                } else if current.is_some() && is_rdf(&ns, local.as_ref(), b"li") {
                    in_li = matches!(event, Event::Start(_));
                    text.clear();
                }
            }
            Event::Text(ref t) if current.is_some() => {
                text.push_str(&t.decode().map_err(|e| Error::Xml(e.to_string()))?);
            }
            Event::GeneralRef(ref g) if current.is_some() => {
                let resolved = match g.resolve_char_ref()? {
                    Some(c) => Some(c.to_string()),
                    None => {
                        let name = g.decode().map_err(|e| Error::Xml(e.to_string()))?;
                        quick_xml::escape::resolve_predefined_entity(&name).map(str::to_string)
                    }
                };
                text.push_str(&resolved.ok_or_else(|| Error::Xml("unknown entity".into()))?);
            }
            Event::End(ref e) => {
                let local = e.local_name();
                let value = text.trim();
                if is_rdf(&ns, local.as_ref(), b"li") {
                    match current {
                        Some(Owned::Tags) if in_li && !value.is_empty() => out.tags.push(value.to_string()),
                        Some(Owned::People) if in_li && !value.is_empty() => out.people.push(value.to_string()),
                        _ => {}
                    }
                    in_li = false;
                } else if let Some(o) = owned(&ns, local.as_ref()) {
                    if o == Owned::Rating && !value.is_empty() {
                        out.rating = parse_rating(value);
                    }
                    current = None;
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// New sidecar XML: `existing` with our properties replaced by `sidecar`'s,
/// or a fresh packet when there is none.
pub fn update(existing: Option<&str>, sidecar: &Sidecar) -> Result<String> {
    let Some(existing) = existing else { return Ok(fresh(sidecar)) };
    let mut r = reader(existing);
    let mut w = Writer::new(Vec::new());
    let mut written = false;
    // Depth inside an owned element being dropped (0: not dropping).
    let mut skip = 0u32;
    // Whitespace held back until we know whether an owned element (dropped with it) follows.
    let mut pending: Option<Event<'_>> = None;
    loop {
        let (ns, event) = r.read_resolved_event()?;
        let starts_owned =
            matches!(&event, Event::Start(e) | Event::Empty(e) if owned(&ns, e.local_name().as_ref()).is_some());
        if skip == 0 {
            if matches!(&event, Event::Text(t) if t.iter().all(u8::is_ascii_whitespace)) {
                if let Some(p) = pending.take() {
                    w.write_event(p)?;
                }
                pending = Some(event.into_owned());
                continue;
            }
            if let Some(p) = pending.take()
                && !starts_owned
            {
                w.write_event(p)?;
            }
        }
        if skip > 0 {
            match event {
                Event::Start(_) => skip += 1,
                Event::End(_) => skip -= 1,
                Event::Eof => return Err(Error::Xml("unclosed element".into())),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(ref e) | Event::Empty(ref e) if is_rdf(&ns, e.local_name().as_ref(), b"Description") => {
                let empty = matches!(event, Event::Empty(_));
                let mut start = BytesStart::new(String::from_utf8_lossy(e.name().as_ref()).into_owned());
                let mut declared = Vec::new();
                for a in e.attributes() {
                    let a = a?;
                    let (ans, alocal) = r.resolver().resolve_attribute(a.key);
                    if owned(&ans, alocal.as_ref()) == Some(Owned::Rating) {
                        continue;
                    }
                    if let Some(prefix) = a.key.as_ref().strip_prefix(b"xmlns:") {
                        declared.push(prefix.to_vec());
                    }
                    start.push_attribute(a);
                }
                if written {
                    w.write_event(if empty { Event::Empty(start) } else { Event::Start(start) })?;
                    continue;
                }
                written = true;
                // Our properties go into the first description.
                let mut ensure_ns = |prefix: &str, uri: &[u8]| {
                    if !declared.iter().any(|d| d == prefix.as_bytes()) {
                        start.push_attribute((format!("xmlns:{prefix}").as_bytes(), uri));
                    }
                };
                if sidecar.rating.is_some() {
                    ensure_ns("xmp", NS_XMP);
                }
                if !sidecar.tags.is_empty() {
                    ensure_ns("dc", NS_DC);
                }
                if !sidecar.people.is_empty() {
                    ensure_ns("Iptc4xmpExt", NS_IPTC_EXT);
                }
                if let Some(rating) = sidecar.rating {
                    start.push_attribute(("xmp:Rating", rating.to_string().as_str()));
                }
                w.write_event(Event::Start(start))?;
                write_bags(&mut w, sidecar)?;
                if empty {
                    w.write_event(Event::End(BytesEnd::new(String::from_utf8_lossy(e.name().as_ref()))))?;
                }
            }
            Event::Start(ref e) if owned(&ns, e.local_name().as_ref()).is_some() => skip = 1,
            Event::Empty(ref e) if owned(&ns, e.local_name().as_ref()).is_some() => {}
            Event::Eof => break,
            other => w.write_event(other)?,
        }
    }
    if !written {
        return Err(Error::Xml("no rdf:Description to update".into()));
    }
    String::from_utf8(w.into_inner()).map_err(|e| Error::Xml(e.to_string()))
}

/// `dc:subject` and `Iptc4xmpExt:PersonInImage` as indented bags.
fn write_bags(w: &mut Writer<Vec<u8>>, sidecar: &Sidecar) -> Result<()> {
    for (name, items) in [("dc:subject", &sidecar.tags), ("Iptc4xmpExt:PersonInImage", &sidecar.people)] {
        if items.is_empty() {
            continue;
        }
        w.write_event(Event::Text(BytesText::from_escaped("\n      ")))?;
        w.write_event(Event::Start(BytesStart::new(name)))?;
        w.write_event(Event::Text(BytesText::from_escaped("\n        ")))?;
        w.write_event(Event::Start(BytesStart::new("rdf:Bag")))?;
        for item in items {
            w.write_event(Event::Text(BytesText::from_escaped("\n          ")))?;
            w.write_event(Event::Start(BytesStart::new("rdf:li")))?;
            w.write_event(Event::Text(BytesText::new(item)))?;
            w.write_event(Event::End(BytesEnd::new("rdf:li")))?;
        }
        w.write_event(Event::Text(BytesText::from_escaped("\n        ")))?;
        w.write_event(Event::End(BytesEnd::new("rdf:Bag")))?;
        w.write_event(Event::Text(BytesText::from_escaped("\n      ")))?;
        w.write_event(Event::End(BytesEnd::new(name)))?;
    }
    Ok(())
}

/// A new packet in the same layout PhotoStat uses.
fn fresh(sidecar: &Sidecar) -> String {
    let skeleton = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Tintype\">\n  \
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n    \
<rdf:Description rdf:about=\"\"/>\n  \
</rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>\n";
    update(Some(skeleton), sidecar).expect("the skeleton is valid XMP")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a PhotoStat sidecar, with its own fields that we must keep.
    const PHOTOSTAT: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Adobe XMP Core Test.SNAPSHOT\">
  <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">
    <rdf:Description rdf:about=\"\"
        xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"
        xmlns:dc=\"http://purl.org/dc/elements/1.1/\"
        xmlns:photostat=\"http://photostat.app/xmp/1.0/\"
        xmlns:Iptc4xmpExt=\"http://iptc.org/std/Iptc4xmpExt/2008-02-29/\"
      xmp:Rating=\"4\"
      photostat:place=\"Harbourside\"
      photostat:analysisHash=\"a1b2c3d4e5f60718\">
      <dc:subject>
        <rdf:Bag>
          <rdf:li>Barn</rdf:li>
          <rdf:li>Fish &amp; Chips</rdf:li>
        </rdf:Bag>
      </dc:subject>
      <Iptc4xmpExt:PersonInImage>
        <rdf:Bag>
          <rdf:li>Ada</rdf:li>
        </rdf:Bag>
      </Iptc4xmpExt:PersonInImage>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>
<?xpacket end=\"w\"?>";

    fn sc(rating: Option<u8>, tags: &[&str], people: &[&str]) -> Sidecar {
        Sidecar {
            rating,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            people: people.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn reads_photostat_sidecars() {
        assert_eq!(parse(PHOTOSTAT).unwrap(), sc(Some(4), &["Barn", "Fish & Chips"], &["Ada"]));
    }

    #[test]
    fn update_replaces_ours_and_keeps_the_rest() {
        let xml = update(Some(PHOTOSTAT), &sc(Some(2), &["Farm"], &[])).unwrap();
        assert_eq!(parse(&xml).unwrap(), sc(Some(2), &["Farm"], &[]));
        for kept in ["photostat:place=\"Harbourside\"", "analysisHash=\"a1b2c3d4e5f60718\"", "xpacket end"] {
            assert!(xml.contains(kept), "{kept} missing from\n{xml}");
        }
        assert!(!xml.contains("Barn") && !xml.contains("PersonInImage>"), "{xml}");
        // Updating again with the same data changes nothing.
        assert_eq!(update(Some(&xml), &parse(&xml).unwrap()).unwrap(), xml);
    }

    #[test]
    fn clearing_the_rating_removes_it() {
        let xml = update(Some(PHOTOSTAT), &sc(None, &["Barn"], &["Ada"])).unwrap();
        assert_eq!(parse(&xml).unwrap().rating, None);
        assert!(!xml.contains("Rating"));
    }

    #[test]
    fn fresh_sidecars_round_trip() {
        let s = sc(Some(5), &["a <b>"], &["Grace Hopper"]);
        let xml = update(None, &s).unwrap();
        assert_eq!(parse(&xml).unwrap(), s);
        assert!(xml.starts_with("<?xpacket"));
    }

    #[test]
    fn element_form_and_other_prefixes() {
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
            <rdf:Description xmlns:xap="http://ns.adobe.com/xap/1.0/" xmlns:d="http://purl.org/dc/elements/1.1/">
              <xap:Rating>3</xap:Rating><d:subject><rdf:Seq><rdf:li>x</rdf:li></rdf:Seq></d:subject>
            </rdf:Description>
            <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Exposure="+0.5"/>
            </rdf:RDF></x:xmpmeta>"#;
        assert_eq!(parse(xml).unwrap(), sc(Some(3), &["x"], &[]));
        let out = update(Some(xml), &sc(Some(1), &[], &[])).unwrap();
        assert_eq!(parse(&out).unwrap(), sc(Some(1), &[], &[]));
        assert!(out.contains("crs:Exposure=\"+0.5\""), "{out}");
        assert!(!out.contains("xap:Rating>"), "old element form dropped: {out}");
    }

    #[test]
    fn unrated_and_rejected_read_as_none() {
        for v in ["0", "-1"] {
            let xml = PHOTOSTAT.replace("xmp:Rating=\"4\"", &format!("xmp:Rating=\"{v}\""));
            assert_eq!(parse(&xml).unwrap().rating, None);
        }
    }

    #[test]
    fn sidecar_files() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("DSCF1.RAF");
        assert_eq!(sidecar_path(&photo), dir.path().join("DSCF1.RAF.xmp"));
        assert_eq!(read_sidecar(&photo).unwrap(), None);
        write_sidecar(&photo, &sc(Some(3), &["t"], &[])).unwrap();
        assert_eq!(read_sidecar(&photo).unwrap(), Some(sc(Some(3), &["t"], &[])));
        assert!(!photo.exists(), "only the sidecar is written");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left");
    }
}
