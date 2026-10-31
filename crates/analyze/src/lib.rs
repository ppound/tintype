//! Image analysis that produces searchable metadata.
//!
//! SigLIP (B/16, 224 px, Apache-2.0) embeds photos and text into one 768-d
//! space, so a description can be compared with every photo. Embeddings are
//! L2-normalized: their dot product is the cosine similarity, and
//! [`match_probability`] turns that into SigLIP's calibrated probability that
//! the text describes the image.
//!
//! Planned: perceptual hashes (near-duplicates), zero-shot tags from the same
//! embeddings, an aesthetic score, and optional cloud taggers behind a trait.

use std::path::Path;

use image::imageops::FilterType;
use image::{Rgb, RgbImage, RgbaImage};
use tintype_codecs::Rgba;
use tintype_infer::{Model, Tensor};
use tokenizers::Tokenizer;

/// Bump when embeddings change (model, preprocessing); photos are then re-analyzed.
pub const ANALYSIS_VERSION: u32 = 1;
pub const EMBEDDING_DIM: usize = 768;
/// Long edge the photo is decoded at before it is resized to the model input.
pub const ANALYSIS_EDGE: u32 = 448;

const INPUT: u32 = 224;
const MAX_TOKENS: usize = 64;
/// SigLIP pads with `</s>`.
const PAD: i64 = 1;
/// Learned temperature (log) and bias of google/siglip-base-patch16-224.
const LOGIT_SCALE: f32 = 4.764_997;
const LOGIT_BIAS: f32 = -12.932_437;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] tintype_infer::Error),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("unexpected model output: {0}")]
    Output(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Embeds photos. Thread-safe; share one per process.
pub struct ImageEncoder {
    model: Model,
}

impl ImageEncoder {
    pub fn load(vision: &Path) -> Result<Self> {
        Ok(Self { model: Model::load(vision, &[1, 3, INPUT as usize, INPUT as usize])? })
    }

    /// The embedding of an upright image.
    pub fn embed(&self, img: &Rgba) -> Result<Vec<f32>> {
        pooled(self.model.run(preprocess(img)?)?)
    }
}

/// Embeds descriptions, for comparing with [`ImageEncoder`] embeddings.
pub struct TextEncoder {
    model: Model,
    tokenizer: Tokenizer,
}

impl TextEncoder {
    pub fn load(text: &Path, tokenizer: &Path) -> Result<Self> {
        Ok(Self {
            model: Model::load_tokens(text, &[1, MAX_TOKENS])?,
            tokenizer: Tokenizer::from_file(tokenizer).map_err(|e| Error::Tokenizer(e.to_string()))?,
        })
    }

    /// The embedding of a search like "dog on a beach".
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.embed(&format!("a photo of {}.", query.trim().trim_end_matches('.')))
    }

    /// The embedding of `text` as given (lowercased, as SigLIP was trained).
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.tokens(text)?;
        pooled(self.model.run_tokens(&[1, MAX_TOKENS], ids)?)
    }

    fn tokens(&self, text: &str) -> Result<Vec<i64>> {
        let enc = self.tokenizer.encode(text.to_lowercase(), true).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(pad_tokens(enc.get_ids()))
    }
}

/// Cosine similarity of two normalized embeddings.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// SigLIP's probability that a text matches an image, from their similarity.
pub fn match_probability(similarity: f32) -> f32 {
    1.0 / (1.0 + (-(similarity * LOGIT_SCALE.exp() + LOGIT_BIAS)).exp())
}

/// The similarity at which [`match_probability`] reaches `p`.
pub fn similarity_for_probability(p: f32) -> f32 {
    ((p / (1.0 - p)).ln() - LOGIT_BIAS) / LOGIT_SCALE.exp()
}

/// Truncates (keeping the final `</s>`) or pads to the model's fixed length.
fn pad_tokens(ids: &[u32]) -> Vec<i64> {
    let mut out: Vec<i64> = ids.iter().map(|&i| i as i64).collect();
    if out.len() > MAX_TOKENS {
        let last = *out.last().expect("not empty");
        out.truncate(MAX_TOKENS - 1);
        out.push(last);
    }
    out.resize(MAX_TOKENS, PAD);
    out
}

/// Squashes to 224x224 (as SigLIP's processor does, no crop) and scales to [-1, 1], NCHW.
fn preprocess(img: &Rgba) -> Result<Tensor> {
    let rgba = RgbaImage::from_raw(img.width, img.height, img.pixels.clone())
        .ok_or_else(|| Error::Output("image buffer size mismatch".into()))?;
    let rgb = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
    let small: RgbImage = image::imageops::resize(&rgb, INPUT, INPUT, FilterType::CatmullRom);
    let plane = (INPUT * INPUT) as usize;
    let mut data = vec![0f32; 3 * plane];
    for (x, y, Rgb(px)) in small.enumerate_pixels() {
        let i = (y * INPUT + x) as usize;
        for c in 0..3 {
            data[c * plane + i] = px[c] as f32 / 127.5 - 1.0;
        }
    }
    Ok(Tensor { shape: vec![1, 3, INPUT as usize, INPUT as usize], data })
}

/// Picks the pooled `[1, 768]` output and normalizes it.
fn pooled(outputs: Vec<(String, Tensor)>) -> Result<Vec<f32>> {
    let mut emb = outputs
        .into_iter()
        .find(|(_, t)| t.shape == [1, EMBEDDING_DIM])
        .map(|(_, t)| t.data)
        .ok_or_else(|| Error::Output(format!("no [1, {EMBEDDING_DIM}] embedding")))?;
    let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        emb.iter_mut().for_each(|x| *x /= norm);
    }
    Ok(emb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_pad_and_truncate() {
        assert_eq!(pad_tokens(&[262, 266, 1])[..4], [262, 266, 1, PAD]);
        assert_eq!(pad_tokens(&[262, 266, 1]).len(), MAX_TOKENS);
        let long: Vec<u32> = (10..110).chain([1]).collect();
        let t = pad_tokens(&long);
        assert_eq!((t.len(), t[MAX_TOKENS - 2], t[MAX_TOKENS - 1]), (MAX_TOKENS, 72, 1));
    }

    #[test]
    fn preprocess_scales_to_unit_range() {
        let img = Rgba { width: 3, height: 2, pixels: [[0, 255, 128, 255]; 6].concat() };
        let t = preprocess(&img).unwrap();
        assert_eq!(t.shape, [1, 3, 224, 224]);
        let plane = 224 * 224;
        assert_eq!((t.data[0], t.data[plane]), (-1.0, 1.0));
        assert!(t.data[2 * plane].abs() < 0.01);
    }

    #[test]
    fn probability_round_trips() {
        for p in [0.001, 0.1, 0.5, 0.9] {
            assert!((match_probability(similarity_for_probability(p)) - p).abs() < 1e-4);
        }
        assert!(match_probability(0.0) < 1e-5, "unrelated text and image");
    }

    #[test]
    fn picks_pooled_output() {
        let hidden = Tensor { shape: vec![1, 196, EMBEDDING_DIM], data: vec![0.0; 196 * EMBEDDING_DIM] };
        let mut v = vec![0.0; EMBEDDING_DIM];
        v[0] = 3.0;
        v[1] = 4.0;
        let pooled_out = Tensor { shape: vec![1, EMBEDDING_DIM], data: v };
        let emb = pooled(vec![("h".into(), hidden), ("p".into(), pooled_out)]).unwrap();
        assert_eq!((emb[0], emb[1]), (0.6, 0.8));
    }
}
