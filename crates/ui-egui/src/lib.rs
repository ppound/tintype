//! The egui desktop UI. Every action dispatches a `tintype_engine::Command` on
//! a background thread; the UI holds no library logic of its own.

mod app;
mod backend;
mod thumbs;

use std::path::PathBuf;

use eframe::egui;
use tintype_engine::Engine;

pub use app::{ScreenshotPlan, TintypeApp};

/// Opens the library and runs the app until the window closes.
pub fn run(library: Option<PathBuf>) -> Result<(), String> {
    let library = library.or_else(tintype_engine::default_library_dir).ok_or("could not find a data folder")?;
    let screenshot = std::env::var_os("TINTYPE_SCREENSHOT").map(|path| ScreenshotPlan {
        path: path.into(),
        select: std::env::var("TINTYPE_SCREENSHOT_SELECT").ok().and_then(|v| v.parse().ok()),
        view: std::env::var("TINTYPE_SCREENSHOT_VIEW").ok(),
        smart_query: std::env::var("TINTYPE_SCREENSHOT_SMART").ok(),
    });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tintype")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tintype",
        options,
        Box::new(move |cc| {
            let engine = Engine::open(&library)?;
            Ok(Box::new(TintypeApp::new(cc, engine, screenshot)))
        }),
    )
    .map_err(|e| e.to_string())
}
