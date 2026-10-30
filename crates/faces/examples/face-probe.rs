//! Run face analysis on photos, save annotated copies and crops, print the
//! pairwise similarity of every face found.
//! `cargo run --release -p tintype-faces --example face-probe -- MODELS_DIR OUT_DIR photo...`

use std::path::Path;
use std::time::Instant;

use tintype_faces::{FaceAnalyzer, similarity};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (models, out) = (Path::new(&args[0]), Path::new(&args[1]));
    std::fs::create_dir_all(out).unwrap();
    let analyzer =
        FaceAnalyzer::load(&models.join(tintype_infer::YUNET.file), &models.join(tintype_infer::SFACE.file)).unwrap();

    let mut all = Vec::new();
    for (pi, photo) in args[2..].iter().enumerate() {
        let t = Instant::now();
        let img = tintype_codecs::decode_preview(Path::new(photo), tintype_faces::ANALYSIS_EDGE).unwrap();
        let decoded = t.elapsed();
        let t = Instant::now();
        let faces = analyzer.analyze(&img).unwrap();
        println!("photo {pi}: {} faces, decode {decoded:?}, analyze {:?}  {photo}", faces.len(), t.elapsed());

        let mut canvas = image::RgbaImage::from_raw(img.width, img.height, img.pixels.clone()).unwrap();
        for (fi, f) in faces.iter().enumerate() {
            let [x, y, w, h] = f.detection.bbox;
            for i in 0..=(w as i32) {
                for (px, py) in [(x as i32 + i, y as i32), (x as i32 + i, (y + h) as i32)] {
                    if let Some(p) = canvas.get_pixel_mut_checked(px as u32, py as u32) {
                        *p = image::Rgba([0, 255, 0, 255]);
                    }
                }
            }
            for i in 0..=(h as i32) {
                for (px, py) in [(x as i32, y as i32 + i), ((x + w) as i32, y as i32 + i)] {
                    if let Some(p) = canvas.get_pixel_mut_checked(px as u32, py as u32) {
                        *p = image::Rgba([0, 255, 0, 255]);
                    }
                }
            }
            for [lx, ly] in f.detection.landmarks {
                for d in -2..=2 {
                    for e in -2..=2 {
                        if let Some(p) = canvas.get_pixel_mut_checked((lx as i32 + d) as u32, (ly as i32 + e) as u32) {
                            *p = image::Rgba([255, 0, 0, 255]);
                        }
                    }
                }
            }
            let crop = image::RgbaImage::from_raw(f.crop.width, f.crop.height, f.crop.pixels.clone()).unwrap();
            crop.save(out.join(format!("p{pi}f{fi}.png"))).unwrap();
            println!("  face p{pi}f{fi} score {:.2} box {:.0}x{:.0}", f.detection.score, w, h);
            all.push((format!("p{pi}f{fi}"), f.embedding.clone()));
        }
        image::DynamicImage::ImageRgba8(canvas).thumbnail(1000, 1000).save(out.join(format!("p{pi}.jpg"))).unwrap();
    }
    println!("\nsimilarity:");
    for (i, (a, ea)) in all.iter().enumerate() {
        let row: Vec<String> = all[..i].iter().map(|(_, eb)| format!("{:5.2}", similarity(ea, eb))).collect();
        println!("{a:>7} {}", row.join(" "));
    }
}
