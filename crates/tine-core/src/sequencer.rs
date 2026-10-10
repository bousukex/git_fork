//! Finishing, skipping or abandoning multi-step operations, and resolving
//! conflicted files.

use std::path::Path;

use anyhow::{Result, bail};
use bstr::{BString, ByteSlice};

use crate::{
    integrate::{Step, run_step},
    model::RepoState,
    runner::GitRunner,
};

fn command(state: RepoState) -> Result<&'static str> {
    Ok(match state {
        RepoState::Merging => "merge",
        RepoState::Rebasing => "rebase",
        RepoState::CherryPicking => "cherry-pick",
        RepoState::Reverting => "revert",
        RepoState::Bisecting | RepoState::Clean => bail!("no operation in progress"),
    })
}

/// Continues a rebase, cherry-pick or revert after its conflicts were
/// resolved. A merge is finished by committing instead (the commit box).
pub fn continue_op(git: &GitRunner, git_dir: &Path, state: RepoState) -> Result<Step> {
    if state == RepoState::Merging {
        bail!("a merge is finished by committing");
    }
    let cmd = command(state)?;
    run_step(git, git_dir, &[cmd, "--continue"], state)
}

/// Skips the current commit of a rebase, cherry-pick or revert.
pub fn skip(git: &GitRunner, git_dir: &Path, state: RepoState) -> Result<Step> {
    if state == RepoState::Merging {
        bail!("a merge can't skip; abort it instead");
    }
    let cmd = command(state)?;
    run_step(git, git_dir, &[cmd, "--skip"], state)
}

/// Abandons the operation and returns to the state before it started.
pub fn abort(git: &GitRunner, state: RepoState) -> Result<()> {
    let cmd = command(state)?;
    git.output([cmd, "--abort"])?;
    Ok(())
}

/// Records an empty commit for a cherry-pick whose changes are already
/// present (the alternative to skipping it).
pub fn commit_empty(git: &GitRunner) -> Result<()> {
    git.output(["commit", "--allow-empty", "--no-edit"])?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Take HEAD's version ("ours").
    Ours,
    /// Take the incoming version ("theirs").
    Theirs,
    /// Resolve by deleting the file.
    Delete,
    /// The file has been edited by hand; stage it as is.
    MarkResolved,
}

fn paths_os(paths: &[BString]) -> Vec<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    paths.iter().map(|p| std::ffi::OsString::from_vec(p.to_vec())).collect()
}

pub fn resolve(git: &GitRunner, paths: &[BString], how: Resolution) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let os = paths_os(paths);
    let run = |head: &[&str]| -> Result<()> {
        let mut args: Vec<std::ffi::OsString> = head.iter().map(Into::into).collect();
        args.push("--".into());
        args.extend(os.iter().cloned());
        git.output(&args)?;
        Ok(())
    };
    match how {
        Resolution::Ours => {
            run(&["checkout", "--ours"])?;
            run(&["add"])
        }
        Resolution::Theirs => {
            run(&["checkout", "--theirs"])?;
            run(&["add"])
        }
        Resolution::Delete => run(&["rm", "-q", "--ignore-unmatch"]),
        Resolution::MarkResolved => run(&["add"]),
    }
}

/// Whether a work tree file still contains conflict markers.
pub fn has_conflict_markers(workdir: &Path, path: &BString) -> bool {
    let Ok(p) = path.to_path() else { return false };
    let Ok(content) = std::fs::read(workdir.join(p)) else { return false };
    content.lines().any(|l| l.starts_with(b"<<<<<<< ") || l == b"=======" || l.starts_with(b">>>>>>> "))
}

/// Opens the configured merge tool for one file; fails with guidance when
/// none is configured.
pub fn mergetool(git: &GitRunner, path: &BString) -> Result<()> {
    if git.output(["config", "--get", "merge.tool"]).is_err() {
        bail!(
            "no merge tool is configured; set one with e.g. `git config --global merge.tool meld` \
             (meld, kdiff3 and vscode are common choices)"
        );
    }
    let mut args: Vec<std::ffi::OsString> = ["mergetool", "--no-prompt", "--"].iter().map(Into::into).collect();
    args.extend(paths_os(std::slice::from_ref(path)));
    git.output(&args)?;
    Ok(())
}
