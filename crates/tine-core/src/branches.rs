//! Branch and tag operations, with the checks that decide what is allowed
//! before git is asked to do anything.

use anyhow::{Result, bail};
use bstr::ByteSlice;

use crate::{
    model::{GitRef, Head, RefKind, RefsSnapshot},
    refs::branches_in_other_worktrees,
    runner::GitRunner,
};

/// Fails with git's own message if `name` is not a valid branch name.
pub fn validate_branch_name(git: &GitRunner, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("enter a branch name");
    }
    git.output(["check-ref-format", "--branch", name])
        .map_err(|_| anyhow::anyhow!("'{name}' is not a valid branch name"))?;
    Ok(())
}

pub fn validate_tag_name(git: &GitRunner, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("enter a tag name");
    }
    git.output(["check-ref-format", &format!("refs/tags/{name}")])
        .map_err(|_| anyhow::anyhow!("'{name}' is not a valid tag name"))?;
    Ok(())
}

/// Switches to a local branch. Refuses branches checked out in another
/// worktree (git would too, less clearly).
pub fn switch(git: &GitRunner, branch: &str) -> Result<()> {
    if branches_in_other_worktrees(git, git.workdir())?.iter().any(|b| b == branch) {
        bail!("'{branch}' is checked out in another worktree");
    }
    git.output(["switch", branch, "--"])?;
    Ok(())
}

/// Checks out a commit (or tag) without a branch.
pub fn switch_detached(git: &GitRunner, commit: &str) -> Result<()> {
    git.output(["switch", "--detach", commit, "--"])?;
    Ok(())
}

/// What checking out a remote branch `remote/name` should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCheckout {
    /// A local branch already tracks it: switch to that.
    SwitchExisting(String),
    /// No local branch of that name: create one tracking the remote branch.
    CreateTracking(String),
    /// A local branch of that name exists but tracks something else (or
    /// nothing); the user must choose a name or switch to it anyway.
    NameTaken { local: String, upstream: Option<String>, suggestion: String },
}

/// Decides how to check out `remote_branch` (e.g. `origin/feature/x`).
pub fn plan_remote_checkout(refs: &RefsSnapshot, remote_branch: &GitRef) -> RemoteCheckout {
    let name = remote_branch.short_name.split_once('/').map_or(remote_branch.short_name.as_str(), |(_, n)| n);
    let locals: Vec<&GitRef> = refs.refs.iter().filter(|r| r.kind == RefKind::LocalBranch).collect();
    if let Some(l) = locals.iter().find(|l| l.upstream.as_deref() == Some(&remote_branch.short_name)) {
        return RemoteCheckout::SwitchExisting(l.short_name.clone());
    }
    match locals.iter().find(|l| l.short_name == name) {
        None => RemoteCheckout::CreateTracking(name.to_owned()),
        Some(l) => {
            let suggestion = (2..)
                .map(|n| format!("{name}-{n}"))
                .find(|c| !locals.iter().any(|l| &l.short_name == c))
                .expect("some suffix is free");
            RemoteCheckout::NameTaken { local: l.short_name.clone(), upstream: l.upstream.clone(), suggestion }
        }
    }
}

/// Creates `local` tracking `remote_branch` and switches to it.
pub fn create_tracking(git: &GitRunner, local: &str, remote_branch: &str) -> Result<()> {
    validate_branch_name(git, local)?;
    git.output(["switch", "-c", local, "--track", remote_branch, "--"])?;
    Ok(())
}

/// Result of creating a branch and optionally switching to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    Created,
    CreatedAndSwitched,
    /// The branch exists now, but switching to it failed (e.g. local
    /// changes would be overwritten).
    CreatedNotSwitched(String),
}

pub fn create_branch(git: &GitRunner, name: &str, start: &str, checkout: bool) -> Result<Created> {
    validate_branch_name(git, name)?;
    git.output(["branch", name, start])?;
    if !checkout {
        return Ok(Created::Created);
    }
    Ok(match git.output(["switch", name, "--"]) {
        Ok(_) => Created::CreatedAndSwitched,
        Err(e) => Created::CreatedNotSwitched(format!("{e:#}")),
    })
}

pub fn rename_branch(git: &GitRunner, old: &str, new: &str) -> Result<()> {
    validate_branch_name(git, new)?;
    git.output(["branch", "-m", old, new])?;
    Ok(())
}

/// Whether a local branch may be deleted, decided before running git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteCheck {
    /// Fully merged: `branch -d` will work.
    Safe,
    /// Has commits not merged into HEAD or its upstream: needs `-D`.
    Unmerged,
    /// The current branch can't be deleted.
    Current,
    InOtherWorktree,
    /// Already gone (stale view).
    Missing,
}

pub fn check_delete(git: &GitRunner, refs: &RefsSnapshot, branch: &str) -> Result<DeleteCheck> {
    if matches!(&refs.head, Some(Head::Branch { name, .. } | Head::Unborn { name }) if name == branch) {
        return Ok(DeleteCheck::Current);
    }
    if branches_in_other_worktrees(git, git.workdir())?.iter().any(|b| b == branch) {
        return Ok(DeleteCheck::InOtherWorktree);
    }
    let full = format!("refs/heads/{branch}");
    if git.output(["rev-parse", "--verify", "-q", &full]).is_err() {
        return Ok(DeleteCheck::Missing);
    }
    // Mirror `branch -d`: a branch with an existing upstream must be merged
    // into that upstream; otherwise into HEAD.
    let upstream = git.output(["for-each-ref", "--format=%(upstream)", &full])?;
    let upstream = upstream.trim().to_str_lossy().into_owned();
    let base = if !upstream.is_empty() && git.output(["rev-parse", "--verify", "-q", &upstream]).is_ok() {
        upstream
    } else {
        "HEAD".to_owned()
    };
    let merged = git.output(["merge-base", "--is-ancestor", &full, &base]).is_ok();
    let _ = refs;
    Ok(if merged { DeleteCheck::Safe } else { DeleteCheck::Unmerged })
}

/// Deletes a local branch; `force` is required for unmerged branches.
pub fn delete_branch(git: &GitRunner, branch: &str, force: bool) -> Result<()> {
    git.output(["branch", if force { "-D" } else { "-d" }, branch])?;
    Ok(())
}

/// Creates a lightweight tag, or an annotated one when `message` is given.
pub fn create_tag(git: &GitRunner, name: &str, target: &str, message: Option<&str>) -> Result<()> {
    validate_tag_name(git, name)?;
    match message.filter(|m| !m.trim().is_empty()) {
        Some(m) => git.output_with_input(["tag", "-a", name, target, "-F", "-"], m.as_bytes(), &[])?,
        None => git.output(["tag", name, target])?,
    };
    Ok(())
}

pub fn delete_tag(git: &GitRunner, name: &str) -> Result<()> {
    git.output(["tag", "-d", name])?;
    Ok(())
}
