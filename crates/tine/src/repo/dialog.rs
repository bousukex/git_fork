//! State of the modal dialog a repository tab may show (rendered by
//! `ui::dialogs`).

use tine_core::{
    integrate::{MergeMode, ResetMode},
    model::{Oid, RepoState},
};

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
}

impl Integration {
    pub fn describe(&self) -> String {
        match self {
            Integration::Merge { target, .. } => format!("merge {target}"),
            Integration::Rebase { onto } => format!("rebase onto {onto}"),
            Integration::CherryPick { commit } => format!("cherry-pick {commit}"),
            Integration::Revert { commit } => format!("revert {commit}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
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
    Reset { target: Oid, label: String, mode: ResetMode, lost: Vec<String>, changed: bool },
    Stash { message: String, include_untracked: bool, keep_index: bool },
    DropStash { oid: Oid, label: String },
    Abort { state: RepoState },
    /// A cherry-pick/revert/rebase step became empty.
    EmptyStep { state: RepoState },
    /// Marking resolved files that still contain conflict markers.
    MarkResolved { paths: Vec<bstr::BString> },
    UndoStashApply { oid: Oid },
}
