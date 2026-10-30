//! Image decoding for display: thumbnails and screen-sized previews, upright
//! according to EXIF orientation.
//!
//! JPEGs (and the JPEG embedded in Fujifilm RAF files) are decoded with DCT
//! scaling, so a 40 MP photo decodes at 1/2 to 1/8 size instead of full size.
//! Other formats go through the `image` crate. RAW formats other than RAF and
//! HEIC are not decoded yet.

use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot decode image: {0}")]
    Image(#[from] image::ImageError),
    #[error("cannot decode JPEG: {0}")]
    Jpeg(#[from] jpeg_decoder::Error),
    #[error("{0} files cannot be previewed yet")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decoded 8-bit RGBA pixels, row-major.
#[derive(Debug, Clone)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl From<DynamicImage> for Rgba {
    fn from(img: DynamicImage) -> Self {
        let rgba = img.into_rgba8();
        Self { width: rgba.width(), height: rgba.height(), pixels: rgba.into_raw() }
    }
}

/// Fujifilm RAF signature.
pub const RAF_MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";

/// Byte range (offset, length) of the JPEG embedded in a Fujifilm RAF, or
/// `None` if `file` is not a RAF. Leaves the file position unspecified.
pub fn raf_embedded_jpeg(file: &mut (impl Read + Seek)) -> std::io::Result<Option<(u64, u32)>> {
    let mut head = [0u8; 92];
    file.seek(SeekFrom::Start(0))?;
    if read_up_to(file, &mut head)? < head.len() || !head.starts_with(RAF_MAGIC) {
        return Ok(None);
    }
    // Big-endian offset and length at bytes 84..92.
    let offset = u32::from_be_bytes(head[84..88].try_into().expect("4 bytes"));
    let len = u32::from_be_bytes(head[88..92].try_into().expect("4 bytes"));
    Ok(Some((offset.into(), len)))
}

/// Like `read_exact`, but a short read is not an error. Returns bytes read.
pub fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Decodes `path` upright, scaled so its longer edge is at most `max_edge`.
pub fn decode_preview(path: &Path, max_edge: u32) -> Result<Rgba> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let img = match ext.as_str() {
        "jpg" | "jpeg" | "jpe" => decode_jpeg(&std::fs::read(path)?, max_edge)?,
        "raf" => {
            let mut file = File::open(path)?;
            let Some((offset, len)) = raf_embedded_jpeg(&mut file)? else {
                return Err(Error::Unsupported("this RAF".into()));
            };
            let mut jpeg = vec![0; len as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut jpeg)?;
            decode_jpeg(&jpeg, max_edge)?
        }
        "png" | "webp" | "tif" | "tiff" => {
            let mut decoder =
                ImageReader::new(BufReader::new(File::open(path)?)).with_guessed_format()?.into_decoder()?;
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            let mut img = DynamicImage::from_decoder(decoder)?;
            img.apply_orientation(orientation);
            fit(img, max_edge)
        }
        _ => return Err(Error::Unsupported(if ext.is_empty() { "these".into() } else { ext.to_uppercase() })),
    };
    Ok(img.into())
}

/// Decodes a JPEG with DCT scaling to the smallest size at least `max_edge`
/// on the long side, then resizes down and applies EXIF orientation.
fn decode_jpeg(bytes: &[u8], max_edge: u32) -> Result<DynamicImage> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.read_info()?;
    let info = decoder.info().expect("read_info succeeded");
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    let scale = max_edge as f64 / w.max(h).max(1) as f64;
    if scale < 1.0 {
        let req = |v: u32| ((v as f64 * scale).ceil() as u16).max(1);
        decoder.scale(req(w), req(h))?;
    }
    let pixels = decoder.decode()?;
    let info = decoder.info().expect("decoded");
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    let rgb: Vec<u8> = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => pixels.iter().flat_map(|&l| [l, l, l]).collect(),
        jpeg_decoder::PixelFormat::L16 => pixels.as_chunks::<2>().0.iter().flat_map(|&[hi, _]| [hi, hi, hi]).collect(),
        // Adobe CMYK JPEGs store inverted values.
        jpeg_decoder::PixelFormat::CMYK32 => pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[c, m, y, k]| [c, m, y].map(|v| (u32::from(v) * u32::from(k) / 255) as u8))
            .collect(),
    };
    let img = RgbImage::from_raw(w, h, rgb).ok_or_else(|| Error::Unsupported("this JPEG layout".into()))?;
    let mut img = fit(DynamicImage::ImageRgb8(img), max_edge);
    img.apply_orientation(jpeg_orientation(bytes));
    Ok(img)
}

fn jpeg_orientation(bytes: &[u8]) -> Orientation {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|e| e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?.value.get_uint(0))
        .and_then(|v| Orientation::from_exif(v as u8))
        .unwrap_or(Orientation::NoTransforms)
}

fn fit(img: DynamicImage, max_edge: u32) -> DynamicImage {
    if img.width().max(img.height()) <= max_edge { img } else { img.thumbnail(max_edge, max_edge) }
}

/// A disk cache of thumbnails, keyed by path, size and modification time, so an
/// edited file gets a fresh thumbnail. Safe to share between threads.
#[derive(Debug, Clone)]
pub struct ThumbCache {
    dir: PathBuf,
    edge: u32,
}

impl ThumbCache {
    /// Thumbnails are stored with their long edge at most `edge` pixels.
    pub fn new(dir: impl Into<PathBuf>, edge: u32) -> Self {
        Self { dir: dir.into(), edge }
    }

    pub fn edge(&self) -> u32 {
        self.edge
    }

    /// The thumbnail for `path`, from the cache or freshly decoded (and then cached).
    pub fn get(&self, path: &Path) -> Result<Rgba> {
        let cached = self.cache_path(path)?;
        if let Ok(bytes) = std::fs::read(&cached)
            && let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        {
            return Ok(img.into());
        }
        let thumb = decode_preview(path, self.edge)?;
        // A failed cache write only costs a re-decode next time.
        let _ = self.store(&cached, &thumb);
        Ok(thumb)
    }

    fn cache_path(&self, path: &Path) -> Result<PathBuf> {
        let md = std::fs::metadata(path)?;
        let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        let mut h = blake3::Hasher::new();
        h.update(path.as_os_str().as_encoded_bytes());
        h.update(&md.len().to_le_bytes());
        h.update(&mtime.to_le_bytes());
        h.update(&self.edge.to_le_bytes());
        let key = h.finalize().to_hex();
        Ok(self.dir.join(&key[..2]).join(format!("{}.jpg", &key[..32])))
    }

    fn store(&self, cached: &Path, thumb: &Rgba) -> Result<()> {
        std::fs::create_dir_all(cached.parent().expect("cache path has a parent"))?;
        let rgb: Vec<u8> = thumb.pixels.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
        let tmp = cached.with_extension("tmp");
        let mut out = std::io::BufWriter::new(File::create(&tmp)?);
        JpegEncoder::new_with_quality(&mut out, 85).encode(
            &rgb,
            thumb.width,
            thumb.height,
            image::ExtendedColorType::Rgb8,
        )?;
        drop(out);
        std::fs::rename(tmp, cached)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real (pixel-carrying) JPEG, `w` x `h`, left half red, right half blue.
    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img =
            RgbImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).unwrap();
        out
    }

    #[test]
    fn jpeg_is_scaled_to_fit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jpg");
        std::fs::write(&path, jpeg(1600, 1200)).unwrap();
        let img = decode_preview(&path, 256).unwrap();
        assert_eq!((img.width, img.height), (256, 192));
        assert_eq!(img.pixels.len(), 256 * 192 * 4);
        // Left side stays red.
        let px = &img.pixels[(96 * 256 + 10) * 4..][..3];
        assert!(px[0] > 200 && px[2] < 60, "{px:?}");
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.jpg");
        std::fs::write(&path, jpeg(100, 50)).unwrap();
        let img = decode_preview(&path, 256).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
    }

    #[test]
    fn raf_uses_embedded_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.RAF");
        let preview = jpeg(800, 600);
        let mut raf = RAF_MAGIC.to_vec();
        raf.resize(84, 0);
        raf.extend_from_slice(&200u32.to_be_bytes());
        raf.extend_from_slice(&(preview.len() as u32).to_be_bytes());
        raf.resize(200, 0);
        raf.extend_from_slice(&preview);
        std::fs::write(&path, raf).unwrap();
        let img = decode_preview(&path, 400).unwrap();
        assert_eq!((img.width, img.height), (400, 300));
    }

    #[test]
    fn cache_round_trip_and_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, jpeg(640, 480)).unwrap();
        let cache = ThumbCache::new(dir.path().join("thumbs"), 128);

        let first = cache.get(&path).unwrap();
        let entry = cache.cache_path(&path).unwrap();
        assert!(entry.exists());
        let again = cache.get(&path).unwrap();
        assert_eq!((again.width, again.height), (first.width, first.height));

        std::fs::write(&path, jpeg(320, 480)).unwrap();
        assert_ne!(cache.cache_path(&path).unwrap(), entry, "new size, new key");
        assert_eq!(cache.get(&path).unwrap().height, 128);
    }

    #[test]
    fn unsupported_formats_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.heic");
        std::fs::write(&path, b"....").unwrap();
        assert!(matches!(decode_preview(&path, 100), Err(Error::Unsupported(e)) if e == "HEIC"));
    }
}
