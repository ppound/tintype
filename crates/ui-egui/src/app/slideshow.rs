//! Full-screen slideshow of the current results, with keys to rate and tag
//! photos as they go by.

use std::path::Path;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Key, Rect, Sense, TextEdit, ViewportCommand, pos2, vec2,
};

use super::{STAR, TintypeApp, camera_name, fit, full_uv, paint_star};
use crate::thumbs::Slot;

/// Seconds per photo the + and − keys step through.
const INTERVALS: [f32; 10] = [2.0, 3.0, 4.0, 5.0, 7.0, 10.0, 15.0, 20.0, 30.0, 60.0];
/// The caption and pointer hide this long after the last key or mouse move.
const IDLE: Duration = Duration::from_millis(2500);
/// How long a change ("★★★★", "+ beach") stays on screen.
const FLASH: Duration = Duration::from_millis(1400);
const OVERLAY: Color32 = Color32::from_black_alpha(170);
const TEXT: Color32 = Color32::from_gray(225);
const WEAK: Color32 = Color32::from_gray(150);

const HELP: &[(&str, &str)] = &[
    ("Space", "Pause or play"),
    ("→  ←", "Next or previous photo"),
    ("Home  End", "First or last photo"),
    ("1 – 5", "Rate the photo"),
    ("0", "Clear its rating"),
    ("T", "Add tags (start a tag with − to remove it)"),
    ("+  −", "Show each photo longer or shorter"),
    ("I", "Always show the caption"),
    ("F", "Show or hide face boxes"),
    ("H", "Show or hide these keys"),
    ("Esc", "Leave the slideshow"),
];

pub(super) struct Slideshow {
    playing: bool,
    /// The photo on screen and when it appeared in full; its time starts then.
    shown: Option<(i64, Instant)>,
    /// Last key press or mouse move; the caption and pointer hide after a while.
    active_at: Instant,
    /// The tag box, while open.
    tagging: Option<String>,
    /// The tags last added, for adding again with an empty Enter.
    last_tags: Vec<String>,
    help: bool,
    flash: Option<(String, Instant)>,
    was_fullscreen: bool,
}

impl TintypeApp {
    pub(super) fn start_slideshow(&mut self, ctx: &egui::Context) {
        if self.results.is_empty() {
            return;
        }
        self.select(self.selected.unwrap_or(0));
        let was_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if self.screenshot.is_none() {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
        }
        self.slideshow = Some(Slideshow {
            playing: true,
            shown: None,
            active_at: Instant::now(),
            tagging: None,
            last_tags: Vec::new(),
            help: false,
            flash: None,
            was_fullscreen,
        });
    }

    /// For screenshots: whether the photo on screen has loaded in full.
    pub(super) fn slide_shown(&self) -> bool {
        self.slideshow.as_ref().is_none_or(|s| s.shown.is_some())
    }

    /// For screenshots: open the key list or the tag box.
    pub(super) fn slideshow_panel(&mut self, panel: &str) {
        if let Some(s) = &mut self.slideshow {
            s.playing = false;
            match panel {
                "keys" => s.help = true,
                "tags" => s.tagging = Some("su".into()),
                _ => {}
            }
        }
    }

    pub(super) fn stop_slideshow(&mut self, ctx: &egui::Context) {
        if let Some(s) = self.slideshow.take()
            && !s.was_fullscreen
            && self.screenshot.is_none()
        {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
        self.scroll_to_selected = true;
    }

    fn slide_to(&mut self, index: usize) {
        self.select(index);
        if let Some(s) = &mut self.slideshow {
            s.shown = None;
        }
    }

    /// The next (or previous) photo, wrapping around at the ends.
    fn slide_step(&mut self, forward: bool) {
        let n = self.results.len();
        if n == 0 {
            return;
        }
        let cur = self.selected.unwrap_or(0);
        self.slide_to(if forward { (cur + 1) % n } else { (cur + n - 1) % n });
    }

    fn flash(&mut self, text: impl Into<String>) {
        if let Some(s) = &mut self.slideshow {
            s.flash = Some((text.into(), Instant::now()));
        }
    }

    /// Keys while the slideshow runs; the rest of the app's keys are off.
    pub(super) fn slideshow_keys(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.slideshow else { return };
        let (any_key, moved) = ctx.input(|i| {
            let key = i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. }));
            (key, i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_click())
        });
        if any_key || moved {
            s.active_at = Instant::now();
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        if s.tagging.is_some() {
            // The tag box has the keyboard; Esc closes it (see `tag_box`).
            return;
        }
        if s.help {
            if pressed(Key::Escape) || pressed(Key::H) || pressed(Key::F1) {
                s.help = false;
            }
            return;
        }
        if pressed(Key::Escape) {
            self.stop_slideshow(ctx);
            return;
        }
        if pressed(Key::Space) {
            s.playing = !s.playing;
            // Restart the photo's time, so playing again doesn't move on at once.
            s.shown = s.shown.map(|(id, _)| (id, Instant::now()));
            let text = if s.playing { "Playing" } else { "Paused" };
            self.flash(text);
        }
        if pressed(Key::H) || pressed(Key::F1) || pressed(Key::Questionmark) {
            self.slideshow.as_mut().expect("running").help = true;
        }
        if pressed(Key::T) {
            // Keep the "t" itself out of the box.
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_))));
            self.slideshow.as_mut().expect("running").tagging = Some(String::new());
        }
        if pressed(Key::I) {
            self.settings.slide_caption = !self.settings.slide_caption;
            self.flash(if self.settings.slide_caption { "Caption always on" } else { "Caption hides" });
        }
        if pressed(Key::F) {
            self.people.hide_boxes = !self.people.hide_boxes;
        }
        let longer = pressed(Key::Plus) || pressed(Key::Equals);
        let shorter = pressed(Key::Minus);
        if longer || shorter {
            let cur = INTERVALS.iter().position(|&s| s >= self.settings.slide_seconds).unwrap_or(INTERVALS.len() - 1);
            let next = if longer { (cur + 1).min(INTERVALS.len() - 1) } else { cur.saturating_sub(1) };
            self.settings.slide_seconds = INTERVALS[next];
            self.flash(format!("{} s per photo", INTERVALS[next]));
        }

        if pressed(Key::ArrowRight) || pressed(Key::PageDown) {
            self.slide_step(true);
        } else if pressed(Key::ArrowLeft) || pressed(Key::PageUp) || pressed(Key::Backspace) {
            self.slide_step(false);
        } else if pressed(Key::Home) {
            self.slide_to(0);
        } else if pressed(Key::End) {
            self.slide_to(self.results.len().saturating_sub(1));
        }

        let digits = [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
        if let Some(id) = self.selected.and_then(|i| self.results.get(i)).map(|h| h.id)
            && let Some(n) = digits.iter().position(|&k| pressed(k))
        {
            self.rate(id, (n > 0).then_some(n as u8));
            self.flash(if n == 0 { "Rating cleared".to_string() } else { "★".repeat(n) });
        }
    }

    pub(super) fn slideshow_view(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(i) = self.selected.filter(|&i| i < self.results.len()) else {
            self.stop_slideshow(&ctx);
            return;
        };
        let hit = self.results[i].clone();
        let rect = ui.max_rect();
        let resp = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);

        let (tex, error) = match self.thumbs.preview(hit.id, &hit.path) {
            Slot::Ready(t) => (Some(t.clone()), None),
            Slot::Failed(e) => (None, Some(e.clone())),
            Slot::Pending => (None, None),
        };
        let full = tex.is_some() || error.is_some();
        let tex = tex.or_else(|| self.thumbs.loaded_thumb(hit.id).cloned());
        if let Some(t) = &tex {
            let image = fit(t.size_vec2(), rect);
            painter.image(t.id(), image, full_uv(), Color32::WHITE);
            self.paint_face_boxes(&painter, hit.id, image);
        } else if !full {
            ui.put(Rect::from_center_size(rect.center(), vec2(32.0, 32.0)), egui::Spinner::new());
        }
        if let Some(e) = error {
            painter.text(rect.center(), Align2::CENTER_CENTER, e, FontId::proportional(16.0), WEAK);
        }
        if full && let Some(next) = self.results.get((i + 1) % self.results.len()).cloned() {
            // Load the next photo while this one shows.
            self.thumbs.preview(next.id, &next.path);
        }

        let Some(s) = &mut self.slideshow else { return };
        let now = Instant::now();
        if full && s.shown.is_none_or(|(id, _)| id != hit.id) {
            s.shown = Some((hit.id, now));
        }
        let idle = now.duration_since(s.active_at) > IDLE;
        let (playing, tagging, help) = (s.playing, s.tagging.is_some(), s.help);
        let flash = s.flash.clone().filter(|(_, at)| now.duration_since(*at) < FLASH);

        // Time to move on?
        let interval = Duration::from_secs_f32(self.settings.slide_seconds);
        if playing
            && !tagging
            && !help
            && let Some((_, at)) = s.shown
        {
            let elapsed = now.duration_since(at);
            if elapsed >= interval {
                self.slide_step(true);
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after(interval - elapsed);
            }
        }
        if !idle {
            ctx.request_repaint_after(IDLE);
        } else if !tagging && !help {
            ctx.set_cursor_icon(CursorIcon::None);
        }

        // Clicks: right half forward, left half back.
        if resp.clicked() && !tagging && !help {
            let forward = resp.interact_pointer_pos().is_none_or(|p| p.x >= rect.center().x);
            self.slide_step(forward);
        }

        if self.settings.slide_caption || !idle || !playing || tagging {
            self.slide_caption(&painter, rect, i, hit.id, &hit.path);
            let state = if playing {
                format!("Playing  ·  {} s per photo", self.settings.slide_seconds)
            } else {
                "Paused".into()
            };
            label(&painter, rect.right_top() + vec2(-20.0, 20.0), Align2::RIGHT_TOP, &state, 15.0, TEXT);
            let hint = "H for keys   ·   Esc to leave";
            label(&painter, rect.right_top() + vec2(-20.0, 52.0), Align2::RIGHT_TOP, hint, 13.0, WEAK);
        }
        if let Some((text, at)) = flash {
            let color = if text.starts_with('★') { STAR } else { TEXT };
            label(&painter, pos2(rect.center().x, rect.top() + 48.0), Align2::CENTER_TOP, &text, 26.0, color);
            ctx.request_repaint_after(FLASH.saturating_sub(now.duration_since(at)));
        }
        if help {
            help_overlay(&painter, rect);
        }
        if tagging {
            self.tag_box(&ctx, rect, hit.id);
        }
    }

    /// Name, place in the results, date and camera, rating and tags.
    fn slide_caption(&self, painter: &egui::Painter, rect: Rect, index: usize, id: i64, path: &str) {
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let detail = self.detail.as_ref().filter(|d| d.id == id);
        let mut lines = vec![(format!("{name}    {} of {}", index + 1, self.results.len()), TEXT)];
        if let Some(d) = detail {
            let m = &d.meta;
            let about: Vec<String> = [
                m.taken.map(|t| t.format("%e %b %Y, %H:%M").to_string().trim().to_string()),
                camera_name(m.make.as_deref(), m.model.as_deref()),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !about.is_empty() {
                lines.push((about.join("    "), WEAK));
            }
            if !d.tags.is_empty() {
                lines.push((d.tags.join(",  "), TEXT));
            }
        }
        let line_h = 22.0;
        let stars_h = 24.0;
        let height = lines.len() as f32 * line_h + stars_h + 20.0;
        let width = lines
            .iter()
            .map(|(t, _)| painter.layout_no_wrap(t.clone(), FontId::proportional(15.0), TEXT).size().x)
            .fold(5.0 * 22.0, f32::max)
            + 28.0;
        let bg = Rect::from_min_size(pos2(rect.left() + 20.0, rect.bottom() - 20.0 - height), vec2(width, height));
        painter.rect_filled(bg, 8.0, OVERLAY);
        let mut y = bg.top() + 10.0;
        for (text, color) in lines.iter().take(2) {
            painter.text(pos2(bg.left() + 14.0, y), Align2::LEFT_TOP, text, FontId::proportional(15.0), *color);
            y += line_h;
        }
        let rating = detail.and_then(|d| d.rating);
        for n in 1..=5u8 {
            let filled = rating.is_some_and(|r| r >= n);
            let center = pos2(bg.left() + 14.0 + 9.0 + (n - 1) as f32 * 22.0, y + stars_h / 2.0 - 2.0);
            paint_star(painter, center, 9.0, filled, if filled { STAR } else { WEAK });
        }
        y += stars_h;
        for (text, color) in lines.iter().skip(2) {
            painter.text(pos2(bg.left() + 14.0, y), Align2::LEFT_TOP, text, FontId::proportional(15.0), *color);
            y += line_h;
        }
    }

    /// The tag box: Enter adds the tags typed (comma-separated; "-tag"
    /// removes one), an empty Enter adds the last tags again, Esc closes.
    fn tag_box(&mut self, ctx: &egui::Context, rect: Rect, id: i64) {
        let known: Vec<String> = self
            .facets
            .get("tags")
            .map(|c| c.iter().map(|f| f.path.trim_start_matches('/').to_string()).collect())
            .unwrap_or_default();
        let Some(s) = &mut self.slideshow else { return };
        let Some(text) = &mut s.tagging else { return };
        let (mut submit, mut close) = (false, false);
        egui::Area::new(egui::Id::unique("slideshow-tags"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(rect.width().min(520.0));
                    ui.label("Add tags, separated by commas. Start one with − to remove it.");
                    let edit =
                        ui.add(TextEdit::singleline(text).hint_text("beach, sunset").desired_width(f32::INFINITY));
                    edit.request_focus();
                    // The word being typed, completed with Tab from the library's tags.
                    let word =
                        text.rsplit(',').next().unwrap_or("").trim().trim_start_matches(['-', '−']).to_lowercase();
                    let matches: Vec<&String> = if word.is_empty() {
                        Vec::new()
                    } else {
                        known
                            .iter()
                            .filter(|t| t.to_lowercase().starts_with(&word) && t.to_lowercase() != word)
                            .take(6)
                            .collect()
                    };
                    if !matches.is_empty() {
                        ui.weak(format!("Tab: {}", matches.iter().map(|m| m.as_str()).collect::<Vec<_>>().join("   ")));
                    } else if text.trim().is_empty() && !s.last_tags.is_empty() {
                        ui.weak(format!("Enter adds {} again", s.last_tags.join(", ")));
                    }
                    if let Some(first) = matches.first()
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab))
                    {
                        let keep = text.len() - text.rsplit(',').next().unwrap_or("").len();
                        let sign = if text[keep..].trim_start().starts_with(['-', '−']) { "-" } else { "" };
                        let lead = if keep > 0 { " " } else { "" };
                        *text = format!("{}{lead}{sign}{first}", &text[..keep]);
                        if let Some(mut state) = egui::TextEdit::load_state(ctx, edit.id) {
                            let end = egui::text::CCursor::new(text.chars().count());
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                            state.store(ctx, edit.id);
                        }
                    }
                    submit = ui.input(|i| i.key_pressed(Key::Enter));
                    close = ui.input(|i| i.key_pressed(Key::Escape));
                });
            });
        if close {
            s.tagging = None;
        } else if submit {
            let typed = std::mem::take(text);
            s.tagging = None;
            let (mut add, remove) = parse_tags(&typed);
            if add.is_empty() && remove.is_empty() {
                add = s.last_tags.clone();
            } else if !add.is_empty() {
                s.last_tags = add.clone();
            }
            if add.is_empty() && remove.is_empty() {
                return;
            }
            let mut parts: Vec<String> = add.iter().map(|t| format!("+ {t}")).collect();
            parts.extend(remove.iter().map(|t| format!("− {t}")));
            self.edit_tags(id, add, remove);
            self.flash(parts.join("   "));
        }
    }
}

/// Tags to add and to remove from what was typed: "beach, -dog, sunset".
fn parse_tags(typed: &str) -> (Vec<String>, Vec<String>) {
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    for t in typed.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match t.strip_prefix(['-', '−']).map(str::trim) {
            Some("") => {}
            Some(r) => remove.push(r.to_string()),
            None => add.push(t.to_string()),
        }
    }
    (add, remove)
}

/// Text on a dark rounded box.
fn label(painter: &egui::Painter, pos: egui::Pos2, align: Align2, text: &str, size: f32, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(size), color);
    let pad = vec2(12.0, 6.0);
    let rect = align.anchor_size(pos, galley.size() + 2.0 * pad);
    painter.rect_filled(rect, 8.0, OVERLAY);
    painter.galley(rect.min + pad, galley, color);
}

fn help_overlay(painter: &egui::Painter, rect: Rect) {
    let row_h = 28.0;
    let size = vec2(520.0, HELP.len() as f32 * row_h + 70.0);
    let bg = Rect::from_center_size(rect.center(), size);
    painter.rect_filled(bg, 10.0, Color32::from_black_alpha(215));
    let font = FontId::proportional(15.0);
    painter.text(
        bg.left_top() + vec2(24.0, 18.0),
        Align2::LEFT_TOP,
        "Slideshow keys",
        FontId::proportional(18.0),
        TEXT,
    );
    for (n, (key, what)) in HELP.iter().enumerate() {
        let y = bg.top() + 58.0 + n as f32 * row_h;
        painter.text(pos2(bg.left() + 24.0, y), Align2::LEFT_TOP, *key, font.clone(), STAR);
        painter.text(pos2(bg.left() + 150.0, y), Align2::LEFT_TOP, *what, font.clone(), TEXT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_tags() {
        let (add, remove) = parse_tags(" beach, -dog ,, − old  tag, -, sunset ");
        assert_eq!(add, ["beach", "sunset"]);
        assert_eq!(remove, ["dog", "old  tag"]);
        assert_eq!(parse_tags("   "), (vec![], vec![]));
    }
}
