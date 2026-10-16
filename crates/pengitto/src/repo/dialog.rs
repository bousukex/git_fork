//! State of the modal dialog a repository tab may show (rendered by
//! `ui::dialogs`).

use pengitto_core::{
    integrate::{MergeMode, ResetMode},
    model::{Oid, RepoState},
};

/// A reset that only makes sense while HEAD is still at `head` on `branch`
/// (undoing an interactive rebase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetGuard {
    /// Full ref (`None`: detached HEAD).
    pub branch: Option<String>,
    pub head: Oid,
}

/// What a checkout switches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    /// An existing local branch.
    Branch(String),
    /// A new local branch tracking a remote branch.
    Track { local: String, remote: String },
    /// A commit or tag, without a branch.
    Detached { target: String, label: String },
}

impl CheckoutTarget {
    pub fn label(&self) -> String {
        match self {
            CheckoutTarget::Branch(b) => b.clone(),
            CheckoutTarget::Track { local, remote } => format!("{local} (tracking {remote})"),
            CheckoutTarget::Detached { label, .. } => label.clone(),
        }
    }
}

/// An operation that needs a clean work tree, waiting for the user to
/// decide what to do with local changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integration {
    Merge { target: String, mode: MergeMode },
    Rebase { onto: String },
    CherryPick { commit: String },
    Revert { commit: String },
    /// Pull `branch` (its name on the remote) from `remote`.
    Pull { remote: String, branch: String, rebase: bool },
    InteractiveRebase { range: Box<pengitto_core::interactive::Range>, plan: Box<pengitto_core::interactive::Plan> },
}

impl Integration {
    pub fn describe(&self) -> String {
        match self {
            Integration::Merge { target, .. } => format!("merge {target}"),
            Integration::Rebase { onto } => format!("rebase onto {onto}"),
            Integration::CherryPick { commit } => format!("cherry-pick {commit}"),
            Integration::Revert { commit } => format!("revert {commit}"),
            Integration::Pull { remote, branch, .. } => format!("pull {remote}/{branch}"),
            Integration::InteractiveRebase { .. } => "rebase interactively".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Planning an interactive rebase.
    InteractiveRebase(Box<super::rebase::RebaseDialog>),
    CreateBranch { start: String, start_label: String, name: String, checkout: bool },
    RenameBranch { old: String, new: String },
    /// `force`: the branch has unmerged commits.
    DeleteBranch { name: String, force: bool },
    CreateTag { target: String, target_label: String, name: String, message: String },
    DeleteTag { name: String },
    /// A local branch named like the remote branch already exists but
    /// tracks something else.
    RemoteNameTaken { remote: String, local: String, upstream: Option<String>, new_name: String },
    Detach { target: String, label: String },
    /// Switching failed because of local changes.
    StashAndSwitch { target: CheckoutTarget },
    Merge { target: String, label: String, mode: MergeMode, stash_first: bool },
    Rebase { onto: String, label: String, stash_first: bool },
    /// An integration with local changes present (decided in the UI, or
    /// found right before running).
    DirtyTree { action: Integration },
    /// `lost`: what a hard reset destroys, as confirmed by the user; the
    /// reset re-checks it right before running.
    Reset { target: Oid, label: String, mode: ResetMode, lost: Vec<String>, changed: bool, guard: Option<ResetGuard> },
    Stash { message: String, include_untracked: bool, keep_index: bool },
    DropStash { oid: Oid, label: String },
    Abort { state: RepoState },
    /// A cherry-pick/revert/rebase step became empty.
    EmptyStep { state: RepoState },
    /// Marking resolved files that still contain conflict markers.
    MarkResolved { paths: Vec<bstr::BString> },
    UndoStashApply { oid: Oid },
    /// `remote: None` fetches all remotes.
    Fetch { remote: Option<String>, prune: bool },
    /// `remote_branch`: e.g. `origin/main` (empty until chosen).
    Pull { remote_branch: String, rebase: bool, stash_first: bool },
    /// `lease`: the remote branch a force push may replace, as (remote,
    /// branch, where its remote-tracking ref pointed when the dialog showed
    /// it). Taken again only when the user picks another target.
    Push {
        branch: String,
        remote: String,
        remote_branch: String,
        set_upstream: bool,
        tags: bool,
        force: bool,
        lease: (String, String, Option<Oid>),
    },
    /// Deleting `full_ref` (e.g. `refs/heads/x`) on `remote`.
    DeleteRemoteRef { remote: String, full_ref: String },
    /// A new worktree at `path` (`path_edited`: typed or picked, so the
    /// branch name no longer sets it); `name`: the new branch's.
    /// `path`: where it goes (any bytes); `path_text`: that, as edited
    /// (typing sets `path` from it).
    AddWorktree {
        path: std::path::PathBuf,
        path_text: String,
        path_edited: bool,
        how: super::worktree::AddHow,
        name: String,
        open: bool,
    },
    LockWorktree { worktree: pengitto_core::worktree::Worktree, reason: String },
    /// `losses`: what it would lose (`None` while looked at); `discard`:
    /// confirmed that it goes.
    RemoveWorktree {
        worktree: pengitto_core::worktree::Worktree,
        losses: Option<Result<pengitto_core::worktree::Losses, String>>,
        seq: crate::worker::Gen,
        discard: bool,
    },
    /// What `git worktree prune` would remove.
    PruneWorktrees { lines: Vec<String> },
    /// Giving up the merge editor's unsaved changes, then `then`.
    LeaveMerge { then: super::merge::MergeLeave },
    /// Editing the merge editor's result as text (`editor`: which opening
    /// of the editor it is for).
    EditMergeResult { editor: crate::worker::Gen, text: String, original: String },
}
