//! Score photos with the LAION aesthetic predictor and measure sharpness.
//! `cargo run --release -p tintype-analyze --example aesthetic-probe -- CLIP_L14.onnx HEAD.pth photo...`

use std::path::Path;
use std::time::Instant;

use tintype_analyze::{AestheticScorer, QUALITY_EDGE, sharpness};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scorer = AestheticScorer::load(Path::new(&args[0]), Path::new(&args[1])).unwrap();
    for photo in &args[2..] {
        let t = Instant::now();
        let img = tintype_codecs::decode_preview(Path::new(photo), QUALITY_EDGE).unwrap();
        let (score, sharp) = (scorer.score(&img).unwrap(), sharpness(&img).unwrap());
        println!("{score:.3}  sharpness {sharp:8.1}  {:?}  {photo}", t.elapsed());
    }
}
