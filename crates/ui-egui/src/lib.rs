//! The egui desktop UI. Every action dispatches a `tintype_engine::Command` on
//! a background thread; the UI holds no library logic of its own.

mod app;
mod backend;
mod thumbs;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;
use tintype_engine::Engine;

pub use app::{ScreenshotPlan, TintypeApp};

/// Opens the library and runs the app until the window closes.
pub fn run(library: Option<PathBuf>) -> Result<(), String> {
    let library = library.or_else(tintype_engine::default_library_dir).ok_or("could not find a data folder")?;
    log_panics(&library);
    let screenshot = std::env::var_os("TINTYPE_SCREENSHOT").map(|path| ScreenshotPlan {
        path: path.into(),
        select: std::env::var("TINTYPE_SCREENSHOT_SELECT").ok().and_then(|v| v.parse().ok()),
        view: std::env::var("TINTYPE_SCREENSHOT_VIEW").ok(),
        smart_query: std::env::var("TINTYPE_SCREENSHOT_SMART").ok(),
        add_folder: std::env::var_os("TINTYPE_SCREENSHOT_ADD").map(PathBuf::from),
    });
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tintype")
            .with_icon(Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon/256x256.png"))
                    .expect("the bundled icon is a valid PNG"),
            ))
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    if screenshot.is_some() {
        // Start from defaults and leave the user's window and panel sizes alone.
        options.persist_window = false;
        options.persistence_path = Some(std::env::temp_dir().join("tintype-screenshot-state"));
    }
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

/// Crash reports go to `crash.log` in the library folder: a release build on
/// Windows has no console to print them to. A crash of the window itself also
/// says so in a dialog; panics in workers are caught, and logged here too.
fn log_panics(library: &Path) {
    let log = library.join("crash.log");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let entry = format!(
            "--- unix time {secs}, Tintype {}, thread '{name}': {info}
{}
",
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        );
        // Start over rather than grow without bound.
        let keep = std::fs::metadata(&log).is_ok_and(|m| m.len() < 1_000_000);
        let file = std::fs::OpenOptions::new().create(true).append(keep).write(true).truncate(!keep).open(&log);
        if let Ok(mut f) = file {
            let _ = f.write_all(entry.as_bytes());
        }
        if name == "main" {
            rfd::MessageDialog::new()
                .set_title("Tintype")
                .set_description(format!(
                    "Tintype ran into a bug and has to close. Nothing in your photos was changed.

{info}

Details are in {}",
                    log.display()
                ))
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
    }));
}
