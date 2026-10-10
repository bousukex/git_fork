//! State of one open repository (one tab) and its reaction to worker results.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tine_core::{
    CliReader, GitRunner, RepoPaths, RepoReader,
    diff::{DiffOptions, DiffSource},
    graph::Graph,
    log::LogOptions,
    maintenance,
    model::{ChangedFile, CommitDetails, CommitSummary, Oid, RefsSnapshot, WorkingStatus},
};

use crate::{
    diff_model::DiffDoc,
    watcher::{FsChange, RepoWatcher},
    worker::{Gen, Msg, Worker},
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

    fn finish(&mut self) {
        self.take_reload();
        self.graph.finish();
        self.loading = false;
        self.elapsed = self.started.take().map(|s| s.elapsed());
    }
}

/// A file diff being shown (or loaded).
#[derive(Default)]
pub struct DiffPane {
    pub file: Option<ChangedFile>,
    pub source: Option<DiffSource>,
    pub doc: Option<Result<Arc<DiffDoc>, String>>,
    seq: Gen,
}

impl DiffPane {
    pub fn is_loading(&self) -> bool {
        self.file.is_some() && self.doc.is_none()
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
    pub changes_selected: Option<(Side, ChangedFile)>,
    pub changes_diff: DiffPane,
    pub diff_opts: DiffOptions,

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
    ops_in_flight: usize,
    held_fs: FsChange,
}

#[derive(Default)]
struct Gens {
    refs: Gen,
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
        let reader = CliReader::open(path)?;
        let paths = reader.paths().clone();
        let git = reader.runner().clone();
        let title = paths
            .workdir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| paths.workdir.display().to_string());
        let worker = Worker::new(Arc::new(reader), ctx.clone());
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
            errors: Vec::new(),
            activity: Vec::new(),
            sidebar_filter: String::new(),
            commit_subject: String::new(),
            commit_body: String::new(),
            open_request: None,
            _watcher: None,
            ops_in_flight: 0,
            held_fs: FsChange::default(),
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
        Ok(tab)
    }

    /// Builds git's commit-graph cache when missing; until it exists the
    /// first page of history waits for a full walk of the repository.
    fn ensure_commit_graph(&mut self) {
        if maintenance::has_commit_graph(&self.git) {
            return;
        }
        let git = self.git.clone();
        self.activity.push("Writing commit-graph cache (git commit-graph write --reachable)…".into());
        self.worker.job("commit-graph", move || {
            let t = Instant::now();
            maintenance::write_commit_graph(&git)?;
            Ok(format!("commit-graph cache written in {:.1?}", t.elapsed()))
        });
    }

    /// Reloads refs, status and history.
    pub fn refresh(&mut self) {
        self.gens.refs = self.gens.bump();
        self.worker.refs(self.gens.refs);
        self.refresh_status();
        self.gens.log = self.gens.bump();
        self.history.begin_reload();
        self.worker.log(self.gens.log, LogOptions::default());
    }

    pub fn refresh_status(&mut self) {
        self.gens.status = self.gens.bump();
        self.worker.status(self.gens.status);
    }

    /// Applies finished background work. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let msgs: Vec<Msg> = self.worker.poll().collect();
        let changed = !msgs.is_empty();
        for msg in msgs {
            self.handle(msg);
        }
        changed
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Refs(seq, r) if seq == self.gens.refs => match r {
                Ok(refs) => {
                    self.refs_by_commit.clear();
                    for (i, r) in refs.refs.iter().enumerate() {
                        self.refs_by_commit.entry(r.target).or_default().push(i);
                    }
                    self.refs = refs;
                }
                Err(e) => self.error(format!("Failed to read branches: {e:#}")),
            },
            Msg::Status(seq, r) if seq == self.gens.status => match r {
                Ok(st) => {
                    self.status = st;
                    self.status_loaded = true;
                    self.revalidate_changes_selection();
                    if self.changes_selected.is_none() {
                        let first = self.status.conflicted.iter().chain(&self.status.unstaged).next().map(|f| (Side::Unstaged, f))
                            .or_else(|| self.status.staged.first().map(|f| (Side::Staged, f)));
                        if let Some((side, f)) = first {
                            self.select_change(side, f.clone());
                        }
                    }
                }
                Err(e) => self.error(format!("Failed to read status: {e:#}")),
            },
            Msg::LogBatch(seq, batch) if seq == self.gens.log => {
                self.history.push_batch(batch);
                self.after_history_grew();
            }
            Msg::LogDone(seq, r) if seq == self.gens.log => {
                self.history.finish();
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
            Msg::Diff(seq, r) if seq == self.commit_diff.seq => {
                self.commit_diff.doc = Some(r.map(|d| Arc::new(DiffDoc::new(d))).map_err(|e| format!("{e:#}")));
            }
            Msg::Diff(seq, r) if seq == self.changes_diff.seq => {
                self.changes_diff.doc = Some(r.map(|d| Arc::new(DiffDoc::new(d))).map_err(|e| format!("{e:#}")));
            }
            Msg::Job(r) => match r {
                Ok(line) => self.activity.push(line),
                Err(e) => self.error(format!("{e:#}")),
            },
            Msg::Fs(change) => self.on_fs_change(change),
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

    fn on_fs_change(&mut self, change: FsChange) {
        if self.ops_in_flight > 0 {
            self.held_fs.merge(change);
        } else if change.refs {
            self.refresh();
        } else if change.status {
            self.refresh_status();
        }
    }

    pub fn error(&mut self, e: String) {
        log::warn!("{}: {e}", self.title);
        self.errors.push(e);
    }

    pub fn select_commit(&mut self, id: Oid) {
        if self.selected_commit == Some(id) {
            return;
        }
        self.view = View::History;
        self.selected_commit = Some(id);
        self.commit = CommitPane { id: Some(id), seq: self.gens.bump(), ..Default::default() };
        self.commit_diff = DiffPane::default();
        self.worker.details(self.commit.seq, id);
    }

    /// Selects a commit and scrolls to it (loading it first if needed).
    pub fn reveal_commit(&mut self, id: Oid) {
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
        self.commit_diff = DiffPane { file: Some(file.clone()), source: Some(source), doc: None, seq: self.gens.bump() };
        self.worker.diff(self.commit_diff.seq, source, file.clone(), self.diff_opts);
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
        self.changes_diff = DiffPane { file: Some(file.clone()), source: Some(source), doc, seq: self.gens.bump() };
        self.worker.diff(self.changes_diff.seq, source, file, self.diff_opts);
    }

    /// After a status refresh, keep the selected file if it still has
    /// changes on the same side, and reload its diff.
    fn revalidate_changes_selection(&mut self) {
        let Some((side, file)) = &self.changes_selected else { return };
        let list = match side {
            Side::Unstaged => &self.status.unstaged,
            Side::Staged => &self.status.staged,
        };
        let still = list.iter().chain(&self.status.conflicted).find(|f| f.path == file.path).cloned();
        match still {
            Some(f) => {
                self.changes_selected = Some((*side, f));
                self.load_changes_diff();
            }
            None => {
                self.changes_selected = None;
                self.changes_diff = DiffPane::default();
            }
        }
    }

    pub fn set_diff_options(&mut self, opts: DiffOptions) {
        if self.diff_opts == opts {
            return;
        }
        self.diff_opts = opts;
        if let (Some(file), Some(source)) = (self.commit_diff.file.clone(), self.commit_diff.source) {
            self.commit_diff.seq = self.gens.bump();
            self.commit_diff.doc = None;
            self.worker.diff(self.commit_diff.seq, source, file, opts);
        }
        self.load_changes_diff();
    }

    pub fn head_commit(&self) -> Option<Oid> {
        self.refs.head_target()
    }
}
