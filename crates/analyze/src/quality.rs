//! How good a photo is: the LAION aesthetic score, and sharpness for picking
//! the best frame of a burst.
//!
//! Measured on a real library: the LAION predictor (CLIP ViT-L/14 + a small
//! MLP trained on human ratings) put well-composed scenes on top and weak
//! snapshots at the bottom; asking SigLIP "beautiful vs bad photo" only ranked
//! subjects. Absolute sharpness was no use for spotting rejects (a plane in an
//! empty sky and shallow-focus portraits scored lowest), so it is only used to
//! compare frames of one stack.

use std::io::Read;
use std::path::Path;

use image::imageops::FilterType;
use image::{GrayImage, RgbImage, RgbaImage};
use tintype_codecs::Rgba;
use tintype_infer::{Model, Tensor};

use crate::{Error, Result};

/// Bump when sharpness changes; photos are then measured again.
pub const SHARPNESS_VERSION: u32 = 1;
/// Bump when the aesthetic score changes.
pub const AESTHETIC_VERSION: u32 = 1;
/// Long edge the photo is decoded at for the quality pass.
pub const QUALITY_EDGE: u32 = 1200;

const CLIP_INPUT: u32 = 224;
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];
const CLIP_DIM: usize = 768;
/// (out, in) of each linear layer of the head, in order. The archive's
/// blocks 0..=9 are weight, bias of each in turn (checked against the
/// pinned file's pickle with `pickletools`, without loading it).
const HEAD_LAYERS: [(usize, usize); 5] = [(1024, 768), (128, 1024), (64, 128), (16, 64), (1, 16)];

/// Scores photos with the LAION aesthetic predictor (roughly 1-10; most
/// photos land between 4 and 7). Thread-safe.
pub struct AestheticScorer {
    clip: Model,
    head: Vec<(Vec<f32>, Vec<f32>)>,
}

impl AestheticScorer {
    pub fn load(clip_vision: &Path, head: &Path) -> Result<Self> {
        Ok(Self {
            clip: Model::load(clip_vision, &[1, 3, CLIP_INPUT as usize, CLIP_INPUT as usize])?,
            head: read_head(head)?,
        })
    }

    pub fn score(&self, img: &Rgba) -> Result<f32> {
        let out = self.clip.run(clip_input(img)?)?;
        let mut emb = out
            .into_iter()
            .find(|(_, t)| t.shape == [1, CLIP_DIM])
            .map(|(_, t)| t.data)
            .ok_or_else(|| Error::Output(format!("no [1, {CLIP_DIM}] CLIP embedding")))?;
        let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        emb.iter_mut().for_each(|x| *x /= norm.max(f32::EPSILON));
        Ok(apply_head(&self.head, &emb))
    }
}

/// The head's weights from the PyTorch zip, as raw little-endian float32 blocks.
fn read_head(path: &Path) -> Result<Vec<(Vec<f32>, Vec<f32>)>> {
    let bad = |m: String| Error::Output(format!("{}: {m}", path.display()));
    let file = std::fs::File::open(path).map_err(|e| bad(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| bad(e.to_string()))?;
    let mut block = |k: usize, len: usize| -> Result<Vec<f32>> {
        let mut entry = zip.by_name(&format!("archive/data/{k}")).map_err(|e| bad(e.to_string()))?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| bad(e.to_string()))?;
        if bytes.len() != len * 4 {
            return Err(bad(format!("block {k} has {} bytes, expected {}", bytes.len(), len * 4)));
        }
        Ok(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
    };
    HEAD_LAYERS
        .iter()
        .enumerate()
        .map(|(i, &(out, inp))| Ok((block(2 * i, out * inp)?, block(2 * i + 1, out)?)))
        .collect()
}

/// The "linearMSE" head: five linear layers, no activations.
fn apply_head(layers: &[(Vec<f32>, Vec<f32>)], input: &[f32]) -> f32 {
    let mut x = input.to_vec();
    for (w, b) in layers {
        let inp = x.len();
        x = b
            .iter()
            .enumerate()
            .map(|(o, bias)| bias + w[o * inp..(o + 1) * inp].iter().zip(&x).map(|(a, v)| a * v).sum::<f32>())
            .collect();
    }
    x[0]
}

/// CLIP's preprocessing: shortest side to 224, centre crop, normalize, NCHW.
fn clip_input(img: &Rgba) -> Result<Tensor> {
    let rgb = to_rgb(img)?;
    let k = CLIP_INPUT as f32 / rgb.width().min(rgb.height()) as f32;
    let (w, h) = (
        ((rgb.width() as f32 * k).round() as u32).max(CLIP_INPUT),
        ((rgb.height() as f32 * k).round() as u32).max(CLIP_INPUT),
    );
    let resized = image::imageops::resize(&rgb, w, h, FilterType::CatmullRom);
    let crop = image::imageops::crop_imm(&resized, (w - CLIP_INPUT) / 2, (h - CLIP_INPUT) / 2, CLIP_INPUT, CLIP_INPUT)
        .to_image();
    let plane = (CLIP_INPUT * CLIP_INPUT) as usize;
    let mut data = vec![0f32; 3 * plane];
    for (x, y, px) in crop.enumerate_pixels() {
        let i = (y * CLIP_INPUT + x) as usize;
        for c in 0..3 {
            data[c * plane + i] = (px[c] as f32 / 255.0 - CLIP_MEAN[c]) / CLIP_STD[c];
        }
    }
    Ok(Tensor { shape: vec![1, 3, CLIP_INPUT as usize, CLIP_INPUT as usize], data })
}

fn to_rgb(img: &Rgba) -> Result<RgbImage> {
    let rgba = RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
        .ok_or_else(|| Error::Output("image buffer size mismatch".into()))?;
    Ok(image::DynamicImage::ImageRgba8(rgba).into_rgb8())
}

/// How sharp the sharpest part of the photo is: the variance of the Laplacian
/// in an 8x8 grid of tiles, taking the 90th percentile tile (the subject is
/// sharp somewhere; the background may be blurred on purpose). Only meaningful
/// between frames of the same scene at the same size.
pub fn sharpness(img: &Rgba) -> Result<f32> {
    let gray: GrayImage = image::DynamicImage::ImageRgb8(to_rgb(img)?).into_luma8();
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    if w < 24 || h < 24 {
        return Ok(0.0);
    }
    let px = |x: usize, y: usize| gray.as_raw()[y * w + x] as f32;
    let (tw, th) = ((w - 2) / 8, (h - 2) / 8);
    let mut tiles = Vec::with_capacity(64);
    for ty in 0..8 {
        for tx in 0..8 {
            let (mut sum, mut sq, mut n) = (0f64, 0f64, 0f64);
            for y in 1 + ty * th..1 + (ty + 1) * th {
                for x in 1 + tx * tw..1 + (tx + 1) * tw {
                    let lap = 4.0 * px(x, y) - px(x - 1, y) - px(x + 1, y) - px(x, y - 1) - px(x, y + 1);
                    sum += f64::from(lap);
                    sq += f64::from(lap * lap);
                    n += 1.0;
                }
            }
            tiles.push((sq / n - (sum / n).powi(2)) as f32);
        }
    }
    tiles.sort_by(f32::total_cmp);
    Ok(tiles[(tiles.len() * 9) / 10 - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(edge: u32, cell: u32) -> Rgba {
        let img = RgbaImage::from_fn(edge, edge, |x, y| {
            let v = if (x / cell + y / cell).is_multiple_of(2) { 255 } else { 0 };
            image::Rgba([v, v, v, 255])
        });
        image::DynamicImage::ImageRgba8(img).into()
    }

    #[test]
    fn blurring_lowers_sharpness() {
        let sharp = checker(240, 6);
        let rgba = RgbaImage::from_raw(sharp.width, sharp.height, sharp.pixels.clone()).unwrap();
        let blurred: Rgba = image::DynamicImage::ImageRgba8(image::imageops::blur(&rgba, 3.0)).into();
        assert!(sharpness(&sharp).unwrap() > 4.0 * sharpness(&blurred).unwrap());
        assert_eq!(sharpness(&checker(10, 2)).unwrap(), 0.0, "too small to judge");
    }

    #[test]
    fn head_is_a_chain_of_linear_layers() {
        // 2 -> 1: y = 2a - b + 0.5, then 1 -> 1: z = 3y + 1.
        let layers = vec![(vec![2.0, -1.0], vec![0.5]), (vec![3.0], vec![1.0])];
        assert_eq!(apply_head(&layers, &[1.0, 1.0]), 3.0 * 1.5 + 1.0);
    }

    #[test]
    fn clip_input_crops_the_centre() {
        // A wide image: left third red, rest blue. The centre crop is all blue.
        let img = RgbaImage::from_fn(600, 200, |x, _| {
            if x < 200 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) }
        });
        let t = clip_input(&image::DynamicImage::ImageRgba8(img).into()).unwrap();
        assert_eq!(t.shape, [1, 3, 224, 224]);
        let plane = 224 * 224;
        let red_at = |i: usize| t.data[i] * CLIP_STD[0] + CLIP_MEAN[0];
        assert!(red_at(112 * 224 + 112) < 0.01 && red_at(112 * 224 + 2) < 0.2, "centre and left edge are blue");
        assert!((t.data[2 * plane + 112 * 224 + 112] * CLIP_STD[2] + CLIP_MEAN[2] - 1.0).abs() < 0.01);
    }
}
