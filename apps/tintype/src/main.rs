//! The Tintype desktop app. `tintype [--library DIR]`

// No console window behind the app on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut library = std::env::var_os("TINTYPE_LIBRARY").map(PathBuf::from);
    while let Some(arg) = args.next() {
        if arg == "--library" {
            library = args.next().map(PathBuf::from);
        }
    }
    if let Err(e) = tintype_ui_egui::run(library) {
        rfd::MessageDialog::new()
            .set_title("Tintype")
            .set_description(format!("Tintype could not start:\n\n{e}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}
