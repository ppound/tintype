//! Local model inference and model management.
//!
//! Models are never bundled. Each is described by a [`ModelSpec`] (where to
//! get it, its size, license and SHA-256); [`ModelStore::ensure`] downloads it
//! on first use and refuses anything whose hash doesn't match.
//!
//! Inference runs on `tract`, a pure-Rust ONNX runtime, so models need no
//! native libraries. That suits the small face models; an ONNX Runtime backend
//! for large models and GPUs can sit beside it later.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("downloading {model}: {message}")]
    Download { model: &'static str, message: String },
    #[error("{model} download is corrupt (SHA-256 mismatch); try again")]
    Checksum { model: &'static str },
    #[error("model error: {0}")]
    Model(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A downloadable model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
    pub license: &'static str,
}

/// YuNet face detector (OpenCV model zoo, 2023-03).
pub const YUNET: ModelSpec = ModelSpec {
    name: "YuNet face detector",
    file: "face_detection_yunet_2023mar.onnx",
    url: "https://huggingface.co/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx",
    sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
    bytes: 232_589,
    license: "MIT",
};

/// SFace face recognizer (OpenCV model zoo, 2021-12): 128-d embeddings.
pub const SFACE: ModelSpec = ModelSpec {
    name: "SFace face recognizer",
    file: "face_recognition_sface_2021dec.onnx",
    url: "https://huggingface.co/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx",
    sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
    bytes: 38_696_353,
    license: "Apache-2.0",
};

/// Where downloaded models live.
#[derive(Debug, Clone)]
pub struct ModelStore {
    dir: PathBuf,
}

impl ModelStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(spec.file)
    }

    /// Present with the expected size (the hash was checked when it was downloaded).
    pub fn is_installed(&self, spec: &ModelSpec) -> bool {
        std::fs::metadata(self.path(spec)).is_ok_and(|m| m.len() == spec.bytes)
    }

    /// Returns the model's path, downloading and verifying it first if needed.
    /// `progress` receives (bytes so far, total bytes).
    pub fn ensure(&self, spec: &ModelSpec, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf> {
        let path = self.path(spec);
        if self.is_installed(spec) {
            return Ok(path);
        }
        std::fs::create_dir_all(&self.dir)?;
        let fail = |message: String| Error::Download { model: spec.name, message };
        let mut response = ureq::get(spec.url).call().map_err(|e| fail(e.to_string()))?;
        let mut body = response.body_mut().with_config().limit(spec.bytes + 1).reader();

        let part = path.with_extension("part");
        let mut out = File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let n = body.read(&mut buf).map_err(|e| fail(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, spec.bytes);
        }
        out.sync_all()?;
        drop(out);

        if format!("{:x}", hasher.finalize()) != spec.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Checksum { model: spec.name });
        }
        std::fs::rename(&part, &path)?;
        Ok(path)
    }
}

/// SHA-256 of a file, hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut BufReader::new(File::open(path)?), &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// A dense f32 tensor, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// An ONNX model compiled for one fixed input shape. Cheap to share between
/// threads (`run` takes `&self`).
pub struct Model {
    plan: std::sync::Arc<TypedSimplePlan>,
    output_names: Vec<String>,
}

impl Model {
    /// Loads and optimizes `path` for a single f32 input of `input_shape`.
    pub fn load(path: &Path, input_shape: &[usize]) -> Result<Self> {
        let err = |e: TractError| Error::Model(format!("{}: {e:#}", path.display()));
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(err)?
            .with_input_fact(0, f32::fact(input_shape).into())
            .map_err(err)?;
        let output_names =
            model.output_outlets().map_err(err)?.iter().map(|o| model.node(o.node).name.clone()).collect();
        let plan = model.into_optimized().map_err(err)?.into_runnable().map_err(err)?;
        Ok(Self { plan, output_names })
    }

    pub fn output_names(&self) -> &[String] {
        &self.output_names
    }

    /// Runs the model; outputs come back in model order with their names.
    pub fn run(&self, input: Tensor) -> Result<Vec<(String, Tensor)>> {
        let err = |e: TractError| Error::Model(format!("{e:#}"));
        let input =
            tract_ndarray::ArrayD::from_shape_vec(input.shape, input.data).map_err(|e| Error::Model(e.to_string()))?;
        let outputs = self.plan.run(tvec!(tract_onnx::prelude::Tensor::from(input).into())).map_err(err)?;
        outputs
            .iter()
            .zip(&self.output_names)
            .map(|(t, name)| {
                let view = t.to_plain_array_view::<f32>().map_err(err)?;
                Ok((name.clone(), Tensor { shape: view.shape().to_vec(), data: view.iter().copied().collect() }))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path());
        assert!(!store.is_installed(&YUNET));
        std::fs::write(store.path(&YUNET), b"short").unwrap();
        assert!(!store.is_installed(&YUNET), "wrong size");
    }

    #[test]
    fn sha256_of_known_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
