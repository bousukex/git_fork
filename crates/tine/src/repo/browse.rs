//! Browsing views over the history — File History and Blame — kept on a
//! stack: blame from a file's history, then blame the revision before,
//! and Esc steps back. The view underneath (history or local changes, with
//! its selection) is left as it was.

use std::sync::Arc;

use bstr::BString;
use tine_core::{
    CancelToken,
    diff::DiffSource,
    file_history::FileHistoryEntry,
    model::{ChangeKind, ChangedFile, Oid},
    search::SearchEnd,
};

use super::{DiffPane, RepoTab};
use crate::{blame_model::BlameDoc, worker::Gen};

pub enum Browse {
    FileHistory(FileHistoryView),
    Blame(BlameView),
}

impl Browse {
    fn stop(&self) {
        match self {
            Browse::FileHistory(v) => v.cancel.cancel(),
            Browse::Blame(v) => v.cancel.cancel(),
        }
    }
}

pub struct FileHistoryView {
    /// Identifies this view (egui ids, scroll state) for as long as it is
    /// on the stack.
    pub view_id: u64,
    /// Where the history starts, and the file's path there.
    pub start: Oid,
    pub path: BString,
    /// Shown with the title, e.g. "before its deletion in 1a2b3c4d".
    pub note: Option<String>,
    seq: Gen,
    cancel: CancelToken,
    pub entries: Vec<FileHistoryEntry>,
    pub loading: bool,
    pub error: Option<String>,
    pub selected: Option<usize>,
}

pub struct BlameView {
    pub view_id: u64,
    /// `None`: the work tree (uncommitted lines included).
    pub rev: Option<Oid>,
    /// For a work-tree blame: HEAD and the file's mtime and size when it
    /// was requested (it is redone only when these change).
    stamp: Option<WorkTreeStamp>,
    pub path: BString,
    pub ignore_whitespace: bool,
    seq: Gen,
    cancel: CancelToken,
    pub doc: Option<Result<Arc<BlameDoc>, String>>,
    /// Selected line (index into the doc's lines).
    pub selected: Option<usize>,
    /// Line to bring into view once loaded (e.g. the same line after
    /// blaming the previous revision).
    pub scroll_to: Option<usize>,
}

type WorkTreeStamp = (Option<Oid>, Option<std::time::SystemTime>, u64);

impl FileHistoryView {
    /// The token of the history being loaded (tests check it gets stopped).
    #[doc(hidden)]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

impl BlameView {
    #[doc(hidden)]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

/// The file at the state a commit left it, as a change of that commit.
fn change_of(entry: &FileHistoryEntry) -> ChangedFile {
    let kind = match entry.status {
        Some('A') => ChangeKind::Added,
        Some('D') => ChangeKind::Deleted,
        Some('R') => ChangeKind::Renamed,
        Some('C') => ChangeKind::Copied,
        Some('T') => ChangeKind::TypeChanged,
        _ => ChangeKind::Modified,
    };
    ChangedFile { path: entry.path.clone(), orig_path: entry.old_path.clone(), kind, is_submodule: false }
}

impl RepoTab {
    /// Opens the history of `path` going back from `start`.
    pub fn open_file_history(&mut self, start: Oid, path: BString, note: Option<String>) {
        let seq = self.gens.bump();
        let cancel = CancelToken::default();
        self.worker.file_history(seq, start, path.clone(), cancel.clone());
        self.browse_diff = DiffPane::default();
        let view_id = self.gens.bump();
        self.browse.push(Browse::FileHistory(FileHistoryView {
            view_id,
            start,
            path,
            note,
            seq,
            cancel,
            entries: Vec::new(),
            loading: true,
            error: None,
            selected: None,
        }));
    }

    /// Opens blame of `path` at `rev` (the work tree without one).
    pub fn open_blame(&mut self, rev: Option<Oid>, path: BString, scroll_to: Option<usize>) {
        let ignore_whitespace = self.browse.iter().rev().find_map(|b| match b {
            Browse::Blame(v) => Some(v.ignore_whitespace),
            _ => None,
        });
        let mut view = BlameView {
            view_id: self.gens.bump(),
            stamp: None,
            rev,
            path,
            ignore_whitespace: ignore_whitespace.unwrap_or(false),
            seq: 0,
            cancel: CancelToken::default(),
            doc: None,
            selected: scroll_to,
            scroll_to,
        };
        self.request_blame(&mut view);
        self.browse.push(Browse::Blame(view));
    }

    fn work_tree_stamp(&self, path: &[u8]) -> WorkTreeStamp {
        use std::os::unix::ffi::OsStrExt as _;
        let meta = std::fs::metadata(self.paths.workdir.join(std::ffi::OsStr::from_bytes(path))).ok();
        (self.head_commit(), meta.as_ref().and_then(|m| m.modified().ok()), meta.map_or(0, |m| m.len()))
    }

    fn request_blame(&mut self, view: &mut BlameView) {
        if view.rev.is_none() {
            view.stamp = Some(self.work_tree_stamp(&view.path));
        }
        view.cancel.cancel();
        view.cancel = CancelToken::default();
        view.seq = self.gens.bump();
        view.doc = None;
        self.worker.blame(view.seq, view.path.clone(), view.rev, view.ignore_whitespace, view.cancel.clone());
    }

    /// Blames the top view again (whitespace option changed, or the work
    /// tree changed).
    pub fn reload_blame(&mut self) {
        let Some(Browse::Blame(mut view)) = self.browse.pop() else { return };
        self.request_blame(&mut view);
        self.browse.push(Browse::Blame(view));
    }

    pub fn set_blame_whitespace(&mut self, ignore: bool) {
        if let Some(Browse::Blame(v)) = self.browse.last_mut()
            && v.ignore_whitespace != ignore
        {
            v.ignore_whitespace = ignore;
            self.reload_blame();
        }
    }

    /// Back one step (Esc).
    pub fn close_browse(&mut self) {
        if let Some(b) = self.browse.pop() {
            b.stop();
        }
        // The diff belongs to the file history underneath, if any.
        self.browse_diff = DiffPane::default();
        if let Some(Browse::FileHistory(v)) = self.browse.last()
            && let Some(i) = v.selected
        {
            self.load_history_entry_diff(i);
        }
    }

    /// Leaves browsing altogether.
    pub fn clear_browse(&mut self) {
        for b in self.browse.drain(..) {
            b.stop();
        }
        self.browse_diff = DiffPane::default();
    }

    /// Shows a commit in the history (closing the browsing views).
    pub fn show_commit(&mut self, id: Oid) {
        self.clear_browse();
        self.view = super::View::History;
        self.reveal_commit(id);
    }

    pub fn select_history_entry(&mut self, i: usize) {
        let Some(Browse::FileHistory(v)) = self.browse.last_mut() else { return };
        if v.selected == Some(i) || i >= v.entries.len() {
            return;
        }
        v.selected = Some(i);
        self.load_history_entry_diff(i);
    }

    fn load_history_entry_diff(&mut self, i: usize) {
        let Some(Browse::FileHistory(v)) = self.browse.last() else { return };
        let Some(entry) = v.entries.get(i) else { return };
        let file = change_of(entry);
        let source = DiffSource::Commit { id: entry.commit.id, parent: entry.commit.parents.first().copied() };
        let seq = self.gens.bump();
        self.browse_diff = DiffPane { file: Some(file.clone()), source: Some(source), seq, ..Default::default() };
        self.worker.diff(seq, source, file, self.diff_opts);
    }

    /// File History from a file of a commit; a file the commit deleted is
    /// followed from the commit before.
    pub fn file_history_of_commit_file(&mut self, commit: Oid, parent: Option<Oid>, file: &ChangedFile) {
        match (file.kind, parent) {
            (ChangeKind::Deleted, Some(p)) => {
                self.open_file_history(p, file.path.clone(), Some(format!("before its deletion in {}", commit.short())))
            }
            (ChangeKind::Deleted, None) => self.error("The file doesn't exist before this commit".into()),
            _ => self.open_file_history(commit, file.path.clone(), None),
        }
    }

    /// Blame from a File History entry (the version before, if that commit
    /// deleted the file).
    pub fn blame_of_history_entry(&mut self, i: usize) {
        let Some(Browse::FileHistory(v)) = self.browse.last() else { return };
        let Some(e) = v.entries.get(i) else { return };
        let (id, parent, path, deleted) = (e.commit.id, e.commit.parents.first().copied(), e.path.clone(), e.status == Some('D'));
        let kind = if deleted { ChangeKind::Deleted } else { ChangeKind::Modified };
        let file = ChangedFile { path, orig_path: None, kind, is_submodule: false };
        self.blame_of_commit_file(id, parent, &file);
    }

    /// Blame of a file of a commit (the version before, if it deleted it).
    pub fn blame_of_commit_file(&mut self, commit: Oid, parent: Option<Oid>, file: &ChangedFile) {
        match (file.kind, parent) {
            (ChangeKind::Deleted, Some(p)) => self.open_blame(Some(p), file.path.clone(), None),
            (ChangeKind::Deleted, None) => self.error("The file doesn't exist before this commit".into()),
            _ => self.open_blame(Some(commit), file.path.clone(), None),
        }
    }

    /// Why File History / Blame of a local change isn't available, if not.
    pub fn local_browse_block(&self, file: &ChangedFile) -> Option<&'static str> {
        match file.kind {
            ChangeKind::Untracked | ChangeKind::Added => Some("Not committed yet"),
            _ => self.path_browse_block(&file.path),
        }
    }

    /// Why a path has no history from HEAD, if it hasn't (a new file, a
    /// staged rename's new name, …).
    pub fn path_browse_block(&self, path: &[u8]) -> Option<&'static str> {
        let Some(head) = self.head_commit() else { return Some("No commits yet") };
        match &self.head_paths {
            Some((h, paths)) if *h == head => (!paths.contains(path)).then_some("Not in the last commit"),
            _ => Some("Checking the last commit…"),
        }
    }

    /// File History of a local change: from HEAD.
    pub fn file_history_of_local(&mut self, path: BString) {
        match self.head_commit() {
            Some(head) => self.open_file_history(head, path, None),
            None => self.error("No commits yet".into()),
        }
    }

    /// Blame of a local file: the work tree if it's there, else HEAD.
    pub fn blame_of_local(&mut self, path: BString) {
        use std::os::unix::ffi::OsStrExt as _;
        let exists = self.paths.workdir.join(std::ffi::OsStr::from_bytes(&path)).is_file();
        let rev = if exists { None } else { self.head_commit() };
        if !exists && rev.is_none() {
            return self.error("No commits yet".into());
        }
        self.open_blame(rev, path, None);
    }

    /// Blame the revision before the selected line's commit, keeping the
    /// line in view.
    pub fn blame_previous(&mut self, line: usize) {
        let Some(Browse::Blame(v)) = self.browse.last() else { return };
        let Some(Ok(doc)) = &v.doc else { return };
        let Some(l) = doc.blame.lines.get(line) else { return };
        let Some((prev, path)) = doc.blame.origins[l.origin].previous.clone() else { return };
        let target = (l.orig_line as usize).saturating_sub(1);
        self.open_blame(Some(prev), path, Some(target));
    }

    pub(super) fn on_file_history_batch(&mut self, seq: Gen, batch: Vec<FileHistoryEntry>) {
        let mut first = false;
        for b in &mut self.browse {
            if let Browse::FileHistory(v) = b
                && v.seq == seq
            {
                first = v.entries.is_empty() && v.selected.is_none();
                v.entries.extend(batch);
                break;
            }
        }
        // Show the newest change right away.
        if first && matches!(self.browse.last(), Some(Browse::FileHistory(v)) if v.seq == seq) {
            self.select_history_entry(0);
        }
    }

    pub(super) fn on_file_history_done(&mut self, seq: Gen, result: anyhow::Result<SearchEnd>) {
        for b in &mut self.browse {
            if let Browse::FileHistory(v) = b
                && v.seq == seq
            {
                v.loading = false;
                if let Err(e) = result {
                    v.error = Some(format!("{e:#}"));
                }
                return;
            }
        }
    }

    pub(super) fn on_blame(&mut self, seq: Gen, result: anyhow::Result<Arc<BlameDoc>>) {
        for b in &mut self.browse {
            if let Browse::Blame(v) = b
                && v.seq == seq
            {
                if let Ok(doc) = &result
                    && let Some(line) = v.scroll_to
                {
                    let last = doc.blame.lines.len().saturating_sub(1);
                    v.scroll_to = Some(line.min(last));
                    v.selected = Some(line.min(last));
                }
                v.doc = Some(result.map_err(|e| format!("{e:#}")));
                return;
            }
        }
    }

    /// The work tree may have changed: work-tree blames whose file (or
    /// HEAD) changed are redone.
    pub(super) fn browse_work_tree_changed(&mut self) {
        let outdated: Vec<usize> = self
            .browse
            .iter()
            .enumerate()
            .filter(|(_, b)| matches!(b, Browse::Blame(v) if v.rev.is_none() && v.stamp != Some(self.work_tree_stamp(&v.path))))
            .map(|(i, _)| i)
            .collect();
        for i in outdated {
            let Browse::Blame(mut view) = self.browse.remove(i) else { unreachable!() };
            self.request_blame(&mut view);
            self.browse.insert(i, Browse::Blame(view));
        }
    }

    /// Stops everything the browsing views are loading (the tab closes).
    pub(super) fn stop_browse(&self) {
        for b in &self.browse {
            b.stop();
        }
    }
}
