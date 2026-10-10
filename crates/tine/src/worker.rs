//! Runs git work off the UI thread.
//!
//! Every request carries a generation number chosen by the UI. Results come
//! back tagged with it, and the UI drops anything older than the latest
//! request of that kind, so a slow stale answer can never overwrite fresher
//! state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use tine_core::{
    CancelToken, RepoReader,
    diff::{DiffOptions, DiffSource},
    log::LogOptions,
    model::{ChangedFile, CommitDetails, CommitSummary, FileDiff, Oid, RefsSnapshot, WorkingStatus},
};

pub type Gen = u64;

/// Result of a write operation.
#[derive(Debug, Default)]
pub struct OpOutcome {
    /// One line for the activity log.
    pub summary: String,
    /// Branches/HEAD may have moved (history needs a check).
    pub refs_changed: bool,
}

type OpFn = Box<dyn FnOnce() -> anyhow::Result<OpOutcome> + Send>;

/// A write operation waiting in a repository's queue.
struct QueuedOp {
    kind: OpKind,
    run: OpFn,
    /// The submitting tab; gone if the tab was closed meanwhile.
    reply: Sender<Msg>,
    ctx: egui::Context,
}

/// Result of an operation whose tab was closed before it finished.
pub struct OrphanedResult {
    /// Repository key (canonical git dir), see [`repo_key`].
    pub repo: PathBuf,
    pub kind: OpKind,
    pub result: anyhow::Result<OpOutcome>,
}

/// One queue (and thread) per repository for the whole process, so writes
/// stay serialized even if a tab is closed and the repository reopened
/// while an operation is still running.
fn queues() -> &'static Mutex<HashMap<PathBuf, Sender<QueuedOp>>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Sender<QueuedOp>>>> = OnceLock::new();
    QUEUES.get_or_init(Default::default)
}

fn orphans() -> &'static Mutex<Vec<OrphanedResult>> {
    static ORPHANS: OnceLock<Mutex<Vec<OrphanedResult>>> = OnceLock::new();
    ORPHANS.get_or_init(Default::default)
}

/// Results of operations whose tab no longer exists (for the app to show).
pub fn take_orphaned_results() -> Vec<OrphanedResult> {
    std::mem::take(&mut *orphans().lock().unwrap_or_else(|e| e.into_inner()))
}

/// Identifies a repository across tabs.
pub fn repo_key(git_dir: &Path) -> PathBuf {
    git_dir.canonicalize().unwrap_or_else(|_| git_dir.to_path_buf())
}

fn queue_for(repo: &Path) -> anyhow::Result<Sender<QueuedOp>> {
    let mut map = queues().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = map.get(repo) {
        return Ok(tx.clone());
    }
    let (tx, rx) = unbounded::<QueuedOp>();
    let key = repo.to_path_buf();
    std::thread::Builder::new().name("tine-ops".into()).spawn(move || {
        for op in rx {
            let result = (op.run)();
            if let Err(crossbeam_channel::SendError(Msg::OpDone(kind, result))) =
                op.reply.send(Msg::OpDone(op.kind, result))
            {
                orphans().lock().unwrap_or_else(|e| e.into_inner()).push(OrphanedResult {
                    repo: key.clone(),
                    kind,
                    result,
                });
            }
            op.ctx.request_repaint();
        }
    })?;
    map.insert(repo.to_path_buf(), tx.clone());
    Ok(tx)
}

pub enum Msg {
    Refs(Gen, anyhow::Result<RefsSnapshot>),
    Status(Gen, anyhow::Result<WorkingStatus>),
    LogBatch(Gen, Vec<CommitSummary>),
    LogDone(Gen, anyhow::Result<()>),
    Details(Gen, anyhow::Result<(CommitDetails, Vec<ChangedFile>)>),
    Diff(Gen, anyhow::Result<FileDiff>),
    /// A maintenance or write job finished; `Ok` carries a log line.
    Job(anyhow::Result<String>),
    /// Files changed on disk (from the watcher).
    Fs(crate::watcher::FsChange),
    /// A queued write operation finished.
    OpDone(OpKind, anyhow::Result<OpOutcome>),
    HeadMessage(Gen, anyhow::Result<String>),
}

/// What a queued write operation was, so the UI can react to its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpKind {
    Stage,
    Unstage,
    Discard,
    Commit { message: String, amend: bool },
}

impl OpKind {
    pub fn label(&self) -> &'static str {
        match self {
            OpKind::Stage => "Stage",
            OpKind::Unstage => "Unstage",
            OpKind::Discard => "Discard",
            OpKind::Commit { amend: false, .. } => "Commit",
            OpKind::Commit { amend: true, .. } => "Amend",
        }
    }
}

pub struct Worker {
    reader: Arc<dyn RepoReader>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    log_cancel: Option<CancelToken>,
    /// Key of the repository's shared write queue.
    repo: PathBuf,
}

impl Worker {
    pub fn new(reader: Arc<dyn RepoReader>, ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let repo = repo_key(&reader.paths().git_dir);
        Self { reader, tx, rx, ctx, log_cancel: None, repo }
    }

    pub fn repo_key(&self) -> &Path {
        &self.repo
    }

    /// Queues a write operation behind any others for this repository; its
    /// result arrives as [`Msg::OpDone`] (or, if this tab is gone by then,
    /// through [`take_orphaned_results`]).
    pub fn op(&self, kind: OpKind, f: impl FnOnce() -> anyhow::Result<OpOutcome> + Send + 'static) {
        let op = QueuedOp { kind: kind.clone(), run: Box::new(f), reply: self.tx.clone(), ctx: self.ctx.clone() };
        let sent = queue_for(&self.repo).and_then(|q| q.send(op).map_err(|_| anyhow::anyhow!("queue closed")));
        if let Err(e) = sent {
            let _ = self.tx.send(Msg::OpDone(kind, Err(anyhow::anyhow!("failed to queue the operation: {e:#}"))));
        }
    }

    pub fn head_message(&self, seq: Gen, git: tine_core::GitRunner) {
        self.spawn("head-message", move |_, tx| {
            let _ = tx.send(Msg::HeadMessage(seq, tine_core::ops::head_message(&git)));
        });
    }

    pub fn reader(&self) -> &Arc<dyn RepoReader> {
        &self.reader
    }

    /// For producers outside the worker (the file watcher).
    pub fn sender(&self) -> Sender<Msg> {
        self.tx.clone()
    }

    /// Results received since the last call.
    pub fn poll(&self) -> impl Iterator<Item = Msg> + '_ {
        self.rx.try_iter()
    }

    fn spawn(&self, name: &str, job: impl FnOnce(&dyn RepoReader, &Sender<Msg>) + Send + 'static) {
        let reader = self.reader.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name(format!("tine-{name}")).spawn(move || {
            job(&*reader, &tx);
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            let _ = self.tx.send(Msg::Job(Err(anyhow::anyhow!("failed to start worker thread: {e}"))));
        }
    }

    pub fn refs(&self, seq: Gen) {
        self.spawn("refs", move |r, tx| {
            let _ = tx.send(Msg::Refs(seq, r.refs()));
        });
    }

    pub fn status(&self, seq: Gen) {
        self.spawn("status", move |r, tx| {
            let _ = tx.send(Msg::Status(seq, r.status()));
        });
    }

    /// Starts streaming history, cancelling any load still in progress.
    pub fn log(&mut self, seq: Gen, opts: LogOptions) {
        if let Some(c) = self.log_cancel.take() {
            c.cancel();
        }
        let cancel = CancelToken::default();
        self.log_cancel = Some(cancel.clone());
        let ctx = self.ctx.clone();
        self.spawn("log", move |r, tx| {
            let result = r.log(&opts, &cancel, &mut |batch| {
                let ok = tx.send(Msg::LogBatch(seq, batch)).is_ok();
                ctx.request_repaint();
                ok
            });
            let _ = tx.send(Msg::LogDone(seq, result));
        });
    }

    pub fn details(&self, seq: Gen, id: Oid) {
        self.spawn("details", move |r, tx| {
            let result = r.commit_details(id).and_then(|d| {
                let parent = d.parents.first().copied();
                Ok((d, r.commit_files(id, parent)?))
            });
            let _ = tx.send(Msg::Details(seq, result));
        });
    }

    pub fn diff(&self, seq: Gen, source: DiffSource, file: ChangedFile, opts: DiffOptions) {
        self.spawn("diff", move |r, tx| {
            let _ = tx.send(Msg::Diff(seq, r.file_diff(source, &file, opts)));
        });
    }

    /// Runs a maintenance or write job; its result arrives as [`Msg::Job`].
    pub fn job(&self, name: &str, f: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        self.spawn(name, move |_, tx| {
            let _ = tx.send(Msg::Job(f()));
        });
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(c) = self.log_cancel.take() {
            c.cancel();
        }
    }
}
