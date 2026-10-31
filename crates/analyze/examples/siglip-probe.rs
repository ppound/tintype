//! Embed photos and texts with SigLIP and print every match probability.
//! `cargo run --release -p tintype-analyze --example siglip-probe -- MODELS_DIR photo... -- "text"...`

use std::path::Path;
use std::time::Instant;

use tintype_analyze::{ANALYSIS_EDGE, ImageEncoder, TextEncoder, match_probability, similarity};
use tintype_infer::{SIGLIP_TEXT, SIGLIP_TOKENIZER, SIGLIP_VISION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let models = Path::new(&args[0]);
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (photos, texts) = (&args[1..split], args.get(split + 1..).unwrap_or_default());

    let images = ImageEncoder::load(&models.join(SIGLIP_VISION.file)).unwrap();
    let text = TextEncoder::load(&models.join(SIGLIP_TEXT.file), &models.join(SIGLIP_TOKENIZER.file)).unwrap();

    let mut image_embs = Vec::new();
    for p in photos {
        let t = Instant::now();
        let img = tintype_codecs::decode_preview(Path::new(p), ANALYSIS_EDGE).unwrap();
        let emb = images.embed(&img).unwrap();
        println!("{:?} {:?} {p}", t.elapsed(), &emb[..4]);
        image_embs.push(emb);
    }
    for s in texts {
        let emb = text.embed_query(s).unwrap();
        let row: Vec<String> = image_embs
            .iter()
            .map(|e| format!("{:.3}/{:.4}", similarity(e, &emb), match_probability(similarity(e, &emb))))
            .collect();
        println!("{s:>24}  {:?}  {}", &emb[..3], row.join("  "));
    }
}
