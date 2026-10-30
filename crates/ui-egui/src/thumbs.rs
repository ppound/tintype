//! Loads thumbnails and full-view previews on worker threads and turns them
//! into textures.
//!
//! The newest request is served first (fast scrolling shows what is on screen
//! now, not what was there a second ago), and thumbnails that scrolled out of
//! view before a worker reached them are skipped.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use egui::{ColorImage, TextureHandle, TextureOptions};
use tintype_engine::{Rgba, ThumbCache};

/// Long edge of the full-view preview.
const PREVIEW_EDGE: u32 = 2560;
/// Long edge of face crops as displayed.
const FACE_EDGE: u32 = 160;
/// Textures kept before off-screen ones are dropped.
const MAX_TEXTURES: usize = 1500;
/// Uploads per frame; more would make scrolling stutter.
const UPLOADS_PER_FRAME: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Thumb,
    Preview,
    Face,
}

struct Request {
    id: i64,
    path: PathBuf,
    kind: Kind,
}

enum Outcome {
    Done(Rgba),
    Failed(String),
    Skipped,
}

pub enum Slot {
    Pending,
    Ready(TextureHandle),
    Failed(String),
}

#[derive(Default)]
struct Queue {
    requests: Mutex<Vec<Request>>,
    ready: Condvar,
}

pub struct Thumbs {
    queue: Arc<Queue>,
    /// Ids visible last frame; workers skip thumbnails not in it.
    wanted: Arc<Mutex<HashSet<i64>>>,
    seen_this_frame: HashSet<i64>,
    results: Receiver<(i64, Kind, Outcome)>,
    thumbs: HashMap<i64, Slot>,
    preview: Option<(i64, Slot)>,
    faces: HashMap<i64, Slot>,
}

impl Thumbs {
    pub fn new(cache: Arc<ThumbCache>, ctx: egui::Context) -> Self {
        let queue = Arc::new(Queue::default());
        let wanted = Arc::new(Mutex::new(HashSet::new()));
        let (tx, results) = unbounded();
        let workers = thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(1).max(2));
        for i in 0..workers {
            let (queue, wanted, cache, tx, ctx) =
                (queue.clone(), wanted.clone(), cache.clone(), tx.clone(), ctx.clone());
            thread::Builder::new()
                .name(format!("tintype-thumb-{i}"))
                .spawn(move || worker(&queue, &wanted, &cache, &tx, &ctx))
                .expect("spawn thumbnail worker");
        }
        Self {
            queue,
            wanted,
            seen_this_frame: HashSet::new(),
            results,
            thumbs: HashMap::new(),
            preview: None,
            faces: HashMap::new(),
        }
    }

    /// Call once per frame before drawing: uploads finished images.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        let mut uploaded = 0;
        while uploaded < UPLOADS_PER_FRAME {
            let Ok((id, kind, outcome)) = self.results.try_recv() else { break };
            let slot = match outcome {
                Outcome::Done(img) => {
                    uploaded += 1;
                    let image =
                        ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.pixels);
                    Some(Slot::Ready(ctx.load_texture(format!("{kind:?}{id}"), image, TextureOptions::LINEAR)))
                }
                Outcome::Failed(e) => Some(Slot::Failed(e)),
                Outcome::Skipped => None,
            };
            match (kind, slot) {
                (Kind::Thumb, Some(slot)) => {
                    self.thumbs.insert(id, slot);
                }
                (Kind::Thumb, None) => {
                    self.thumbs.remove(&id);
                }
                (Kind::Preview, Some(slot)) if self.preview.as_ref().is_some_and(|(p, _)| *p == id) => {
                    self.preview = Some((id, slot));
                }
                (Kind::Preview, _) => {}
                (Kind::Face, Some(slot)) => {
                    self.faces.insert(id, slot);
                }
                (Kind::Face, None) => {
                    self.faces.remove(&id);
                }
            }
        }
        if uploaded == UPLOADS_PER_FRAME {
            ctx.request_repaint();
        }
    }

    /// Call once per frame after drawing: records what is on screen.
    pub fn end_frame(&mut self) {
        let seen = std::mem::take(&mut self.seen_this_frame);
        if self.thumbs.len() > MAX_TEXTURES {
            self.thumbs.retain(|id, slot| seen.contains(id) || matches!(slot, Slot::Pending));
        }
        *self.wanted.lock().expect("wanted lock") = seen;
    }

    /// The thumbnail for a visible photo, requesting it if needed.
    pub fn thumb(&mut self, id: i64, path: &str) -> &Slot {
        self.seen_this_frame.insert(id);
        if !self.thumbs.contains_key(&id) {
            // Visible now, so a worker must not skip it before end_frame publishes the set.
            self.wanted.lock().expect("wanted lock").insert(id);
            self.request(Request { id, path: path.into(), kind: Kind::Thumb });
            self.thumbs.insert(id, Slot::Pending);
        }
        &self.thumbs[&id]
    }

    /// The large preview of one photo; only one is kept at a time.
    pub fn preview(&mut self, id: i64, path: &str) -> &Slot {
        if self.preview.as_ref().is_none_or(|(p, _)| *p != id) {
            self.request(Request { id, path: path.into(), kind: Kind::Preview });
            self.preview = Some((id, Slot::Pending));
        }
        &self.preview.as_ref().expect("just set").1
    }

    /// The display crop of a face, requesting it if needed.
    pub fn face(&mut self, id: i64, path: &std::path::Path) -> &Slot {
        if !self.faces.contains_key(&id) {
            self.request(Request { id, path: path.into(), kind: Kind::Face });
            self.faces.insert(id, Slot::Pending);
        }
        &self.faces[&id]
    }

    /// The thumbnail texture if already loaded, without requesting anything.
    pub fn loaded_thumb(&self, id: i64) -> Option<&TextureHandle> {
        match self.thumbs.get(&id) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }

    /// Requests still waiting for a worker.
    pub fn queued(&self) -> usize {
        self.queue.requests.lock().expect("queue lock").len()
    }

    /// Forget everything, e.g. after a rescan changed files.
    pub fn clear(&mut self) {
        self.queue.requests.lock().expect("queue lock").clear();
        self.thumbs.clear();
        self.preview = None;
    }

    fn request(&self, req: Request) {
        self.queue.requests.lock().expect("queue lock").push(req);
        self.queue.ready.notify_one();
    }
}

fn worker(
    queue: &Queue,
    wanted: &Mutex<HashSet<i64>>,
    cache: &ThumbCache,
    tx: &Sender<(i64, Kind, Outcome)>,
    ctx: &egui::Context,
) {
    loop {
        let req = {
            let mut requests = queue.requests.lock().expect("queue lock");
            loop {
                // Previews first (the user is waiting on exactly that one), then newest thumbnails.
                let next = requests.iter().rposition(|r| r.kind == Kind::Preview).or(requests.len().checked_sub(1));
                if let Some(i) = next {
                    break requests.remove(i);
                }
                requests = queue.ready.wait(requests).expect("queue lock");
            }
        };
        let outcome = match req.kind {
            Kind::Thumb if !wanted.lock().expect("wanted lock").contains(&req.id) => Outcome::Skipped,
            Kind::Thumb => to_outcome(cache.get(req.path.as_ref())),
            Kind::Preview => to_outcome(tintype_engine::decode_preview(&req.path, PREVIEW_EDGE)),
            Kind::Face => to_outcome(tintype_engine::decode_preview(&req.path, FACE_EDGE)),
        };
        if tx.send((req.id, req.kind, outcome)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn to_outcome(r: Result<Rgba, tintype_engine::codecs::Error>) -> Outcome {
    match r {
        Ok(img) => Outcome::Done(img),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

impl std::fmt::Debug for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Thumb => "thumb",
            Kind::Preview => "preview",
            Kind::Face => "face",
        })
    }
}
