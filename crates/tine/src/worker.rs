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
    model::{ChangedFile, CommitDetails, CommitSummary, Oid, RefsSnapshot, WorkingStatus},
};

use crate::{blame_model::BlameDoc, diff_model::DiffDoc, settings::DocOptions};

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
    StashConflict(crate::repo::StashConflict),
    /// A stash conflict was finished or undone.
    ClearStashConflict,
    /// The auto-stashed changes were applied.
    ClearPendingAutostash(tine_core::model::Oid),
    /// The auto-stashed changes are back to waiting in their stash (their
    /// apply was undone).
    PendingAutostash(tine_core::model::Oid),
    /// Resolved conflicts of unknown origin were finished.
    ClearGenericConflict,
    /// An interactive rebase stopped at an `edit` (show the commit box).
    EditStop,
    /// An interactive rebase started by Tine has ended.
    RebaseEnded(Box<tine_core::interactive::Ended>),
    /// Telling whether it ended failed (the session is kept).
    RebaseEndFailed(String),
    /// The finished interactive rebase was undone.
    RebaseUndone,
    /// The merge editor's result was written.
    MergeSaved(Box<MergeSaved>),
    /// A worktree was added (`open`: as a tab); `warning`: git failed
    /// after making it (a post-checkout hook, say).
    WorktreeAdded { path: std::path::PathBuf, open: bool, warning: Option<String> },
    /// The worktrees changed: list them again.
    Worktrees,
}

/// A merge editor save that went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSaved {
    /// Which opening of the editor it was saved from.
    pub editor: Gen,
    pub path: bstr::BString,
    /// The editor's change counter when it was saved.
    pub edit_gen: u64,
    /// The file as written.
    pub work: tine_core::merge_file::WorkFile,
    /// With "mark resolved": whether that worked too.
    pub staged: Option<Result<(), String>>,
}

/// A conflicted file opened for the merge editor (plain text to show).
pub struct MergeOpened {
    pub opened: tine_core::merge_file::Opened,
    pub text: Arc<crate::merge_model::MergeText>,
    /// What `text` was made with.
    pub doc: DocOptions,
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
    ResetLossesChanged { target: tine_core::model::Oid, lost: Vec<String>, guard: Option<crate::repo::dialog::ResetGuard> },
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
    /// A diff, prepared for display (plain; colors may follow).
    Diff(Gen, anyhow::Result<Arc<DiffDoc>>),
    /// The same diff with syntax highlighting.
    DiffHighlighted(Gen, Arc<DiffDoc>),
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
    /// A blame, prepared for display (plain; colors may follow).
    Blame(Gen, anyhow::Result<Arc<BlameDoc>>),
    /// The same blame with syntax highlighting.
    BlameHighlighted(Gen, Arc<BlameDoc>),
    Files(Gen, anyhow::Result<Arc<FileIndex>>),
    /// The commits an interactive rebase from `base` would take.
    RebaseRange(Gen, anyhow::Result<tine_core::interactive::Range>),
    /// The paths in a commit (HEAD), for "is this file committed?".
    HeadPaths(Gen, Oid, anyhow::Result<Arc<std::collections::HashSet<bstr::BString>>>),
    /// A conflicted file opened for the merge editor (plain; colors may
    /// follow).
    MergeOpened(Gen, anyhow::Result<Box<MergeOpened>>),
    /// Syntax colors for the merge editor's text `.1`.
    MergeHighlighted(Gen, Arc<crate::merge_model::MergeText>, Arc<crate::merge_model::MergeSpans>),
    Worktrees(Gen, anyhow::Result<Vec<tine_core::worktree::Worktree>>),
    /// What removing worktree `.1` would lose.
    WorktreeLosses(Gen, std::path::PathBuf, anyhow::Result<tine_core::worktree::Losses>),
    /// What pruning would remove.
    PrunePreview(Gen, anyhow::Result<Vec<String>>),
    /// Whether the repository uses Git LFS (and git-lfs is installed, when
    /// asked).
    LfsState(Gen, anyhow::Result<bool>, Option<bool>),
    /// Which files of a list go through Git LFS.
    LfsTracked(Gen, crate::repo::lfs::LfsTarget, anyhow::Result<std::collections::HashSet<bstr::BString>>),
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
    InteractiveRebase,
    /// Noticing that an interactive rebase ended (also outside Tine).
    FinishRebase,
    /// Saving the merge editor's result (and marking it resolved);
    /// `editor`: which opening of the editor it is for.
    SaveMerge { resolved: bool, editor: Gen },
    AddWorktree(String),
    RemoveWorktree(std::path::PathBuf),
    LockWorktree(String),
    UnlockWorktree(String),
    PruneWorktrees,
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
            OpKind::InteractiveRebase => "Interactive rebase",
            OpKind::FinishRebase => "Finish interactive rebase",
            OpKind::SaveMerge { resolved: false, .. } => "Save",
            OpKind::SaveMerge { resolved: true, .. } => "Save and mark resolved",
            OpKind::AddWorktree(_) => "Add worktree",
            OpKind::RemoveWorktree(_) => "Remove worktree",
            OpKind::LockWorktree(_) => "Lock worktree",
            OpKind::UnlockWorktree(_) => "Unlock worktree",
            OpKind::PruneWorktrees => "Prune worktrees",
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
    /// No background reads start (the tab sleeps while its worktree is
    /// removed).
    paused: bool,
    /// Key of the repository's shared write queue.
    repo: PathBuf,
    running: Arc<RunningJobs>,
}

/// Background jobs still running, by name (tests check that stopped work
/// really ends, not just that its token was set).
#[derive(Debug, Default)]
pub struct RunningJobs(std::sync::Mutex<std::collections::HashMap<String, usize>>);

impl RunningJobs {
    pub fn count(&self, name: &str) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(name).copied().unwrap_or(0)
    }

    fn change(&self, name: &str, up: bool) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(name.to_owned()).or_default();
        *n = if up { *n + 1 } else { n.saturating_sub(1) };
    }
}

/// Counts a job as running until dropped (also if the job panics).
struct Running(Arc<RunningJobs>, String);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.change(&self.1, false);
    }
}

impl Worker {
    pub fn new(reader: Arc<dyn RepoReader>, ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        // Linked worktrees share refs, so they share one write queue.
        let repo = repo_key(&reader.paths().common_dir);
        Self { reader, tx, rx, ctx, log_cancel: None, search_cancel: None, fail_spawns: false, paused: false, repo, running: Arc::default() }
    }

    /// The jobs of this worker still running (they outlive it).
    #[doc(hidden)]
    pub fn running_jobs(&self) -> Arc<RunningJobs> {
        self.running.clone()
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

    /// Stops the history and search reads, and starts no others until
    /// [`Self::resume`] (write operations still queue).
    pub fn pause(&mut self) {
        self.paused = true;
        for c in [self.log_cancel.take(), self.search_cancel.take()].into_iter().flatten() {
            c.cancel();
        }
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Delivers `msg` as if a job had sent it.
    #[doc(hidden)]
    pub fn send_for_test(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// Runs `job` on its own thread; reports (and returns) a failure to
    /// start it.
    fn spawn(&self, name: &str, job: impl FnOnce(&dyn RepoReader, &Sender<Msg>) + Send + 'static) -> Result<(), String> {
        if self.paused {
            return Err("paused".into());
        }
        let reader = self.reader.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.running.change(name, true);
        let running = Running(self.running.clone(), name.to_owned());
        let spawned = if self.fail_spawns {
            Err(std::io::Error::other("refused by the test"))
        } else {
            std::thread::Builder::new().name(format!("tine-{name}")).spawn(move || {
                let _running = running;
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
    /// for display: plain first, then with colors.
    pub fn blame(
        &self,
        seq: Gen,
        path: bstr::BString,
        rev: Option<Oid>,
        ignore_whitespace: bool,
        opts: DocOptions,
        cancel: CancelToken,
    ) {
        let ctx = self.ctx.clone();
        let started = self.spawn("blame", move |r, tx| {
            let result = r.blame(&path, rev, ignore_whitespace, &cancel).and_then(|b| prepared(BlameDoc::build(b, opts, &cancel)));
            send_blame(tx, &ctx, seq, result, &path, opts, &cancel);
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::Blame(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Prepares a blame already loaded again (the display options changed),
    /// without running git.
    pub fn rebuild_blame(&self, seq: Gen, doc: Arc<BlameDoc>, path: bstr::BString, opts: DocOptions, cancel: CancelToken) {
        let ctx = self.ctx.clone();
        let started = self.spawn("blame", move |_, tx| {
            let result = prepared(BlameDoc::build(doc.blame.clone(), opts, &cancel));
            send_blame(tx, &ctx, seq, result, &path, opts, &cancel);
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

    /// Reads what an interactive rebase from `base` would take.
    pub fn rebase_range(&self, seq: Gen, base: Option<Oid>, cancel: CancelToken) {
        let started = self.spawn("rebase-range", move |r, tx| {
            let result = r.rebase_range(base, &cancel);
            if !cancel.is_cancelled() {
                let _ = tx.send(Msg::RebaseRange(seq, result));
            }
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::RebaseRange(seq, Err(anyhow::anyhow!(e))));
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

    /// Loads a file's diff and prepares it for display: plain first, then
    /// with colors.
    pub fn diff(&self, seq: Gen, source: DiffSource, file: ChangedFile, opts: DiffOptions, doc: DocOptions, cancel: CancelToken) {
        let ctx = self.ctx.clone();
        let started = self.spawn("diff", move |r, tx| {
            let result = r.file_diff(source, &file, opts, &cancel).and_then(|d| prepared(DiffDoc::build(d, doc, &cancel)));
            // Stopped (the pane was replaced or closed): nobody waits for it.
            if cancel.is_cancelled() {
                return;
            }
            let plain = result.as_ref().ok().filter(|_| doc.highlight).cloned();
            let _ = tx.send(Msg::Diff(seq, result));
            if let Some(plain) = plain {
                ctx.request_repaint();
                if let Some(colored) = plain.highlighted(&file.path, &cancel)
                    && !cancel.is_cancelled()
                {
                    let _ = tx.send(Msg::DiffHighlighted(seq, Arc::new(colored)));
                }
            }
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::Diff(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Opens a conflicted file for the merge editor: its plain text first,
    /// syntax colors after (if wanted).
    pub fn merge_open(&self, seq: Gen, path: bstr::BString, doc: DocOptions, cancel: CancelToken) {
        let ctx = self.ctx.clone();
        let started = self.spawn("merge-open", move |r, tx| {
            let result = r.merge_open(&path, &cancel).map(|opened| {
                let text = Arc::new(crate::merge_model::MergeText::new(&opened.merge, doc.tab_width));
                Box::new(MergeOpened { opened, text, doc })
            });
            if cancel.is_cancelled() {
                return;
            }
            let text = result.as_ref().ok().filter(|_| doc.highlight).map(|o| o.text.clone());
            let _ = tx.send(Msg::MergeOpened(seq, result));
            if let Some(text) = text {
                ctx.request_repaint();
                send_merge_colors(tx, seq, text, &path, &cancel);
            }
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::MergeOpened(seq, Err(anyhow::anyhow!(e))));
        }
    }

    pub fn worktrees(&self, seq: Gen) {
        let started = self.spawn("worktrees", move |r, tx| {
            let _ = tx.send(Msg::Worktrees(seq, r.worktrees()));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::Worktrees(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Looks at what removing `w` would lose (run where `git` is).
    pub fn worktree_losses(&self, seq: Gen, git: tine_core::GitRunner, w: tine_core::worktree::Worktree) {
        let path = w.path.clone();
        let started = self.spawn("worktree-losses", move |_, tx| {
            let _ = tx.send(Msg::WorktreeLosses(seq, w.path.clone(), tine_core::worktree::losses(&git, &w)));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::WorktreeLosses(seq, path, Err(anyhow::anyhow!(e))));
        }
    }

    /// Whether the repository uses Git LFS; with `installed`, also whether
    /// git-lfs is there.
    pub fn lfs_state(&self, seq: Gen, git: tine_core::GitRunner, installed: bool) {
        let _ = self.spawn("lfs-state", move |_, tx| {
            let used = tine_core::lfs::repo_uses_lfs(&git);
            let installed = installed.then(|| tine_core::lfs::installed(&git));
            let _ = tx.send(Msg::LfsState(seq, used, installed));
        });
    }

    /// Which rows of a list go through Git LFS (on either side).
    pub fn lfs_tracked(&self, seq: Gen, git: tine_core::GitRunner, target: crate::repo::lfs::LfsTarget, rows: Vec<tine_core::lfs::LfsRow>) {
        let _ = self.spawn("lfs-tracked", move |_, tx| {
            let _ = tx.send(Msg::LfsTracked(seq, target, tine_core::lfs::tracked_rows(&git, &rows)));
        });
    }

    pub fn prune_preview(&self, seq: Gen, git: tine_core::GitRunner) {
        let started = self.spawn("prune-preview", move |_, tx| {
            let _ = tx.send(Msg::PrunePreview(seq, tine_core::worktree::prune_preview(&git)));
        });
        if let Err(e) = started {
            let _ = self.tx.send(Msg::PrunePreview(seq, Err(anyhow::anyhow!(e))));
        }
    }

    /// Syntax colors for the merge editor's text (after it changed).
    pub fn merge_highlight(&self, seq: Gen, text: Arc<crate::merge_model::MergeText>, path: bstr::BString, cancel: CancelToken) {
        let _ = self.spawn("merge-highlight", move |_, tx| send_merge_colors(tx, seq, text, &path, &cancel));
    }

    /// Runs a maintenance or write job; its result arrives as [`Msg::Job`].
    pub fn job(&self, name: &str, f: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
        let _ = self.spawn(name, move |_, tx| {
            let _ = tx.send(Msg::Job(f()));
        });
    }
}

fn send_merge_colors(tx: &Sender<Msg>, seq: Gen, text: Arc<crate::merge_model::MergeText>, path: &[u8], cancel: &CancelToken) {
    if let Some(spans) = text.highlight(path, cancel)
        && !cancel.is_cancelled()
    {
        let _ = tx.send(Msg::MergeHighlighted(seq, text, Arc::new(spans)));
    }
}

/// Sends a prepared blame, then (if wanted) its highlighted version.
fn send_blame(
    tx: &Sender<Msg>,
    ctx: &egui::Context,
    seq: Gen,
    result: anyhow::Result<Arc<BlameDoc>>,
    path: &[u8],
    opts: DocOptions,
    cancel: &CancelToken,
) {
    // Stopped (the view was replaced or closed): nobody waits for it.
    if cancel.is_cancelled() {
        return;
    }
    let plain = result.as_ref().ok().filter(|_| opts.highlight).cloned();
    let _ = tx.send(Msg::Blame(seq, result));
    if let Some(plain) = plain {
        ctx.request_repaint();
        if let Some(colored) = plain.highlighted(path, cancel)
            && !cancel.is_cancelled()
        {
            let _ = tx.send(Msg::BlameHighlighted(seq, Arc::new(colored)));
        }
    }
}

/// A doc prepared on a worker, or why not (`None`: stopped).
fn prepared<T>(doc: Option<T>) -> anyhow::Result<Arc<T>> {
    doc.map(Arc::new).ok_or_else(|| tine_core::Cancelled.into())
}

impl Drop for Worker {
    /// A closed tab stops its background reads.
    fn drop(&mut self) {
        for c in [self.log_cancel.take(), self.search_cancel.take()].into_iter().flatten() {
            c.cancel();
        }
    }
}
