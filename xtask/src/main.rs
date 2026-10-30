//! `cargo xtask <task>`
//!
//! - `layers`: fail if a crate depends on a crate in the same or a higher layer.
//! - `ci`: fmt check, clippy, tests, layers.

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Lower layers may not depend on higher ones, nor on peers in their own layer.
const LAYERS: &[&[&str]] = &[
    &["tintype-meta", "tintype-codecs", "tintype-infer", "tintype-testkit"],
    &["tintype-catalog"],
    &["tintype-index", "tintype-analyze", "tintype-faces"],
    &["tintype-jobs"],
    &["tintype-engine"],
    &["tintype-ui-egui"],
    &["tintype", "tintype-cli"],
];

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("layers") => layers(),
        Some("ci") => ci(),
        _ => bail!("usage: cargo xtask <layers|ci>"),
    }
}

fn layers() -> Result<()> {
    let rank: HashMap<&str, usize> =
        LAYERS.iter().enumerate().flat_map(|(i, names)| names.iter().map(move |n| (*n, i))).collect();

    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: Value = serde_json::from_slice(&out.stdout)?;

    let mut errors = Vec::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        let name = pkg["name"].as_str().unwrap_or_default();
        if name == "xtask" {
            continue;
        }
        let Some(&own) = rank.get(name) else {
            errors.push(format!("{name} is not assigned a layer in xtask/src/main.rs"));
            continue;
        };
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            // Dev-dependencies (test fixtures) are exempt.
            if dep["kind"].as_str() == Some("dev") {
                continue;
            }
            let dep_name = dep["name"].as_str().unwrap_or_default();
            if let Some(&theirs) = rank.get(dep_name)
                && theirs >= own
            {
                errors.push(format!("{name} (layer {own}) depends on {dep_name} (layer {theirs})"));
            }
        }
    }

    if errors.is_empty() {
        println!("layers ok");
        Ok(())
    } else {
        for e in &errors {
            eprintln!("layer violation: {e}");
        }
        bail!("{} layer violation(s)", errors.len())
    }
}

fn ci() -> Result<()> {
    run(&["fmt", "--all", "--check"])?;
    run(&["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])?;
    run(&["test", "--workspace"])?;
    layers()
}

fn run(args: &[&str]) -> Result<()> {
    println!("$ cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO")).args(args).status()?;
    if !status.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}
