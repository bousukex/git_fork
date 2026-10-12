//! A repository reader whose calls wait for the test, so a test decides in
//! which order background results arrive (and can hold one back forever).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use bstr::BString;
use crossbeam_channel::{Sender, bounded};
use tine::repo::RepoTab;
use tine_core::{
    CancelToken, CliReader, RepoPaths, RepoReader,
    blame::Blame,
    diff::{DiffOptions, DiffSource},
    file_history::FileHistoryEntry,
    log::LogOptions,
    model::{ChangedFile, CommitDetails, CommitSummary, FileDiff, Oid, RefsSnapshot, WorkingStatus},
    search::{SearchEnd, SearchKind},
};

/// Holds each call until the test releases it.
#[derive(Default)]
pub struct Gate {
    on: AtomicBool,
    calls: Mutex<Vec<Option<Sender<()>>>>,
    /// What each held call was for (in arrival order).
    labels: Mutex<Vec<String>>,
    /// Every call, held or not.
    total: AtomicUsize,
}

impl Gate {
    pub fn closed() -> Self {
        Gate { on: AtomicBool::new(true), ..Default::default() }
    }

    fn wait(&self) {
        self.wait_as(String::new());
    }

    /// Like `wait`, telling what the call is for (see [`Self::find`]).
    fn wait_as(&self, label: String) {
        self.total.fetch_add(1, Ordering::Relaxed);
        // Checked and queued under the lock `open` takes: a call can't slip
        // in between its check and `open` releasing the queue.
        let rx = {
            let mut calls = self.calls.lock().unwrap();
            if !self.on.load(Ordering::Relaxed) {
                return;
            }
            let (tx, rx) = bounded(1);
            calls.push(Some(tx));
            self.labels.lock().unwrap().push(label);
            rx
        };
        // An error means the gate is gone (the test ended): go on.
        let _ = rx.recv();
    }

    /// Calls that have arrived so far.
    pub fn arrived(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The first held call whose label matches, by its index.
    pub fn find(&self, matches: impl Fn(&str) -> bool) -> Option<usize> {
        let _calls = self.calls.lock().unwrap();
        self.labels.lock().unwrap().iter().position(|l| matches(l))
    }

    /// Calls still held (not released).
    pub fn held(&self) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| c.is_some()).count()
    }

    /// Calls so far, including those an open gate let through.
    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Lets call `i` (in arrival order) through.
    pub fn release(&self, i: usize) {
        if let Some(tx) = self.calls.lock().unwrap()[i].take() {
            let _ = tx.send(());
        }
    }

    /// Lets the oldest waiting call through.
    pub fn release_next(&self) {
        let mut calls = self.calls.lock().unwrap();
        if let Some(tx) = calls.iter_mut().find_map(Option::take) {
            let _ = tx.send(());
        }
    }

    /// Lets every call through from now on.
    pub fn open(&self) {
        let mut calls = self.calls.lock().unwrap();
        self.on.store(false, Ordering::Relaxed);
        for tx in calls.iter_mut().filter_map(Option::take) {
            let _ = tx.send(());
        }
    }
}

/// Which calls wait (an open gate lets everything through).
#[derive(Default)]
pub struct Gates {
    pub log: Gate,
    pub search: Gate,
    pub file_history: Gate,
    pub blame: Gate,
    pub files: Gate,
    pub diff: Gate,
    pub rebase_range: Gate,
    pub merge_open: Gate,
    pub worktrees: Gate,
    /// Calls that ended stopped (git never ran, or was stopped).
    pub stopped_diffs: AtomicUsize,
    pub stopped_blames: AtomicUsize,
    pub stopped_merges: AtomicUsize,
}

fn count_stop<T>(r: anyhow::Result<T>, counter: &AtomicUsize) -> anyhow::Result<T> {
    if r.as_ref().err().is_some_and(|e| e.is::<tine_core::Cancelled>()) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    r
}

pub struct Gated {
    pub inner: CliReader,
    pub gates: Arc<Gates>,
}

impl RepoReader for Gated {
    fn paths(&self) -> &RepoPaths {
        self.inner.paths()
    }
    fn refs(&self) -> anyhow::Result<RefsSnapshot> {
        self.inner.refs()
    }
    fn status(&self) -> anyhow::Result<WorkingStatus> {
        self.inner.status()
    }
    fn log(&self, opts: &LogOptions, cancel: &CancelToken, on_batch: &mut dyn FnMut(Vec<CommitSummary>) -> bool) -> anyhow::Result<()> {
        self.gates.log.wait();
        self.inner.log(opts, cancel, on_batch)
    }
    fn commit_details(&self, id: Oid) -> anyhow::Result<CommitDetails> {
        self.inner.commit_details(id)
    }
    fn commit_files(&self, id: Oid, parent: Option<Oid>) -> anyhow::Result<Vec<ChangedFile>> {
        self.inner.commit_files(id, parent)
    }
    fn file_diff(&self, source: DiffSource, file: &ChangedFile, opts: DiffOptions, cancel: &CancelToken) -> anyhow::Result<FileDiff> {
        self.gates.diff.wait_as(format!("{source:?} {} {opts:?}", file.path));
        count_stop(self.inner.file_diff(source, file, opts, cancel), &self.gates.stopped_diffs)
    }
    fn search(
        &self,
        kind: SearchKind,
        query: &str,
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<Oid>) -> bool,
    ) -> anyhow::Result<SearchEnd> {
        self.gates.search.wait();
        self.inner.search(kind, query, cancel, on_batch)
    }
    fn file_history(
        &self,
        start: Oid,
        path: &[u8],
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<FileHistoryEntry>) -> bool,
    ) -> anyhow::Result<SearchEnd> {
        self.gates.file_history.wait();
        self.inner.file_history(start, path, cancel, on_batch)
    }
    fn blame(&self, path: &[u8], rev: Option<Oid>, ignore_whitespace: bool, cancel: &CancelToken) -> anyhow::Result<Blame> {
        self.gates.blame.wait();
        count_stop(self.inner.blame(path, rev, ignore_whitespace, cancel), &self.gates.stopped_blames)
    }
    fn tracked_files(&self, cancel: &CancelToken) -> anyhow::Result<Vec<BString>> {
        self.gates.files.wait();
        self.inner.tracked_files(cancel)
    }
    fn paths_at(&self, rev: Oid, cancel: &CancelToken) -> anyhow::Result<Vec<BString>> {
        self.inner.paths_at(rev, cancel)
    }
    fn rebase_range(&self, base: Option<Oid>, cancel: &CancelToken) -> anyhow::Result<tine_core::interactive::Range> {
        self.gates.rebase_range.wait();
        self.inner.rebase_range(base, cancel)
    }
    fn worktrees(&self) -> anyhow::Result<Vec<tine_core::worktree::Worktree>> {
        self.gates.worktrees.wait();
        self.inner.worktrees()
    }
    fn merge_open(&self, path: &[u8], cancel: &CancelToken) -> anyhow::Result<tine_core::merge_file::Opened> {
        self.gates.merge_open.wait();
        count_stop(self.inner.merge_open(path, cancel), &self.gates.stopped_merges)
    }
}

/// A tab reading through [`Gated`]; returns once refs and status are in
/// (and the history, unless its gate is closed).
pub fn gated_tab(dir: &std::path::Path, gates: Gates) -> (RepoTab, Arc<Gates>) {
    let inner = CliReader::open(dir).unwrap();
    let git = inner.runner().clone();
    let gates = Arc::new(gates);
    let log_gated = gates.log.on.load(Ordering::Relaxed);
    let reader = Gated { inner, gates: gates.clone() };
    let mut tab = RepoTab::with_reader(Arc::new(reader), git, &egui::Context::default());
    assert!(super::poll_until(&mut tab, |t| t.status_loaded && t.refs.head.is_some() && (log_gated || !t.history.loading)));
    (tab, gates)
}
