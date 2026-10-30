//! Test fixtures. Builds tiny JPEG files (headers only, no pixel data) carrying
//! chosen EXIF, so tests never depend on a checked-in photo corpus.

use std::io::Cursor;
use std::path::Path;

use exif::experimental::Writer;
use exif::{Field, In, Rational, Tag, Value};

/// Degrees, minutes, seconds as EXIF rationals (numerator, denominator).
pub type Dms = [(u32, u32); 3];

/// EXIF to embed in a synthetic JPEG. `Default` writes no EXIF at all.
#[derive(Debug, Clone)]
pub struct JpegSpec {
    pub width: u16,
    pub height: u16,
    pub make: Option<&'static str>,
    pub model: Option<&'static str>,
    pub lens: Option<&'static str>,
    /// `"YYYY:MM:DD HH:MM:SS"`, as EXIF stores it.
    pub taken: Option<&'static str>,
    pub iso: Option<u16>,
    pub f_number: Option<(u32, u32)>,
    pub exposure: Option<(u32, u32)>,
    pub focal_mm: Option<(u32, u32)>,
    /// (lat dms, 'N'|'S', lon dms, 'E'|'W')
    pub gps: Option<(Dms, u8, Dms, u8)>,
}

impl Default for JpegSpec {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            make: None,
            model: None,
            lens: None,
            taken: None,
            iso: None,
            f_number: None,
            exposure: None,
            focal_mm: None,
            gps: None,
        }
    }
}

impl JpegSpec {
    /// A Canon shot at ISO 1600, f/2.8, 1/250s, 50mm, in London (51.5N, 0.125W).
    pub fn canon() -> Self {
        Self {
            make: Some("Canon"),
            model: Some("Canon EOS R5"),
            lens: Some("RF24-70mm F2.8 L IS USM"),
            taken: Some("2023:05:14 10:30:00"),
            iso: Some(1600),
            f_number: Some((28, 10)),
            exposure: Some((1, 250)),
            focal_mm: Some((50, 1)),
            gps: Some(([(51, 1), (30, 1), (0, 1)], b'N', [(0, 1), (7, 1), (30, 1)], b'W')),
            ..Self::default()
        }
    }

    /// A Nikon shot at ISO 200, f/8, 1/1000s, 24mm, no GPS.
    pub fn nikon() -> Self {
        Self {
            make: Some("NIKON CORPORATION"),
            model: Some("NIKON Z 6"),
            lens: Some("NIKKOR Z 24-70mm f/4 S"),
            taken: Some("2021:08:02 17:45:10"),
            iso: Some(200),
            f_number: Some((8, 1)),
            exposure: Some((1, 1000)),
            focal_mm: Some((24, 1)),
            ..Self::default()
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8]; // SOI
        if let Some(tiff) = self.exif_tiff() {
            let len = u16::try_from(2 + 6 + tiff.len()).expect("EXIF too large for APP1");
            out.extend_from_slice(&[0xFF, 0xE1]);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(b"Exif\0\0");
            out.extend_from_slice(&tiff);
        }
        // SOF0: 8-bit, height, width, 3 components.
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
    }

    /// A minimal Fujifilm RAF: header pointing at this JPEG, then the JPEG. No sensor data.
    pub fn to_raf_bytes(&self) -> Vec<u8> {
        const JPEG_OFFSET: u32 = 160;
        let jpeg = self.to_bytes();
        let mut out = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
        out.resize(84, 0);
        out.extend_from_slice(&JPEG_OFFSET.to_be_bytes());
        out.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
        out.resize(JPEG_OFFSET as usize, 0);
        out.extend_from_slice(&jpeg);
        out
    }

    pub fn write(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(path, self.to_bytes()).expect("write fixture");
    }

    fn exif_tiff(&self) -> Option<Vec<u8>> {
        let ascii = |tag, s: &str| field(tag, Value::Ascii(vec![s.as_bytes().to_vec()]));
        let rat = |tag, (num, denom): (u32, u32)| field(tag, Value::Rational(vec![Rational { num, denom }]));
        let dms =
            |tag, v: Dms| field(tag, Value::Rational(v.iter().map(|&(num, denom)| Rational { num, denom }).collect()));

        let mut fields = Vec::new();
        fields.extend(self.make.map(|s| ascii(Tag::Make, s)));
        fields.extend(self.model.map(|s| ascii(Tag::Model, s)));
        fields.extend(self.lens.map(|s| ascii(Tag::LensModel, s)));
        fields.extend(self.taken.map(|s| ascii(Tag::DateTimeOriginal, s)));
        fields.extend(self.iso.map(|v| field(Tag::PhotographicSensitivity, Value::Short(vec![v]))));
        fields.extend(self.f_number.map(|v| rat(Tag::FNumber, v)));
        fields.extend(self.exposure.map(|v| rat(Tag::ExposureTime, v)));
        fields.extend(self.focal_mm.map(|v| rat(Tag::FocalLength, v)));
        if let Some((lat, lat_ref, lon, lon_ref)) = self.gps {
            fields.push(dms(Tag::GPSLatitude, lat));
            fields.push(field(Tag::GPSLatitudeRef, Value::Ascii(vec![vec![lat_ref]])));
            fields.push(dms(Tag::GPSLongitude, lon));
            fields.push(field(Tag::GPSLongitudeRef, Value::Ascii(vec![vec![lon_ref]])));
        }
        if fields.is_empty() {
            return None;
        }

        let mut writer = Writer::new();
        for f in &fields {
            writer.push_field(f);
        }
        let mut buf = Cursor::new(Vec::new());
        writer.write(&mut buf, false).expect("serialize EXIF");
        Some(buf.into_inner())
    }
}

fn field(tag: Tag, value: Value) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value }
}
