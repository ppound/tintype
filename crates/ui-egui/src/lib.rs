//! The egui desktop UI. Every action dispatches a `tintype_engine::Command` on
//! a background thread; the UI holds no library logic of its own.

mod app;
mod backend;
mod gather;
mod os;
mod thumbs;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_wgpu::{WgpuSetup, wgpu};
use tintype_engine::Engine;

pub use app::{ScreenshotPlan, TintypeApp};

/// Opens the library and runs the app until the window closes.
pub fn run(library: Option<PathBuf>) -> Result<(), String> {
    let library = library.or_else(tintype_engine::default_library_dir).ok_or("could not find a data folder")?;
    // Which Tintype has the library open: older versions don't say, so then any will do.
    let pid_file = library.join("tintype.pid");
    if tintype_engine::library_in_use(&library) && !wait_for_restart(&library) {
        let pid = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse().ok());
        if os::focus_other_instance(pid) {
            return Ok(());
        }
        return Err("Tintype already has this library open, in another window or in tintype-cli.              Switch to it, or close it first."
            .into());
    }
    let _ = std::fs::create_dir_all(&library);
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    let started = Instant::now();
    log_panics(&library);
    let screenshot = std::env::var_os("TINTYPE_SCREENSHOT").map(|path| ScreenshotPlan {
        path: path.into(),
        select: std::env::var("TINTYPE_SCREENSHOT_SELECT").ok().and_then(|v| v.parse().ok()),
        select_name: std::env::var("TINTYPE_SCREENSHOT_SELECT_NAME").ok(),
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
    // The window asks little of the GPU. On a laptop with two, the faster one
    // can be switched off during sleep, and the window can't survive losing it.
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.power_preference = wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    }
    if screenshot.is_some() {
        // Start from defaults and leave the user's window and panel sizes alone.
        options.persist_window = false;
        options.persistence_path = Some(std::env::temp_dir().join("tintype-screenshot-state"));
    }
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            "Tintype",
            options,
            Box::new(move |cc| {
                let engine = Engine::open(&library)?;
                Ok(Box::new(TintypeApp::new(cc, engine, screenshot)))
            }),
        )
    }));
    match ran {
        Ok(result) => result.map_err(|e| e.to_string()),
        // The graphics driver was reset, as when waking from sleep: start
        // again, where analysis carries on. Not if it happens right at launch.
        Err(_) if GPU_LOST.load(Ordering::Relaxed) && started.elapsed() > Duration::from_secs(30) => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env(RESTARTED, "1")
                .spawn()
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Set for the copy started after the GPU went away (see `run`).
const RESTARTED: &str = "TINTYPE_RESTARTED";

/// After a restart, waits (up to 30 s) for the old copy to let go of the
/// library. Returns whether it did.
fn wait_for_restart(library: &Path) -> bool {
    if std::env::var_os(RESTARTED).is_none() {
        return false;
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(200));
        if !tintype_engine::library_in_use(library) {
            return true;
        }
    }
    false
}

/// Set when the window's renderer panicked, which means the GPU went away.
static GPU_LOST: AtomicBool = AtomicBool::new(false);

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
        let gpu = info.location().is_some_and(|l| l.file().contains("wgpu"));
        if name == "main" && gpu {
            GPU_LOST.store(true, Ordering::Relaxed);
        } else if name == "main" {
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
