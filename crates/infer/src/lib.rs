//! Local inference behind one trait, so backends can be swapped per model.
//!
//! Planned: an `ort` (ONNX Runtime) backend with DirectML / CoreML / CUDA
//! execution providers and CPU fallback; candle for transformer models where it
//! fits better. Models are never bundled: a registry lists name, URL, size,
//! license and SHA-256, and weights download on first use.
