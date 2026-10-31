//! The main window: search bar and filters on top, facets on the left, the
//! photo grid (or one photo, large) in the middle, details on the right.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Button, CollapsingHeader, Color32, FontId, Key, Layout, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextEdit, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};
use tintype_engine::{
    Command, Engine, Event, FacetCount, Hit, PhotoRecord, PhotoRef, Response, ScanProgress, Sort, Stats, split_smart,
};

use crate::backend::{Backend, Job, Msg, Reply};
use crate::thumbs::{Slot, Thumbs};

mod people;
mod smart;
use people::PeopleState;
use smart::SmartState;

/// Upper bound on hits loaded into the grid.
const SEARCH_LIMIT: usize = 200_000;
const TYPING_DEBOUNCE: Duration = Duration::from_millis(250);
const NOTICE_TTL: Duration = Duration::from_secs(10);
/// Facet values shown before "Show all".
const FACET_PREVIEW: usize = 12;
const GRID_GAP: f32 = 6.0;
/// (sidebar title, facet field)
const FACETS: &[(&str, &str)] = &[
    ("Camera", "camera"),
    ("Lens", "lenses"),
    ("Date", "date"),
    ("Tags", "tags"),
    ("People", "people"),
    ("File type", "ext"),
    ("Folder", "folder"),
];
const STAR: Color32 = Color32::from_rgb(242, 184, 48);

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    thumb_size: f32,
    sort: Sort,
    /// Smart search: match the words by what the photos show.
    smart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { thumb_size: 160.0, sort: Sort::Newest, smart: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Photos,
    People,
}

#[derive(PartialEq, Eq)]
enum View {
    Grid,
    Loupe,
}

struct ScanState {
    label: String,
    fraction: Option<f32>,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

/// Automated screenshot for development (`TINTYPE_SCREENSHOT=out.png`).
pub struct ScreenshotPlan {
    pub path: PathBuf,
    /// Select this result index once results arrive.
    pub select: Option<usize>,
    /// "loupe", "people" or "person" (the first person's page).
    pub view: Option<String>,
    /// Run this smart search first (an empty string shows smart mode with no query).
    pub smart_query: Option<String>,
}

struct Screenshot {
    plan: ScreenshotPlan,
    frames: u32,
    selected: bool,
    requested: bool,
    saved: Arc<AtomicBool>,
}

pub struct TintypeApp {
    backend: Backend,
    thumbs: Thumbs,
    settings: Settings,
    tab: Tab,
    people: PeopleState,
    smart: SmartState,
    /// Photo to open once the next search returns (from a face on the People tab).
    pending_open: Option<i64>,

    query: String,
    /// The search box's widget id, for focusing it with Ctrl+F.
    search_box: Option<egui::Id>,
    query_error: Option<String>,
    filters: Vec<(&'static str, String)>,
    search_due: Option<Instant>,
    search_seq: u64,
    results: Vec<Hit>,
    total: usize,
    searched_once: bool,

    facets: HashMap<&'static str, Vec<FacetCount>>,
    facet_seqs: HashMap<&'static str, u64>,
    /// Folder facets skip down through single-child levels (C: > Users > me > Pictures).
    folder_base: String,
    expanded: HashSet<&'static str>,

    selected: Option<usize>,
    detail: Option<PhotoRecord>,
    detail_seq: u64,
    tag_input: String,
    view: View,
    columns: usize,
    /// Scroll offset and height of the grid viewport last frame.
    grid_view: (f32, f32),
    scroll_to_selected: bool,

    scan: Option<ScanState>,
    stats: Option<Stats>,
    /// Library setting, once the engine has said.
    write_sidecars: Option<bool>,
    notice: Option<Notice>,
    screenshot: Option<Screenshot>,
}

impl TintypeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, engine: Engine, screenshot: Option<ScreenshotPlan>) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = vec2(8.0, 6.0);
            style.spacing.button_padding = vec2(8.0, 4.0);
        });
        let settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let backend = Backend::start(engine, ctx.clone());
        let thumbs = Thumbs::new(backend.thumbs(), ctx);
        let mut app = Self {
            backend,
            thumbs,
            settings,
            tab: Tab::Photos,
            people: PeopleState::default(),
            smart: SmartState::default(),
            pending_open: None,
            query: String::new(),
            search_box: None,
            query_error: None,
            filters: Vec::new(),
            search_due: None,
            search_seq: 0,
            results: Vec::new(),
            total: 0,
            searched_once: false,
            facets: HashMap::new(),
            facet_seqs: HashMap::new(),
            folder_base: "/".into(),
            expanded: HashSet::new(),
            selected: None,
            detail: None,
            detail_seq: 0,
            tag_input: String::new(),
            view: View::Grid,
            columns: 1,
            grid_view: (0.0, 0.0),
            scroll_to_selected: false,
            scan: None,
            stats: None,
            write_sidecars: None,
            notice: None,
            screenshot: screenshot.map(|plan| Screenshot {
                plan,
                frames: 0,
                selected: false,
                requested: false,
                saved: Arc::default(),
            }),
        };
        if let Some(q) = app.screenshot.as_ref().and_then(|s| s.plan.smart_query.clone()) {
            app.settings.smart = true;
            app.query = q;
        }
        app.refresh_stats();
        app.backend.send(Job::Settings, Command::Settings { write_sidecars: None });
        app.search();
        app
    }

    // ---- requests -------------------------------------------------------

    fn full_query(&self) -> String {
        let mut q = self.query.trim().to_string();
        for (field, path) in &self.filters {
            q.push_str(&format!(" {field}:\"{}\"", path.replace('"', "\\\"")));
        }
        q
    }

    /// What facet counts are taken over: in smart mode the words rank rather
    /// than filter, so facets count the photos the field terms allow.
    fn facet_query(&self) -> String {
        let q = self.full_query();
        if self.settings.smart { split_smart(&q).0 } else { q }
    }

    fn search(&mut self) {
        self.search_due = None;
        let cmd = Command::Search {
            query: self.full_query(),
            smart: self.settings.smart,
            sort: self.settings.sort,
            limit: SEARCH_LIMIT,
            offset: 0,
        };
        self.search_seq = self.backend.send(Job::Search, cmd);
        self.folder_base = "/".into();
        for &(_, field) in FACETS {
            self.request_facet(field);
        }
    }

    fn facet_parent(&self, field: &str) -> String {
        match self.filter(field) {
            Some(path) => path.to_string(),
            None if field == "folder" => self.folder_base.clone(),
            None => "/".into(),
        }
    }

    fn request_facet(&mut self, field: &'static str) {
        let cmd = Command::Facets { field: field.into(), query: self.facet_query(), parent: self.facet_parent(field) };
        let seq = self.backend.send(Job::Facets(field), cmd);
        self.facet_seqs.insert(field, seq);
    }

    fn refresh_stats(&mut self) {
        self.backend.send(Job::Stats, Command::Stats);
    }

    fn select(&mut self, index: usize) {
        let Some(hit) = self.results.get(index) else { return };
        let id = hit.id;
        if self.selected != Some(index) {
            self.tag_input.clear();
        }
        self.selected = Some(index);
        self.scroll_to_selected = true;
        self.detail_seq = self.backend.send(Job::Show, Command::Show { photo: PhotoRef::Id(id) });
        self.load_photo_faces(id);
    }

    fn rate(&mut self, id: i64, rating: Option<u8>) {
        self.backend.send(Job::Edit, Command::Rate { photo: PhotoRef::Id(id), rating });
    }

    fn edit_tags(&mut self, id: i64, add: Vec<String>, remove: Vec<String>) {
        self.backend.send(Job::Edit, Command::Tag { photo: PhotoRef::Id(id), add, remove });
    }

    fn start_scan(&mut self, cmd: Command) {
        self.scan = Some(ScanState { label: "Starting scan…".into(), fraction: None });
        self.backend.send(Job::Scan, cmd);
    }

    fn add_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().set_title("Add a photo folder").pick_folder() {
            self.start_scan(Command::Scan { roots: vec![dir], hash: true });
        }
    }

    fn filter(&self, field: &str) -> Option<&str> {
        self.filters.iter().find(|(f, _)| *f == field).map(|(_, p)| p.as_str())
    }

    fn set_filter(&mut self, field: &'static str, path: Option<String>) {
        self.filters.retain(|(f, _)| *f != field);
        if let Some(p) = path.filter(|p| p != "/") {
            self.filters.push((field, p));
        }
        self.search();
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
    }

    // ---- replies --------------------------------------------------------

    fn handle_messages(&mut self) {
        let msgs: Vec<Msg> = self.backend.poll().collect();
        for msg in msgs {
            match msg {
                Msg::Progress(event) => self.on_progress(event),
                Msg::Reply(r) => {
                    let Reply { job, seq, result } = *r;
                    self.on_reply(job, seq, result)
                }
            }
        }
    }

    fn on_progress(&mut self, event: Event) {
        if self.on_analyze_progress(event) || self.on_face_progress(event) {
            return;
        }
        let Some(scan) = &mut self.scan else { return };
        (scan.label, scan.fraction) = match event {
            Event::Scan(ScanProgress::Walking { found }) => (format!("Finding photos… {}", thousands(found)), None),
            Event::Scan(ScanProgress::Analyzing { done, total }) => (
                format!("Reading photos: {} of {}", thousands(done), thousands(total)),
                Some(done as f32 / total.max(1) as f32),
            ),
            Event::Reindex { done, total } => {
                ("Updating search index…".into(), Some(done as f32 / total.max(1) as f32))
            }
            Event::Download { .. } | Event::Faces { .. } | Event::Analyze { .. } => return,
        };
    }

    fn on_reply(&mut self, job: Job, seq: u64, result: Result<Response, String>) {
        if self.on_people_reply(job, seq, &result) || self.on_analyze_reply(job, &result) {
            return;
        }
        match (job, result) {
            (Job::Search, _) if seq != self.search_seq => {}
            (Job::Search, Ok(Response::Search(r))) => {
                let prev = self.selected.and_then(|i| self.results.get(i)).map(|h| h.id);
                self.results = r.hits;
                self.total = r.total;
                self.query_error = None;
                self.searched_once = true;
                self.selected = prev.and_then(|id| self.results.iter().position(|h| h.id == id));
                if self.selected.is_none() {
                    self.detail = None;
                    self.view = View::Grid;
                }
                if let Some(photo) = self.pending_open.take()
                    && let Some(i) = self.results.iter().position(|h| h.id == photo)
                {
                    self.select(i);
                    self.view = View::Loupe;
                }
            }
            (Job::Search, Err(e)) => self.query_error = Some(e),

            (Job::Facets(field), _) if self.facet_seqs.get(field) != Some(&seq) => {}
            (Job::Facets(field), Ok(Response::Facets { counts })) => {
                // Descend through folders that hold everything (C: > Users > me > Pictures).
                if field == "folder" && self.filter("folder").is_none() && counts.len() == 1 {
                    self.folder_base = counts[0].path.clone();
                    self.request_facet("folder");
                } else {
                    self.facets.insert(field, counts);
                }
            }

            (Job::Show, _) if seq != self.detail_seq => {}
            (Job::Show, Ok(Response::Photo(p))) => self.detail = Some(*p),

            (Job::Edit, Ok(Response::Photo(p))) => {
                if self.detail.as_ref().is_some_and(|d| d.id == p.id) {
                    self.detail = Some(*p);
                }
                self.request_facet("tags");
            }

            (Job::Scan, result) => {
                self.scan = None;
                match result {
                    Ok(Response::Scan(r)) if r.cancelled => self.notify("Scan cancelled. Rescan to finish.", false),
                    Ok(Response::Scan(r)) => {
                        let mut parts = vec![format!("{} photos", thousands(r.found))];
                        let counts =
                            [(r.added, "new"), (r.updated, "updated"), (r.moved, "moved"), (r.removed, "removed")];
                        for (n, what) in counts {
                            if n > 0 {
                                parts.push(format!("{} {what}", thousands(n)));
                            }
                        }
                        if r.unreadable > 0 {
                            parts.push(format!("{} without readable metadata", thousands(r.unreadable)));
                        }
                        if r.sidecars.imported > 0 {
                            parts.push(format!("{} .xmp sidecars read", thousands(r.sidecars.imported)));
                        }
                        if r.sidecars.failed > 0 {
                            parts.push(format!(
                                "{} sidecars could not be read or written",
                                thousands(r.sidecars.failed)
                            ));
                        }
                        self.notify(format!("Scan finished: {}", parts.join(", ")), false);
                        if r.updated > 0 || r.removed > 0 {
                            self.thumbs.clear();
                        }
                    }
                    Ok(_) => {}
                    Err(e) => self.notify(e, true),
                }
                self.refresh_stats();
                self.search();
            }

            (Job::Stats, Ok(Response::Stats(s))) => self.stats = Some(s),
            (Job::Settings, Ok(Response::Settings { write_sidecars, sidecars })) => {
                self.write_sidecars = Some(write_sidecars);
                if let Some(s) = sidecars.filter(|s| s.written + s.failed > 0) {
                    let failed = if s.failed > 0 { format!(", {} failed", thousands(s.failed)) } else { String::new() };
                    self.notify(format!("Wrote {} sidecars{failed}", thousands(s.written)), s.failed > 0);
                }
            }

            (_, Err(e)) => self.notify(e, true),
            (_, Ok(_)) => {}
        }
    }

    // ---- input ----------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F))
            && let Some(id) = self.search_box
        {
            ctx.memory_mut(|m| m.request_focus(id));
        }
        self.people_keys(ctx);
        if ctx.egui_wants_keyboard_input() || self.results.is_empty() || self.tab != Tab::Photos {
            return;
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        let last = self.results.len() - 1;
        let cur = self.selected;
        let step = |d: isize| cur.map_or(0, |c| c.saturating_add_signed(d).min(last));
        let target = match self.view {
            View::Grid if pressed(Key::ArrowRight) => Some(step(1)),
            View::Grid if pressed(Key::ArrowLeft) => Some(step(-1)),
            View::Grid if pressed(Key::ArrowDown) => Some(step(self.columns as isize)),
            View::Grid if pressed(Key::ArrowUp) => Some(step(-(self.columns as isize))),
            View::Grid if pressed(Key::Home) => Some(0),
            View::Grid if pressed(Key::End) => Some(last),
            View::Loupe if pressed(Key::ArrowRight) || pressed(Key::Space) => Some(step(1)),
            View::Loupe if pressed(Key::ArrowLeft) || pressed(Key::Backspace) => Some(step(-1)),
            _ => None,
        };
        if let Some(t) = target {
            self.select(t);
        }
        if self.view == View::Grid && pressed(Key::Enter) && self.selected.is_some() {
            self.view = View::Loupe;
        }
        if self.view == View::Loupe && pressed(Key::Escape) {
            self.view = View::Grid;
            self.scroll_to_selected = true;
        }
        let digits = [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
        if let Some(id) = self.selected.map(|i| self.results[i].id)
            && let Some(n) = digits.iter().position(|&k| pressed(k))
        {
            self.rate(id, (n > 0).then_some(n as u8));
        }
    }

    // ---- panels ---------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Tintype").strong().size(18.0));
            ui.add_space(12.0);
            for (tab, label) in [(Tab::Photos, "Photos"), (Tab::People, "People")] {
                if ui.selectable_label(self.tab == tab, RichText::new(label).size(15.0)).clicked() && self.tab != tab {
                    self.tab = tab;
                    if tab == Tab::People {
                        self.refresh_people();
                    }
                }
            }
            ui.add_space(12.0);
            if self.tab == Tab::People {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.library_buttons(ui));
                return;
            }
            let search_width = (ui.available_width() - 550.0).max(220.0);
            let hint = if self.settings.smart {
                "Describe it: dog on a beach   red car at night   year:2024"
            } else {
                "Search: beach   make:fujifilm   iso:>1600   year:2024   rating:>=4"
            };
            let resp = ui.add(
                TextEdit::singleline(&mut self.query)
                    .id_salt("tintype-search")
                    .hint_text(hint)
                    .desired_width(search_width),
            );
            self.search_box = Some(resp.id);
            if resp.changed() {
                self.search_due = Some(Instant::now() + TYPING_DEBOUNCE);
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.search();
            }
            if !self.query.is_empty() && ui.small_button("×").on_hover_text("Clear search").clicked() {
                self.query.clear();
                self.search();
            }
            if ui
                .toggle_value(&mut self.settings.smart, "Smart")
                .on_hover_text(
                    "Find photos by what they show: \"dog on a beach\", \"red car at night\".\n\
                     Field terms like year:2024 or rating:>=4 still filter. Best matches come first.",
                )
                .changed()
            {
                self.search();
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.library_buttons(ui);
                ui.add(egui::Slider::new(&mut self.settings.thumb_size, 96.0..=320.0).show_value(false))
                    .on_hover_text("Thumbnail size");
                let before = self.settings.sort;
                ui.add_enabled_ui(!self.settings.smart, |ui| {
                    let label = if self.settings.smart { "Best match" } else { sort_label(self.settings.sort) };
                    egui::ComboBox::from_id_salt("sort").selected_text(label).show_ui(ui, |ui| {
                        for s in [Sort::Newest, Sort::Oldest, Sort::Relevance] {
                            ui.selectable_value(&mut self.settings.sort, s, sort_label(s));
                        }
                    });
                })
                .response
                .on_disabled_hover_text("Smart search shows the best matches first");
                if self.settings.sort != before {
                    self.search();
                }
            });
        });

        if self.tab == Tab::People {
            ui.add_space(4.0);
            return;
        }
        if let Some(err) = &self.query_error {
            ui.colored_label(ui.visuals().error_fg_color, err);
        }
        self.smart_banner(ui);
        if !self.filters.is_empty() {
            let mut remove = None;
            let mut clear = false;
            ui.horizontal_wrapped(|ui| {
                ui.weak("Filtered by");
                for (i, (field, path)) in self.filters.iter().enumerate() {
                    let text = format!("{}: {}  ×", facet_title(field), facet_label(field, "/", path));
                    if ui.button(text).on_hover_text("Remove this filter").clicked() {
                        remove = Some(i);
                    }
                }
                clear = ui.link("Clear all").clicked();
            });
            if clear {
                self.filters.clear();
                self.search();
            } else if let Some(i) = remove {
                self.filters.remove(i);
                self.search();
            }
        }
        ui.add_space(4.0);
    }

    /// Rescan and Add folder, right-aligned in the top bar.
    fn library_buttons(&mut self, ui: &mut egui::Ui) {
        let idle = self.scan.is_none();
        if ui
            .add_enabled(idle, Button::new("Rescan"))
            .on_hover_text("Look for new, changed and removed photos in your folders")
            .clicked()
        {
            self.start_scan(Command::Rescan { hash: true });
        }
        if ui.add_enabled(idle, Button::new("Add folder…")).clicked() {
            self.add_folder();
        }
        ui.menu_button("Settings", |ui| {
            let Some(mut on) = self.write_sidecars else {
                ui.spinner();
                return;
            };
            let resp = ui.checkbox(&mut on, "Save ratings, tags and people in .xmp sidecars").on_hover_text(
                "Writes photo.jpg.xmp next to each photo you rate, tag or name someone in, so the \
                     information travels with your photos and other apps can read it. Your photos \
                     themselves are never changed. Sidecars edited by other apps are always read.",
            );
            if resp.changed() {
                self.write_sidecars = Some(on);
                self.backend.send(Job::Settings, Command::Settings { write_sidecars: Some(on) });
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(scan) = &self.scan {
                ui.spinner();
                ui.label(&scan.label);
                if let Some(f) = scan.fraction {
                    ui.add(egui::ProgressBar::new(f).desired_width(160.0));
                }
                if ui.small_button("Cancel").clicked() {
                    self.stop_jobs();
                }
            } else if let Some(stats) = &self.stats {
                ui.label(format!("{} photos", thousands(stats.photos)));
                if !self.filters.is_empty() || !self.query.trim().is_empty() {
                    ui.weak(format!("· {} match", thousands(self.total as u64)));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(n) = &self.notice {
                    if n.at.elapsed() > NOTICE_TTL {
                        self.notice = None;
                    } else {
                        let color = if n.error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                        ui.colored_label(color, &n.text);
                    }
                }
            });
        });
    }

    fn facet_panel(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<(&'static str, Option<String>)> = None;
        ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            ui.add_space(4.0);
            for &(title, field) in FACETS {
                let counts = self.facets.get(field).cloned().unwrap_or_default();
                let active = self.filter(field).map(str::to_string);
                if counts.is_empty() && active.is_none() {
                    continue;
                }
                CollapsingHeader::new(RichText::new(title).strong()).id_salt(field).default_open(true).show(ui, |ui| {
                    let parent = self.facet_parent(field);
                    if let Some(path) = &active {
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("All").clicked() {
                                chosen = Some((field, None));
                            }
                            // Breadcrumb: each ancestor is a link back up.
                            let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                            for (i, _) in segments.iter().enumerate() {
                                let prefix = format!("/{}", segments[..=i].join("/"));
                                ui.weak("›");
                                let label = facet_label(field, &format!("/{}", segments[..i].join("/")), &prefix);
                                if i + 1 == segments.len() {
                                    ui.label(RichText::new(label).strong());
                                } else if ui.link(label).clicked() {
                                    chosen = Some((field, Some(prefix)));
                                }
                            }
                        });
                    } else if field == "folder" && parent != "/" {
                        ui.label(RichText::new(parent.trim_start_matches('/')).small().weak());
                    }
                    let limit = if self.expanded.contains(field) { usize::MAX } else { FACET_PREVIEW };
                    for c in counts.iter().take(limit) {
                        if facet_row(ui, &facet_label(field, &parent, &c.path), c.count).clicked() {
                            chosen = Some((field, Some(c.path.clone())));
                        }
                    }
                    if counts.len() > FACET_PREVIEW {
                        let expanded = self.expanded.contains(field);
                        let text =
                            if expanded { "Show fewer".to_string() } else { format!("Show all {}", counts.len()) };
                        if ui.link(text).clicked() {
                            if expanded {
                                self.expanded.remove(field);
                            } else {
                                self.expanded.insert(field);
                            }
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });
        if let Some((field, path)) = chosen {
            self.set_filter(field, path);
        }
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let Some(p) = self.detail.clone() else {
            ui.add_space(12.0);
            ui.weak("Select a photo to see its details.");
            return;
        };
        let mut new_rating = None;
        let mut add_tag = None;
        let mut remove_tag = None;

        ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            ui.add_space(8.0);
            let path = Path::new(&p.path);
            let name = path.file_name().map_or(p.path.clone(), |n| n.to_string_lossy().into_owned());
            ui.label(RichText::new(name).heading());
            if let Some(dir) = path.parent() {
                ui.label(RichText::new(dir.display().to_string()).small().weak());
            }
            if ui.small_button("Copy path").clicked() {
                ui.ctx().copy_text(p.path.clone());
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for n in 1..=5u8 {
                    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
                    let filled = p.rating.is_some_and(|r| r >= n);
                    let color = match (filled, resp.hovered()) {
                        (true, _) => STAR,
                        (false, true) => ui.visuals().text_color(),
                        (false, false) => ui.visuals().weak_text_color(),
                    };
                    paint_star(ui.painter(), rect.center(), 10.0, filled, color);
                    let hint =
                        if p.rating == Some(n) { "Clear rating (0)".into() } else { format!("Rate {n} (key {n})") };
                    if resp.on_hover_text(hint).clicked() {
                        new_rating = Some(if p.rating == Some(n) { None } else { Some(n) });
                    }
                }
            });

            ui.add_space(6.0);
            let m = &p.meta;
            egui::Grid::new("info").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                let mut row = |label: &str, value: Option<String>| {
                    if let Some(v) = value {
                        ui.weak(label);
                        ui.label(v);
                        ui.end_row();
                    }
                };
                row("Taken", m.taken.map(|t| t.format("%e %b %Y, %H:%M").to_string().trim().to_string()));
                row("Camera", camera_name(m.make.as_deref(), m.model.as_deref()));
                row("Lens", m.lens.clone());
                let exposure: Vec<String> = [
                    m.f_number.map(|f| format!("f/{f}")),
                    m.exposure_s.map(shutter),
                    m.iso.map(|i| format!("ISO {i}")),
                    m.focal_mm.map(|f| format!("{f} mm")),
                ]
                .into_iter()
                .flatten()
                .collect();
                row("Exposure", (!exposure.is_empty()).then(|| exposure.join("   ")));
                row("Size", m.width.zip(m.height).map(|(w, h)| format!("{w} × {h}")));
                row("File", Some(format!("{:.1} MB", p.size as f64 / 1_048_576.0)));
            });
            if let Some(g) = m.gps {
                let url = format!("https://www.openstreetmap.org/?mlat={0}&mlon={1}#map=15/{0}/{1}", g.lat, g.lon);
                ui.hyperlink_to(format!("{:.5}, {:.5}  (open map)", g.lat, g.lon), url);
            }

            ui.add_space(10.0);
            ui.label(RichText::new("Tags").strong());
            if !p.tags.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for t in &p.tags {
                        if ui.button(format!("{t}  ×")).on_hover_text("Remove tag").clicked() {
                            remove_tag = Some(t.clone());
                        }
                    }
                });
            }
            let edit =
                ui.add(TextEdit::singleline(&mut self.tag_input).hint_text("Add a tag…").desired_width(f32::INFINITY));
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.tag_input.trim().is_empty() {
                add_tag = Some(std::mem::take(&mut self.tag_input));
                edit.request_focus();
            }

            self.photo_faces_section(ui, p.id);

            if let Some(e) = &p.error {
                ui.add_space(8.0);
                ui.colored_label(ui.visuals().warn_fg_color, format!("Metadata could not be read: {e}"));
            }

            ui.add_space(10.0);
            CollapsingHeader::new(format!("All metadata ({})", p.meta.fields.len())).show(ui, |ui| {
                egui::Grid::new("exif").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in &p.meta.fields {
                        ui.weak(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            });
        });

        if let Some(r) = new_rating {
            self.rate(p.id, r);
        }
        if add_tag.is_some() || remove_tag.is_some() {
            self.edit_tags(p.id, add_tag.into_iter().collect(), remove_tag.into_iter().collect());
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new("Welcome to Tintype").size(28.0).strong());
            ui.add_space(8.0);
            ui.label("Add a folder of photos to get started.");
            ui.weak("Photos are read where they are. Nothing is moved, copied or changed.");
            ui.add_space(16.0);
            if ui.add(Button::new(RichText::new("Add folder…").size(16.0)).min_size(vec2(160.0, 36.0))).clicked() {
                self.add_folder();
            }
        });
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        if self.results.is_empty() {
            ui.centered_and_justified(|ui| {
                if self.searched_once {
                    ui.weak("No photos match.");
                }
            });
            return;
        }
        // The slider sets the smallest cell; cells grow to fill the row exactly.
        let width = ui.available_width() - ui.spacing().scroll.bar_width - 2.0;
        let cols = ((width + GRID_GAP) / (self.settings.thumb_size + GRID_GAP)).floor().max(1.0) as usize;
        let size = ((width + GRID_GAP) / cols as f32 - GRID_GAP).floor();
        self.columns = cols;
        let rows = self.results.len().div_ceil(cols);
        let stride = size + GRID_GAP;

        let mut area = ScrollArea::vertical().auto_shrink(false);
        if std::mem::take(&mut self.scroll_to_selected)
            && let Some(i) = self.selected
        {
            let top = (i / cols) as f32 * stride;
            let (offset, height) = self.grid_view;
            if top < offset {
                area = area.vertical_scroll_offset(top);
            } else if top + stride > offset + height {
                area = area.vertical_scroll_offset(top + stride - height);
            }
        }

        let mut clicked = None;
        let mut opened = None;
        let output = area.show_rows(ui, size, rows, |ui, range| {
            ui.spacing_mut().item_spacing = vec2(GRID_GAP, GRID_GAP);
            for row in range {
                ui.horizontal(|ui| {
                    for i in row * cols..((row + 1) * cols).min(self.results.len()) {
                        let resp = self.cell(ui, i, size);
                        if resp.double_clicked() {
                            opened = Some(i);
                        } else if resp.clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            }
        });
        self.grid_view = (output.state.clamped_offset().y, output.inner_rect.height());

        if let Some(i) = opened {
            self.select(i);
            self.view = View::Loupe;
        } else if let Some(i) = clicked {
            self.select(i);
            self.scroll_to_selected = false;
        }
    }

    fn cell(&mut self, ui: &mut egui::Ui, i: usize, size: f32) -> egui::Response {
        let (id, path) = (self.results[i].id, self.results[i].path.clone());
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        let visuals = ui.visuals().clone();
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
        match self.thumbs.thumb(id, &path) {
            Slot::Ready(tex) => {
                painter.image(tex.id(), fit(tex.size_vec2(), rect.shrink(3.0)), full_uv(), Color32::WHITE);
            }
            Slot::Pending => {}
            Slot::Failed(_) => {
                let ext = Path::new(&path).extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    ext,
                    FontId::proportional(14.0),
                    visuals.weak_text_color(),
                );
            }
        }
        if self.selected == Some(i) {
            painter.rect_stroke(rect, 4.0, Stroke::new(3.0, visuals.selection.bg_fill), StrokeKind::Inside);
        } else if resp.hovered() {
            painter.rect_stroke(rect, 4.0, visuals.widgets.hovered.bg_stroke, StrokeKind::Inside);
        }
        let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        resp.on_hover_text_at_pointer(name)
    }

    fn loupe(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.selected else {
            self.view = View::Grid;
            return;
        };
        let hit = self.results[i].clone();
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click());

        let (tex, error) = match self.thumbs.preview(hit.id, &hit.path) {
            Slot::Ready(t) => (Some(t.clone()), None),
            Slot::Failed(e) => (None, Some(e.clone())),
            Slot::Pending => (None, None),
        };
        let loading = tex.is_none() && error.is_none();
        let tex = tex.or_else(|| self.thumbs.loaded_thumb(hit.id).cloned());
        let painter = ui.painter_at(rect);
        if let Some(t) = &tex {
            let image = fit(t.size_vec2(), rect.shrink(16.0));
            painter.image(t.id(), image, full_uv(), Color32::WHITE);
            self.paint_face_boxes(&painter, hit.id, image);
        }
        if loading {
            ui.put(Rect::from_center_size(rect.center(), vec2(32.0, 32.0)), egui::Spinner::new());
        }
        if let Some(e) = error {
            painter.text(rect.center(), Align2::CENTER_CENTER, e, FontId::proportional(16.0), Color32::LIGHT_GRAY);
        }

        let name = Path::new(&hit.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let caption =
            format!("{name}   ·   {} of {}   ·   ← → to browse, F for faces, Esc for grid", i + 1, self.results.len());
        painter.text(
            pos2(rect.center().x, rect.bottom() - 8.0),
            Align2::CENTER_BOTTOM,
            caption,
            FontId::proportional(13.0),
            Color32::from_gray(170),
        );
        if resp.double_clicked() {
            self.view = View::Grid;
            self.scroll_to_selected = true;
        }
    }

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.screenshot else { return };
        s.frames += 1;
        ctx.request_repaint();
        // Ready once a search has answered, even with no results.
        if !s.selected && self.searched_once {
            s.selected = true;
            let view = s.plan.view.clone();
            if let Some(i) = s.plan.select.filter(|_| !self.results.is_empty()) {
                self.select(i.min(self.results.len() - 1));
            }
            match view.as_deref() {
                Some("loupe") => self.view = View::Loupe,
                Some("people" | "person") => {
                    self.tab = Tab::People;
                    self.refresh_people();
                }
                _ => {}
            }
            return;
        }
        if self.screenshot.as_ref().is_some_and(|s| s.plan.view.as_deref() == Some("person"))
            && self.people.person.is_none()
            && let Some(first) = self.people.people.first().map(|p| p.id)
        {
            self.open_person(first);
        }
        let Some(s) = &mut self.screenshot else { return };
        let settled = s.frames > 90 && self.thumbs.queued() == 0 && self.scan.is_none();
        if s.selected && settled && !s.requested {
            s.requested = true;
            let (path, saved) = (s.plan.path.clone(), s.saved.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                let (w, h) = (img.size[0] as u32, img.size[1] as u32);
                if let Err(e) = image::save_buffer(&path, &rgba, w, h, image::ExtendedColorType::Rgba8) {
                    eprintln!("screenshot failed: {e}");
                }
                saved.store(true, Ordering::Relaxed);
            })));
        }
        if s.saved.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for TintypeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_messages();
        self.thumbs.begin_frame(&ctx);
        if let Some(due) = self.search_due {
            let now = Instant::now();
            if now >= due {
                self.search();
            } else {
                ctx.request_repaint_after(due - now);
            }
        }
        if self.notice.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.handle_keys(&ctx);

        let empty_library = self.stats.as_ref().is_some_and(|s| s.photos == 0) && self.scan.is_none();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.tab == Tab::People && !empty_library {
            egui::CentralPanel::default().show(ui, |ui| self.people_view(ui));
            self.thumbs.end_frame();
            self.screenshot_step(&ctx);
            return;
        }
        if !empty_library {
            egui::Panel::left("facets")
                .resizable(true)
                .default_size(270.0)
                .min_size(180.0)
                .show(ui, |ui| self.facet_panel(ui));
            egui::Panel::right("details")
                .resizable(true)
                .default_size(300.0)
                .min_size(220.0)
                .show(ui, |ui| self.details(ui));
        }
        let loupe_frame = egui::Frame::central_panel(ui.style()).fill(Color32::from_gray(18));
        let frame = if self.view == View::Loupe { loupe_frame } else { egui::Frame::central_panel(ui.style()) };
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if empty_library {
                self.welcome(ui);
            } else if self.view == View::Loupe {
                self.loupe(ui);
            } else {
                self.grid(ui);
            }
        });

        self.thumbs.end_frame();
        self.screenshot_step(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // Screenshot runs change settings for the shot; don't keep them.
        if self.screenshot.is_none() {
            eframe::set_value(storage, eframe::APP_KEY, &self.settings);
        }
    }
}

fn sort_label(s: Sort) -> &'static str {
    match s {
        Sort::Newest => "Newest first",
        Sort::Oldest => "Oldest first",
        Sort::Relevance => "Best match",
    }
}

fn facet_title(field: &str) -> &'static str {
    FACETS.iter().find(|(_, f)| *f == field).map_or("Filter", |(t, _)| t)
}

/// How a facet value reads in the sidebar, relative to `parent`.
fn facet_label(field: &str, parent: &str, path: &str) -> String {
    let rel = path.strip_prefix(parent).unwrap_or(path).trim_start_matches('/');
    if field == "date" && parent.trim_start_matches('/').len() == 4 {
        // A month under a year.
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        if let Some(name) = rel.parse::<usize>().ok().and_then(|m| MONTHS.get(m.wrapping_sub(1))) {
            return (*name).to_string();
        }
    }
    if field == "ext" { rel.to_uppercase() } else { rel.to_string() }
}

fn camera_name(make: Option<&str>, model: Option<&str>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model.into()),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (a, b) => a.or(b).map(str::to_string),
    }
}

fn shutter(s: f64) -> String {
    if s > 0.0 && s < 1.0 { format!("1/{:.0} s", 1.0 / s) } else { format!("{s} s") }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The largest rect with `size`'s aspect ratio that fits in `bounds`, centered.
fn fit(size: Vec2, bounds: Rect) -> Rect {
    let scale = (bounds.width() / size.x).min(bounds.height() / size.y);
    Rect::from_center_size(bounds.center(), size * scale)
}

/// One clickable facet value: label on the left (clipped, full text on hover),
/// count on the right.
fn facet_row(ui: &mut egui::Ui, label: &str, count: u64) -> egui::Response {
    let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let visuals = ui.visuals();
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let font = FontId::proportional(ui.text_style_height(&egui::TextStyle::Body));
    let count_rect = ui.painter().text(
        rect.right_center() - vec2(4.0, 0.0),
        Align2::RIGHT_CENTER,
        thousands(count),
        font.clone(),
        visuals.weak_text_color(),
    );
    let label_clip = Rect::from_min_max(rect.min, pos2(count_rect.left() - 8.0, rect.max.y));
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, visuals.text_color());
    let clipped = galley.size().x > label_clip.width() - 4.0;
    ui.painter().with_clip_rect(label_clip).galley(
        rect.left_center() + vec2(4.0, -galley.size().y / 2.0),
        galley,
        visuals.text_color(),
    );
    if clipped { resp.on_hover_text(label) } else { resp }
}

/// A five-pointed star. Filled as a pentagon plus five point triangles,
/// since egui fills only convex shapes.
fn paint_star(painter: &egui::Painter, center: egui::Pos2, radius: f32, filled: bool, color: Color32) {
    let point = |i: usize, r: f32| {
        let angle = std::f32::consts::PI * (i as f32 / 5.0) - std::f32::consts::FRAC_PI_2;
        center + vec2(angle.cos(), angle.sin()) * r
    };
    let inner = radius * 0.45;
    let outline: Vec<_> = (0..10).map(|i| point(i, if i % 2 == 0 { radius } else { inner })).collect();
    if filled {
        let pentagon: Vec<_> = (0..5).map(|i| point(2 * i + 1, inner)).collect();
        painter.add(egui::Shape::convex_polygon(pentagon, color, Stroke::NONE));
        for i in 0..5 {
            let tip = [outline[2 * i], outline[(2 * i + 1) % 10], outline[(2 * i + 9) % 10]];
            painter.add(egui::Shape::convex_polygon(tip.to_vec(), color, Stroke::NONE));
        }
    }
    painter.add(egui::Shape::closed_line(outline, Stroke::new(1.3, color)));
}

fn full_uv() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(4253), "4,253");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(shutter(0.004), "1/250 s");
        assert_eq!(shutter(2.0), "2 s");
    }

    #[test]
    fn facet_labels() {
        assert_eq!(facet_label("date", "/2024", "/2024/05"), "May");
        assert_eq!(facet_label("date", "/", "/2024"), "2024");
        assert_eq!(facet_label("ext", "/", "/raf"), "RAF");
        assert_eq!(facet_label("camera", "/FUJIFILM", "/FUJIFILM/X-T5"), "X-T5");
        assert_eq!(camera_name(Some("Canon"), Some("Canon EOS R5")).as_deref(), Some("Canon EOS R5"));
        assert_eq!(camera_name(Some("FUJIFILM"), Some("X-T5")).as_deref(), Some("FUJIFILM X-T5"));
    }

    #[test]
    fn fit_keeps_aspect() {
        let r = fit(vec2(400.0, 200.0), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)));
        assert_eq!(r.size(), vec2(100.0, 50.0));
        assert_eq!(r.center(), pos2(50.0, 50.0));
        // Upscales small images to fill the view.
        let r = fit(vec2(20.0, 40.0), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)));
        assert_eq!(r.size(), vec2(50.0, 100.0));
    }
}
