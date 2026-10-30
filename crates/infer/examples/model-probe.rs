//! Load a model at a fixed input shape and time one run on zeros.
//! `cargo run --release -p tintype-infer --example model-probe -- model.onnx 1,3,640,640`

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let shape: Vec<usize> = args[2].split(',').map(|d| d.parse().unwrap()).collect();
    let t = Instant::now();
    let model = tintype_infer::Model::load(args[1].as_ref(), &shape).unwrap();
    println!("load+optimize {:?}", t.elapsed());
    let input = tintype_infer::Tensor { data: vec![0.0; shape.iter().product()], shape };
    for _ in 0..3 {
        let t = Instant::now();
        let out = model.run(input.clone()).unwrap();
        println!("run {:?}", t.elapsed());
        if args.len() > 3 {
            for (name, t) in &out {
                println!("  {name} {:?}", t.shape);
            }
        }
    }
}
