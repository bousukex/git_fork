//! Runs git work off the UI thread.
//!
//! Every request carries a generation number chosen by the UI. Results come
//! back tagged with it, and the UI drops anything older than the latest
//! request of that kind, so a slow stale answer can never overwrite fresher
//! state.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
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
    /// Things the UI must follow up on, in order.
    pub follow_ups: Vec<FollowUp>,
}

impl OpOutcome {
    pub fn new(summary: impl Into<String>) -> Self {
        Self { summary: summary.into(), ..Default::default() }
    }

    pub fn moving_refs(mut self) -> Self {
        self.refs_changed = true;
        self
    }

    pub fn with(mut self, f: FollowUp) -> Self {
        self.follow_ups.push(f);
        self
    }
}

/// Things a successful operation leaves for the UI to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUp {
    /// Stopped with conflicts (the in-progress banner takes over).
    Conflicts,
    /// The current step of this operation would be an empty commit.
    Empty(tine_core::model::RepoState),
    /// Where auto-stashed local changes ended up.
    AutoStash(tine_core::autostash::AutoStash),
    /// Applying a stash conflicted; the stash was kept.
    StashConflict { oid: tine_core::model::Oid, was_clean_before: bool, created_untracked: Vec<bstr::BString> },
    /// A stash conflict was finished or undone.
    ClearStashConflict,
    /// The auto-stashed changes were applied.
    ClearPendingAutostash(tine_core::model::Oid),
    /// Resolved conflicts of unknown origin were finished.
    ClearGenericConflict,
}

/// An operation needs the user's decision before it can go on; the UI
/// shows a dialog instead of an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedsDecision {
    /// The branch has unmerged commits; deleting needs `-D`.
    ForceDeleteBranch(String),
    /// Local changes block switching; offer to stash them first.
    StashAndSwitch(crate::repo::dialog::CheckoutTarget),
    /// Tracked files changed after the user started an integration; ask
    /// whether to stash them first.
    LocalChanges(crate::repo::dialog::Integration),
    /// What a hard reset would destroy changed since it was confirmed.
    ResetLossesChanged { target: tine_core::model::Oid, lost: Vec<String> },
}

impl std::fmt::Display for NeedsDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NeedsDecision::ForceDeleteBranch(b) => write!(f, "'{b}' has unmerged commits"),
            NeedsDecision::StashAndSwitch(t) => write!(f, "local changes prevent switching to '{}'", t.label()),
            NeedsDecision::LocalChanges(a) => write!(f, "local changes prevent {}", a.describe()),
            NeedsDecision::ResetLossesChanged { .. } => write!(f, "the changes a hard reset would discard have changed"),
        }
    }
}

impl std::error::Error for NeedsDecision {}

pub use crate::ops_registry::{OrphanedResult, repo_key, take_orphaned_results};
use crate::ops_registry::{self, OpContext, OpId};

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
    OpDone(OpId, OpKind, anyhow::Result<OpOutcome>),
    HeadMessage(Gen, anyhow::Result<String>),
    SearchBatch(Gen, Vec<Oid>),
    SearchDone(Gen, anyhow::Result<tine_core::search::SearchEnd>),
    FileHistoryBatch(Gen, Vec<tine_core::file_history::FileHistoryEntry>),
    FileHistoryDone(Gen, anyhow::Result<tine_core::search::SearchEnd>),
    Blame(Gen, anyhow::Result<Arc<crate::blame_model::BlameDoc>>),
    Files(Gen, anyhow::Result<Arc<FileIndex>>),
    /// The paths in a commit (HEAD), for "is this file committed?".
    HeadPaths(Gen, Oid, anyhow::Result<Arc<std::collections::HashSet<bstr::BString>>>),
}

/// The repository's tracked files, prepared for Quick Launch matching.
pub struct FileIndex {
    pub paths: Vec<bstr::BString>,
    pub prepared: Vec<tine_core::fuzzy::Prepared>,
}

/// What a queued write operation was, so the UI can react to its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpKind {
    Stage,
    Unstage,
    Discard,
    Commit { message: String, amend: bool },
    /// Hunk/line level stage, unstage or discard.
    Patch(tine_core::patch::PatchMode),
    Checkout(String),
    CreateBranch(String),
    RenameBranch(String),
    DeleteBranch(String),
    Tag(String),
    DeleteTag(String),
    Merge(String),
    Rebase(String),
    CherryPick(String),
    Revert(String),
    Reset(String),
    Stash,
    StashApply,
    StashPop,
    StashDrop,
    Continue,
    Abort,
    Skip,
    Resolve,
    MergeTool,
    FinishStashConflict,
    UndoStashApply,
    /// Unstaging files whose conflicts were resolved.
    UnstageResolved,
    /// State handed over from a closed tab (not a git operation).
    Recovered,
    Fetch(String),
    Pull(String),
    Push(String),
    DeleteRemoteRef(String),
}

impl OpKind {
    pub fn label(&self) -> &'static str {
        use tine_core::patch::PatchMode;
        match self {
            OpKind::Stage => "Stage",
            OpKind::Unstage => "Unstage",
            OpKind::Discard => "Discard",
            OpKind::Commit { amend: false, .. } => "Commit",
            OpKind::Commit { amend: true, .. } => "Amend",
            OpKind::Patch(PatchMode::Stage) => "Stage lines",
            OpKind::Patch(PatchMode::Unstage) => "Unstage lines",
            OpKind::Patch(PatchMode::Discard) => "Discard lines",
            OpKind::Checkout(_) => "Checkout",
            OpKind::CreateBranch(_) => "Create branch",
            OpKind::RenameBranch(_) => "Rename branch",
            OpKind::DeleteBranch(_) => "Delete branch",
            OpKind::Tag(_) => "Create tag",
            OpKind::DeleteTag(_) => "Delete tag",
            OpKind::Merge(_) => "Merge",
            OpKind::Rebase(_) => "Rebase",
            OpKind::CherryPick(_) => "Cherry-pick",
            OpKind::Revert(_) => "Revert",
            OpKind::Reset(_) => "Reset",
            OpKind::Stash => "Stash",
            OpKind::StashApply => "Apply stash",
            OpKind::StashPop => "Pop stash",
            OpKind::StashDrop => "Drop stash",
            OpKind::Continue => "Continue",
            OpKind::Abort => "Abort",
            OpKind::Skip => "Skip",
            OpKind::Resolve => "Resolve",
            OpKind::MergeTool => "Merge tool",
            OpKind::FinishStashConflict => "Finish stash",
            OpKind::UndoStashApply => "Undo stash apply",
            OpKind::UnstageResolved => "Unstage resolved files",
            OpKind::Recovered => "Recovered state",
            OpKind::Fetch(_) => "Fetch",
            OpKind::Pull(_) => "Pull",
            OpKind::Push(_) => "Push",
            OpKind::DeleteRemoteRef(_) => "Delete on remote",
        }
    }
}

pub struct Worker {
    reader: Arc<dyn RepoReader>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    log_cancel: Option<CancelToken>,
    search_cancel: Option<CancelToken>,
    /// Tests: make starting background threads fail.
    fail_spawns: bool,
    /// Key of the repository's shared write queue.
    repo: PathBuf,
}

impl Worker {
    pub fn new(reader: Arc<dyn RepoReader>, ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        // Linked worktrees share refs, so they share one write queue.
        let repo = repo_key(&reader.paths().common_dir);
        Self { reader, tx, rx, ctx, log_cancel: None, search_cancel: None, fail_spawns: false, repo }
    }

    pub fn repo_key(&self) -> &Path {
        &self.repo
    }

    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// Queues a write operation behind any others for this repository; its
    /// result arrives as [`Msg::OpDone`] (or, if this tab is gone by then,
    /// through [`take_orphaned_results`]).
    pub fn op(&self, kind: OpKind, f: impl FnOnce(&OpContext) -> anyhow::Result<OpOutcome> + Send + 'static) -> Option<OpId> {
        let paths = self.reader.paths().clone();
        match ops_registry::submit(&self.repo, &paths, kind.clone(), self.tx.clone(), self.ctx.clone(), Box::new(f)) {
            Ok(id) => Some(id),
            Err(e) => {
                let _ = self.tx.send(Msg::OpDone(0, kind, Err(anyhow::anyhow!("failed to queue the operation: {e:#}"))));
                None
            }
        }
    }

    pub fn head_message(&self, seq: Gen, git: tine_core::GitRunner) {
        let _ = self.spawn("head-message", move |_, tx| {
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

    /// Makes later background threads fail to start (tests).
    #[doc(hidden)]
    pub fn fail_spawns_for_test(&mut self) {
        self.fail_spawns = true;
    }

    #[doc(hidden)]
    pub fn allow_spawns_for_test(&mut self) {
        self.fail_spawns = false;
    }

    /// Runs `job` on its own thread; reports (and returns) a failure to
    /// start it.
    fn spawn(&self, name: &str, job: impl FnOnce(&dyn RepoReader, &Sender<Msg>) + Send + 'static) -> Result<(), String> {
        let reader = self.reader.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let spawned = if self.fail_spawns {
            Err(std::io::Error::other("refused by the test"))
        } else {
            std::thread::Builder::new().name(format!("tine-{name}")).spawn(move || {
                job(&*reader, &tx);
                ctx.request_repaint();
            })
        };
        spawned.map(drop).map_err(|e| {
            let msg = format!("failed to start worker thread: {e}");
            let _ = self.tx.send(Msg::Job(Err(anyhow::anyhow!(msg.clone()))));
            msg
        })
    }

    pub fn refs(&self, seq: Gen) {
        let _ = self.spawn("refs", move |r, tx| {
            let _ = tx.send(Msg::Refs(seq, r.refs()));
        });
    }

    pub fn status(&self, seq: Gen) {
        let _ = self.spawn("status", move |r, tx| {
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
        let _ = self.spawn("log", move |r, tx| {
            let result = r.log(&opts, &cancel, &mut |batch| {
                let ok = tx.send(Msg::LogBatch(seq, batch)).is_ok();
                ctx.request_repaint();
                ok
            });
            let _ = tx.send(Msg::LogDone(seq, result));
        });
    }

    /// Starts a search, stopping the previous one. `cancel` is also kept by
    /// the caller (closing the search bar stops it).
    pub fn search(&mut self, seq: Gen, kind: tine_core::search::SearchKind, query: String, cancel: CancelToken) {
        if let Some(c) = self.search_cancel.replace(cancel.clone()) {
            c.cancel();
        }
        let ctx = self.ctx.clone();
        let started = self.spawn("search", move |r, tx| {
            let result = r.search(kind, &query, &cancel, &mut |batch| {
                let ok = tx.send(Msg::SearchBatch(seq, batch)).is_ok();
                ctx.request_repaint();
                ok
            });
            let _ = tx.send(Msg::SearchDone(seq, result));
        });
        // The search must still end (as failed), or it would stay "running".
        if let Err(e) = started {
            let _ = self.tx.send(Msg::SearchDone(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Streams the history of `path` from `start`.
    pub fn file_history(&self, seq: Gen, start: Oid, path: bstr::BString, cancel: CancelToken) {
        let ctx = self.ctx.clone();
        let started = self.spawn("file-history", move |r, tx| {
            let result = r.file_history(start, &path, &cancel, &mut |batch| {
                let ok = tx.send(Msg::FileHistoryBatch(seq, batch)).is_ok();
                ctx.request_repaint();
                ok
            });
            let _ = tx.send(Msg::FileHistoryDone(seq, result));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::FileHistoryDone(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Blames `path` at `rev` (the work tree without one) and prepares it
    /// for display.
    pub fn blame(&self, seq: Gen, path: bstr::BString, rev: Option<Oid>, ignore_whitespace: bool, cancel: CancelToken) {
        let started = self.spawn("blame", move |r, tx| {
            let result = r
                .blame(&path, rev, ignore_whitespace, &cancel)
                .map(|b| Arc::new(crate::blame_model::BlameDoc::new(b)));
            let _ = tx.send(Msg::Blame(seq, result));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::Blame(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Lists and prepares the tracked files (for Quick Launch).
    pub fn files(&self, seq: Gen, cancel: CancelToken) {
        let started = self.spawn("files", move |r, tx| {
            let result = r.tracked_files(&cancel).and_then(|paths| {
                let mut prepared = Vec::with_capacity(paths.len());
                for p in &paths {
                    if prepared.len() % 4096 == 0 && cancel.is_cancelled() {
                        return Err(tine_core::Cancelled.into());
                    }
                    prepared.push(tine_core::fuzzy::Prepared::new(&p.to_string()));
                }
                Ok(Arc::new(FileIndex { paths, prepared }))
            });
            let _ = tx.send(Msg::Files(seq, result));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::Files(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// The paths in commit `rev`.
    pub fn head_paths(&self, seq: Gen, rev: Oid, cancel: CancelToken) {
        let started = self.spawn("head-paths", move |r, tx| {
            let result = r.paths_at(rev, &cancel).map(|p| Arc::new(p.into_iter().collect()));
            let _ = tx.send(Msg::HeadPaths(seq, rev, result));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::HeadPaths(seq, rev, Err(anyhow::anyhow!(e))));
        }
    }

    pub fn details(&self, seq: Gen, id: Oid) {
        let _ = self.spawn("details", move |r, tx| {
            let result = r.commit_details(id).and_then(|d| {
                let parent = d.parents.first().copied();
                Ok((d, r.commit_files(id, parent)?))
            });
            let _ = tx.send(Msg::Details(seq, result));
        });
    }

    pub fn diff(&self, seq: Gen, source: DiffSource, file: ChangedFile, opts: DiffOptions) {
        let _ = self.spawn("diff", move |r, tx| {
            let _ = tx.send(Msg::Diff(seq, r.file_diff(source, &file, opts)));
        });
    }

    /// Runs a maintenance or write job; its result arrives as [`Msg::Job`].
    pub fn job(&self, name: &str, f: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        let _ = self.spawn(name, move |_, tx| {
            let _ = tx.send(Msg::Job(f()));
        });
    }
}

impl Drop for Worker {
    /// A closed tab stops its background reads.
    fn drop(&mut self) {
        for c in [self.log_cancel.take(), self.search_cancel.take()].into_iter().flatten() {
            c.cancel();
        }
    }
}
