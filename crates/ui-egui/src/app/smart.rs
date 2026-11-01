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
/// ViT-L/14 on tract: 2.0 s per photo per core, ~6x across cores.
const AESTHETIC_SECONDS_PER_PHOTO: f64 = 0.35;

#[derive(Default)]
pub(super) struct SmartState {
    analyzing: Option<AnalyzeProgress>,
    /// Don't start analysis by itself: the user stopped it, or it stopped
    /// making progress. Starting it by hand clears this.
    pub held: bool,
}

/// Analysis runs two passes, one after the other: smart search, then quality.
struct AnalyzeProgress {
    /// Photos each pass had to do when it started...
    smart: u64,
    quality: u64,
    /// ...and photos done, counting through both passes.
    done: u64,
    stop: bool,
    /// Step 1 only needs auto tags: the photos were embedded already.
    tagging: bool,
}

impl AnalyzeProgress {
    /// "Smart search: 1,200 of 36,624 photos (step 1 of 2)", and the fraction done overall.
    fn status(&self, done: u64) -> (String, f32) {
        let fraction = done as f32 / (self.smart + self.quality).max(1) as f32;
        let (what, done, of, step) = if done < self.smart || self.quality == 0 {
            let what = if self.tagging { "Tagging photos by subject" } else { "Analyzing photos for smart search" };
            (what, done.min(self.smart), self.smart, 1)
        } else {
            ("Measuring photo quality", (done - self.smart).min(self.quality), self.quality, 2)
        };
        let step = if self.smart > 0 && self.quality > 0 { format!("  (step {step} of 2)") } else { String::new() };
        (format!("{what}: {} of {}{step}", thousands(done), thousands(of)), fraction.min(1.0))
    }
}

impl SmartState {
    pub fn stop(&mut self) {
        if let Some(a) = &mut self.analyzing {
            a.stop = true;
            self.held = true;
        }
    }
}

impl TintypeApp {
    /// While the aesthetic score is on, photos without one are scored by
    /// themselves: on launch, after a scan, and when it is turned on. Auto
    /// tags for photos analyzed already are quick, so they always are.
    pub(super) fn auto_analyze(&mut self) {
        let on = self.aesthetic.is_some_and(|(on, _)| on);
        let todo = self.stats.as_ref().is_some_and(|s| s.unanalyzed > 0);
        let tags = self.stats.as_ref().is_some_and(|s| s.smart_ready && s.needs_subjects > 0);
        if ((on && todo) || tags) && !self.smart.held && self.screenshot.is_none() {
            self.start_analyze();
        }
    }

    pub(super) fn start_analyze(&mut self) {
        if self.smart.analyzing.is_some() || self.scan.is_some() {
            return;
        }
        self.smart.held = false;
        let (smart, quality, tagging) = self.stats.as_ref().map_or((0, 0, false), |s| {
            (s.needs_smart_search, s.needs_quality, s.needs_subjects == s.needs_smart_search)
        });
        self.smart.analyzing = Some(AnalyzeProgress { smart, quality, done: 0, stop: false, tagging });
        self.scan = Some(ScanState { label: "Preparing smart search…".into(), fraction: None });
        self.backend.send(Job::Analyze, Command::Analyze { batch: ANALYZE_BATCH });
    }

    /// Returns false if the event isn't about analysis.
    pub(super) fn on_analyze_progress(&mut self, event: Event) -> bool {
        let (Event::Analyze { done, .. }, Some(scan), Some(a)) = (event, &mut self.scan, &self.smart.analyzing) else {
            return false;
        };
        let (label, fraction) = a.status(a.done + done);
        scan.label = label;
        scan.fraction = Some(fraction);
        true
    }

    /// Returns false if the reply isn't for analysis.
    pub(super) fn on_analyze_reply(&mut self, job: Job, result: &Result<Response, String>) -> bool {
        match (job, result) {
            (Job::Analyze, Ok(Response::Analyze(r))) => {
                let Some(a) = &mut self.smart.analyzing else { return true };
                a.done += r.processed;
                if r.cancelled || a.stop || r.remaining == 0 || r.processed == 0 {
                    let (status, _) = a.status(a.done);
                    // Stuck: don't start again by itself.
                    self.smart.held |= r.remaining > 0;
                    self.smart.analyzing = None;
                    self.scan = None;
                    let msg = if r.remaining > 0 {
                        format!("Analysis paused. {status}")
                    } else {
                        "Analysis finished: every photo is ready for smart search and quality sorting".to_string()
                    };
                    self.notify(msg, false);
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
                self.smart.held = true;
                self.smart.analyzing = None;
                self.scan = None;
                self.notify(format!("Analysis stopped: {e}"), true);
                self.refresh_stats();
            }
            _ => return false,
        }
        true
    }

    /// Measured: smart search ~0.15 s per photo; the aesthetic score adds ~0.35 s.
    fn seconds_per_photo(&self) -> f64 {
        SECONDS_PER_PHOTO + if self.aesthetic.is_some_and(|(on, _)| on) { AESTHETIC_SECONDS_PER_PHOTO } else { 0.0 }
    }

    /// What "Best photos first" will use, for its tooltip.
    pub(super) fn best_sort_hint(&self) -> String {
        let Some(s) = &self.stats else { return String::new() };
        if s.taste_ratings >= s.taste_needed {
            format!("Ordered by your taste, learned from the {} photos you rated.", s.taste_ratings)
        } else if self.aesthetic.is_some_and(|(on, _)| on) {
            format!(
                "Ordered by the aesthetic score. After you rate {} more photos, by your own taste.",
                s.taste_needed - s.taste_ratings
            )
        } else {
            format!(
                "Turn on Settings > Aesthetic score, or rate {} more photos, so Tintype can order by quality.",
                s.taste_needed - s.taste_ratings
            )
        }
    }

    /// In the status bar after the aesthetic pass was stopped: a way to carry on.
    pub(super) fn resume_link(&mut self, ui: &mut egui::Ui) {
        let on = self.aesthetic.is_some_and(|(on, _)| on);
        let todo = self.stats.as_ref().map_or(0, |s| s.unanalyzed);
        if !on || todo == 0 || !self.smart.held || self.smart.analyzing.is_some() {
            return;
        }
        ui.weak("·");
        let link = ui.link(format!("{} photos not scored yet", thousands(todo)));
        if link.on_hover_text("Score them now").clicked() {
            self.start_analyze();
        }
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
                    duration(unanalyzed as f64 * self.seconds_per_photo())
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
    use super::{AnalyzeProgress, duration};

    #[test]
    fn analysis_shows_each_pass() {
        let a = AnalyzeProgress { smart: 36_624, quality: 36_624, done: 0, stop: false, tagging: false };
        assert_eq!(a.status(0).0, "Analyzing photos for smart search: 0 of 36,624  (step 1 of 2)");
        let (label, fraction) = a.status(36_625);
        assert_eq!(label, "Measuring photo quality: 1 of 36,624  (step 2 of 2)");
        assert!((fraction - 0.5).abs() < 0.001);
        let quality_only = AnalyzeProgress { smart: 0, quality: 10, done: 0, stop: false, tagging: false };
        assert_eq!(quality_only.status(3).0, "Measuring photo quality: 3 of 10");
        let tags = AnalyzeProgress { smart: 36_624, quality: 10, done: 0, stop: false, tagging: true };
        assert_eq!(tags.status(2_000).0, "Tagging photos by subject: 2,000 of 36,624  (step 1 of 2)");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(20.0), "under a minute");
        assert_eq!(duration(4253.0 * 0.15), "about 11 minutes");
        assert_eq!(duration(200_000.0 * 0.15), "about 8.3 hours");
    }
}
