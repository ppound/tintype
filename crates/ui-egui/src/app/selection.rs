//! Picking several photos (Ctrl+click, Shift+click, Ctrl+A), rating and
//! tagging them together, and showing them in the file manager, copied
//! together into one folder when they're spread over several.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use eframe::egui::{self, Key, Modifiers, RichText, TextEdit};
use tintype_engine::{Command, Hit, PhotoRef, Selection};

use super::{COMMAND, TintypeApp, star_row, tag_chip, thousands};
use crate::backend::Job;
use crate::gather::{self, Progress};
use crate::os;

/// (text, is an error) notices sent from helper threads.
pub(super) type HelperNotices =
    (crossbeam_channel::Sender<(String, bool)>, crossbeam_channel::Receiver<(String, bool)>);

/// Copies being made in the background for "Show in folder".
pub(super) struct Gathering {
    total: usize,
    progress: Arc<Progress>,
}

impl TintypeApp {
    /// The photos picked, in grid order: the Ctrl/Shift+clicked ones, or else the selected one.
    pub(super) fn picked_hits(&self) -> Vec<&Hit> {
        if self.picked.is_empty() {
            return self.selected.and_then(|i| self.results.get(i)).into_iter().collect();
        }
        self.results.iter().filter(|h| self.picked.contains(&h.id)).collect()
    }

    pub(super) fn is_picked(&self, i: usize) -> bool {
        if self.picked.is_empty() { self.selected == Some(i) } else { self.picked.contains(&self.results[i].id) }
    }

    /// A click on grid cell `i`: Ctrl (Cmd) adds or removes it, Shift picks
    /// the range from the last click, a plain click picks it alone.
    pub(super) fn click_cell(&mut self, i: usize, modifiers: Modifiers) {
        let id = self.results[i].id;
        if modifiers.shift {
            let from = self.anchor.or(self.selected).unwrap_or(i).min(self.results.len() - 1);
            if !modifiers.command {
                self.picked.clear();
            }
            self.picked.extend(self.results[from.min(i)..=from.max(i)].iter().map(|h| h.id));
            self.select(i);
        } else if modifiers.command {
            if self.picked.is_empty()
                && let Some(s) = self.selected
            {
                self.picked.insert(self.results[s].id);
            }
            if self.picked.insert(id) {
                self.select(i);
            } else {
                self.picked.remove(&id);
                // The details move to another picked photo.
                if self.selected == Some(i)
                    && let Some(j) = self.results.iter().rposition(|h| self.picked.contains(&h.id))
                {
                    self.select(j);
                }
            }
            self.anchor = Some(i);
        } else {
            self.picked.clear();
            self.select(i);
            self.anchor = Some(i);
        }
        if self.picked.len() == 1 {
            self.picked.clear();
        }
    }

    pub(super) fn pick_all(&mut self) {
        self.picked = self.results.iter().map(|h| h.id).collect();
        if self.selected.is_none() && !self.results.is_empty() {
            self.select(0);
        }
        if self.picked.len() == 1 {
            self.picked.clear();
        }
    }

    pub(super) fn clear_picked(&mut self) {
        self.picked.clear();
        self.anchor = self.selected;
        self.picked_tag_input.clear();
        self.picked_summary = None;
        self.summary_for.clear();
    }

    /// After new results: keep the picked photos that are still shown.
    pub(super) fn keep_picked(&mut self) {
        let shown: HashSet<i64> = self.results.iter().map(|h| h.id).collect();
        self.picked.retain(|id| shown.contains(id));
        if self.picked.len() == 1 {
            self.picked.clear();
        }
        self.anchor = self.selected;
    }

    /// The top of the details panel while several photos are picked.
    pub(super) fn picked_bar(&mut self, ui: &mut egui::Ui) {
        if self.picked.is_empty() {
            return;
        }
        let paths = self.picked_paths();
        let folders = folders(&paths);
        ui.add_space(8.0);
        ui.label(RichText::new(format!("{} photos selected", thousands(paths.len() as u64))).heading());
        let hint = if folders > 1 {
            format!(
                "They're in {folders} folders, so Tintype copies them into one first, \
                 to drag them all at once. Your photos aren't changed."
            )
        } else {
            "Open their folder with them selected, to drag them to a browser or anywhere else".into()
        };
        ui.horizontal(|ui| {
            let busy = self.gathering.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Show in folder")).on_hover_text(hint).clicked() {
                self.show_in_folder(ui.ctx(), paths);
            }
            if ui.button("Clear").on_hover_text("Select just one photo again (Esc)").clicked() {
                self.clear_picked();
            }
        });
        if self.summary_for != self.picked {
            self.summary_for = self.picked.clone();
            let photos = self.picked_ids();
            self.summary_seq = self.backend.send(Job::Selection, Command::Selection { photos });
            self.summary_edit = false;
        }
        if let Some(sel) = self.picked_summary.clone() {
            self.picked_edits(ui, &sel);
        }
        ui.weak(format!(
            "{COMMAND}+click adds or removes a photo, Shift+click a range, {COMMAND}+A selects all. \
             Below: the last photo clicked."
        ));
        ui.separator();
    }

    /// Rating and tags for all the picked photos at once.
    fn picked_edits(&mut self, ui: &mut egui::Ui, sel: &Selection) {
        let n = thousands(sel.photos);
        ui.add_space(4.0);
        let common = sel.common_rating();
        let rating = star_row(ui, common.flatten(), |r| format!("Rate all {n} photos {r} (key {r})"));
        if common.is_none() {
            let unrated = sel.ratings[0];
            ui.weak(if unrated == 0 {
                "Their ratings differ.".to_string()
            } else {
                format!("Their ratings differ; {} unrated.", thousands(unrated))
            });
        }
        let mut add = None;
        let mut remove = None;
        if !sel.tags.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (tag, count) in &sel.tags {
                    let all = *count == sel.photos;
                    let label = if all { tag.clone() } else { format!("{tag} ({})", thousands(*count)) };
                    let (name, close) = tag_chip(ui, &label);
                    let hint = if all {
                        format!("On all {n} photos")
                    } else {
                        format!("On {} of {n} photos. Click to add it to the rest.", thousands(*count))
                    };
                    if name.on_hover_text(hint).clicked() && !all {
                        add = Some(tag.clone());
                    }
                    if close.on_hover_text(format!("Remove from all {n} photos")).clicked() {
                        remove = Some(tag.clone());
                    }
                }
            });
        }
        let edit = ui.add(
            TextEdit::singleline(&mut self.picked_tag_input)
                .hint_text(format!("Add a tag to all {n}…"))
                .desired_width(f32::INFINITY),
        );
        if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.picked_tag_input.trim().is_empty() {
            add = Some(std::mem::take(&mut self.picked_tag_input));
            edit.request_focus();
        }
        if let Some(r) = rating {
            self.rate_picked(r);
        }
        if add.is_some() || remove.is_some() {
            let photos = self.picked_ids();
            let cmd = Command::TagMany { photos, add: add.into_iter().collect(), remove: remove.into_iter().collect() };
            self.summary_seq = self.backend.send(Job::Selection, cmd);
            self.summary_edit = true;
        }
    }

    pub(super) fn rate_picked(&mut self, rating: Option<u8>) {
        let photos = self.picked_ids();
        let n = thousands(photos.len() as u64);
        self.notify(
            match rating {
                Some(r) => format!("Rated {n} photos {r} {}", if r == 1 { "star" } else { "stars" }),
                None => format!("Cleared the rating of {n} photos"),
            },
            false,
        );
        self.summary_seq = self.backend.send(Job::Selection, Command::RateMany { photos, rating });
        self.summary_edit = true;
    }

    /// The picked photos' ratings and tags, fresh or just edited.
    pub(super) fn on_picked_summary(&mut self, sel: Selection) {
        self.picked_summary = Some(sel);
        if std::mem::take(&mut self.summary_edit) {
            self.request_facet("tags");
            // The details below show one of the picked photos.
            if let Some(id) = self.selected.filter(|&i| self.is_picked(i)).map(|i| self.results[i].id) {
                self.detail_seq = self.backend.send(Job::Show, Command::Show { photo: PhotoRef::Id(id) });
            }
        }
    }

    fn picked_ids(&self) -> Vec<i64> {
        self.picked_hits().iter().map(|h| h.id).collect()
    }

    /// The files to show for the picked photos (the cover of a stack: the JPG of a RAF+JPG pair).
    pub(super) fn picked_paths(&self) -> Vec<PathBuf> {
        self.picked_hits().iter().map(|h| PathBuf::from(&h.path)).collect()
    }

    /// Opens the file manager with `paths` selected. Photos from several
    /// folders are first copied together into one.
    pub(super) fn show_in_folder(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        let Some(first) = paths.first() else { return };
        let tx = self.helper_notices.0.clone();
        let ctx = ctx.clone();
        if folders(&paths) == 1 {
            let folder = first.parent().map(Path::to_path_buf).unwrap_or_default();
            std::thread::spawn(move || {
                if let Err(e) = os::reveal(&folder, &paths) {
                    let _ = tx.send((e, true));
                    ctx.request_repaint();
                }
            });
            return;
        }
        if self.gathering.is_some() {
            return;
        }
        let progress = Arc::new(Progress::default());
        self.gathering = Some(Gathering { total: paths.len(), progress: progress.clone() });
        let folder_count = folders(&paths);
        std::thread::spawn(move || {
            let dest = gather::folder();
            let g = gather::gather(&paths, &dest, &progress);
            let copied = g.copied.len();
            let notice = match g.error {
                Some(e) => (e, true),
                None if copied < paths.len() => {
                    (format!("Stopped after copying {copied} of {} photos.", paths.len()), false)
                }
                None => (
                    format!(
                        "Copied {} photos from {folder_count} folders into one. Drag them to your browser to upload.",
                        thousands(copied as u64)
                    ),
                    false,
                ),
            };
            if copied > 0
                && let Err(e) = os::reveal(&dest, &g.copied)
            {
                let _ = tx.send((e, true));
            } else {
                let _ = tx.send(notice);
            }
            progress.finished.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    /// Notices from helper threads, and the end of copying.
    pub(super) fn poll_helpers(&mut self) {
        let notices: Vec<(String, bool)> = self.helper_notices.1.try_iter().collect();
        for (text, error) in notices {
            self.notify(text, error);
        }
        if self.gathering.as_ref().is_some_and(|g| g.progress.finished.load(Ordering::Relaxed)) {
            self.gathering = None;
        }
    }

    /// Copying progress, in the status bar.
    pub(super) fn gathering_status(&mut self, ui: &mut egui::Ui) {
        let Some(g) = &self.gathering else { return };
        let done = g.progress.done.load(Ordering::Relaxed);
        ui.spinner();
        ui.label(format!(
            "Copying photos into one folder: {} of {}",
            thousands(done as u64),
            thousands(g.total as u64)
        ));
        ui.add(egui::ProgressBar::new(done as f32 / g.total.max(1) as f32).desired_width(160.0));
        if ui.small_button("Cancel").clicked() {
            g.progress.cancel.store(true, Ordering::Relaxed);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
}

/// How many folders `paths` are in.
fn folders(paths: &[PathBuf]) -> usize {
    paths.iter().map(|p| p.parent()).collect::<BTreeSet<_>>().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_folders() {
        let paths: Vec<PathBuf> = ["a/1.jpg", "a/2.jpg", "b/1.jpg"].iter().map(PathBuf::from).collect();
        assert_eq!(folders(&paths), 2);
        assert_eq!(folders(&paths[..2]), 1);
    }
}
