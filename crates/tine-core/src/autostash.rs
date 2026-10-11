//! Running an operation that needs a clean work tree by stashing local
//! changes first and restoring them afterwards.
//!
//! Every outcome says where the user's changes are, so they can always be
//! recovered: restored in place, still in the stash (with its id), or
//! applied with conflicts. Restoring uses an uncancellable runner, so a
//! cancelled operation still gives the changes back.

use std::path::Path;

use anyhow::Result;
use bstr::{BString, ByteSlice};

use crate::{
    model::{Oid, RepoState},
    runner::GitRunner,
    stash,
    status::{self, repo_state},
};

/// Where the local changes ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStash {
    /// There were no changes; nothing was stashed.
    NotNeeded,
    /// Stashed, then re-applied and the stash dropped.
    Restored,
    /// The operation stopped half-way (conflicts): the changes stay stashed
    /// until it is finished or aborted.
    Kept(Oid),
    /// Re-applying conflicted; the stash was kept.
    Conflicted { oid: Oid, created_untracked: Vec<BString> },
    /// Re-applied, but what was staged came back unstaged; the stash is kept
    /// as the record of the staging.
    RestoredUnstaged(Oid),
    /// Not re-applied (e.g. a failed operation moved HEAD); stash kept.
    NotRestored { oid: Oid, reason: String },
    /// Re-applying failed for another reason; the stash was kept.
    RestoreFailed(Oid, String),
}

impl AutoStash {
    /// The stash still holding the changes, if any.
    pub fn stash(&self) -> Option<Oid> {
        match self {
            AutoStash::Kept(o)
            | AutoStash::Conflicted { oid: o, .. }
            | AutoStash::RestoredUnstaged(o)
            | AutoStash::NotRestored { oid: o, .. }
            | AutoStash::RestoreFailed(o, _) => Some(*o),
            AutoStash::NotNeeded | AutoStash::Restored => None,
        }
    }

    /// One line for the user about where their changes are.
    pub fn describe(&self) -> Option<String> {
        match self {
            AutoStash::NotNeeded | AutoStash::Restored => None,
            AutoStash::Kept(o) => Some(format!(
                "your local changes are saved in stash {}; apply it after finishing the operation",
                o.short()
            )),
            AutoStash::Conflicted { oid, .. } => Some(format!(
                "re-applying your local changes conflicted; resolve the files (the changes are also kept in stash {})",
                oid.short()
            )),
            AutoStash::RestoredUnstaged(o) => Some(format!(
                "your local changes are back, but what was staged is now unstaged; stash {} keeps the original staging",
                o.short()
            )),
            AutoStash::NotRestored { oid, reason } => {
                Some(format!("your local changes were not re-applied ({reason}); they are kept in stash {}", oid.short()))
            }
            AutoStash::RestoreFailed(o, e) => {
                Some(format!("could not re-apply your local changes ({e}); they are kept in stash {}", o.short()))
            }
        }
    }
}

/// HEAD as (branch, commit) for detecting that an operation moved it.
fn head(git: &GitRunner) -> (Option<String>, Option<String>) {
    let branch = git.output(["symbolic-ref", "-q", "HEAD"]).ok().map(|b| b.trim().to_str_lossy().into_owned());
    let oid = git.output(["rev-parse", "-q", "--verify", "HEAD"]).ok().map(|b| b.trim().to_str_lossy().into_owned());
    (branch, oid)
}

fn restore(git: &GitRunner, oid: Oid) -> AutoStash {
    match stash::apply(git, oid) {
        Ok(a) if a.conflicts => AutoStash::Conflicted { oid, created_untracked: a.created_untracked },
        Ok(a) if !a.index_restored => AutoStash::RestoredUnstaged(oid),
        Ok(_) => match stash::drop(git, oid) {
            Ok(()) => AutoStash::Restored,
            Err(e) => AutoStash::RestoreFailed(oid, format!("{e:#}")),
        },
        Err(e) => AutoStash::RestoreFailed(oid, format!("{e:#}")),
    }
}

/// Runs `op` with local changes stashed away (including untracked files).
/// With `any_change`, untracked files alone also trigger stashing (they can
/// block a checkout); otherwise only changes to tracked files do.
pub fn run<T>(
    git: &GitRunner,
    git_dir: &Path,
    label: &str,
    any_change: bool,
    op: impl FnOnce() -> Result<T>,
) -> (Result<T>, AutoStash) {
    run_with_stash(git, git_dir, label, any_change, |_| op())
}

/// [`run`], telling `op` the stash it made (if any), e.g. to record it
/// before starting something that may stop half-way.
pub fn run_with_stash<T>(
    git: &GitRunner,
    git_dir: &Path,
    label: &str,
    any_change: bool,
    op: impl FnOnce(Option<Oid>) -> Result<T>,
) -> (Result<T>, AutoStash) {
    let dirty = if any_change { status::has_any_changes(git) } else { status::has_tracked_changes(git) };
    let dirty = match dirty {
        Ok(d) => d,
        Err(e) => return (Err(e), AutoStash::NotNeeded),
    };
    if !dirty {
        return (op(None), AutoStash::NotNeeded);
    }
    let start = head(git);
    let oid = match stash::push(git, &format!("tine: auto stash before {label}"), true, false) {
        Ok(Some(oid)) => oid,
        Ok(None) => return (op(None), AutoStash::NotNeeded),
        Err(e) => return (Err(e), AutoStash::NotNeeded),
    };
    let result = op(Some(oid));
    // Recovery must complete even if the user cancelled the operation.
    let git = git.uncancellable();
    let report = if repo_state(git_dir) != RepoState::Clean {
        // Stopped half-way: re-applying now would mix the user's changes
        // into the conflict resolution.
        AutoStash::Kept(oid)
    } else if result.is_err() && head(&git) != start {
        let now = head(&git);
        let at = now.0.or(now.1).unwrap_or_else(|| "an unborn branch".into());
        AutoStash::NotRestored { oid, reason: format!("the failed operation left HEAD at {at}") }
    } else {
        restore(&git, oid)
    };
    (result, report)
}
