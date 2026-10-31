//! Smart search: the toggle's banner and the batch-by-batch analysis loop
//! that embeds photos so they can be found by what they show.

use eframe::egui::{self, Button, RichText};
use tintype_engine::{Command, Event, Response};

use super::{ScanState, TintypeApp, thousands};
use crate::backend::Job;

/// Photos per analysis batch; other commands run between batches.
const ANALYZE_BATCH: usize = 200;
/// Measured on a 14-core laptop: 4,253 photos (JPG and RAF) took 12.7 minutes,
/// ~0.18 s each, with a compile competing for the cores.
const SECONDS_PER_PHOTO: f64 = 0.15;

#[derive(Default)]
pub(super) struct SmartState {
    analyzing: Option<AnalyzeProgress>,
}

struct AnalyzeProgress {
    total: u64,
    done: u64,
    stop: bool,
}

impl SmartState {
    pub fn stop(&mut self) {
        if let Some(a) = &mut self.analyzing {
            a.stop = true;
        }
    }
}

impl TintypeApp {
    fn start_analyze(&mut self) {
        if self.smart.analyzing.is_some() || self.scan.is_some() {
            return;
        }
        let total = self.stats.as_ref().map_or(1, |s| s.unanalyzed.max(1));
        self.smart.analyzing = Some(AnalyzeProgress { total, done: 0, stop: false });
        self.scan = Some(ScanState { label: "Preparing smart search…".into(), fraction: None });
        self.backend.send(Job::Analyze, Command::Analyze { batch: ANALYZE_BATCH });
    }

    /// Returns false if the event isn't about analysis.
    pub(super) fn on_analyze_progress(&mut self, event: Event) -> bool {
        let (Event::Analyze { done, .. }, Some(scan), Some(a)) = (event, &mut self.scan, &self.smart.analyzing) else {
            return false;
        };
        let done = a.done + done;
        scan.label = format!("Analyzing photos for smart search: {} of {}", thousands(done), thousands(a.total));
        scan.fraction = Some(done as f32 / a.total as f32);
        true
    }

    /// Returns false if the reply isn't for analysis.
    pub(super) fn on_analyze_reply(&mut self, job: Job, result: &Result<Response, String>) -> bool {
        match (job, result) {
            (Job::Analyze, Ok(Response::Analyze(r))) => {
                let Some(a) = &mut self.smart.analyzing else { return true };
                a.done += r.processed;
                if r.cancelled || a.stop || r.remaining == 0 || r.processed == 0 {
                    let done = a.done;
                    self.smart.analyzing = None;
                    self.scan = None;
                    let msg = format!("Analyzed {} photos for smart search", thousands(done));
                    self.notify(if r.remaining > 0 { format!("{msg}. Paused.") } else { msg }, false);
                    self.refresh_stats();
                } else {
                    self.backend.send(Job::Analyze, Command::Analyze { batch: ANALYZE_BATCH });
                }
                // Results fill in as photos are analyzed.
                if self.settings.smart {
                    self.search();
                }
            }
            (Job::Analyze, Err(e)) => {
                self.smart.analyzing = None;
                self.scan = None;
                self.notify(format!("Analysis stopped: {e}"), true);
                self.refresh_stats();
            }
            _ => return false,
        }
        true
    }

    /// Under the search bar while smart search is on and photos still need analysis.
    pub(super) fn smart_banner(&mut self, ui: &mut egui::Ui) {
        let Some(stats) = &self.stats else { return };
        if !self.settings.smart || stats.unanalyzed == 0 || self.smart.analyzing.is_some() {
            return;
        }
        let (unanalyzed, all) = (stats.unanalyzed, stats.unanalyzed == stats.photos);
        let download = (!stats.smart_ready).then_some(stats.smart_download_bytes);
        let mut start = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if all {
                ui.label(RichText::new("Smart search finds photos by what they show").strong());
                ui.label(format!(
                    "Tintype looks at each photo once first: {} photos, {} on this computer. \
                     Nothing is uploaded.",
                    thousands(unanalyzed),
                    duration(unanalyzed as f64 * SECONDS_PER_PHOTO)
                ));
            } else {
                ui.label(format!(
                    "{} photos haven't been analyzed yet, so smart search can't find them.",
                    thousands(unanalyzed)
                ));
            }
            if let Some(bytes) = download {
                ui.weak(format!(
                    "The first time, the open SigLIP model ({} MB, Apache-2.0) is downloaded.",
                    bytes / 1_000_000
                ));
            }
            start = ui.add_enabled(self.scan.is_none(), Button::new("Analyze photos")).clicked();
        });
        if start {
            self.start_analyze();
        }
    }
}

/// "under a minute", "about 3 minutes", "about 1.5 hours".
fn duration(secs: f64) -> String {
    match secs {
        s if s < 60.0 => "under a minute".into(),
        s if s < 90.0 * 60.0 => format!("about {:.0} minutes", (s / 60.0).ceil()),
        s => format!("about {:.1} hours", s / 3600.0),
    }
}

#[cfg(test)]
mod tests {
    use super::duration;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(20.0), "under a minute");
        assert_eq!(duration(4253.0 * 0.15), "about 11 minutes");
        assert_eq!(duration(200_000.0 * 0.15), "about 8.3 hours");
    }
}
