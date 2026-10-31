//! Photo metadata extraction.
//!
//! Reads EXIF from JPEG, TIFF-based RAW (DNG, NEF, CR2, ARW, ...), Fujifilm RAF,
//! HEIF, PNG and WebP containers without ExifTool. A file with no EXIF is not an error: it
//! yields a [`PhotoMeta`] with only the dimensions filled in.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Seek, SeekFrom};
use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime};
use exif::{Exif, Field, In, Tag, Value};
use serde::{Deserialize, Serialize};

pub mod xmp;

/// File extensions (lowercase) that the scanner treats as photos.
pub const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "tif", "tiff", "png", "webp", "heic", "heif", "avif", // common
    "dng", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "raf", "rw2", "orf", "pef", "srw", "3fr", "erf", "kdc",
    "mrw", "x3f", // raw
];

/// Bump when extraction improves, so a rescan re-reads photos cataloged by an older version.
pub const EXTRACTOR_VERSION: u32 = 3;

/// Longest value kept for a single entry in [`PhotoMeta::fields`].
const MAX_FIELD_LEN: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unreadable EXIF: {0}")]
    Exif(#[from] exif::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Gps {
    pub lat: f64,
    pub lon: f64,
    pub alt: Option<f64>,
}

/// The metadata Tintype understands, plus every primary-IFD EXIF field as display text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhotoMeta {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    /// Capture time as recorded by the camera (local time, no zone).
    pub taken: Option<NaiveDateTime>,
    pub iso: Option<u32>,
    pub f_number: Option<f32>,
    /// Exposure time in seconds.
    pub exposure_s: Option<f64>,
    pub focal_mm: Option<f32>,
    pub focal_35mm: Option<u32>,
    pub orientation: Option<u16>,
    pub gps: Option<Gps>,
    pub software: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub description: Option<String>,
    /// All primary-image EXIF fields, tag name -> display value. Feeds full-text search.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

/// EXIF must fit in one 64 KiB APP1 segment near the start of a JPEG.
const RAF_JPEG_PREFIX: u32 = 256 * 1024;

/// Reads metadata from a photo on disk.
pub fn read_path(path: &Path) -> Result<PhotoMeta> {
    let mut file = File::open(path)?;
    let raf = tintype_codecs::raf_embedded_jpeg(&mut file)?;
    let parsed = if let Some((offset, len)) = raf {
        // A RAF's EXIF lives in its embedded JPEG.
        let mut jpeg = vec![0; len.min(RAF_JPEG_PREFIX) as usize];
        file.seek(SeekFrom::Start(offset))?;
        let n = tintype_codecs::read_up_to(&mut file, &mut jpeg)?;
        jpeg.truncate(n);
        exif::Reader::new().read_from_container(&mut Cursor::new(jpeg))
    } else {
        file.seek(SeekFrom::Start(0))?;
        exif::Reader::new().read_from_container(&mut BufReader::new(file))
    };
    let mut meta = match parsed {
        Ok(exif) => from_exif(&exif),
        Err(exif::Error::NotFound(_)) => PhotoMeta::default(),
        Err(e) => return Err(e.into()),
    };
    // The header is authoritative for dimensions; EXIF pixel dimensions are often stale after edits.
    if raf.is_some() {
        // The EXIF we read describes the embedded preview, not the sensor image.
        (meta.width, meta.height) = (None, None);
    } else if let Ok(size) = imagesize::size(path) {
        meta.width = u32::try_from(size.width).ok();
        meta.height = u32::try_from(size.height).ok();
    }
    Ok(meta)
}

/// Builds [`PhotoMeta`] from parsed EXIF.
pub fn from_exif(exif: &Exif) -> PhotoMeta {
    let make = string(exif, Tag::Make);
    // Many cameras repeat the make in the model ("Canon" / "Canon EOS R5"); keep the model as written.
    let model = string(exif, Tag::Model);
    let lens = string(exif, Tag::LensModel).or_else(|| string(exif, Tag::LensMake));

    PhotoMeta {
        width: uint(exif, Tag::PixelXDimension),
        height: uint(exif, Tag::PixelYDimension),
        make,
        model,
        lens,
        taken: datetime(exif),
        iso: uint(exif, Tag::PhotographicSensitivity).filter(|&v| v > 0),
        f_number: rational(exif, Tag::FNumber).map(|v| v as f32),
        exposure_s: rational(exif, Tag::ExposureTime),
        focal_mm: rational(exif, Tag::FocalLength).map(|v| v as f32),
        focal_35mm: uint(exif, Tag::FocalLengthIn35mmFilm).filter(|&v| v > 0),
        orientation: uint(exif, Tag::Orientation).and_then(|v| u16::try_from(v).ok()),
        gps: gps(exif),
        software: string(exif, Tag::Software),
        artist: string(exif, Tag::Artist),
        copyright: string(exif, Tag::Copyright),
        description: string(exif, Tag::ImageDescription),
        fields: all_fields(exif),
    }
}

fn field(exif: &Exif, tag: Tag) -> Option<&Field> {
    exif.get_field(tag, In::PRIMARY)
}

fn clean(s: &str) -> Option<String> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '"');
    (!t.is_empty()).then(|| t.to_string())
}

fn string(exif: &Exif, tag: Tag) -> Option<String> {
    display(exif, field(exif, tag)?)
}

/// A field's value as clean display text. ASCII fields may hold several
/// NUL-separated strings, often with empty padding entries.
fn display(exif: &Exif, f: &Field) -> Option<String> {
    match &f.value {
        Value::Ascii(parts) => {
            let parts: Vec<_> = parts.iter().filter_map(|p| clean(&String::from_utf8_lossy(p))).collect();
            clean(&parts.join(" "))
        }
        _ => clean(&f.display_value().with_unit(exif).to_string()),
    }
}

fn uint(exif: &Exif, tag: Tag) -> Option<u32> {
    field(exif, tag)?.value.get_uint(0)
}

fn rational(exif: &Exif, tag: Tag) -> Option<f64> {
    let v = match &field(exif, tag)?.value {
        Value::Rational(v) => v.first()?.to_f64(),
        Value::SRational(v) => v.first()?.to_f64(),
        _ => return None,
    };
    v.is_finite().then_some(v)
}

fn datetime(exif: &Exif) -> Option<NaiveDateTime> {
    [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime].into_iter().find_map(|tag| {
        let Value::Ascii(parts) = &field(exif, tag)?.value else {
            return None;
        };
        let dt = exif::DateTime::from_ascii(parts.first()?).ok()?;
        NaiveDate::from_ymd_opt(dt.year.into(), dt.month.into(), dt.day.into())?.and_hms_opt(
            dt.hour.into(),
            dt.minute.into(),
            dt.second.into(),
        )
    })
}

fn gps_coord(exif: &Exif, tag: Tag, ref_tag: Tag, negative_ref: u8) -> Option<f64> {
    let Value::Rational(v) = &field(exif, tag)?.value else {
        return None;
    };
    let [d, m, s, ..] = v.as_slice() else {
        return None;
    };
    let degrees = d.to_f64() + m.to_f64() / 60.0 + s.to_f64() / 3600.0;
    let negative = matches!(
        field(exif, ref_tag).map(|f| &f.value),
        Some(Value::Ascii(r)) if r.first().and_then(|s| s.first()) == Some(&negative_ref)
    );
    let signed = if negative { -degrees } else { degrees };
    signed.is_finite().then_some(signed)
}

fn gps(exif: &Exif) -> Option<Gps> {
    let lat = gps_coord(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S')?;
    let lon = gps_coord(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W')?;
    // (0, 0) is what many phones write when they have no fix.
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    let below_sea = field(exif, Tag::GPSAltitudeRef).and_then(|f| f.value.get_uint(0)) == Some(1);
    let alt = rational(exif, Tag::GPSAltitude).map(|a| if below_sea { -a } else { a });
    Some(Gps { lat, lon, alt })
}

fn all_fields(exif: &Exif) -> BTreeMap<String, String> {
    exif.fields()
        .filter(|f| f.ifd_num == In::PRIMARY && f.tag != Tag::MakerNote)
        .filter(|f| !matches!(&f.value, Value::Undefined(bytes, _) if bytes.len() > 64))
        .filter_map(|f| {
            let mut value = display(exif, f)?;
            if value.len() > MAX_FIELD_LEN {
                let cut = (0..=MAX_FIELD_LEN).rev().find(|&i| value.is_char_boundary(i))?;
                value.truncate(cut);
            }
            Some((f.tag.to_string(), value))
        })
        .collect()
}

/// True if `path` has an extension in [`PHOTO_EXTENSIONS`].
pub fn is_photo(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PHOTO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tintype_testkit::JpegSpec;

    #[test]
    fn reads_core_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        JpegSpec::canon().write(&path);

        let m = read_path(&path).unwrap();
        assert_eq!(m.make.as_deref(), Some("Canon"));
        assert_eq!(m.model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(m.lens.as_deref(), Some("RF24-70mm F2.8 L IS USM"));
        assert_eq!(m.iso, Some(1600));
        assert_eq!(m.f_number, Some(2.8));
        assert_eq!(m.exposure_s, Some(1.0 / 250.0));
        assert_eq!(m.focal_mm, Some(50.0));
        assert_eq!(m.taken.unwrap().to_string(), "2023-05-14 10:30:00");
        assert_eq!((m.width, m.height), (Some(640), Some(480)));
        let gps = m.gps.unwrap();
        assert!((gps.lat - 51.5).abs() < 1e-9);
        assert!((gps.lon + 0.125).abs() < 1e-9);
        assert_eq!(m.fields.get("Make").map(String::as_str), Some("Canon"));
    }

    #[test]
    fn reads_fujifilm_raf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("DSCF0001.RAF");
        std::fs::write(&path, JpegSpec::canon().to_raf_bytes()).unwrap();

        let m = read_path(&path).unwrap();
        assert_eq!(m.model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(m.iso, Some(1600));
        assert_eq!(m.width, None, "preview dimensions are not the photo's");
    }

    #[test]
    fn missing_exif_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.jpg");
        JpegSpec::default().write(&path);

        let m = read_path(&path).unwrap();
        assert_eq!(m.make, None);
        assert_eq!((m.width, m.height), (Some(640), Some(480)));
    }

    #[test]
    fn photo_extensions() {
        assert!(is_photo(Path::new("x/IMG_0001.JPG")));
        assert!(is_photo(Path::new("x/DSC_1.nef")));
        assert!(!is_photo(Path::new("x/notes.txt")));
        assert!(!is_photo(Path::new("x/noext")));
    }
}
