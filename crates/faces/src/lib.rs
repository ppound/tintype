//! Faces: detect -> align -> embed, plus grouping and matching.
//!
//! Detection is YuNet, recognition is SFace (both from the OpenCV model zoo,
//! MIT and Apache-2.0). Embeddings are L2-normalized, so the dot product of
//! two embeddings is their cosine similarity.

mod cluster;

use std::path::Path;

use image::imageops::FilterType;
use image::{Rgb, RgbImage, RgbaImage};
use tintype_codecs::Rgba;
use tintype_infer::{Model, Tensor};

pub use cluster::{cluster, match_score, normalize, similarity};

/// Bump when detection or embedding changes; photos are then re-analyzed.
pub const FACES_VERSION: u32 = 1;
pub const EMBEDDING_DIM: usize = 128;
/// Long edge the photo is decoded at before detection; crops and alignment use this resolution.
pub const ANALYSIS_EDGE: u32 = 1600;

const DETECT_SIZE: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];
const SCORE_THRESHOLD: f32 = 0.8;
const NMS_IOU: f32 = 0.3;
/// Faces smaller than this (pixels, at ANALYSIS_EDGE) are too small to recognize reliably.
const MIN_FACE_PX: f32 = 36.0;
const ALIGNED: u32 = 112;
/// Where SFace expects the five landmarks in its 112x112 input.
const REFERENCE: [[f32; 2]; 5] =
    [[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366], [41.5493, 92.3655], [70.7299, 92.2041]];
/// Edge of the square face crops saved for the UI.
pub const CROP_EDGE: u32 = 160;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] tintype_infer::Error),
    #[error("unexpected model output: {0}")]
    Output(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A detected face, in pixels of the analyzed image.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// x, y, width, height.
    pub bbox: [f32; 4],
    /// Eyes, nose tip, mouth corners, as YuNet orders them.
    pub landmarks: [[f32; 2]; 5],
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Face {
    pub detection: Detection,
    /// Bounding box as fractions of the image (x, y, w, h), resolution-independent.
    pub rel_bbox: [f32; 4],
    pub embedding: Vec<f32>,
    /// A square crop around the face for display.
    pub crop: Rgba,
}

/// Loaded face models. Thread-safe; share one per process.
pub struct FaceAnalyzer {
    detector: Model,
    recognizer: Model,
}

impl FaceAnalyzer {
    pub fn load(yunet: &Path, sface: &Path) -> Result<Self> {
        Ok(Self {
            detector: Model::load(yunet, &[1, 3, DETECT_SIZE, DETECT_SIZE])?,
            recognizer: Model::load(sface, &[1, 3, ALIGNED as usize, ALIGNED as usize])?,
        })
    }

    /// Finds and embeds every face in an upright image.
    pub fn analyze(&self, img: &Rgba) -> Result<Vec<Face>> {
        let rgba = RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
            .ok_or_else(|| Error::Output("image buffer size mismatch".into()))?;
        let rgb = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
        let (w, h) = (rgb.width() as f32, rgb.height() as f32);
        self.detect(&rgb)?
            .into_iter()
            .filter(|d| d.bbox[2].min(d.bbox[3]) >= MIN_FACE_PX * w.max(h) / ANALYSIS_EDGE as f32)
            .map(|d| {
                let aligned = align(&rgb, &d.landmarks);
                let embedding = self.embed(&aligned)?;
                let crop = crop_square(&rgb, &d.bbox);
                let rel_bbox = [d.bbox[0] / w, d.bbox[1] / h, d.bbox[2] / w, d.bbox[3] / h];
                Ok(Face { detection: d, rel_bbox, embedding, crop })
            })
            .collect()
    }

    fn detect(&self, img: &RgbImage) -> Result<Vec<Detection>> {
        // Letterbox into the model's fixed square input: scale to fit, pad right/bottom.
        let k = DETECT_SIZE as f32 / img.width().max(img.height()) as f32;
        let (sw, sh) =
            (((img.width() as f32 * k).round() as u32).max(1), ((img.height() as f32 * k).round() as u32).max(1));
        let small = image::imageops::resize(img, sw, sh, FilterType::Triangle);
        let plane = DETECT_SIZE * DETECT_SIZE;
        let mut data = vec![0f32; 3 * plane];
        for (x, y, Rgb([r, g, b])) in small.enumerate_pixels() {
            let i = y as usize * DETECT_SIZE + x as usize;
            // YuNet takes BGR, 0-255.
            data[i] = *b as f32;
            data[plane + i] = *g as f32;
            data[2 * plane + i] = *r as f32;
        }
        let outputs = self.detector.run(Tensor { shape: vec![1, 3, DETECT_SIZE, DETECT_SIZE], data })?;
        let mut dets = decode_yunet(&outputs.into_iter().map(|(_, t)| t.data).collect::<Vec<_>>())?;
        for d in &mut dets {
            for v in &mut d.bbox {
                *v /= k;
            }
            for p in &mut d.landmarks {
                p[0] /= k;
                p[1] /= k;
            }
        }
        Ok(dets)
    }

    fn embed(&self, aligned: &RgbImage) -> Result<Vec<f32>> {
        let plane = (ALIGNED * ALIGNED) as usize;
        let mut data = vec![0f32; 3 * plane];
        for (x, y, Rgb(px)) in aligned.enumerate_pixels() {
            let i = (y * ALIGNED + x) as usize;
            // SFace takes RGB, 0-255.
            for c in 0..3 {
                data[c * plane + i] = px[c] as f32;
            }
        }
        let shape = vec![1, 3, ALIGNED as usize, ALIGNED as usize];
        let out = self.recognizer.run(Tensor { shape, data })?;
        let mut emb = out.into_iter().next().map(|(_, t)| t.data).unwrap_or_default();
        if emb.len() != EMBEDDING_DIM {
            return Err(Error::Output(format!("embedding has {} values, expected {EMBEDDING_DIM}", emb.len())));
        }
        normalize(&mut emb);
        Ok(emb)
    }
}

/// Decodes YuNet's 12 outputs (cls, obj, bbox, kps at strides 8/16/32) into
/// detections in 640x640 input pixels, after thresholding and NMS.
fn decode_yunet(out: &[Vec<f32>]) -> Result<Vec<Detection>> {
    if out.len() != 12 {
        return Err(Error::Output(format!("YuNet returned {} outputs, expected 12", out.len())));
    }
    let mut dets = Vec::new();
    for (si, &stride) in STRIDES.iter().enumerate() {
        let cols = DETECT_SIZE / stride;
        let n = cols * cols;
        let (cls, obj, bbox, kps) = (&out[si], &out[3 + si], &out[6 + si], &out[9 + si]);
        if cls.len() != n || obj.len() != n || bbox.len() != 4 * n || kps.len() != 10 * n {
            return Err(Error::Output(format!("YuNet stride {stride} outputs have unexpected sizes")));
        }
        for idx in 0..n {
            let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
            if score < SCORE_THRESHOLD {
                continue;
            }
            let (r, c) = ((idx / cols) as f32, (idx % cols) as f32);
            let s = stride as f32;
            let b = &bbox[idx * 4..idx * 4 + 4];
            let (cx, cy) = ((c + b[0]) * s, (r + b[1]) * s);
            let (w, h) = (b[2].exp() * s, b[3].exp() * s);
            let mut landmarks = [[0f32; 2]; 5];
            for (n, p) in landmarks.iter_mut().enumerate() {
                *p = [(kps[idx * 10 + 2 * n] + c) * s, (kps[idx * 10 + 2 * n + 1] + r) * s];
            }
            dets.push(Detection { bbox: [cx - w / 2.0, cy - h / 2.0, w, h], landmarks, score });
        }
    }
    Ok(nms(dets, NMS_IOU))
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);
    let x2 = (a[0] + a[2]).min(b[0] + b[2]);
    let y2 = (a[1] + a[3]).min(b[1] + b[3]);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Greedy non-maximum suppression, highest score first.
fn nms(mut dets: Vec<Detection>, threshold: f32) -> Vec<Detection> {
    dets.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for d in dets {
        if kept.iter().all(|k| iou(&k.bbox, &d.bbox) < threshold) {
            kept.push(d);
        }
    }
    kept
}

/// Least-squares similarity transform (rotation, uniform scale, translation)
/// mapping `src` points onto `dst`. Returns `[a, b, tx, ty]` for
/// `x' = a*x - b*y + tx`, `y' = b*x + a*y + ty`.
fn similarity_transform(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [f32; 4] {
    let mean = |p: &[[f32; 2]; 5]| {
        let (sx, sy) = p.iter().fold((0.0, 0.0), |(x, y), q| (x + q[0], y + q[1]));
        [sx / 5.0, sy / 5.0]
    };
    let (ms, md) = (mean(src), mean(dst));
    let (mut num_a, mut num_b, mut den) = (0.0, 0.0, 0.0);
    for (s, d) in src.iter().zip(dst) {
        let (sx, sy, dx, dy) = (s[0] - ms[0], s[1] - ms[1], d[0] - md[0], d[1] - md[1]);
        num_a += sx * dx + sy * dy;
        num_b += sx * dy - sy * dx;
        den += sx * sx + sy * sy;
    }
    let (a, b) = if den > 0.0 { (num_a / den, num_b / den) } else { (1.0, 0.0) };
    [a, b, md[0] - (a * ms[0] - b * ms[1]), md[1] - (b * ms[0] + a * ms[1])]
}

/// Warps the face so its landmarks land on SFace's reference positions.
fn align(img: &RgbImage, landmarks: &[[f32; 2]; 5]) -> RgbImage {
    let [a, b, tx, ty] = similarity_transform(landmarks, &REFERENCE);
    // Invert: for each output pixel, find where it comes from.
    let det = a * a + b * b;
    RgbImage::from_fn(ALIGNED, ALIGNED, |u, v| {
        let (dx, dy) = (u as f32 - tx, v as f32 - ty);
        let (x, y) = ((a * dx + b * dy) / det, (-b * dx + a * dy) / det);
        bilinear(img, x, y)
    })
}

fn bilinear(img: &RgbImage, x: f32, y: f32) -> Rgb<u8> {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let px = |xi: i64, yi: i64| -> [f32; 3] {
        if xi < 0 || yi < 0 || xi >= w || yi >= h {
            return [0.0; 3];
        }
        let Rgb(p) = *img.get_pixel(xi as u32, yi as u32);
        p.map(f32::from)
    };
    let (x0, y0) = (x0 as i64, y0 as i64);
    let (p00, p10, p01, p11) = (px(x0, y0), px(x0 + 1, y0), px(x0, y0 + 1), px(x0 + 1, y0 + 1));
    Rgb(std::array::from_fn(|c| {
        let top = p00[c] * (1.0 - fx) + p10[c] * fx;
        let bottom = p01[c] * (1.0 - fx) + p11[c] * fx;
        (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8
    }))
}

/// A square, padded crop centered on the face, resized for display.
fn crop_square(img: &RgbImage, bbox: &[f32; 4]) -> Rgba {
    let side = (bbox[2].max(bbox[3]) * 1.5).min(img.width().min(img.height()) as f32).max(1.0);
    let (cx, cy) = (bbox[0] + bbox[2] / 2.0, bbox[1] + bbox[3] / 2.0);
    let x = (cx - side / 2.0).clamp(0.0, img.width() as f32 - side) as u32;
    let y = (cy - side / 2.0).clamp(0.0, img.height() as f32 - side) as u32;
    let side = side as u32;
    let view = image::imageops::crop_imm(img, x, y, side, side).to_image();
    let resized = image::imageops::resize(&view, CROP_EDGE, CROP_EDGE, FilterType::Triangle);
    image::DynamicImage::ImageRgb8(resized).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(x: f32, score: f32) -> Detection {
        Detection { bbox: [x, 0.0, 10.0, 10.0], landmarks: [[0.0; 2]; 5], score }
    }

    #[test]
    fn nms_keeps_best_of_overlaps() {
        let kept = nms(vec![det(0.0, 0.85), det(1.0, 0.95), det(50.0, 0.9)], 0.3);
        let xs: Vec<f32> = kept.iter().map(|d| d.bbox[0]).collect();
        assert_eq!(xs, vec![1.0, 50.0]);
    }

    #[test]
    fn similarity_transform_recovers_known_transform() {
        // dst = 2 * rot(30deg) * src + (5, -3)
        let (c, s) = (2.0 * 30f32.to_radians().cos(), 2.0 * 30f32.to_radians().sin());
        let src = [[10.0, 10.0], [30.0, 12.0], [20.0, 25.0], [12.0, 35.0], [28.0, 36.0]];
        let dst = src.map(|[x, y]| [c * x - s * y + 5.0, s * x + c * y - 3.0]);
        let [a, b, tx, ty] = similarity_transform(&src, &dst);
        for (got, want) in [(a, c), (b, s), (tx, 5.0), (ty, -3.0)] {
            assert!((got - want).abs() < 1e-3, "{got} vs {want}");
        }
    }

    #[test]
    fn align_maps_landmarks_to_reference() {
        // An image with a bright dot at each landmark; after alignment the dots sit at REFERENCE.
        let landmarks = REFERENCE.map(|[x, y]| [x * 3.0 + 40.0, y * 3.0 + 20.0]);
        let mut img = RgbImage::new(500, 500);
        for [x, y] in landmarks {
            for dy in -3..=3 {
                for dx in -3..=3 {
                    img.put_pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32, Rgb([255, 255, 255]));
                }
            }
        }
        let out = align(&img, &landmarks);
        for [x, y] in REFERENCE {
            assert!(out.get_pixel(x.round() as u32, y.round() as u32)[0] > 200);
        }
        assert_eq!(out.get_pixel(5, 5)[0], 0);
    }

    #[test]
    fn decode_finds_one_confident_anchor() {
        let mut out: Vec<Vec<f32>> = Vec::new();
        for (k, per) in [(0, 1), (3, 1), (6, 4), (9, 10)] {
            for &s in &STRIDES {
                let n = (DETECT_SIZE / s).pow(2);
                out.insert(k + STRIDES.iter().position(|&x| x == s).unwrap(), vec![0.0; n * per]);
            }
        }
        // Stride 32, cell (row 2, col 3): cls 1, obj 1, box centered in cell, 64x64.
        let idx = 2 * 20 + 3;
        out[2][idx] = 1.0;
        out[5][idx] = 1.0;
        out[8][idx * 4..idx * 4 + 4].copy_from_slice(&[0.5, 0.5, 2f32.ln(), 2f32.ln()]);
        let dets = decode_yunet(&out).unwrap();
        assert_eq!(dets.len(), 1);
        let b = dets[0].bbox;
        assert_eq!(b, [3.5 * 32.0 - 32.0, 2.5 * 32.0 - 32.0, 64.0, 64.0]);
    }
}
