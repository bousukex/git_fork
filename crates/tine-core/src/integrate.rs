//! Operations that bring commits together: merge, rebase, cherry-pick,
//! revert, and reset. Conflicts are a normal result, not an error: the
//! repository is then in a multi-step state handled by [`crate::sequencer`].

use std::{collections::HashSet, path::Path};

use anyhow::{Result, bail};
use bstr::{BString, ByteSlice};

use crate::{
    model::{ChangeKind, RepoState},
    runner::GitRunner,
    status::{self, repo_state},
};

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Done,
    /// Stopped with conflicts; the repository is in a multi-step state.
    Conflicts,
    /// A cherry-pick/revert/rebase step would produce an empty commit.
    Empty,
}

/// Runs a command that stops with exit code 1 when it hits conflicts. A
/// failure counts as conflicts only if unmerged index entries exist, and as
/// an empty step only if git says so; any other error (a hook, signing, a
/// lock) is returned as is.
pub(crate) fn run_step(git: &GitRunner, git_dir: &Path, args: &[&str], expect: RepoState) -> Result<Step> {
    run_step_with(git, git_dir, expect, || git.output(args))
}

/// [`run_step`] for a command run by `command` (e.g. with progress).
pub(crate) fn run_step_with(
    git: &GitRunner,
    git_dir: &Path,
    expect: RepoState,
    command: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Step> {
    match command() {
        Ok(_) => Ok(if status::has_conflicts(git)? { Step::Conflicts } else { Step::Done }),
        Err(e) => {
            if e.downcast_ref::<crate::Cancelled>().is_some() {
                return Err(e);
            }
            if status::has_conflicts(git)? {
                return Ok(Step::Conflicts);
            }
            let msg = format!("{e:#}");
            let empty = msg.contains("is now empty") || msg.contains("nothing to commit") || msg.contains("allow-empty");
            if empty && repo_state(git_dir) == expect {
                return Ok(Step::Empty);
            }
            Err(e)
        }
    }
}

/// Uncommitted changes to tracked files block an operation that could not
/// restore them on abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyWorkTree;

impl std::fmt::Display for DirtyWorkTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("there are uncommitted changes")
    }
}

impl std::error::Error for DirtyWorkTree {}

/// Checks, right before running, that no other operation is in progress,
/// nothing is conflicted, and (unless `allow_dirty`) tracked files have no
/// changes. The UI's status may be stale; this reads git directly.
pub fn ensure_ready(git: &GitRunner, git_dir: &Path, allow_dirty: bool) -> Result<()> {
    let state = repo_state(git_dir);
    if state != RepoState::Clean {
        bail!("another operation ({state:?}) is in progress");
    }
    if status::has_conflicts(git)? {
        bail!("there are unresolved conflicts");
    }
    if !allow_dirty && status::has_tracked_changes(git)? {
        return Err(DirtyWorkTree.into());
    }
    Ok(())
}

/// Everything `reset --hard <target>` would destroy: uncommitted changes to
/// tracked files, plus untracked files and directories that files of the
/// target commit would replace. Untracked entries are marked.
pub fn hard_reset_losses(git: &GitRunner, target: &str) -> Result<Vec<String>> {
    let st = status::read(git)?;
    let mut lost: Vec<String> = st
        .staged
        .iter()
        .chain(&st.conflicted)
        .chain(st.unstaged.iter().filter(|f| f.kind != ChangeKind::Untracked))
        .map(|f| f.path.to_str_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let tree_out = git.output(["ls-tree", "-r", "-z", "--name-only", target, "--"])?;
    let files: HashSet<&[u8]> = tree_out.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
    let dirs: HashSet<&[u8]> = files
        .iter()
        .flat_map(|f| f.iter().enumerate().filter(|(_, b)| **b == b'/').map(move |(i, _)| &f[..i]))
        .collect();
    let mut untracked: Vec<String> = st
        .unstaged
        .iter()
        .filter(|f| f.kind == ChangeKind::Untracked)
        .filter(|f| {
            let p: &BString = &f.path;
            let p = p.as_slice();
            // Same path, a directory in the target, or under a file of it.
            files.contains(p)
                || dirs.contains(p)
                || p.iter().enumerate().any(|(i, b)| *b == b'/' && files.contains(&p[..i]))
        })
        .map(|f| format!("{} (untracked)", f.path.to_str_lossy()))
        .collect();
    untracked.sort();
    lost.extend(untracked);
    Ok(lost)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeMode {
    /// Fast-forward when possible, otherwise a merge commit.
    #[default]
    Default,
    /// Always create a merge commit.
    NoFastForward,
    /// Only fast-forward; fail otherwise.
    FastForwardOnly,
}

/// Merges `target`. A full ref name (`refs/heads/x`) avoids ambiguity with
/// same-named tags; the message is built as if the short name was given.
pub fn merge(git: &GitRunner, git_dir: &Path, target: &str, mode: MergeMode) -> Result<Step> {
    let mut args = vec!["merge".to_owned(), "--no-edit".to_owned()];
    match mode {
        MergeMode::Default => {}
        MergeMode::NoFastForward => args.push("--no-ff".into()),
        MergeMode::FastForwardOnly => args.push("--ff-only".into()),
    }
    if let Some(message) = merge_message(git, target)? {
        // fmt-merge-msg already added the log if `merge.log` asks for it.
        args.extend(["--no-log".into(), "-m".into(), message]);
    }
    args.push(target.to_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_step(git, git_dir, &args, RepoState::Merging)
}

/// The message git would write for merging the short name of a full ref
/// (honouring `merge.suppressDest` and `merge.log`), or `None` to let git
/// name `target` itself.
fn merge_message(git: &GitRunner, target: &str) -> Result<Option<String>> {
    let (kind, name) = if let Some(n) = target.strip_prefix("refs/heads/") {
        ("branch", n)
    } else if let Some(n) = target.strip_prefix("refs/remotes/") {
        ("remote-tracking branch", n)
    } else if let Some(n) = target.strip_prefix("refs/tags/") {
        ("tag", n)
    } else {
        return Ok(None);
    };
    let oid = git.output(["rev-parse", "--verify", "-q", &format!("{target}^{{commit}}")])?;
    let line = format!("{}\t\t{kind} '{name}' of .\n", oid.trim().to_str_lossy());
    let message = git.output_with_input(["fmt-merge-msg"], line.as_bytes(), &[])?;
    let message = message.trim_end().to_str_lossy().into_owned();
    Ok((!message.is_empty()).then_some(message))
}

pub fn rebase(git: &GitRunner, git_dir: &Path, onto: &str) -> Result<Step> {
    run_step(git, git_dir, &["rebase", onto], RepoState::Rebasing)
}

pub fn cherry_pick(git: &GitRunner, git_dir: &Path, commit: &str) -> Result<Step> {
    run_step(git, git_dir, &["cherry-pick", commit], RepoState::CherryPicking)
}

pub fn revert(git: &GitRunner, git_dir: &Path, commit: &str) -> Result<Step> {
    run_step(git, git_dir, &["revert", "--no-edit", commit], RepoState::Reverting)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Keep index and work tree.
    Soft,
    /// Keep the work tree, reset the index.
    Mixed,
    /// Discard index and work tree changes to tracked files.
    Hard,
}

pub fn reset(git: &GitRunner, target: &str, mode: ResetMode) -> Result<()> {
    let flag = match mode {
        ResetMode::Soft => "--soft",
        ResetMode::Mixed => "--mixed",
        ResetMode::Hard => "--hard",
    };
    if target.starts_with('-') {
        bail!("invalid reset target");
    }
    git.output(["reset", "-q", flag, target, "--"])?;
    Ok(())
}
