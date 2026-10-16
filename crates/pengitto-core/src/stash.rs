//! Stash operations addressed by object id: `stash@{n}` indices shift when
//! stashes are added or dropped (possibly by another program meanwhile), so
//! every operation re-resolves the index from the OID right before it runs.

use std::{collections::HashSet, path::Path};

use anyhow::{Context as _, Result, bail};
use bstr::{BString, ByteSlice};

use crate::{
    model::{ChangeKind, Oid, RepoState},
    refs,
    runner::GitRunner,
    status,
};

/// `git stash` relies on pathspec magic internally (e.g. to remove the
/// untracked files it saved); with literal pathspecs it would leave them in
/// the work tree. We never pass user paths to it, so turn literal mode off.
fn stash_git(git: &GitRunner) -> GitRunner {
    git.clone().with_env("GIT_LITERAL_PATHSPECS", "0")
}

/// Stashes changes; returns the new stash's id, or `None` when there was
/// nothing to stash.
pub fn push(git: &GitRunner, message: &str, include_untracked: bool, keep_index: bool) -> Result<Option<Oid>> {
    let git = &stash_git(git);
    let before = top(git)?;
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if keep_index {
        args.push("--keep-index");
    }
    if !message.trim().is_empty() {
        args.extend(["-m", message]);
    }
    git.output(&args)?;
    let after = top(git)?;
    Ok(if after != before { after } else { None })
}

/// The newest stash, if any.
fn top(git: &GitRunner) -> Result<Option<Oid>> {
    Ok(match git.output(["rev-parse", "--verify", "-q", "refs/stash"]) {
        Ok(out) => Oid::from_hex(out.trim()),
        Err(_) => None,
    })
}

/// Current `stash@{n}` index of the stash with id `oid`.
pub fn index_of(git: &GitRunner, oid: Oid) -> Result<Option<usize>> {
    let snapshot = refs::snapshot(git)?;
    Ok(snapshot.stashes.iter().find(|s| s.id == oid).map(|s| s.index))
}

fn require_index(git: &GitRunner, oid: Oid) -> Result<usize> {
    index_of(git, oid)?.with_context(|| format!("stash {} no longer exists", oid.short()))
}

/// What applying a stash did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Some files conflicted.
    pub conflicts: bool,
    /// Staged changes came back staged (`--index`). When false, their
    /// content was applied as unstaged changes and the stash should be
    /// kept, since it is the only record of what was staged.
    pub index_restored: bool,
    /// No tracked file had changes right before applying.
    pub was_clean_before: bool,
    /// Untracked files from the stash that this apply created (and that
    /// did not exist before) — the only files an undo may remove.
    pub created_untracked: Vec<BString>,
}

impl Applied {
    /// Safe to drop the stash after this apply.
    pub fn complete(&self) -> bool {
        !self.conflicts && self.index_restored
    }
}

fn exists(git: &GitRunner, p: &BString) -> bool {
    p.to_path().is_ok_and(|p| git.workdir().join(p).symlink_metadata().is_ok())
}

/// Applies a stash, restoring its staged changes as staged when possible.
pub fn apply(git: &GitRunner, oid: Oid) -> Result<Applied> {
    let git = &stash_git(git);
    let was_clean_before = !status::has_tracked_changes(git)?;
    let stash_untracked = untracked_files(git, oid)?;
    let pre_existing: HashSet<&BString> = stash_untracked.iter().filter(|p| exists(git, p)).collect();
    let hex = oid.to_hex();

    let (conflicts, index_restored) = match git.output(["stash", "apply", "--index", &hex]) {
        Ok(_) => (false, true),
        Err(e) => {
            if status::has_conflicts(git)? {
                (true, false)
            } else if format!("{e:#}").contains("--index") {
                // The index can't be restored; apply the content only.
                match git.output(["stash", "apply", &hex]) {
                    Ok(_) => (false, false),
                    Err(e2) if status::has_conflicts(git)? => {
                        let _ = e2;
                        (true, false)
                    }
                    Err(e2) => return Err(e2),
                }
            } else {
                return Err(e);
            }
        }
    };
    let created_untracked =
        stash_untracked.iter().filter(|p| !pre_existing.contains(p) && exists(git, p)).cloned().collect();
    Ok(Applied { conflicts, index_restored, was_clean_before, created_untracked })
}

/// Drops the stash with id `oid`.
pub fn drop(git: &GitRunner, oid: Oid) -> Result<()> {
    let git = &stash_git(git);
    let index = require_index(git, oid)?;
    git.output(["stash", "drop", "-q", &format!("stash@{{{index}}}")])?;
    Ok(())
}

/// Applies and — only if that was complete — drops the stash (like `stash
/// pop`, but by id and never losing what was staged).
pub fn pop(git: &GitRunner, oid: Oid) -> Result<Applied> {
    require_index(git, oid)?;
    let applied = apply(git, oid)?;
    if applied.complete() {
        drop(git, oid)?;
    }
    Ok(applied)
}

/// Untracked files saved in a stash (its third parent), if any.
pub fn untracked_files(git: &GitRunner, oid: Oid) -> Result<Vec<BString>> {
    let third = format!("{}^3", oid.to_hex());
    if git.output(["rev-parse", "--verify", "-q", &third]).is_err() {
        return Ok(Vec::new());
    }
    let out = git.output(["ls-tree", "-r", "-z", "--name-only", &third])?;
    Ok(out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(BString::from).collect())
}

/// Undoes applying a stash onto a work tree that had no tracked changes
/// before: resets tracked files to HEAD and removes the untracked files the
/// apply created (`created`, from [`Applied`]), if they are still untracked.
pub fn undo_apply(git: &GitRunner, git_dir: &Path, created: &[BString]) -> Result<()> {
    if status::repo_state(git_dir) != RepoState::Clean {
        bail!("another operation is in progress");
    }
    git.output(["reset", "-q", "--hard", "HEAD"])?;
    let still_untracked: HashSet<BString> = status::read(git)?
        .unstaged
        .into_iter()
        .filter(|f| f.kind == ChangeKind::Untracked)
        .map(|f| f.path)
        .collect();
    for p in created.iter().filter(|p| still_untracked.contains(*p)) {
        let path = git.workdir().join(p.to_path().context("invalid path")?);
        std::fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}
