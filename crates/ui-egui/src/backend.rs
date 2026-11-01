//! Runs the engine on its own thread so the UI never blocks on a command.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    Settings,
    SyncSidecars,
}

pub struct Reply {
    pub job: Job,
    /// Matches the number returned by [`Backend::send`]; lets the UI drop stale replies.
    pub seq: u64,
    pub result: Result<Response, String>,
}

pub enum Msg {
    Reply(Box<Reply>),
    Progress(Event),
}

pub struct Backend {
    tx: Sender<(Job, u64, Command)>,
    rx: Receiver<Msg>,
    seq: u64,
    cancel: Arc<AtomicBool>,
    /// Raised while `waiting` commands are queued that aren't background work.
    pause: Arc<AtomicBool>,
    waiting: Arc<AtomicUsize>,
    thumbs: Arc<ThumbCache>,
    faces_dir: PathBuf,
}

impl Backend {
    pub fn start(mut engine: Engine, ctx: egui::Context) -> Self {
        let (tx, jobs) = unbounded::<(Job, u64, Command)>();
        let (out, rx) = unbounded();
        let cancel = engine.cancel_flag();
        let pause = engine.pause_flag();
        let waiting = Arc::new(AtomicUsize::new(0));
        let (pause_, waiting_) = (pause.clone(), waiting.clone());
        let thumbs = engine.thumbnails();
        let faces_dir = engine.faces_dir();
        thread::Builder::new()
            .name("tintype-engine".into())
            .spawn(move || {
                let mut queue = VecDeque::new();
                loop {
                    if queue.is_empty() {
                        match jobs.recv() {
                            Ok(next) => queue.push_back(next),
                            Err(_) => break,
                        }
                    }
                    queue.extend(jobs.try_iter());
                    let (job, seq, cmd) = queue.remove(next_job(&queue)).expect("a queued command");
                    let run = std::panic::AssertUnwindSafe(|| {
                        engine.execute(cmd, &mut |e| {
                            let _ = out.send(Msg::Progress(e));
                            ctx.request_repaint();
                        })
                    });
                    // A bug in one command reports an error (see crash.log) and the engine carries on.
                    let result = match std::panic::catch_unwind(run) {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(_) => Err("Tintype hit a bug. Details are in crash.log in the library folder.".into()),
                    };
                    if !is_background(job) && waiting_.fetch_sub(1, Ordering::SeqCst) == 1 {
                        pause_.store(false, Ordering::SeqCst);
                        // One may have been queued just now.
                        if waiting_.load(Ordering::SeqCst) > 0 {
                            pause_.store(true, Ordering::SeqCst);
                        }
                    }
                    if out.send(Msg::Reply(Box::new(Reply { job, seq, result }))).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn engine thread");
        Self { tx, rx, seq: 0, cancel, pause, waiting, thumbs, faces_dir }
    }

    /// Queues a command; returns its sequence number. Anything but more
    /// background work ends a running analysis or face batch early, so a
    /// click doesn't wait for a whole batch.
    pub fn send(&mut self, job: Job, cmd: Command) -> u64 {
        if !is_background(job) {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            self.pause.store(true, Ordering::SeqCst);
        }
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

/// Work done in batches, which other commands may interrupt between photos.
fn is_background(job: Job) -> bool {
    matches!(job, Job::Analyze | Job::FindFaces | Job::Scan)
}

/// Which queued command runs next: the first one that isn't background work,
/// if any. A batch queued ahead of a click would stop at once (it is paused
/// for the click), and as the UI queues the next batch before refreshing what
/// it shows, the batches would never get anywhere.
fn next_job<T>(queue: &VecDeque<(Job, u64, T)>) -> usize {
    queue.iter().position(|(job, ..)| !is_background(*job)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_run_before_queued_batches() {
        let queue: VecDeque<(Job, u64, ())> =
            [(Job::FindFaces, 1, ()), (Job::People, 2, ()), (Job::Search, 3, ())].into();
        assert_eq!(next_job(&queue), 1);
        let queue: VecDeque<(Job, u64, ())> = [(Job::Scan, 1, ()), (Job::Analyze, 2, ())].into();
        assert_eq!(next_job(&queue), 0, "batches keep their order");
    }
}
