//! State of one open repository (one tab) and its reaction to worker results.

mod actions;
pub mod browse;
pub mod dialog;
pub mod rebase;
pub mod remote;
pub mod search;

pub use actions::{Needs, StashConflict};

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tine_core::{
    CancelToken, CliReader, GitRunner, RepoPaths, RepoReader,
    diff::{DiffOptions, DiffSource},
    graph::Graph,
    log::LogOptions,
    maintenance,
    model::{ChangedFile, CommitDetails, CommitSummary, Oid, RefsSnapshot, WorkingStatus},
};

use bstr::BString;
use tine_core::{
    model::{ChangeKind, RepoState},
    ops,
    patch::{self, PatchMode},
};

use crate::{
    diff_model::DiffDoc,
    ops_registry::{self, OpContext, OpId},
    settings::DocOptions,
    watcher::{FsChange, RepoWatcher},
    worker::{Gen, Msg, OpKind, OpOutcome, Worker},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    LocalChanges,
    History,
}

#[derive(Default)]
pub struct History {
    pub commits: Vec<CommitSummary>,
    pub graph: Graph,
    pub index: HashMap<Oid, usize>,
    pub loading: bool,
    /// A reload started and its first batch has not arrived yet; the old
    /// history stays visible until then.
    reload_pending: bool,
    /// Bumped whenever the list is replaced (rows mean other commits).
    pub epoch: u64,
    started: Option<Instant>,
    pub elapsed: Option<Duration>,
}

impl History {
    fn begin_reload(&mut self) {
        self.loading = true;
        self.reload_pending = true;
        self.started = Some(Instant::now());
        self.elapsed = None;
    }

    fn take_reload(&mut self) {
        if std::mem::take(&mut self.reload_pending) {
            self.epoch += 1;
            self.commits.clear();
            self.index.clear();
            self.graph = Graph::new();
        }
    }

    fn push_batch(&mut self, batch: Vec<CommitSummary>) {
        self.take_reload();
        for c in &batch {
            self.index.insert(c.id, self.graph.len());
            self.graph.push(c.id, &c.parents);
        }
        self.commits.extend(batch);
    }

    /// Ends a load. A reload that failed before delivering anything keeps
    /// the previous history instead of showing an empty list.
    fn finish(&mut self, ok: bool) {
        if ok {
            self.take_reload();
        } else {
            self.reload_pending = false;
        }
        self.graph.finish();
        self.loading = false;
        self.elapsed = self.started.take().map(|s| s.elapsed());
    }
}

/// What identifies a refs state for deciding whether history must reload.
fn refs_key(r: &RefsSnapshot) -> (Option<tine_core::model::Head>, Vec<(&str, Oid)>) {
    (r.head.clone(), r.refs.iter().map(|r| (r.full_name.as_str(), r.target)).collect())
}

/// Stops a background job when dropped: replacing a pane stops loading
/// (and highlighting) the diff it showed.
#[derive(Default)]
struct StopOnDrop(CancelToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// A file diff being shown (or loaded).
#[derive(Default)]
pub struct DiffPane {
    pub file: Option<ChangedFile>,
    pub source: Option<DiffSource>,
    pub doc: Option<Result<Arc<DiffDoc>, String>>,
    seq: Gen,
    job: StopOnDrop,
    /// Selected rows (indices into `DiffDoc::rows`) for line staging.
    pub selected_rows: std::collections::BTreeSet<usize>,
    /// Row a Shift+click range starts from.
    pub anchor: Option<usize>,
    /// The shown diff may be outdated (an operation finished or a reload is
    /// on its way); line actions wait for the fresh diff.
    pub stale: bool,
}

impl DiffPane {
    pub fn is_loading(&self) -> bool {
        self.file.is_some() && self.doc.is_none()
    }

    /// The token of the diff being loaded (tests check it gets stopped).
    #[doc(hidden)]
    pub fn cancel_token(&self) -> CancelToken {
        self.job.0.clone()
    }

    #[doc(hidden)]
    pub fn seq_for_test(&self) -> Gen {
        self.seq
    }

    /// Takes a highlighted version of the shown diff.
    fn highlighted(&mut self, seq: Gen, doc: Arc<DiffDoc>) {
        if seq == self.seq && seq != 0 && matches!(self.doc, Some(Ok(_))) {
            self.doc = Some(Ok(doc));
        }
    }
}

/// Bottom pane of the history view.
#[derive(Default)]
pub struct CommitPane {
    pub id: Option<Oid>,
    pub details: Option<Result<CommitDetails, String>>,
    pub files: Vec<ChangedFile>,
    pub selected_file: Option<usize>,
    seq: Gen,
}

/// How discard gets rid of untracked files (the trash, or a test stub).
type UntrackedRemover = Arc<dyn Fn(&std::path::Path) -> anyhow::Result<()> + Send + Sync>;

/// Whether hunk/line actions are available for the shown diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineActions {
    pub side: Side,
    /// Why the actions are disabled, if they are.
    pub disabled: Option<&'static str>,
}

/// What a hunk/line action applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTarget {
    /// Every changed line of a hunk.
    Hunk(usize),
    /// All selected lines, in any hunk.
    Selection,
    /// Only the selected lines inside one hunk (what a hunk's button shows).
    SelectedIn(usize),
}

/// Changed lines picked for a partial patch.
struct PatchSelection {
    file: ChangedFile,
    doc: Arc<DiffDoc>,
    lines: Vec<(usize, usize)>,
}

/// Lines waiting for the user to confirm a discard.
pub struct LineDiscard {
    pub file: ChangedFile,
    pub diff: Arc<tine_core::model::FileDiff>,
    pub lines: Vec<(usize, usize)>,
}

/// Which list a local-changes file was picked from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Unstaged,
    Staged,
}

pub struct RepoTab {
    pub paths: RepoPaths,
    pub title: String,
    git: GitRunner,
    worker: Worker,
    gens: Gens,

    pub refs: RefsSnapshot,
    /// Commit id → indices into `refs.refs`, for badges.
    pub refs_by_commit: HashMap<Oid, Vec<usize>>,
    pub status: WorkingStatus,
    pub status_loaded: bool,
    pub history: History,

    pub view: View,
    pub selected_commit: Option<Oid>,
    /// Row to bring into view on the next frame.
    pub scroll_to_row: Option<usize>,
    /// Commit to select once it has been loaded.
    pending_select: Option<Oid>,
    pub commit: CommitPane,
    pub commit_diff: DiffPane,
    /// The file whose diff is shown (the focus of the selection).
    pub changes_selected: Option<(Side, ChangedFile)>,
    /// All selected paths, on the same side as `changes_selected`.
    pub changes_multi: std::collections::BTreeSet<BString>,
    /// Preferred file to select when the current one disappears (the next
    /// file after staging/unstaging, like Fork).
    changes_next: Option<(Side, BString)>,
    pub changes_diff: DiffPane,
    /// Files waiting for the user to confirm a discard.
    pub pending_discard: Option<Vec<ChangedFile>>,
    pub pending_line_discard: Option<LineDiscard>,
    pub amend: bool,
    /// Messages of successful commits, collected by the app for history.
    pub committed_messages: Vec<String>,
    head_message_seq: Gen,
    /// The new-commit draft set aside while amending.
    amend_draft: Option<(String, String)>,
    untracked_remover: UntrackedRemover,
    merge_msg_loaded: bool,
    pub diff_opts: DiffOptions,
    /// How diffs and blame are prepared (from the app's settings).
    pub doc_opts: DocOptions,

    pub errors: Vec<String>,
    pub activity: Vec<String>,
    pub sidebar_filter: String,
    pub commit_subject: String,
    pub commit_body: String,
    /// Another repository (e.g. a submodule) the user asked to open.
    pub open_request: Option<std::path::PathBuf>,
    _watcher: Option<RepoWatcher>,
    /// While our own git operations run, file events are held back here
    /// and applied once they finish (the operation refreshes anyway).
    held_fs: FsChange,
    /// Registry generation (operations finished on this repository) at
    /// the time of the latest refs / status request, and the generation the
    /// last successful refs / status result reflects. Writes wait until
    /// both are fresh, so nothing acts on pre-operation state — also after
    /// operations from other tabs or worktrees.
    refs_req_regen: u64,
    status_req_regen: u64,
    refs_synced: u64,
    status_synced: u64,
    seen_regen: u64,
    /// Conflicts of unknown origin (e.g. a stash applied on the command
    /// line): the paths, so resolving can be finished from the banner.
    pub generic_conflict: Option<std::collections::BTreeSet<BString>>,
    /// The commit draft set aside while a merge/rebase/… is in progress.
    op_draft: Option<(String, String)>,
    /// Failed operation whose record the error banner links to.
    pub last_failed_op: Option<OpId>,
    pub output_open: bool,
    /// Modal dialog waiting for the user (set with `show_dialog`).
    pub dialog: Option<dialog::Dialog>,
    /// Tells the modals of this tab apart (see [`Self::modal_token`]).
    modal_gen: u64,
    /// A stash application that stopped with conflicts.
    pub stash_conflict: Option<StashConflict>,
    /// Changes stashed before an operation that stopped half-way.
    pub pending_autostash: Option<Oid>,
    /// (current, total) while a rebase is in progress.
    pub rebase_progress: Option<(usize, usize)>,
    /// Why a rebase in progress stopped (without conflicts).
    pub rebase_stop: Option<tine_core::integrate::StopReason>,
    /// The rebase in progress was started by Tine (its messages apply).
    pub rebase_session: bool,
    rebase_finish_tried: Option<String>,
    /// The last interactive rebase finished (it can be undone).
    pub rebase_done: Option<rebase::RebaseDone>,
    rebase_range_seq: Gen,
    rebase_range_cancel: Option<CancelToken>,
    /// The edit stop the commit box was last set up for.
    last_edit_stop: Option<Oid>,
    /// Operation to expand (and scroll to) in the Output panel.
    pub output_focus: Option<OpId>,
    /// Some finished operation may have moved refs.
    ops_moved_refs: bool,
    /// Commit search (Ctrl+F).
    pub search: search::Search,
    /// File History / Blame views over the current view (top = shown).
    pub browse: Vec<browse::Browse>,
    /// The diff shown in File History.
    pub browse_diff: DiffPane,
    /// Tracked files for Quick Launch (loaded when it opens).
    pub files: Option<Arc<crate::worker::FileIndex>>,
    /// The listing in flight (0: none) and its token.
    files_seq: Gen,
    files_cancel: Option<CancelToken>,
    /// The index may have changed since `files` was listed.
    files_stale: bool,
    /// Paths in HEAD (which local files have history), keyed by HEAD.
    pub head_paths: Option<(Oid, Arc<std::collections::HashSet<BString>>)>,
    head_paths_seq: Gen,
    /// The work tree changed during an operation: catch up afterwards.
    work_tree_dirty: bool,
}

#[derive(Default)]
struct Gens {
    refs: Gen,
    /// Refs request after which history reloads only if refs changed.
    refs_then_log: Option<Gen>,
    status: Gen,
    log: Gen,
    next: Gen,
}

impl Gens {
    fn bump(&mut self) -> Gen {
        self.next += 1;
        self.next
    }
}

impl RepoTab {
    pub fn open(path: &Path, ctx: &egui::Context) -> anyhow::Result<Self> {
        Ok(Self::from_reader(CliReader::open(path)?, ctx))
    }

    /// A tab for a repository already discovered (possibly off the UI
    /// thread).
    pub fn from_reader(reader: CliReader, ctx: &egui::Context) -> Self {
        let git = reader.runner().clone();
        Self::with_reader(Arc::new(reader), git, ctx)
    }

    /// A tab reading through any [`RepoReader`] (tests wrap the git one to
    /// control when results arrive); `git` runs the operations.
    pub fn with_reader(reader: Arc<dyn RepoReader>, git: GitRunner, ctx: &egui::Context) -> Self {
        let paths = reader.paths().clone();
        let title = paths
            .workdir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| paths.workdir.display().to_string());
        let worker = Worker::new(reader, ctx.clone());
        let mut tab = RepoTab {
            paths,
            title,
            git,
            worker,
            gens: Gens::default(),
            refs: RefsSnapshot::default(),
            refs_by_commit: HashMap::new(),
            status: WorkingStatus::default(),
            status_loaded: false,
            history: History::default(),
            view: View::History,
            selected_commit: None,
            scroll_to_row: None,
            pending_select: None,
            commit: CommitPane::default(),
            commit_diff: DiffPane::default(),
            changes_selected: None,
            changes_diff: DiffPane::default(),
            diff_opts: DiffOptions::default(),
            doc_opts: DocOptions::default(),
            errors: Vec::new(),
            activity: Vec::new(),
            sidebar_filter: String::new(),
            commit_subject: String::new(),
            commit_body: String::new(),
            open_request: None,
            _watcher: None,
            held_fs: FsChange::default(),
            refs_req_regen: 0,
            status_req_regen: 0,
            refs_synced: 0,
            status_synced: 0,
            seen_regen: 0,
            generic_conflict: None,
            op_draft: None,
            last_failed_op: None,
            output_open: false,
            dialog: None,
            modal_gen: 0,
            stash_conflict: None,
            pending_autostash: None,
            rebase_progress: None,
            rebase_stop: None,
            rebase_session: false,
            rebase_finish_tried: None,
            rebase_done: None,
            rebase_range_seq: 0,
            rebase_range_cancel: None,
            last_edit_stop: None,
            output_focus: None,
            ops_moved_refs: false,
            search: search::Search::default(),
            browse: Vec::new(),
            browse_diff: DiffPane::default(),
            files: None,
            files_seq: 0,
            files_cancel: None,
            files_stale: true,
            head_paths: None,
            head_paths_seq: 0,
            work_tree_dirty: false,
            changes_multi: Default::default(),
            changes_next: None,
            pending_discard: None,
            pending_line_discard: None,
            amend: false,
            committed_messages: Vec::new(),
            head_message_seq: 0,
            amend_draft: None,
            // Untracked files go to the desktop trash rather than being deleted.
            untracked_remover: Arc::new(|p| Ok(trash::delete(p)?)),
            merge_msg_loaded: false,
        };
        tab._watcher = Some(RepoWatcher::start(
            tab.paths.workdir.clone(),
            tab.paths.git_dir.clone(),
            tab.git.clone(),
            tab.worker.sender(),
            ctx.clone(),
        ));
        tab.ensure_commit_graph();
        tab.refresh();
        tab
    }

    /// Builds git's commit-graph cache when missing; until it exists the
    /// first page of history waits for a full walk of the repository.
    fn ensure_commit_graph(&mut self) {
        // Checked off the UI thread too: opening a tab runs no git here.
        let git = self.git.clone();
        self.worker.job("commit-graph", move || {
            if maintenance::has_commit_graph(&git) {
                return Ok(String::new());
            }
            let t = Instant::now();
            maintenance::write_commit_graph(&git)?;
            Ok(format!("commit-graph cache written in {:.1?}", t.elapsed()))
        });
    }

    fn request_refs(&mut self, then_log: bool) {
        self.gens.refs = self.gens.bump();
        self.gens.refs_then_log = then_log.then_some(self.gens.refs);
        self.refs_req_regen = ops_registry::generation(self.repo_key());
        self.worker.refs(self.gens.refs);
    }

    /// Reloads refs, status and history.
    pub fn refresh(&mut self) {
        // Tidying up after an interactive rebase is tried again.
        self.rebase_finish_tried = None;
        self.request_refs(false);
        self.refresh_status();
        self.reload_history();
    }

    pub fn refresh_status(&mut self) {
        // Every reload of the work tree's state passes here (F5, refocus,
        // file watching, after operations).
        self.work_tree_changed();
        self.gens.status = self.gens.bump();
        self.status_req_regen = ops_registry::generation(self.repo_key());
        self.worker.status(self.gens.status);
    }

    /// Cheap check for changes made outside Tine: reloads refs and status,
    /// and the history only if a branch, tag or HEAD actually moved.
    pub fn refresh_if_changed(&mut self) {
        self.request_refs(true);
        self.refresh_status();
    }

    /// An operation on this repository is queued or running (in any tab or
    /// worktree).
    pub fn op_active(&self) -> bool {
        ops_registry::active_for(self.repo_key())
    }

    /// Refs and status were successfully read after the latest finished
    /// operation on this repository.
    pub fn state_fresh(&self) -> bool {
        self.refs_synced.min(self.status_synced) >= ops_registry::generation(self.repo_key())
    }

    fn worker_ctx(&self) -> &egui::Context {
        self.worker.ctx()
    }

    fn reload_history(&mut self) {
        // Other commits may be reachable now.
        self.rerun_search();
        self.gens.log = self.gens.bump();
        self.history.begin_reload();
        self.worker.log(self.gens.log, LogOptions::default());
    }

    /// Applies finished background work. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let msgs: Vec<Msg> = self.worker.poll().collect();
        let changed = !msgs.is_empty();
        for msg in msgs {
            self.handle(msg);
        }
        self.poll_search();
        if self.work_tree_dirty && !self.op_active() {
            self.work_tree_changed();
        }
        // An operation finished elsewhere (another tab or worktree of this
        // repository): reload once nothing else is queued.
        let generation = ops_registry::generation(self.repo_key());
        if generation > self.seen_regen && !self.op_active() {
            self.seen_regen = generation;
            self.refresh_if_changed();
        }
        changed
    }

    /// The history request whose batches are accepted (for tests that
    /// deliver history by hand).
    #[doc(hidden)]
    pub fn log_seq(&self) -> Gen {
        self.gens.log
    }

    #[doc(hidden)]
    pub fn worker_mut(&mut self) -> &mut Worker {
        &mut self.worker
    }

    /// Opens a dialog (replacing any other).
    pub fn show_dialog(&mut self, d: dialog::Dialog) {
        // Commits being read for a rebase dialog no longer get one.
        if !matches!(d, dialog::Dialog::InteractiveRebase(_)) {
            self.stop_rebase_range();
        }
        self.dialog = Some(d);
        self.modal_shown();
    }

    /// Turns Amend off, discarding the commit message it loaded (back to
    /// what was being written before).
    fn cancel_amend(&mut self) {
        if !self.amend {
            return;
        }
        self.amend = false;
        // Ignore a HEAD message still on its way.
        self.head_message_seq = self.gens.bump();
        let (subject, body) = self.amend_draft.take().unwrap_or_default();
        self.commit_subject = subject;
        self.commit_body = body;
    }

    fn modal_shown(&mut self) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.modal_gen = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Changes whenever this tab opens a modal (unique across tabs), so a
    /// new one is told from one that was already showing.
    pub fn modal_token(&self) -> u64 {
        self.modal_gen
    }

    /// A dialog of this tab is waiting for the user (it owns the keyboard).
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some() || self.pending_discard.is_some() || self.pending_line_discard.is_some()
    }

    /// Applies a message as if a background job had sent it (tests use
    /// this to deliver results out of order).
    #[doc(hidden)]
    pub fn deliver(&mut self, msg: Msg) {
        self.handle(msg);
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Refs(seq, r) if seq == self.gens.refs => {
                match r {
                Ok(refs) => {
                    self.refs_synced = self.refs_req_regen;
                    let moved = refs_key(&refs) != refs_key(&self.refs);
                    self.refs_by_commit.clear();
                    for (i, r) in refs.refs.iter().enumerate() {
                        self.refs_by_commit.entry(r.target).or_default().push(i);
                    }
                    self.refs = refs;
                    self.request_head_paths();
                    if self.gens.refs_then_log.take() == Some(seq) && moved {
                        self.reload_history();
                    }
                }
                Err(e) => self.error(format!("Failed to read branches: {e:#}")),
            }
            }
            Msg::Status(seq, r) if seq == self.gens.status => {
                match r {
                Ok(st) => {
                    self.status_synced = self.status_req_regen;
                    self.status = st;
                    self.status_loaded = true;
                    self.rebase_progress = tine_core::status::rebase_progress(&self.paths.git_dir);
                    self.check_rebase_session();
                    self.sync_commit_box_with_state();
                    if self.status.state == RepoState::Clean
                        && self.stash_conflict.is_none()
                        && !self.status.conflicted.is_empty()
                    {
                        let paths = self.conflicted_paths();
                        self.generic_conflict.get_or_insert_with(Default::default).extend(paths);
                    } else if self.status.state != RepoState::Clean {
                        self.generic_conflict = None;
                    }
                    self.revalidate_changes_selection();
                    if self.changes_selected.is_none() {
                        let first = self.status.conflicted.iter().chain(&self.status.unstaged).next().map(|f| (Side::Unstaged, f))
                            .or_else(|| self.status.staged.first().map(|f| (Side::Staged, f)));
                        if let Some((side, f)) = first {
                            let f = f.clone();
                            self.changes_multi = [f.path.clone()].into();
                            self.select_change(side, f);
                        }
                    }
                }
                Err(e) => self.error(format!("Failed to read status: {e:#}")),
            }
            }
            Msg::LogBatch(seq, batch) if seq == self.gens.log => {
                self.history.push_batch(batch);
                self.sync_search_rows();
                self.after_history_grew();
            }
            Msg::SearchBatch(seq, ids) => self.on_search_batch(seq, ids),
            Msg::FileHistoryBatch(seq, batch) => self.on_file_history_batch(seq, batch),
            Msg::FileHistoryDone(seq, r) => self.on_file_history_done(seq, r),
            Msg::Blame(seq, r) => self.on_blame(seq, r),
            Msg::BlameHighlighted(seq, doc) => self.on_blame_highlighted(seq, doc),
            Msg::RebaseRange(seq, r) if seq == self.rebase_range_seq && seq != 0 => self.on_rebase_range(r),
            Msg::Files(seq, r) if seq == self.files_seq => {
                self.files_seq = 0;
                self.files_cancel = None;
                match r {
                    // Only a listing nothing invalidated meanwhile is fresh
                    // (an invalidation drops the request: `files_seq` = 0).
                    Ok(index) => {
                        self.files = Some(index);
                        self.files_stale = false;
                    }
                    // Stays stale: the next Quick Launch tries again.
                    Err(e) if e.is::<tine_core::Cancelled>() => {}
                    Err(e) => self.error(format!("Failed to list files: {e:#}")),
                }
            }
            Msg::HeadPaths(seq, head, r) if seq == self.head_paths_seq => {
                self.head_paths_seq = 0;
                match r {
                    Ok(paths) => self.head_paths = Some((head, paths)),
                    Err(e) => self.error(format!("Failed to list the files of HEAD: {e:#}")),
                }
            }
            Msg::SearchDone(seq, r) => self.on_search_done(seq, r),
            Msg::LogDone(seq, r) if seq == self.gens.log => {
                self.history.finish(r.is_ok());
                // Finishing may replace the history (e.g. with nothing).
                self.sync_search_rows();
                if let Err(e) = r {
                    self.error(format!("Failed to load history: {e:#}"));
                }
                self.after_history_grew();
            }
            Msg::Details(seq, r) if seq == self.commit.seq => match r {
                Ok((details, files)) => {
                    self.commit.details = Some(Ok(details));
                    self.commit.files = files;
                    self.commit.selected_file = None;
                    if !self.commit.files.is_empty() {
                        self.select_commit_file(0);
                    } else {
                        self.commit_diff = DiffPane::default();
                    }
                }
                Err(e) => self.commit.details = Some(Err(format!("{e:#}"))),
            },
            Msg::Diff(seq, r) if seq == self.browse_diff.seq && seq != 0 => {
                self.browse_diff.doc = Some(r.map_err(|e| format!("{e:#}")));
            }
            Msg::Diff(seq, r) if seq == self.commit_diff.seq && seq != 0 => {
                self.commit_diff.doc = Some(r.map_err(|e| format!("{e:#}")));
            }
            Msg::Diff(seq, r) if seq == self.changes_diff.seq && seq != 0 => {
                self.changes_diff.doc = Some(r.map_err(|e| format!("{e:#}")));
                // Rows shift whenever the diff is recomputed.
                self.changes_diff.selected_rows.clear();
                self.changes_diff.anchor = None;
                self.changes_diff.stale = false;
            }
            // Same rows, now colored: the selection stays.
            Msg::DiffHighlighted(seq, doc) => {
                for pane in [&mut self.browse_diff, &mut self.commit_diff, &mut self.changes_diff] {
                    pane.highlighted(seq, doc.clone());
                }
            }
            Msg::Job(r) => match r {
                Ok(line) if line.is_empty() => {}
                Ok(line) => self.activity.push(line),
                Err(e) => self.error(format!("{e:#}")),
            },
            Msg::Fs(change) => self.on_fs_change(change),
            Msg::OpDone(id, kind, result) => self.on_op_done(id, kind, result),
            Msg::HeadMessage(seq, r) if seq == self.head_message_seq => match r {
                Ok(m) if self.amend => {
                    let (subject, body) = ops::split_message(&m);
                    self.commit_subject = subject;
                    self.commit_body = body;
                }
                Ok(_) => {}
                Err(e) => self.error(format!("Failed to read the last commit message: {e:#}")),
            },
            // Stale generation.
            _ => {}
        }
    }

    fn after_history_grew(&mut self) {
        if let Some(id) = self.pending_select
            && let Some(&row) = self.history.index.get(&id)
        {
            self.pending_select = None;
            self.scroll_to_row = Some(row);
        }
    }

    /// Lists the tracked files for Quick Launch, unless a current list is
    /// there (or on its way).
    pub fn request_files(&mut self) {
        if self.files_seq != 0 || (self.files.is_some() && !self.files_stale) {
            return;
        }
        let cancel = CancelToken::default();
        self.files_seq = self.gens.bump();
        self.files_cancel = Some(cancel.clone());
        self.worker.files(self.files_seq, cancel);
    }

    /// The token of the file listing in flight (tests).
    #[doc(hidden)]
    pub fn files_cancel_token(&self) -> Option<CancelToken> {
        self.files_cancel.clone()
    }

    /// Lists HEAD's paths when HEAD moved.
    fn request_head_paths(&mut self) {
        let Some(head) = self.head_commit() else {
            self.head_paths = None;
            return;
        };
        if self.head_paths.as_ref().is_some_and(|(h, _)| *h == head) || self.head_paths_seq != 0 {
            return;
        }
        self.head_paths_seq = self.gens.bump();
        self.worker.head_paths(self.head_paths_seq, head, CancelToken::default());
    }

    /// The work tree or index may have changed: outdated file lists and
    /// work-tree blames are dropped or reloaded — after the running
    /// operation, if there is one.
    pub(super) fn work_tree_changed(&mut self) {
        if self.op_active() {
            self.work_tree_dirty = true;
            return;
        }
        self.work_tree_dirty = false;
        self.files_stale = true;
        if let Some(c) = self.files_cancel.take() {
            c.cancel();
            self.files_seq = 0;
        }
        self.browse_work_tree_changed();
    }

    fn on_fs_change(&mut self, change: FsChange) {
        if ops_registry::active_for(self.repo_key()) {
            self.held_fs.merge(change);
        } else if change.refs {
            self.refresh_if_changed();
        } else if change.status {
            self.refresh_status();
        }
    }

    pub fn error(&mut self, e: String) {
        log::warn!("{}: {e}", self.title);
        self.errors.push(e);
    }

    pub fn select_commit(&mut self, id: Oid) {
        // Selecting a commit shows the history (leaving File History/Blame).
        if !self.browse.is_empty() {
            self.clear_browse();
        }
        self.view = View::History;
        if self.selected_commit == Some(id) {
            return;
        }
        self.selected_commit = Some(id);
        self.commit = CommitPane { id: Some(id), seq: self.gens.bump(), ..Default::default() };
        self.commit_diff = DiffPane::default();
        self.worker.details(self.commit.seq, id);
    }

    /// Selects a commit and scrolls to it (loading it first if needed).
    pub fn reveal_commit(&mut self, id: Oid) {
        self.show_in_search(id);
        self.select_commit(id);
        match self.history.index.get(&id) {
            Some(&row) => self.scroll_to_row = Some(row),
            None => self.pending_select = Some(id),
        }
    }

    pub fn select_commit_file(&mut self, index: usize) {
        let (Some(id), Some(file)) = (self.commit.id, self.commit.files.get(index)) else { return };
        if self.commit.selected_file == Some(index) {
            return;
        }
        self.commit.selected_file = Some(index);
        let parent = match &self.commit.details {
            Some(Ok(d)) => d.parents.first().copied(),
            _ => None,
        };
        let source = DiffSource::Commit { id, parent };
        self.commit_diff = self.load_diff(file.clone(), source);
    }

    /// A pane for the diff of `file`, loading (the old pane, once replaced,
    /// stops its load).
    fn load_diff(&mut self, file: ChangedFile, source: DiffSource) -> DiffPane {
        let pane = DiffPane { file: Some(file.clone()), source: Some(source), seq: self.gens.bump(), ..Default::default() };
        self.worker.diff(pane.seq, source, file, self.diff_opts, self.doc_opts, pane.job.0.clone());
        pane
    }

    pub fn select_change(&mut self, side: Side, file: ChangedFile) {
        if self.changes_selected.as_ref() == Some(&(side, file.clone())) {
            return;
        }
        self.changes_selected = Some((side, file.clone()));
        self.load_changes_diff();
    }

    fn load_changes_diff(&mut self) {
        let Some((side, file)) = self.changes_selected.clone() else {
            self.changes_diff = DiffPane::default();
            return;
        };
        let source = match side {
            Side::Unstaged => DiffSource::Unstaged,
            Side::Staged => DiffSource::Staged,
        };
        // Keep showing the previous diff of the same file while reloading.
        let keep = self.changes_diff.file.as_ref() == Some(&file) && self.changes_diff.source == Some(source);
        let doc = if keep { self.changes_diff.doc.take() } else { None };
        let mut pane = self.load_diff(file, source);
        pane.stale = doc.is_some();
        pane.doc = doc;
        self.changes_diff = pane;
    }

    /// After a status refresh, keep the selected file if it still has
    /// changes on the same side, and reload its diff.
    fn revalidate_changes_selection(&mut self) {
        let Some((side, file)) = self.changes_selected.clone() else { return };
        let list = self.list(side);
        let still = list.iter().find(|f| f.path == file.path).cloned();
        let present: std::collections::BTreeSet<BString> = list.iter().map(|f| f.path.clone()).collect();
        self.changes_multi.retain(|p| present.contains(p));
        match still {
            Some(f) => {
                self.changes_selected = Some((side, f));
                self.load_changes_diff();
            }
            None => {
                self.changes_selected = None;
                self.changes_multi.clear();
                self.changes_diff = DiffPane::default();
                // Continue with the next file (after stage/unstage/discard).
                if let Some((nside, npath)) = self.changes_next.take()
                    && let Some(f) = self.list(nside).iter().find(|f| f.path == npath).cloned()
                {
                    self.changes_multi.insert(f.path.clone());
                    self.select_change(nside, f);
                }
            }
        }
    }

    /// The files shown in one of the local changes lists (conflicts are
    /// listed with the unstaged files).
    pub fn list(&self, side: Side) -> Vec<ChangedFile> {
        match side {
            Side::Unstaged => self.status.conflicted.iter().chain(&self.status.unstaged).cloned().collect(),
            Side::Staged => self.status.staged.clone(),
        }
    }

    /// Updates the selection for a click on `file` in the `side` list
    /// (Ctrl toggles, Shift selects a range from the focused file).
    pub fn click_change(&mut self, side: Side, file: &ChangedFile, ctrl: bool, shift: bool) {
        let same_side = self.changes_selected.as_ref().is_some_and(|(s, _)| *s == side);
        if !same_side {
            self.changes_multi.clear();
        }
        if shift && same_side {
            let list = self.list(side);
            let anchor = self.changes_selected.as_ref().map(|(_, f)| f.path.clone());
            let a = anchor.and_then(|p| list.iter().position(|f| f.path == p));
            let b = list.iter().position(|f| f.path == file.path);
            if let (Some(a), Some(b)) = (a, b) {
                self.changes_multi = list[a.min(b)..=a.max(b)].iter().map(|f| f.path.clone()).collect();
                return;
            }
        }
        if ctrl && same_side {
            if !self.changes_multi.remove(&file.path) {
                self.changes_multi.insert(file.path.clone());
                self.select_change(side, file.clone());
            } else if self.changes_selected.as_ref().is_some_and(|(_, f)| f.path == file.path) {
                // Unselected the focused file: focus another selected one.
                let next = self.list(side).into_iter().find(|f| self.changes_multi.contains(&f.path));
                match next {
                    Some(f) => self.select_change(side, f),
                    None => {
                        self.changes_selected = None;
                        self.changes_diff = DiffPane::default();
                    }
                }
            }
            return;
        }
        self.changes_multi = [file.path.clone()].into();
        self.select_change(side, file.clone());
    }

    /// Files an action on `clicked` applies to: the whole selection if the
    /// clicked file is part of it, otherwise just that file.
    pub fn action_targets(&self, side: Side, clicked: &ChangedFile) -> Vec<ChangedFile> {
        let same_side = self.changes_selected.as_ref().is_some_and(|(s, _)| *s == side);
        if same_side && self.changes_multi.contains(&clicked.path) {
            self.list(side).into_iter().filter(|f| self.changes_multi.contains(&f.path)).collect()
        } else {
            vec![clicked.clone()]
        }
    }

    pub fn selected_on(&self, side: Side) -> Vec<ChangedFile> {
        match &self.changes_selected {
            Some((s, f)) if *s == side => self.action_targets(side, f),
            _ => Vec::new(),
        }
    }

    /// An operation on this repository is queued or running (from any tab
    /// or worktree), or the refresh after one has not completed yet.
    pub fn busy(&self) -> bool {
        self.op_active() || !self.state_fresh()
    }

    /// Remembers which file to select after `targets` leave the `side` list.
    fn plan_next_selection(&mut self, side: Side, targets: &[ChangedFile]) {
        let list = self.list(side);
        let gone: std::collections::BTreeSet<&BString> = targets.iter().map(|f| &f.path).collect();
        let last = list.iter().rposition(|f| gone.contains(&f.path));
        self.changes_next = last.and_then(|i| {
            list[i + 1..].iter().chain(list[..i].iter().rev()).find(|f| !gone.contains(&f.path))
        })
        .map(|f| (side, f.path.clone()));
    }

    fn run_op(&mut self, kind: OpKind, f: impl FnOnce(&GitRunner) -> anyhow::Result<OpOutcome> + Send + 'static) -> Option<OpId> {
        self.run_op_ctx(kind, move |_, git| f(git))
    }

    /// Like `run_op`, with access to the operation context (progress,
    /// cancellation). The runner records every command it runs.
    fn run_op_ctx(
        &mut self,
        kind: OpKind,
        f: impl FnOnce(&OpContext, &GitRunner) -> anyhow::Result<OpOutcome> + Send + 'static,
    ) -> Option<OpId> {
        let git = self.git.clone();
        self.worker.op(kind, move |ctx| f(ctx, &ctx.git(&git)))
    }

    pub fn stage(&mut self, files: Vec<ChangedFile>) {
        // `git add` on a conflicted file marks it resolved even with conflict
        // markers left in it. Until there is an explicit "mark resolved"
        // action (M3), conflicted files are never staged implicitly.
        let (conflicted, files): (Vec<ChangedFile>, Vec<ChangedFile>) =
            files.into_iter().partition(|f| f.kind == tine_core::model::ChangeKind::Conflicted);
        if !conflicted.is_empty() {
            self.activity.push(format!(
                "Skipped {} conflicted file(s); resolve conflicts before staging them",
                conflicted.len()
            ));
        }
        if files.is_empty() || self.busy() {
            return;
        }
        self.plan_next_selection(Side::Unstaged, &files);
        let n = files.len();
        self.run_op(OpKind::Stage, move |git| {
            ops::stage(git, &files)?;
            Ok(OpOutcome::new(format!("Staged {n} file(s)")))
        });
    }

    pub fn unstage(&mut self, files: Vec<ChangedFile>) {
        if files.is_empty() || self.busy() {
            return;
        }
        self.plan_next_selection(Side::Staged, &files);
        let n = files.len();
        self.run_op(OpKind::Unstage, move |git| {
            ops::unstage(git, &files)?;
            Ok(OpOutcome::new(format!("Unstaged {n} file(s)")))
        });
    }

    /// Asks for confirmation before discarding (see `confirm_discard`).
    pub fn request_discard(&mut self, files: Vec<ChangedFile>) {
        let files: Vec<ChangedFile> =
            files.into_iter().filter(|f| f.kind != tine_core::model::ChangeKind::Conflicted).collect();
        if !files.is_empty() && !self.busy() {
            self.pending_discard = Some(files);
            self.modal_shown();
        }
    }

    pub fn confirm_discard(&mut self) {
        let Some(files) = self.pending_discard.take() else { return };
        self.plan_next_selection(Side::Unstaged, &files);
        let remover = self.untracked_remover.clone();
        self.run_op(OpKind::Discard, move |git| {
            let report = ops::discard(git, &files, |p| remover(p))?;
            let done = format!(
                "restored {} file(s), moved {} untracked file(s) to Trash",
                report.restored.len(),
                report.removed.len()
            );
            if report.is_complete() {
                return Ok(OpOutcome::new(format!("Discard: {done}")));
            }
            // Partly done: say exactly what happened and what didn't.
            let failed: Vec<String> =
                report.failed.iter().map(|(p, e)| format!("{} ({e})", p.display())).collect();
            anyhow::bail!("{done}, but could not remove {}: {}", failed.len(), failed.join(", "))
        });
    }

    /// Replaces how untracked files are removed on discard (tests use this
    /// to avoid touching the real trash).
    pub fn set_untracked_remover(&mut self, f: impl Fn(&std::path::Path) -> anyhow::Result<()> + Send + Sync + 'static) {
        self.untracked_remover = Arc::new(f);
    }

    /// Committing is allowed with staged changes, when amending, or when
    /// finishing a merge (whose resolution may leave nothing staged). During
    /// a rebase, cherry-pick or revert, Continue is used instead.
    pub fn can_commit(&self) -> bool {
        let state_ok = match self.status.state {
            RepoState::Clean => self.amend || !self.status.staged.is_empty(),
            RepoState::Merging => !self.amend,
            // Stopped at an `edit`: amend that commit, or add new ones.
            RepoState::Rebasing => match self.edit_stop() {
                Some(id) if self.amend => self.head_commit() == Some(id),
                Some(_) => !self.status.staged.is_empty(),
                None => false,
            },
            _ => false,
        };
        !self.busy() && !self.commit_subject.trim().is_empty() && self.status.conflicted.is_empty() && state_ok
    }

    pub fn commit(&mut self) {
        if !self.can_commit() {
            return;
        }
        let message = ops::join_message(&self.commit_subject, &self.commit_body);
        let amend = self.amend;
        let merging = self.status.state == RepoState::Merging;
        let kind = OpKind::Commit { message: message.clone(), amend };
        // At an edit stop: only if git is still there, with HEAD as shown.
        let guard = (self.status.state == RepoState::Rebasing).then(|| (self.edit_stop(), self.head_commit()));
        let git_dir = self.paths.git_dir.clone();
        self.run_op(kind, move |git| {
            if let Some((stop, head)) = guard {
                let now = match tine_core::integrate::stop_reason(&git_dir) {
                    tine_core::integrate::StopReason::Edit(id) => Some(id),
                    _ => None,
                };
                let head_now = tine_core::interactive::head_now(git).ok().map(|(h, _)| h);
                if stop.is_none() || now != stop || head_now != head {
                    anyhow::bail!("the rebase moved on since; nothing was committed");
                }
            }
            let opts = ops::CommitOptions { amend, allow_empty: merging };
            // Amending only the message must work with nothing staged.
            let opts = if amend { ops::CommitOptions { allow_empty: true, ..opts } } else { opts };
            let id = ops::commit(git, &message, &opts)?;
            let subject = message.lines().next().unwrap_or_default();
            Ok(OpOutcome::new(format!("Committed {} {subject}", id.short())).moving_refs())
        });
    }

    /// Toggles amend. Turning it on sets the draft aside and loads HEAD's
    /// message for editing; turning it off brings the draft back.
    pub fn set_amend(&mut self, amend: bool) {
        if amend == self.amend || (amend && self.status.state != RepoState::Clean && self.edit_stop().is_none()) {
            return;
        }
        self.amend = amend;
        if amend {
            let draft = (std::mem::take(&mut self.commit_subject), std::mem::take(&mut self.commit_body));
            self.amend_draft = Some(draft);
            self.head_message_seq = self.gens.bump();
            self.worker.head_message(self.head_message_seq, self.git.clone());
        } else {
            // Ignore a HEAD message still on its way.
            self.head_message_seq = self.gens.bump();
            if let Some((subject, body)) = self.amend_draft.take() {
                self.commit_subject = subject;
                self.commit_body = body;
            }
        }
    }

    /// Whether the local-changes diff supports hunk/line actions.
    pub fn line_actions(&self) -> Option<LineActions> {
        let (side, file) = self.changes_selected.as_ref()?;
        let Some(Ok(doc)) = &self.changes_diff.doc else { return None };
        // Conflicts are resolved per file (context menu), not by hunk.
        if file.kind == ChangeKind::Conflicted {
            return None;
        }
        let disabled = if doc.binary {
            Some("Binary files are staged as a whole")
        } else if file.is_submodule {
            Some("Submodules are staged as a whole")
        } else if file.kind != ChangeKind::Modified {
            Some("Only modified files can be staged by hunk or line")
        } else if doc.header.iter().any(|h| h.starts_with("old mode") || h.starts_with("new mode")) {
            Some("Files with a mode change are staged as a whole")
        } else if self.diff_opts.ignore_whitespace {
            Some("Turn off \"Ignore whitespace\" to stage by hunk or line")
        } else if self.busy() {
            Some("Waiting for the current operation")
        } else if self.changes_diff.stale {
            Some("Updating the diff")
        } else {
            None
        };
        Some(LineActions { side: *side, disabled })
    }

    pub fn select_diff_rows(&mut self, rows: std::collections::BTreeSet<usize>, anchor: Option<usize>) {
        self.changes_diff.selected_rows = rows;
        self.changes_diff.anchor = anchor;
    }

    /// The changed lines `target` refers to, as `(hunk, line)` pairs.
    fn patch_lines(&self, target: PatchTarget) -> Option<PatchSelection> {
        let (_, file) = self.changes_selected.as_ref()?;
        let Some(Ok(doc)) = &self.changes_diff.doc else { return None };
        let lines: Vec<(usize, usize)> = doc
            .rows
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                r.is_change()
                    && match target {
                        PatchTarget::Hunk(h) => r.hunk == h,
                        PatchTarget::Selection => self.changes_diff.selected_rows.contains(i),
                        PatchTarget::SelectedIn(h) => r.hunk == h && self.changes_diff.selected_rows.contains(i),
                    }
            })
            .filter_map(|(_, r)| Some((r.hunk, r.line?)))
            .collect();
        (!lines.is_empty()).then(|| PatchSelection { file: file.clone(), doc: doc.clone(), lines })
    }

    /// Stages, unstages or (after confirmation) discards a hunk or the
    /// selected lines of the shown local-changes diff.
    pub fn apply_lines(&mut self, mode: PatchMode, target: PatchTarget) {
        let Some(actions) = self.line_actions() else { return };
        if actions.disabled.is_some() {
            return;
        }
        let Some(PatchSelection { file, doc, lines }) = self.patch_lines(target) else { return };
        if mode == PatchMode::Discard {
            self.pending_line_discard = Some(LineDiscard { file, diff: doc.source.clone(), lines });
            self.modal_shown();
            return;
        }
        self.run_patch(mode, file, doc.source.clone(), lines);
    }

    pub fn confirm_line_discard(&mut self) {
        if let Some(d) = self.pending_line_discard.take() {
            self.run_patch(PatchMode::Discard, d.file, d.diff, d.lines);
        }
    }

    fn run_patch(&mut self, mode: PatchMode, file: ChangedFile, diff: Arc<tine_core::model::FileDiff>, lines: Vec<(usize, usize)>) {
        let n = lines.len();
        let set: std::collections::HashSet<(usize, usize)> = lines.into_iter().collect();
        let opts = self.diff_opts;
        self.run_op(OpKind::Patch(mode), move |git| {
            patch::apply(git, &file.path, &diff, opts, mode, |h, l| set.contains(&(h, l)))?;
            let verb = match mode {
                PatchMode::Stage => "Staged",
                PatchMode::Unstage => "Unstaged",
                PatchMode::Discard => "Discarded",
            };
            Ok(OpOutcome::new(format!("{verb} {n} line(s) in {}", file.path)))
        });
    }

    /// Key identifying this repository across tabs (see `worker::repo_key`).
    pub fn repo_key(&self) -> &std::path::Path {
        self.worker.repo_key()
    }

    /// Result of an operation started by a tab (for this work tree) that
    /// has since been closed: its follow-ups — where stashed changes are, a
    /// stash conflict — are taken over here.
    pub fn on_external_op_done(&mut self, kind: OpKind, result: anyhow::Result<OpOutcome>) {
        match result {
            Ok(mut out) => {
                if !out.summary.is_empty() {
                    self.activity.push(format!("{} (from a closed tab)", out.summary));
                }
                for f in std::mem::take(&mut out.follow_ups) {
                    // A question for a closed tab is moot; state is not.
                    if !matches!(f, crate::worker::FollowUp::Empty(_)) {
                        self.handle_follow_up(f);
                    }
                }
            }
            Err(e) => self.error(format!("{} (started in a closed tab) failed: {e:#}", kind.label())),
        }
        self.refresh_if_changed();
    }

    fn on_op_done(&mut self, id: OpId, kind: OpKind, result: anyhow::Result<OpOutcome>) {
        // Whatever the operation did, the shown diff is outdated until the
        // refresh below brings a new one.
        if self.changes_diff.doc.is_some() {
            self.changes_diff.stale = true;
        }
        // A commit may have been created even if a later step failed.
        if matches!(kind, OpKind::Commit { .. }) {
            self.ops_moved_refs = true;
        }
        // Failed or cancelled operations may have moved refs part-way (e.g.
        // a branch created, then the switch failed): always re-check.
        if result.is_err() {
            self.ops_moved_refs = true;
        }
        match result {
            Ok(mut out) => {
                self.ops_moved_refs |= out.refs_changed;
                self.activity.push(std::mem::take(&mut out.summary));
                for f in std::mem::take(&mut out.follow_ups) {
                    self.handle_follow_up(f);
                }
                if let OpKind::Commit { message, amend } = kind {
                    self.commit_subject.clear();
                    self.commit_body.clear();
                    self.amend = false;
                    // After an amend, bring back what was being written before.
                    if amend && let Some((subject, body)) = self.amend_draft.take() {
                        self.commit_subject = subject;
                        self.commit_body = body;
                    }
                    self.committed_messages.push(message);
                }
            }
            Err(e) if e.downcast_ref::<crate::worker::NeedsDecision>().is_some() => {
                self.changes_next = None;
                if let Ok(d) = e.downcast::<crate::worker::NeedsDecision>() {
                    self.handle_decision(d);
                }
            }
            Err(e) => {
                self.changes_next = None;
                self.last_failed_op = Some(id);
                if e.downcast_ref::<tine_core::Cancelled>().is_some() {
                    // The registry checked for lock files left behind.
                    let notes = ops_registry::snapshot(id).map(|s| s.notes).unwrap_or_default();
                    if notes.is_empty() {
                        self.error(format!("{} was cancelled", kind.label()));
                    } else {
                        self.error(format!("{} was cancelled; {}", kind.label(), notes.join("; ")));
                    }
                } else {
                    let message = format!("{} failed: {e:#}", kind.label());
                    self.error(match identity_hint(&message) {
                        Some(hint) => format!("{message}\n{hint}"),
                        None => message,
                    });
                }
            }
        }
        if !self.op_active() {
            // Refs and status are both re-read after every operation (busy()
            // waits for both); history reloads only if refs moved.
            self.held_fs = FsChange::default();
            self.ops_moved_refs = false;
            self.seen_regen = ops_registry::generation(self.repo_key());
            self.refresh_if_changed();
        }
    }

    /// Keeps the commit box consistent with operations in progress. When
    /// one starts, Amend is turned off and the draft set aside; during a
    /// merge/cherry-pick/revert git's prepared message (MERGE_MSG) is shown,
    /// like Fork does. When it ends, the draft comes back.
    fn sync_commit_box_with_state(&mut self) {
        let in_progress = !matches!(self.status.state, RepoState::Clean | RepoState::Bisecting);
        if in_progress && !self.merge_msg_loaded {
            self.merge_msg_loaded = true;
            let draft = if self.amend {
                // The fields hold HEAD's message; the user's draft was set
                // aside when amend was turned on.
                self.amend = false;
                self.commit_subject.clear();
                self.commit_body.clear();
                self.amend_draft.take()
            } else {
                Some((std::mem::take(&mut self.commit_subject), std::mem::take(&mut self.commit_body)))
            };
            self.op_draft = draft.filter(|(s, b)| !s.trim().is_empty() || !b.trim().is_empty());
            if matches!(self.status.state, RepoState::Merging | RepoState::CherryPicking | RepoState::Reverting)
                && let Ok(text) = std::fs::read_to_string(self.paths.git_dir.join("MERGE_MSG"))
            {
                let cleaned: String = text.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
                let (subject, body) = ops::split_message(&cleaned);
                self.commit_subject = subject;
                self.commit_body = body;
            }
        } else if !in_progress && self.merge_msg_loaded {
            // Finished (the commit cleared the box) or aborted (the box still
            // holds the operation's message): bring the draft back — not in
            // Amend (armed for an edit stop, it would rewrite HEAD).
            self.merge_msg_loaded = false;
            self.amend = false;
            self.amend_draft = None;
            self.head_message_seq = self.gens.bump();
            self.commit_subject.clear();
            self.commit_body.clear();
            if let Some((subject, body)) = self.op_draft.take() {
                self.commit_subject = subject;
                self.commit_body = body;
            }
        }
    }

    pub fn set_diff_options(&mut self, opts: DiffOptions) {
        if self.diff_opts == opts {
            return;
        }
        self.diff_opts = opts;
        self.reload_diffs(false);
    }

    /// Loads the shown diffs again (their options changed); `keep` shows
    /// the current ones until the new ones arrive.
    fn reload_diffs(&mut self, keep: bool) {
        for browse in [false, true] {
            let pane = if browse { &self.browse_diff } else { &self.commit_diff };
            if let (Some(file), Some(source)) = (pane.file.clone(), pane.source) {
                let mut fresh = self.load_diff(file, source);
                let pane = if browse { &mut self.browse_diff } else { &mut self.commit_diff };
                if keep {
                    fresh.doc = pane.doc.take();
                }
                *pane = fresh;
            }
        }
        self.load_changes_diff();
    }

    /// Changes how diffs and blame are prepared (tab width, highlighting):
    /// the shown ones are prepared again, the old ones staying up meanwhile.
    pub fn set_doc_options(&mut self, opts: DocOptions) {
        if self.doc_opts == opts {
            return;
        }
        self.doc_opts = opts;
        self.reload_diffs(true);
        self.rebuild_blames();
    }

    pub fn head_commit(&self) -> Option<Oid> {
        self.refs.head_target()
    }
}

/// What to do when git doesn't know who is committing.
pub fn identity_hint(git_message: &str) -> Option<&'static str> {
    const SIGNS: [&str; 4] =
        ["Please tell me who you are", "Author identity unknown", "Committer identity unknown", "unable to auto-detect email address"];
    SIGNS
        .iter()
        .any(|s| git_message.contains(s))
        .then_some("Set your name and email in Settings → Git (Ctrl+,).")
}

impl Drop for RepoTab {
    /// Results that arrived but were not processed yet, and recovery state
    /// (stashed changes waiting, a stash conflict) are handed to the app so
    /// a tab reopened on this work tree can take them over.
    fn drop(&mut self) {
        use crate::{ops_registry::OrphanedResult, worker::FollowUp};
        self.stop_browse();
        self.stop_rebase_range();
        if let Some(c) = self.files_cancel.take() {
            c.cancel();
        }
        let repo = self.repo_key().to_path_buf();
        let origin = self.paths.workdir.clone();
        for msg in self.worker.poll().collect::<Vec<_>>() {
            if let Msg::OpDone(id, kind, result) = msg {
                ops_registry::push_orphan(OrphanedResult { repo: repo.clone(), origin: origin.clone(), id, kind, result });
            }
        }
        let mut carried = OpOutcome::default();
        if let Some(oid) = self.pending_autostash {
            carried = carried.with(FollowUp::AutoStash(tine_core::autostash::AutoStash::Kept(oid)));
        }
        if let Some(sc) = self.stash_conflict.take() {
            carried = carried.with(FollowUp::StashConflict {
                oid: sc.oid,
                was_clean_before: sc.was_clean_before,
                created_untracked: sc.created_untracked,
            });
        }
        if !carried.follow_ups.is_empty() {
            ops_registry::push_orphan(OrphanedResult { repo, origin, id: 0, kind: OpKind::Recovered, result: Ok(carried) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_without_an_identity_point_to_the_settings() {
        // What git really says when it doesn't know who commits.
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("LC_ALL", "C")
                .env_remove("EMAIL")
                .env_remove("GIT_AUTHOR_NAME")
                .env_remove("GIT_AUTHOR_EMAIL")
                .env_remove("GIT_COMMITTER_NAME")
                .env_remove("GIT_COMMITTER_EMAIL")
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        let out = git(&["-c", "user.useConfigOnly=true", "commit", "-q", "--allow-empty", "-m", "x"]);
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(identity_hint(&said).is_some_and(|h| h.contains("Settings")), "{said}");
        assert_eq!(identity_hint("error: pathspec 'x' did not match"), None);
    }

    fn commit(n: u8) -> CommitSummary {
        CommitSummary {
            id: Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap(),
            parents: Vec::new(),
            author_name: "a".into(),
            author_email: "e".into(),
            author_time: 0,
            commit_time: 0,
            subject: String::new(),
        }
    }

    #[test]
    fn failed_reload_keeps_previous_history() {
        let mut h = History::default();
        h.begin_reload();
        h.push_batch(vec![commit(1), commit(2)]);
        h.finish(true);
        assert_eq!(h.commits.len(), 2);

        h.begin_reload();
        h.finish(false);
        assert_eq!(h.commits.len(), 2, "old history must survive a failed reload");
        assert!(!h.loading);

        // A successful reload that yields nothing (all refs deleted) clears it.
        h.begin_reload();
        h.finish(true);
        assert!(h.commits.is_empty() && h.index.is_empty() && h.graph.is_empty());
    }

    #[test]
    fn reload_replaces_on_first_batch() {
        let mut h = History::default();
        h.begin_reload();
        h.push_batch(vec![commit(1)]);
        h.finish(true);
        h.begin_reload();
        assert_eq!(h.commits.len(), 1, "old rows stay visible until new data arrives");
        h.push_batch(vec![commit(3)]);
        assert_eq!(h.commits.len(), 1);
        assert_eq!(h.index.get(&commit(3).id), Some(&0));
        assert!(!h.index.contains_key(&commit(1).id));
    }
}
