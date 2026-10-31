//! Runs the engine on its own thread so the UI never blocks on a command.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use tintype_engine::{Command, Engine, Event, Response, ThumbCache};

/// What a request was for, so its reply can be routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Search,
    Facets(&'static str),
    Show,
    Edit,
    Scan,
    Stats,
    People,
    PhotoFaces,
    PersonFaces,
    FindFaces,
    PeopleEdit,
    Analyze,
}

pub struct Reply {
    pub job: Job,
    /// Matches the number returned by [`Backend::send`]; lets the UI drop stale replies.
    pub seq: u64,
    pub result: Result<Response, String>,
}

pub enum Msg {
    Reply(Reply),
    Progress(Event),
}

pub struct Backend {
    tx: Sender<(Job, u64, Command)>,
    rx: Receiver<Msg>,
    seq: u64,
    cancel: Arc<AtomicBool>,
    thumbs: Arc<ThumbCache>,
    faces_dir: PathBuf,
}

impl Backend {
    pub fn start(mut engine: Engine, ctx: egui::Context) -> Self {
        let (tx, jobs) = unbounded::<(Job, u64, Command)>();
        let (out, rx) = unbounded();
        let cancel = engine.cancel_flag();
        let thumbs = engine.thumbnails();
        let faces_dir = engine.faces_dir();
        thread::Builder::new()
            .name("tintype-engine".into())
            .spawn(move || {
                for (job, seq, cmd) in jobs {
                    let result = engine.execute(cmd, &mut |e| {
                        let _ = out.send(Msg::Progress(e));
                        ctx.request_repaint();
                    });
                    let result = result.map_err(|e| e.to_string());
                    if out.send(Msg::Reply(Reply { job, seq, result })).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn engine thread");
        Self { tx, rx, seq: 0, cancel, thumbs, faces_dir }
    }

    /// Queues a command; returns its sequence number.
    pub fn send(&mut self, job: Job, cmd: Command) -> u64 {
        self.seq += 1;
        // The engine thread only exits when this sender is dropped.
        let _ = self.tx.send((job, self.seq, cmd));
        self.seq
    }

    pub fn poll(&self) -> impl Iterator<Item = Msg> + '_ {
        self.rx.try_iter()
    }

    /// Asks the running command (a scan or a batch) to stop early.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn thumbs(&self) -> Arc<ThumbCache> {
        self.thumbs.clone()
    }

    pub fn face_crop(&self, face: i64) -> PathBuf {
        tintype_engine::face_crop_path(&self.faces_dir, face)
    }
}
