//! The People tab, faces in the details panel and full view, and the
//! batch-by-batch face finding loop.

use std::collections::HashMap;

use eframe::egui::{
    self, Align, Align2, Button, Color32, CornerRadius, FontId, Key, Layout, Rect, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, TextEdit, vec2,
};
use tintype_engine::{Command, Event, FaceGroup, FaceInfo, Person, PhotoRef, Response};

use super::{ScanState, Tab, TintypeApp, View, thousands};
use crate::backend::Job;
use crate::thumbs::Slot;

/// Photos per face-finding batch; other commands run between batches.
const FACE_BATCH: usize = 150;
/// Faces shown per unnamed group.
const GROUP_PREVIEW: usize = 10;
/// Unnamed groups shown before "Show more".
const GROUPS_PAGE: usize = 30;
const ACCENT: Color32 = Color32::from_rgb(90, 160, 255);

#[derive(Default)]
pub(super) struct PeopleState {
    pub loaded: bool,
    pub people: Vec<Person>,
    pub groups: Vec<FaceGroup>,
    pub unscanned: u64,
    pub models_ready: bool,
    seq: u64,
    /// The person page being shown.
    pub person: Option<i64>,
    person_faces: Vec<FaceInfo>,
    person_seq: u64,
    rename: String,
    /// Name typed for each group, keyed by the group's first face (group ids change on regrouping).
    group_names: HashMap<i64, String>,
    groups_shown: usize,
    /// Faces of the photo in the details panel.
    photo_faces: Vec<FaceInfo>,
    photo_faces_for: Option<i64>,
    photo_faces_seq: u64,
    /// The face being named in the details panel, and the text so far.
    naming: Option<(i64, String)>,
    finding: Option<FindProgress>,
    pub hide_boxes: bool,
}

struct FindProgress {
    total: u64,
    done: u64,
    faces: u64,
    stop: bool,
}

/// Lays out `n` cells of a fixed width in as many rows as the width needs.
fn wrap_cells(ui: &mut egui::Ui, n: usize, width: f32, gap: f32, mut cell: impl FnMut(&mut egui::Ui, usize)) {
    let cols = ((ui.available_width() + gap) / (width + gap)).floor().max(1.0) as usize;
    for row in 0..n.div_ceil(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for i in row * cols..((row + 1) * cols).min(n) {
                ui.vertical(|ui| {
                    ui.set_width(width);
                    cell(ui, i);
                });
            }
        });
        ui.add_space(gap);
    }
}

/// Things the user clicked, applied after drawing (drawing borrows the state).
enum Action {
    Command(Command),
    OpenPerson(i64),
    OpenPhoto { photo: i64, person: Option<String> },
    ClosePerson,
    ShowPhotosOf(String),
    FindFaces,
}

impl TintypeApp {
    // ---- requests -------------------------------------------------------

    pub(super) fn refresh_people(&mut self) {
        self.people.seq = self.backend.send(Job::People, Command::People);
    }

    pub(super) fn open_person(&mut self, id: i64) {
        self.people.person = Some(id);
        self.people.person_faces.clear();
        self.people.rename = self.people.people.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default();
        self.refresh_person();
    }

    fn refresh_person(&mut self) {
        if let Some(person) = self.people.person {
            self.people.person_seq = self.backend.send(Job::PersonFaces, Command::PersonFaces { person });
        }
    }

    pub(super) fn load_photo_faces(&mut self, photo: i64) {
        if self.people.photo_faces_for != Some(photo) {
            self.people.photo_faces.clear();
            self.people.naming = None;
        }
        self.people.photo_faces_for = Some(photo);
        let cmd = Command::PhotoFaces { photo: PhotoRef::Id(photo) };
        self.people.photo_faces_seq = self.backend.send(Job::PhotoFaces, cmd);
    }

    pub(super) fn start_find_faces(&mut self) {
        if self.people.finding.is_some() || self.scan.is_some() {
            return;
        }
        let total = self.people.unscanned.max(1);
        self.people.finding = Some(FindProgress { total, done: 0, faces: 0, stop: false });
        self.scan = Some(ScanState { label: "Preparing face models…".into(), fraction: None });
        self.backend.send(Job::FindFaces, Command::FindFaces { batch: FACE_BATCH });
    }

    /// Cancel button: stops a scan or the face loop.
    pub(super) fn stop_jobs(&mut self) {
        if let Some(f) = &mut self.people.finding {
            f.stop = true;
        }
        self.backend.cancel();
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Command(cmd) => {
                self.backend.send(Job::PeopleEdit, cmd);
            }
            Action::OpenPerson(id) => self.open_person(id),
            Action::ClosePerson => {
                self.people.person = None;
                self.refresh_people();
            }
            Action::ShowPhotosOf(name) => {
                self.tab = Tab::Photos;
                self.query.clear();
                self.filters = vec![("people", format!("/{name}"))];
                self.search();
            }
            Action::OpenPhoto { photo, person } => {
                self.tab = Tab::Photos;
                self.query.clear();
                self.filters = person.map(|n| vec![("people", format!("/{n}"))]).unwrap_or_default();
                self.pending_open = Some(photo);
                self.search();
            }
            Action::FindFaces => self.start_find_faces(),
        }
    }

    // ---- replies --------------------------------------------------------

    /// Returns false if the event isn't about faces.
    pub(super) fn on_face_progress(&mut self, event: Event) -> bool {
        let Some(scan) = &mut self.scan else { return false };
        match event {
            Event::Download { model, done, total } => {
                scan.label = format!("Downloading {model}…");
                scan.fraction = Some(done as f32 / total.max(1) as f32);
            }
            Event::Faces { done, .. } => {
                if let Some(f) = &self.people.finding {
                    let done = f.done + done;
                    scan.label = format!("Finding faces: {} of {} photos", thousands(done), thousands(f.total));
                    scan.fraction = Some(done as f32 / f.total as f32);
                }
            }
            _ => return false,
        }
        true
    }

    /// Returns false if the reply isn't for a people job.
    pub(super) fn on_people_reply(&mut self, job: Job, seq: u64, result: &Result<Response, String>) -> bool {
        match (job, result) {
            (Job::People, Ok(Response::People { people, groups, unscanned, models_ready })) => {
                if seq == self.people.seq {
                    self.people.loaded = true;
                    self.people.people = people.clone();
                    self.people.groups = groups.clone();
                    self.people.unscanned = *unscanned;
                    self.people.models_ready = *models_ready;
                    if let Some(p) = self.people.person
                        && !people.iter().any(|x| x.id == p)
                    {
                        // Merged into someone else or forgotten.
                        self.people.person = None;
                    }
                }
            }
            (Job::PersonFaces, Ok(Response::FaceList { faces })) => {
                if seq == self.people.person_seq {
                    self.people.person_faces = faces.clone();
                }
            }
            (Job::PhotoFaces, Ok(Response::FaceList { faces })) => {
                if seq == self.people.photo_faces_seq {
                    self.people.photo_faces = faces.clone();
                }
            }
            (Job::PeopleEdit, Ok(_)) => {
                self.refresh_people();
                self.refresh_person();
                if let Some(photo) = self.people.photo_faces_for {
                    self.load_photo_faces(photo);
                }
                self.request_facet("people");
            }
            (Job::FindFaces, Ok(Response::Faces(r))) => {
                let Some(f) = &mut self.people.finding else { return true };
                f.done += r.processed;
                f.faces += r.faces;
                let finished = r.cancelled || f.stop || r.remaining == 0 || r.processed == 0;
                if finished {
                    let (done, faces) = (f.done, f.faces);
                    self.people.finding = None;
                    self.scan = None;
                    let msg = format!("Checked {} photos: {} faces found", thousands(done), thousands(faces));
                    self.notify(if r.remaining > 0 { format!("{msg}. Paused.") } else { msg }, false);
                    self.search();
                } else {
                    self.backend.send(Job::FindFaces, Command::FindFaces { batch: FACE_BATCH });
                }
                // Show groups as they form.
                self.refresh_people();
            }
            (Job::FindFaces, Err(e)) => {
                self.people.finding = None;
                self.scan = None;
                self.notify(format!("Finding faces stopped: {e}"), true);
            }
            (Job::People | Job::PersonFaces | Job::PhotoFaces | Job::PeopleEdit, Err(e)) => {
                self.notify(e.clone(), true)
            }
            _ => return false,
        }
        true
    }

    // ---- drawing --------------------------------------------------------

    /// Draws a face crop as a rounded square. Clickable.
    fn face_image(&mut self, ui: &mut egui::Ui, face: i64, size: f32) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
        let radius = CornerRadius::same((size * 0.12) as u8);
        ui.painter().rect_filled(rect, radius, ui.visuals().extreme_bg_color);
        let path = self.backend.face_crop(face);
        if let Slot::Ready(tex) = self.thumbs.face(face, &path) {
            ui.painter().add(
                egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
                    .with_texture(tex.id(), Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))),
            );
        }
        if resp.hovered() {
            ui.painter().rect_stroke(rect, radius, ui.visuals().widgets.hovered.bg_stroke, StrokeKind::Inside);
        }
        resp
    }

    pub(super) fn people_view(&mut self, ui: &mut egui::Ui) {
        if !self.people.loaded {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        }
        let mut actions = Vec::new();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(8.0);
            if self.people.person.is_some() {
                self.person_page(ui, &mut actions);
            } else if self.people.people.is_empty() && self.people.groups.is_empty() {
                self.people_intro(ui, &mut actions);
            } else {
                self.people_overview(ui, &mut actions);
            }
        });
        for a in actions {
            self.apply(a);
        }
    }

    fn people_intro(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.label(RichText::new("Find the people in your photos").size(26.0).strong());
            ui.add_space(8.0);
            ui.set_max_width(560.0);
            ui.label(
                "Tintype finds faces and groups the ones that look alike. Name a group once, and you can \
                 find every photo of that person.",
            );
            ui.add_space(4.0);
            ui.weak(
                "Everything runs on this computer: no photos or faces are uploaded. The first time, two small \
                 open-source models (39 MB) are downloaded.",
            );
            ui.add_space(16.0);
            if self.people.finding.is_some() {
                ui.spinner();
                ui.weak("Looking for faces. Groups will appear here as they form.");
            } else if ui
                .add_enabled(
                    self.scan.is_none(),
                    Button::new(RichText::new("Find faces").size(16.0)).min_size(vec2(160.0, 36.0)),
                )
                .clicked()
            {
                actions.push(Action::FindFaces);
            }
        });
    }

    fn people_overview(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.people.unscanned > 0 && self.people.finding.is_none() {
            ui.horizontal(|ui| {
                ui.label(format!("{} photos haven't been checked for faces yet.", thousands(self.people.unscanned)));
                if ui.add_enabled(self.scan.is_none(), Button::new("Find faces")).clicked() {
                    actions.push(Action::FindFaces);
                }
            });
            ui.add_space(8.0);
        }

        if !self.people.people.is_empty() {
            ui.label(RichText::new("People").size(18.0).strong());
            ui.add_space(4.0);
            let people = self.people.people.clone();
            wrap_cells(ui, people.len(), 124.0, 16.0, |ui, i| {
                let p = &people[i];
                let resp = match p.cover_face {
                    Some(f) => self.face_image(ui, f, 124.0),
                    None => ui.allocate_exact_size(vec2(124.0, 124.0), Sense::click()).1,
                };
                let name = ui.add(egui::Label::new(RichText::new(&p.name).strong()).truncate().sense(Sense::click()));
                ui.weak(format!("{} photos", thousands(p.photos)));
                if p.suggested > 0 {
                    ui.colored_label(ACCENT, format!("{} to review", thousands(p.suggested)));
                }
                if resp.clicked() || name.clicked() {
                    actions.push(Action::OpenPerson(p.id));
                }
            });
            ui.add_space(20.0);
        }

        if !self.people.groups.is_empty() {
            ui.label(RichText::new("Unnamed groups").size(18.0).strong());
            ui.weak("Faces that look like the same person. Name a group to find them everywhere; click a face that doesn't belong to take it out.");
            ui.add_space(8.0);
            let shown = self.people.groups_shown.max(GROUPS_PAGE);
            let groups: Vec<FaceGroup> = self.people.groups.iter().take(shown).cloned().collect();
            for g in &groups {
                self.group_row(ui, g, actions);
                ui.add_space(6.0);
            }
            if self.people.groups.len() > shown
                && ui.button(format!("Show more ({} groups)", self.people.groups.len() - shown)).clicked()
            {
                self.people.groups_shown = shown + GROUPS_PAGE;
            }
        }
    }

    fn group_row(&mut self, ui: &mut egui::Ui, g: &FaceGroup, actions: &mut Vec<Action>) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                for &f in g.faces.iter().take(GROUP_PREVIEW) {
                    if self.face_image(ui, f, 72.0).on_hover_text("Not the same person? Click to take it out").clicked()
                    {
                        actions.push(Action::Command(Command::UngroupFace { face: f }));
                    }
                }
                if g.size as usize > GROUP_PREVIEW {
                    ui.weak(format!("+{}", thousands(g.size - GROUP_PREVIEW as u64)));
                }
            });
            let key = g.faces[0];
            let people_names: Vec<String> = self.people.people.iter().map(|p| p.name.clone()).collect();
            ui.horizontal(|ui| {
                let text = self.people.group_names.entry(key).or_default();
                let resp = ui.add(TextEdit::singleline(text).hint_text("Who is this?").desired_width(220.0));
                let typed = text.trim().to_string();
                let mut submit = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !typed.is_empty();
                submit |= ui.add_enabled(!typed.is_empty(), Button::new("Name")).clicked();
                let mut name = typed.clone();
                // Offer existing names that match what's typed.
                if !typed.is_empty() {
                    let lower = typed.to_lowercase();
                    for existing in
                        people_names.iter().filter(|n| n.to_lowercase().starts_with(&lower) && **n != typed).take(4)
                    {
                        if ui.small_button(existing).on_hover_text(format!("Add these faces to {existing}")).clicked() {
                            name = existing.clone();
                            submit = true;
                        }
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.weak(format!("{} faces", thousands(g.size)));
                });
                if submit {
                    actions.push(Action::Command(Command::NameFaces { faces: g.faces.clone(), name }));
                    self.people.group_names.remove(&key);
                }
            });
        });
    }

    fn person_page(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(id) = self.people.person else { return };
        let Some(person) = self.people.people.iter().find(|p| p.id == id).cloned() else {
            ui.spinner();
            return;
        };
        ui.horizontal(|ui| {
            if ui.button("← People").clicked() {
                actions.push(Action::ClosePerson);
            }
            ui.add_space(8.0);
            let resp = ui.add(
                TextEdit::singleline(&mut self.people.rename)
                    .font(egui::TextStyle::Heading)
                    .desired_width(280.0)
                    .hint_text("Name"),
            );
            let new_name = self.people.rename.trim().to_string();
            if resp.lost_focus() && !new_name.is_empty() && new_name != person.name {
                actions.push(Action::Command(Command::RenamePerson { person: id, name: new_name }));
            }
            if ui.button("Show photos").clicked() {
                actions.push(Action::ShowPhotosOf(person.name.clone()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Forget this person").on_hover_text("Their faces become unnamed again").clicked() {
                    actions.push(Action::Command(Command::DeletePerson { person: id }));
                    actions.push(Action::ClosePerson);
                }
            });
        });
        ui.weak(format!(
            "{} confirmed faces in {} photos. Renaming to someone else's name merges the two.",
            thousands(person.faces),
            thousands(person.photos)
        ));
        ui.add_space(10.0);

        let faces = self.people.person_faces.clone();
        let suggested: Vec<i64> = faces.iter().filter(|f| !f.confirmed).map(|f| f.id).collect();
        if !suggested.is_empty() {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ACCENT,
                    format!("{} faces might be {}. Check them below, or", suggested.len(), person.name),
                );
                if ui.button("Confirm all").clicked() {
                    actions.push(Action::Command(Command::NameFaces {
                        faces: suggested.clone(),
                        name: person.name.clone(),
                    }));
                }
            });
            ui.add_space(6.0);
        }

        wrap_cells(ui, faces.len(), 104.0, 12.0, |ui, i| {
            let f = &faces[i];
            {
                {
                    let resp = self.face_image(ui, f.id, 104.0);
                    if !f.confirmed {
                        let r = resp.rect;
                        ui.painter().rect_stroke(
                            r,
                            CornerRadius::same(12),
                            Stroke::new(2.0, ACCENT),
                            StrokeKind::Inside,
                        );
                        ui.painter().text(
                            r.right_top() + vec2(-8.0, 6.0),
                            Align2::RIGHT_TOP,
                            "?",
                            FontId::proportional(18.0),
                            ACCENT,
                        );
                    }
                    if resp.on_hover_text("Open photo").clicked() {
                        actions.push(Action::OpenPhoto { photo: f.photo_id, person: Some(person.name.clone()) });
                    }
                    ui.horizontal(|ui| {
                        if !f.confirmed
                            && ui.small_button("Yes").on_hover_text(format!("This is {}", person.name)).clicked()
                        {
                            actions.push(Action::Command(Command::NameFaces {
                                faces: vec![f.id],
                                name: person.name.clone(),
                            }));
                        }
                        let no = if f.confirmed { "Not them" } else { "No" };
                        if ui.small_button(no).on_hover_text(format!("This is not {}", person.name)).clicked() {
                            actions.push(Action::Command(Command::RejectFace { face: f.id }));
                        }
                    });
                }
            }
        });
    }

    /// The "People" section of the details panel.
    pub(super) fn photo_faces_section(&mut self, ui: &mut egui::Ui, photo: i64) {
        if self.people.photo_faces_for != Some(photo) || self.people.photo_faces.is_empty() {
            return;
        }
        let mut actions = Vec::new();
        ui.add_space(10.0);
        ui.label(RichText::new("People").strong());
        let names: Vec<String> = self.people.people.iter().map(|p| p.name.clone()).collect();
        for f in self.people.photo_faces.clone() {
            ui.horizontal(|ui| {
                if self.face_image(ui, f.id, 44.0).on_hover_text("Show this person's page").clicked()
                    && let Some(p) = f.person_id
                {
                    self.tab = Tab::People;
                    actions.push(Action::OpenPerson(p));
                }
                match (&f.person, f.confirmed) {
                    (Some(name), true) => {
                        ui.label(name);
                        if ui.small_button("×").on_hover_text(format!("Not {name}")).clicked() {
                            actions.push(Action::Command(Command::RejectFace { face: f.id }));
                        }
                    }
                    (Some(name), false) => {
                        ui.label(RichText::new(format!("{name}?")).italics());
                        if ui.small_button("Yes").clicked() {
                            actions.push(Action::Command(Command::NameFaces { faces: vec![f.id], name: name.clone() }));
                        }
                        if ui.small_button("No").clicked() {
                            actions.push(Action::Command(Command::RejectFace { face: f.id }));
                        }
                    }
                    (None, _) => match &mut self.people.naming {
                        Some((id, text)) if *id == f.id => {
                            let resp = ui.add(TextEdit::singleline(text).hint_text("Name…").desired_width(140.0));
                            if !resp.has_focus() && !resp.lost_focus() {
                                resp.request_focus();
                            }
                            let typed = text.trim().to_string();
                            let mut chosen =
                                (resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !typed.is_empty())
                                    .then(|| typed.clone());
                            if !typed.is_empty() {
                                let lower = typed.to_lowercase();
                                for n in names
                                    .iter()
                                    .filter(|n| n.to_lowercase().starts_with(&lower) && **n != typed)
                                    .take(2)
                                {
                                    if ui.small_button(n).clicked() {
                                        chosen = Some(n.clone());
                                    }
                                }
                            }
                            if let Some(name) = chosen {
                                actions.push(Action::Command(Command::NameFaces { faces: vec![f.id], name }));
                                self.people.naming = None;
                            } else if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                                self.people.naming = None;
                            }
                        }
                        _ => {
                            ui.weak("Unknown");
                            if ui.small_button("Name…").clicked() {
                                self.people.naming = Some((f.id, String::new()));
                            }
                        }
                    },
                }
            });
        }
        for a in actions {
            self.apply(a);
        }
    }

    /// Outlines faces on the full view, with names.
    pub(super) fn paint_face_boxes(&self, painter: &egui::Painter, photo: i64, image: Rect) {
        if self.people.hide_boxes || self.people.photo_faces_for != Some(photo) {
            return;
        }
        for f in &self.people.photo_faces {
            let [x, y, w, h] = f.bbox;
            let rect = Rect::from_min_size(
                image.min + vec2(x * image.width(), y * image.height()),
                vec2(w * image.width(), h * image.height()),
            );
            painter.rect_stroke(rect, 4.0, Stroke::new(3.0, Color32::from_black_alpha(120)), StrokeKind::Outside);
            painter.rect_stroke(rect, 4.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
            let label = match (&f.person, f.confirmed) {
                (Some(n), true) => n.clone(),
                (Some(n), false) => format!("{n}?"),
                (None, _) => continue,
            };
            let galley = painter.layout_no_wrap(label, FontId::proportional(14.0), Color32::WHITE);
            let pos = rect.left_top() - vec2(0.0, galley.size().y + 6.0);
            let bg = Rect::from_min_size(pos, galley.size() + vec2(8.0, 4.0));
            painter.rect_filled(bg, 3.0, Color32::from_black_alpha(170));
            painter.galley(pos + vec2(4.0, 2.0), galley, Color32::WHITE);
        }
    }

    pub(super) fn people_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if self.tab == Tab::People && self.people.person.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.apply(Action::ClosePerson);
        }
        if self.tab == Tab::Photos && self.view == View::Loupe && ctx.input(|i| i.key_pressed(Key::F)) {
            self.people.hide_boxes = !self.people.hide_boxes;
        }
    }
}
