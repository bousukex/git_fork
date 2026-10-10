//! Runs git work off the UI thread.
//!
//! Every request carries a generation number chosen by the UI. Results come
//! back tagged with it, and the UI drops anything older than the latest
//! request of that kind, so a slow stale answer can never overwrite fresher
//! state.

use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use tine_core::{
    CancelToken, RepoReader,
    diff::{DiffOptions, DiffSource},
    log::LogOptions,
    model::{ChangedFile, CommitDetails, CommitSummary, FileDiff, Oid, RefsSnapshot, WorkingStatus},
};

pub type Gen = u64;

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
}

pub struct Worker {
    reader: Arc<dyn RepoReader>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    log_cancel: Option<CancelToken>,
}

impl Worker {
    pub fn new(reader: Arc<dyn RepoReader>, ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        Self { reader, tx, rx, ctx, log_cancel: None }
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
