//! DNG previews and image size, read from the TIFF directory tree without
//! touching the raw sensor data.
//!
//! A DNG holds the raw image in one IFD (`NewSubfileType` 0) and one or more
//! rendered previews in others (`NewSubfileType` 1): a JPEG, or for some
//! phones only a small uncompressed RGB thumbnail. Tintype shows the largest
//! preview, so a DNG looks as good as the preview its maker embedded.

use std::io::{Read, Seek, SeekFrom};

const NEW_SUBFILE_TYPE: u16 = 0xFE;
const IMAGE_WIDTH: u16 = 0x100;
const IMAGE_LENGTH: u16 = 0x101;
const BITS_PER_SAMPLE: u16 = 0x102;
const COMPRESSION: u16 = 0x103;
const PHOTOMETRIC: u16 = 0x106;
const STRIP_OFFSETS: u16 = 0x111;
const ORIENTATION: u16 = 0x112;
const SAMPLES_PER_PIXEL: u16 = 0x115;
const STRIP_BYTE_COUNTS: u16 = 0x117;
const SUB_IFDS: u16 = 0x14A;
const JPEG_INTERCHANGE_FORMAT: u16 = 0x201;
const JPEG_INTERCHANGE_FORMAT_LENGTH: u16 = 0x202;
const DEFAULT_CROP_SIZE: u16 = 0xC620;

/// Guards against corrupt or looping directory trees.
const MAX_IFDS: usize = 64;
const MAX_ENTRIES: u16 = 1024;
const MAX_STRIPS: u32 = 4096;
/// Largest preview we will read into memory.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A baseline JPEG stream.
    Jpeg,
    /// Uncompressed 8-bit RGB, row-major.
    Rgb8,
}

/// A rendered preview stored in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub encoding: Encoding,
    /// Byte ranges (offset, length) that together hold the preview.
    pub strips: Vec<(u64, u64)>,
}

/// What Tintype needs from a DNG.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dng {
    /// Width and height of the final image (the raw's default crop), as stored, before orientation.
    pub size: Option<(u32, u32)>,
    /// EXIF orientation of the whole file (1 if absent).
    pub orientation: u16,
    /// The largest preview, if any.
    pub preview: Option<Preview>,
}

impl Dng {
    /// Reads the directory tree of a TIFF-based file; `None` if it isn't TIFF.
    pub fn read(file: &mut (impl Read + Seek)) -> std::io::Result<Option<Self>> {
        let mut head = [0u8; 8];
        file.seek(SeekFrom::Start(0))?;
        if crate::read_up_to(file, &mut head)? < head.len() {
            return Ok(None);
        }
        let big = match &head[..4] {
            b"II*\0" => false,
            b"MM\0*" => true,
            _ => return Ok(None),
        };
        let mut tiff = Tiff { r: file, big };
        let first = tiff.u32_at(&head[4..8]);

        let mut ifds = Vec::new();
        let mut queue = vec![(first, true)];
        let mut seen = Vec::new();
        while let Some((offset, follow_next)) = queue.pop() {
            if offset == 0 || seen.contains(&offset) || seen.len() >= MAX_IFDS {
                continue;
            }
            seen.push(offset);
            let Ok((ifd, next)) = tiff.ifd(offset) else { continue };
            if follow_next {
                queue.push((next, true));
            }
            queue.extend(ifd.sub_ifds.iter().map(|&o| (o, false)));
            ifds.push(ifd);
        }

        let orientation = ifds.first().and_then(|i| i.orientation).unwrap_or(1);
        let size = ifds
            .iter()
            .filter(|i| i.subfile_type == 0)
            .filter_map(|i| i.crop_size.or(Some((i.width?, i.height?))))
            .max_by_key(|&(w, h)| u64::from(w) * u64::from(h));
        let preview = ifds.iter().filter_map(Ifd::preview).max_by_key(|p| u64::from(p.width) * u64::from(p.height));
        Ok(Some(Self { size, orientation, preview }))
    }
}

#[derive(Debug, Default)]
struct Ifd {
    subfile_type: u32,
    width: Option<u32>,
    height: Option<u32>,
    bits: Option<u32>,
    compression: Option<u32>,
    photometric: Option<u32>,
    samples: Option<u32>,
    strip_offsets: Vec<u32>,
    strip_counts: Vec<u32>,
    jpeg: Option<(u32, u32)>,
    orientation: Option<u16>,
    crop_size: Option<(u32, u32)>,
    sub_ifds: Vec<u32>,
}

impl Ifd {
    fn preview(&self) -> Option<Preview> {
        let (width, height) = (self.width.filter(|&w| w > 0)?, self.height.filter(|&h| h > 0)?);
        // Raw data (CFA or linear raw) is never a preview; only 8-bit RGB or YCbCr.
        if self.subfile_type & 1 == 0 || !matches!(self.photometric, Some(2 | 6)) || self.bits != Some(8) {
            return None;
        }
        let strips: Vec<(u64, u64)> = match (self.compression, self.jpeg) {
            // Old-style JPEG points at a whole JFIF stream.
            (Some(6), Some((offset, len))) => vec![(offset.into(), len.into())],
            (Some(6 | 7 | 1), _) if self.strip_offsets.len() == self.strip_counts.len() => {
                self.strip_offsets.iter().zip(&self.strip_counts).map(|(&o, &c)| (o.into(), c.into())).collect()
            }
            _ => return None,
        };
        let total: u64 = strips.iter().map(|&(_, c)| c).sum();
        if strips.is_empty() || total == 0 || total > MAX_PREVIEW_BYTES {
            return None;
        }
        let encoding = match self.compression? {
            1 if self.photometric == Some(2) && self.samples == Some(3) => {
                if total < u64::from(width) * u64::from(height) * 3 {
                    return None;
                }
                Encoding::Rgb8
            }
            // A JPEG preview must be one stream: one strip, or strips back to back.
            6 | 7 if strips.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0) => Encoding::Jpeg,
            _ => return None,
        };
        Some(Preview { width, height, encoding, strips })
    }
}

struct Tiff<'a, R> {
    r: &'a mut R,
    big: bool,
}

impl<R: Read + Seek> Tiff<'_, R> {
    fn u16_at(&self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        if self.big { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) }
    }

    fn u32_at(&self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        if self.big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.r.seek(SeekFrom::Start(offset))?;
        self.r.read_exact(buf)
    }

    /// Reads the IFD at `offset`; returns it and the offset of the next one.
    fn ifd(&mut self, offset: u32) -> std::io::Result<(Ifd, u32)> {
        let mut count = [0u8; 2];
        self.read_at(offset.into(), &mut count)?;
        let count = self.u16_at(&count).min(MAX_ENTRIES);
        let mut entries = vec![0u8; usize::from(count) * 12 + 4];
        self.read_at(u64::from(offset) + 2, &mut entries)?;
        let next = self.u32_at(&entries[entries.len() - 4..]);

        let mut ifd = Ifd::default();
        for e in entries[..entries.len() - 4].as_chunks::<12>().0 {
            let tag = self.u16_at(&e[0..2]);
            let first = |s: &mut Self| s.values(e).ok().and_then(|v| v.first().copied());
            match tag {
                NEW_SUBFILE_TYPE => ifd.subfile_type = first(self).unwrap_or(0),
                IMAGE_WIDTH => ifd.width = first(self),
                IMAGE_LENGTH => ifd.height = first(self),
                BITS_PER_SAMPLE => ifd.bits = first(self),
                COMPRESSION => ifd.compression = first(self),
                PHOTOMETRIC => ifd.photometric = first(self),
                SAMPLES_PER_PIXEL => ifd.samples = first(self),
                ORIENTATION => ifd.orientation = first(self).and_then(|v| u16::try_from(v).ok()),
                JPEG_INTERCHANGE_FORMAT => {
                    ifd.jpeg = first(self).map(|o| (o, ifd.jpeg.map_or(0, |j| j.1)));
                }
                JPEG_INTERCHANGE_FORMAT_LENGTH => {
                    ifd.jpeg = first(self).map(|l| (ifd.jpeg.map_or(0, |j| j.0), l));
                }
                STRIP_OFFSETS | STRIP_BYTE_COUNTS | SUB_IFDS | DEFAULT_CROP_SIZE => {
                    let Ok(v) = self.values(e) else { continue };
                    match tag {
                        STRIP_OFFSETS => ifd.strip_offsets = v,
                        STRIP_BYTE_COUNTS => ifd.strip_counts = v,
                        SUB_IFDS => ifd.sub_ifds = v,
                        _ => ifd.crop_size = (v.len() == 2 && v[0] > 0 && v[1] > 0).then(|| (v[0], v[1])),
                    }
                }
                _ => {}
            }
        }
        Ok((ifd, next))
    }

    /// An entry's values as integers (rationals rounded); empty for other types.
    fn values(&mut self, e: &[u8]) -> std::io::Result<Vec<u32>> {
        let typ = self.u16_at(&e[2..4]);
        let count = self.u32_at(&e[4..8]);
        let size: u32 = match typ {
            3 => 2,      // SHORT
            4 | 13 => 4, // LONG, IFD
            5 => 8,      // RATIONAL
            _ => return Ok(Vec::new()),
        };
        if count == 0 || count > MAX_STRIPS {
            return Ok(Vec::new());
        }
        let len = (size * count) as usize;
        let data = if len <= 4 {
            e[8..8 + len].to_vec()
        } else {
            let mut buf = vec![0u8; len];
            self.read_at(self.u32_at(&e[8..12]).into(), &mut buf)?;
            buf
        };
        Ok(match typ {
            3 => data.as_chunks::<2>().0.iter().map(|c| u32::from(self.u16_at(c))).collect(),
            5 => data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| {
                    let (num, den) = (self.u32_at(&c[..4]), self.u32_at(&c[4..]));
                    if den == 0 { 0 } else { ((f64::from(num) / f64::from(den)).round()) as u32 }
                })
                .collect(),
            _ => data.as_chunks::<4>().0.iter().map(|c| self.u32_at(c)).collect(),
        })
    }
}

/// Reads a preview's bytes.
pub fn read_preview(file: &mut (impl Read + Seek), preview: &Preview) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for &(offset, len) in &preview.strips {
        file.seek(SeekFrom::Start(offset))?;
        let start = out.len();
        out.resize(start + len as usize, 0);
        let n = crate::read_up_to(file, &mut out[start..])?;
        out.truncate(start + n);
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    /// Builds a little-endian DNG-shaped TIFF: IFD0 is an uncompressed RGB
    /// thumbnail, with sub-IFDs for a raw image (with a default crop) and a JPEG
    /// preview.
    pub(crate) fn dng(
        thumb: (u32, u32),
        raw: (u32, u32),
        crop: (u32, u32),
        jpeg: &[u8],
        jpeg_size: (u32, u32),
        orientation: u16,
    ) -> Vec<u8> {
        struct E(u16, u16, Vec<u32>);
        fn ifd(out: &mut Vec<u8>, entries: &[E], next: u32) -> Vec<(usize, u16)> {
            // Returns where each out-of-line value block must be written: (patch position, entry index).
            out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            let mut patches = Vec::new();
            for (i, E(tag, typ, vals)) in entries.iter().enumerate() {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&typ.to_le_bytes());
                let n = if *typ == 5 { vals.len() / 2 } else { vals.len() };
                out.extend_from_slice(&(n as u32).to_le_bytes());
                let bytes: Vec<u8> = match typ {
                    3 => vals.iter().flat_map(|&v| (v as u16).to_le_bytes()).collect(),
                    _ => vals.iter().flat_map(|&v| v.to_le_bytes()).collect(),
                };
                if bytes.len() <= 4 {
                    let mut b = bytes.clone();
                    b.resize(4, 0);
                    out.extend_from_slice(&b);
                } else {
                    patches.push((out.len(), i as u16));
                    out.extend_from_slice(&[0; 4]);
                }
            }
            out.extend_from_slice(&next.to_le_bytes());
            patches
        }
        fn write_blocks(out: &mut Vec<u8>, entries: &[E], patches: Vec<(usize, u16)>) {
            for (pos, i) in patches {
                let E(_, typ, vals) = &entries[usize::from(i)];
                let at = out.len() as u32;
                out[pos..pos + 4].copy_from_slice(&at.to_le_bytes());
                for &v in vals {
                    if *typ == 3 {
                        out.extend_from_slice(&(v as u16).to_le_bytes())
                    } else {
                        out.extend_from_slice(&v.to_le_bytes())
                    }
                }
            }
        }

        let thumb_len = thumb.0 * thumb.1 * 3;
        // Layout: header, IFD0 + blocks, raw IFD + blocks, preview IFD + blocks, thumbnail pixels, JPEG.
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        let mut ifd0 = vec![
            E(0xFE, 4, vec![1]),
            E(0x100, 4, vec![thumb.0]),
            E(0x101, 4, vec![thumb.1]),
            E(0x102, 3, vec![8, 8, 8]),
            E(0x103, 3, vec![1]),
            E(0x106, 3, vec![2]),
            E(0x111, 4, vec![0]),
            E(0x112, 3, vec![u32::from(orientation)]),
            E(0x115, 3, vec![3]),
            E(0x117, 4, vec![thumb_len]),
            E(0x14A, 4, vec![0, 0]),
        ];
        let raw_ifd = vec![
            E(0xFE, 4, vec![0]),
            E(0x100, 4, vec![raw.0]),
            E(0x101, 4, vec![raw.1]),
            E(0x102, 3, vec![16]),
            E(0x103, 3, vec![7]),
            E(0x106, 3, vec![32803]),
            E(0xC620, 5, vec![crop.0, 1, crop.1, 1]),
        ];
        let mut preview_ifd = vec![
            E(0xFE, 4, vec![1]),
            E(0x100, 4, vec![jpeg_size.0]),
            E(0x101, 4, vec![jpeg_size.1]),
            E(0x102, 3, vec![8, 8, 8]),
            E(0x103, 3, vec![7]),
            E(0x106, 3, vec![6]),
            E(0x111, 4, vec![0]),
            E(0x115, 3, vec![3]),
            E(0x117, 4, vec![jpeg.len() as u32]),
        ];
        // Two passes: lay out once to learn offsets, then again with them filled in.
        let mut offsets = (0u32, 0u32, 0u32, 0u32);
        for _ in 0..2 {
            out.truncate(8);
            ifd0[6].2 = vec![offsets.2];
            ifd0[10].2 = vec![offsets.0, offsets.1];
            preview_ifd[6].2 = vec![offsets.3];
            let p = ifd(&mut out, &ifd0, 0);
            write_blocks(&mut out, &ifd0, p);
            offsets.0 = out.len() as u32;
            let p = ifd(&mut out, &raw_ifd, 0);
            write_blocks(&mut out, &raw_ifd, p);
            offsets.1 = out.len() as u32;
            let p = ifd(&mut out, &preview_ifd, 0);
            write_blocks(&mut out, &preview_ifd, p);
            offsets.2 = out.len() as u32;
            out.extend((0..thumb_len).map(|i| if i % 3 == 1 { 200 } else { 0 })); // green
            offsets.3 = out.len() as u32;
            out.extend_from_slice(jpeg);
        }
        out
    }
}
