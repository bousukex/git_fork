//! Worktrees in a tab: the list (kept current), opening one, adding,
//! locking, pruning — and removing one, which the app sees through (tabs on
//! that worktree, or in a folder inside it, are checked first and put to
//! sleep, and closed once it is gone).

use std::path::PathBuf;

use tine_core::{
    model::RepoState,
    worktree::{self, Force, Losses, NewWorktree, Worktree},
};

use super::{DiffPane, Needs, RepoTab, dialog::Dialog};
use crate::{
    ops_registry::OpId,
    worker::{FollowUp, Gen, OpKind, OpOutcome},
};

/// What a new worktree checks out (in the dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddHow {
    /// A new branch, named in the dialog, from `start`.
    NewBranch { start: String, start_label: String },
    Existing { branch: String },
    Detached { commit: String, label: String },
}

/// Removing a worktree, as confirmed (the app carries it out): it goes
/// only while it would lose just `losses` (what the dialog showed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveRequest {
    pub worktree: Worktree,
    pub force: Force,
    pub losses: Losses,
}

impl RepoTab {
    pub(super) fn request_worktrees(&mut self) {
        self.worktrees_seq = self.gens.bump();
        self.worker.worktrees(self.worktrees_seq);
    }

    pub(super) fn on_worktrees(&mut self, seq: Gen, result: anyhow::Result<Vec<Worktree>>) {
        if seq != self.worktrees_seq {
            return;
        }
        match result {
            Ok(list) => {
                self.worktrees = list;
                self.worktrees_error = None;
            }
            Err(e) => self.worktrees_error = Some(format!("{e:#}")),
        }
    }

    /// The main worktree (git lists it first).
    pub fn main_worktree(&self) -> Option<&Worktree> {
        self.worktrees.first().filter(|w| w.main)
    }

    /// This tab's own worktree.
    pub fn is_this_worktree(&self, w: &Worktree) -> bool {
        worktree::same_place(&w.path, &self.paths.workdir)
    }

    /// Opens worktree `w` in a tab (or brings its tab forward).
    pub fn open_worktree(&mut self, w: &Worktree) {
        if w.bare || !w.path.is_dir() {
            self.error(format!("{} can't be opened: its folder isn't there", w.path.display()));
            return;
        }
        self.open_request = Some(w.path.clone());
    }

    /// The dialog for a new worktree: of `branch` (checked out nowhere
    /// else), or with a new branch from HEAD.
    pub fn request_add_worktree(&mut self, branch: Option<String>) {
        let Some(main) = self.main_worktree().map(|w| w.path.clone()) else {
            self.error("The worktrees aren't listed yet; try again in a moment".into());
            return;
        };
        let (how, name) = match branch {
            Some(b) => (AddHow::Existing { branch: b.clone() }, b),
            None => (AddHow::NewBranch { start: "HEAD".into(), start_label: self.current_branch() }, String::new()),
        };
        let path = worktree::default_path(&main, if name.is_empty() { "worktree" } else { &name });
        let path_text = path.to_string_lossy().into_owned();
        self.show_dialog(Dialog::AddWorktree { path, path_text, path_edited: false, how, name, open: true });
    }

    /// Picks the folder to put a new worktree in: the dialog showing now
    /// takes it (not one opened later).
    pub fn pick_worktree_folder(&mut self) {
        if self.folder_picker.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.worker.ctx().clone();
        let pick = self.folder_dialog.clone();
        std::thread::spawn(move || {
            let _ = tx.send(pick());
            ctx.request_repaint();
        });
        self.folder_picker = Some((self.modal_token(), rx));
    }

    /// Picks folders with `pick` instead of the desktop's dialog (tests).
    #[doc(hidden)]
    pub fn set_folder_dialog_for_test(&mut self, pick: impl Fn() -> Option<PathBuf> + Send + Sync + 'static) {
        self.folder_dialog = std::sync::Arc::new(pick);
    }

    /// A folder was picked: the new worktree goes in it — if the dialog it
    /// was picked for is still the one showing.
    pub(super) fn poll_folder_picker(&mut self) {
        let Some((token, rx)) = &self.folder_picker else { return };
        let Ok(dir) = rx.try_recv() else { return };
        let token = *token;
        self.folder_picker = None;
        let Some(dir) = dir else { return };
        if token != self.modal_token() {
            return;
        }
        let main = self.main_worktree().map(|m| m.path.clone());
        if let Some(Dialog::AddWorktree { path, path_text, path_edited, name, how, .. }) = &mut self.dialog {
            let branch = match how {
                AddHow::Existing { branch } => branch.clone(),
                _ => name.clone(),
            };
            let leaf = main.and_then(|m| worktree::default_path(&m, if branch.is_empty() { "worktree" } else { &branch }).file_name().map(|n| n.to_owned()));
            *path = leaf.map_or(dir.clone(), |l| dir.join(l));
            *path_text = path.to_string_lossy().into_owned();
            *path_edited = true;
        }
    }

    /// Where new worktrees go by default for a branch named `name`.
    pub fn default_worktree_path(&self, name: &str) -> Option<PathBuf> {
        let main = self.main_worktree()?;
        Some(worktree::default_path(&main.path, if name.is_empty() { "worktree" } else { name }))
    }

    pub fn add_worktree(&mut self, path: PathBuf, how: NewWorktree, open: bool) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let label = match &how {
            NewWorktree::NewBranch { name, .. } | NewWorktree::Existing { branch: name } => name.clone(),
            NewWorktree::Detached { commit } => commit.clone(),
        };
        self.run_op(OpKind::AddWorktree(label), move |git| {
            let added = worktree::add(git, &path, &how)?;
            Ok(OpOutcome::new(format!("Added the worktree {}", path.display()))
                .moving_refs()
                .with(FollowUp::WorktreeAdded { path, open, warning: added.warning }))
        });
    }

    pub fn request_lock_worktree(&mut self, w: &Worktree) {
        self.show_dialog(Dialog::LockWorktree { worktree: w.clone(), reason: String::new() });
    }

    pub fn lock_worktree(&mut self, w: Worktree, reason: String) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        self.run_op(OpKind::LockWorktree(w.name()), move |git| {
            worktree::lock(git, &w, &reason)?;
            Ok(OpOutcome::new(format!("Locked {}", w.name())).with(FollowUp::Worktrees))
        });
    }

    pub fn unlock_worktree(&mut self, w: Worktree) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        self.run_op(OpKind::UnlockWorktree(w.name()), move |git| {
            worktree::unlock(git, &w)?;
            Ok(OpOutcome::new(format!("Unlocked {}", w.name())).with(FollowUp::Worktrees))
        });
    }

    /// Lists what pruning would remove, then asks — unless another dialog
    /// came up meanwhile (it isn't replaced).
    pub fn request_prune_worktrees(&mut self) {
        self.prune_seq = self.gens.bump();
        self.prune_token = self.modal_token();
        self.worker.prune_preview(self.prune_seq, self.git.clone());
    }

    pub(super) fn on_prune_preview(&mut self, seq: Gen, result: anyhow::Result<Vec<String>>) {
        if seq != self.prune_seq || seq == 0 {
            return;
        }
        self.prune_seq = 0;
        match result {
            Ok(lines) if lines.is_empty() => self.activity.push("No worktree records to prune".into()),
            // Some dialog was shown since it was asked for.
            Ok(_) if self.modal_open() || self.modal_token() != self.prune_token => {}
            Ok(lines) => self.show_dialog(Dialog::PruneWorktrees { lines }),
            Err(e) => self.error(format!("Can't tell what pruning would remove: {e:#}")),
        }
    }

    pub fn prune_worktrees(&mut self) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        self.run_op(OpKind::PruneWorktrees, |git| {
            worktree::prune(git)?;
            Ok(OpOutcome::new("Pruned worktree records").with(FollowUp::Worktrees))
        });
    }

    /// Asks before removing `w`, with what it would lose (looked at first).
    pub fn request_remove_worktree(&mut self, w: &Worktree) {
        if w.main {
            self.error("The main worktree can't be removed".into());
            return;
        }
        let seq = self.gens.bump();
        self.worker.worktree_losses(seq, self.git.clone(), w.clone());
        self.show_dialog(Dialog::RemoveWorktree { worktree: w.clone(), losses: None, seq, discard: false });
    }

    pub(super) fn on_worktree_losses(&mut self, seq: Gen, path: PathBuf, result: anyhow::Result<Losses>) {
        if let Some(Dialog::RemoveWorktree { worktree, losses, seq: s, .. }) = &mut self.dialog
            && *s == seq
            && worktree.path == path
        {
            *losses = Some(result.map_err(|e| format!("{e:#}")));
        }
    }

    /// Confirmed (losing `losses`): the app checks the tabs on it, then has
    /// this tab remove it.
    pub fn confirm_remove_worktree(&mut self, worktree: Worktree, force: Force, losses: Losses) {
        self.remove_requested = Some(RemoveRequest { worktree, force, losses });
    }

    /// Why this tab's worktree (or a folder this tab is in) can't be
    /// removed now: it has work that would be lost, or is busy.
    pub fn removal_block(&self) -> Option<String> {
        if self.merge_dirty() {
            return Some("the merge editor has unsaved changes there".into());
        }
        if !self.commit_subject.trim().is_empty() || !self.commit_body.trim().is_empty() {
            return Some("a commit message is being written there".into());
        }
        if self.status.state != RepoState::Clean {
            return Some(format!("an operation ({:?}) is in progress there", self.status.state));
        }
        if self.busy() {
            return Some("an operation is running there".into());
        }
        None
    }

    /// Removes worktree `req.worktree` (from the main worktree, so also
    /// this tab's own) as confirmed; the operation, if it started.
    pub fn remove_worktree(&mut self, req: RemoveRequest) -> Option<OpId> {
        if !self.allowed(Needs::Idle) {
            return None;
        }
        let Some(main) = self.main_worktree().map(|m| m.path.clone()) else {
            self.error("The worktrees aren't listed yet; try again in a moment".into());
            return None;
        };
        let RemoveRequest { worktree: w, force, losses } = req;
        self.run_op(OpKind::RemoveWorktree(w.path.clone()), move |git| {
            worktree::remove_as_confirmed(&git.with_workdir(&main), &w, &losses, force)?;
            Ok(OpOutcome::new(format!("Removed the worktree {}", w.path.display())).with(FollowUp::Worktrees))
        })
    }

    /// The worktree `removing` is being removed and this tab is on it (or
    /// in a folder inside it): it goes to sleep — no watching, no loading,
    /// no dialogs, nothing to do — until the app closes it, or wakes it
    /// once its repository is known to be there as it was.
    pub fn sleep_for_removal(&mut self, removing: PathBuf) {
        self._watcher = None;
        self.worker.pause();
        self.clear_browse();
        self.stop_rebase_range();
        if let Some(c) = self.files_cancel.take() {
            c.cancel();
        }
        self.files_seq = 0;
        self.head_paths_seq = 0;
        self.prune_seq = 0;
        self.folder_picker = None;
        self.commit_diff = DiffPane::default();
        self.changes_diff = DiffPane::default();
        self.browse_diff = DiffPane::default();
        self.commit = Default::default();
        self.selected_commit = None;
        self.changes_selected = None;
        self.changes_multi.clear();
        // Nothing unsaved in it (checked before).
        self.merge = None;
        self.dialog = None;
        self.pending_discard = None;
        self.pending_line_discard = None;
        self.removal_pending = Some(removing);
    }

    /// Asleep for a worktree removal (see [`Self::sleep_for_removal`]).
    pub fn asleep(&self) -> bool {
        self.removal_pending.is_some()
    }

    /// The removal failed and this tab's repository is still there as it
    /// was: back to work, reloaded.
    pub fn wake(&mut self) {
        self.removal_pending = None;
        self.worker.resume();
        self.start_watching();
        self.refresh();
    }

    pub fn start_watching(&mut self) {
        if self._watcher.is_none() {
            self._watcher = Some(crate::watcher::RepoWatcher::start(
                self.paths.workdir.clone(),
                self.paths.git_dir.clone(),
                self.paths.common_dir.clone(),
                self.git.clone(),
                self.worker.sender(),
                self.worker.ctx().clone(),
            ));
        }
    }

    /// Whether the file watcher runs (tests).
    #[doc(hidden)]
    pub fn watching(&self) -> bool {
        self._watcher.is_some()
    }
}
