//! Write operations. These always go through the git CLI so hooks, signing
//! and configuration behave exactly as on the command line.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use bstr::{BString, ByteSlice};

use crate::{
    log::head_exists,
    model::{ChangeKind, ChangedFile, Oid},
    runner::GitRunner,
};

/// NUL-separated pathspecs for `--pathspec-from-file=- --pathspec-file-nul`
/// (no argument-length limits, any byte allowed in names).
fn pathspec_input<'a>(paths: impl IntoIterator<Item = &'a BString>) -> Vec<u8> {
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p);
        input.push(0);
    }
    input
}

/// Both sides of a rename/copy, so the move is staged/unstaged as a whole.
fn all_paths(files: &[ChangedFile]) -> Vec<&BString> {
    let mut v: Vec<&BString> = files.iter().flat_map(|f| f.orig_path.iter().chain([&f.path])).collect();
    v.sort();
    v.dedup();
    v
}

/// Stages work tree changes (including new and deleted files).
pub fn stage(git: &GitRunner, files: &[ChangedFile]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let input = pathspec_input(all_paths(files));
    git.output_with_input(["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"], &input, &[])?;
    Ok(())
}

/// Removes changes from the index, keeping the work tree.
pub fn unstage(git: &GitRunner, files: &[ChangedFile]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let input = pathspec_input(all_paths(files));
    if head_exists(git) {
        git.output_with_input(["reset", "-q", "--pathspec-from-file=-", "--pathspec-file-nul"], &input, &[])?;
    } else {
        // No commit yet: unstaging means removing from the index.
        git.output_with_input(
            ["rm", "--cached", "-r", "-q", "--pathspec-from-file=-", "--pathspec-file-nul"],
            &input,
            &[],
        )?;
    }
    Ok(())
}

/// What a discard did. Restoring tracked files is all-or-nothing; removing
/// untracked files is per file, so some may fail while others succeed.
#[derive(Debug, Default)]
pub struct DiscardReport {
    /// Tracked paths restored from the index.
    pub restored: Vec<BString>,
    /// Untracked (or rename-target) files handed to the remover.
    pub removed: Vec<PathBuf>,
    /// Files that could not be removed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

impl DiscardReport {
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Throws away unstaged changes.
///
/// - Tracked files are restored from the index (one git call, atomic).
/// - An unstaged rename (an intent-to-add file detected as moved) restores
///   the source and drops the target from the index and the work tree.
/// - Untracked files are handed to `remove_untracked` (the app moves them
///   to the trash); a failure there is recorded and the rest continue.
pub fn discard(
    git: &GitRunner,
    files: &[ChangedFile],
    mut remove_untracked: impl FnMut(&Path) -> Result<()>,
) -> Result<DiscardReport> {
    if files.iter().any(|f| f.kind == ChangeKind::Conflicted) {
        bail!("conflicted files can't be discarded here; resolve or abort the operation first");
    }
    let mut restore: Vec<&BString> = Vec::new();
    let mut drop_from_index: Vec<&BString> = Vec::new();
    let mut remove: Vec<&BString> = Vec::new();
    for f in files {
        match (f.kind, &f.orig_path) {
            (ChangeKind::Untracked, _) => remove.push(&f.path),
            (ChangeKind::Renamed, Some(orig)) => {
                restore.push(orig);
                drop_from_index.push(&f.path);
                remove.push(&f.path);
            }
            (ChangeKind::Copied, Some(_)) => {
                drop_from_index.push(&f.path);
                remove.push(&f.path);
            }
            _ => restore.push(&f.path),
        }
    }

    let mut report = DiscardReport::default();
    if !restore.is_empty() {
        let input = pathspec_input(restore.iter().copied());
        git.output_with_input(
            ["restore", "--worktree", "--pathspec-from-file=-", "--pathspec-file-nul"],
            &input,
            &[],
        )?;
        report.restored = restore.into_iter().cloned().collect();
    }
    if !drop_from_index.is_empty() {
        let input = pathspec_input(drop_from_index.iter().copied());
        git.output_with_input(
            ["rm", "--cached", "-q", "--ignore-unmatch", "--pathspec-from-file=-", "--pathspec-file-nul"],
            &input,
            &[],
        )?;
    }
    for p in remove {
        let path = git.workdir().join(p.to_path().context("invalid path")?);
        match remove_untracked(&path) {
            Ok(()) => report.removed.push(path),
            Err(e) => report.failed.push((path, format!("{e:#}"))),
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitOptions {
    pub amend: bool,
    /// Allow committing with nothing staged (only meaningful with amend or
    /// for explicit empty commits).
    pub allow_empty: bool,
}

/// Creates a commit from the index; returns the new HEAD.
pub fn commit(git: &GitRunner, message: &str, opts: &CommitOptions) -> Result<Oid> {
    if message.trim().is_empty() {
        bail!("commit message is empty");
    }
    let mut args = vec!["commit", "-q", "-F", "-"];
    if opts.amend {
        args.push("--amend");
    }
    if opts.allow_empty {
        args.push("--allow-empty");
    }
    git.output_with_input(&args, message.as_bytes(), &[])?;
    let head = git.output(["rev-parse", "HEAD"])?;
    Oid::from_hex(head.trim()).context("unexpected HEAD after commit")
}

/// Full message of HEAD (for prefilling an amend).
pub fn head_message(git: &GitRunner) -> Result<String> {
    let out = git.output(["log", "-1", "--format=%B", "HEAD", "--"])?;
    Ok(String::from_utf8_lossy(&out).trim_end().to_owned())
}

/// Splits a commit message into subject and description.
pub fn split_message(message: &str) -> (String, String) {
    let message = message.trim_end();
    match message.split_once('\n') {
        Some((subject, rest)) => (subject.trim_end().to_owned(), rest.trim_start_matches('\n').to_owned()),
        None => (message.to_owned(), String::new()),
    }
}

/// Joins subject and description the way git expects (blank line between).
pub fn join_message(subject: &str, description: &str) -> String {
    let (s, d) = (subject.trim(), description.trim_end());
    if d.trim().is_empty() { format!("{s}\n") } else { format!("{s}\n\n{d}\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_split_and_join() {
        assert_eq!(split_message("subj"), ("subj".into(), String::new()));
        assert_eq!(split_message("subj\n\nbody\nmore\n"), ("subj".into(), "body\nmore".into()));
        assert_eq!(join_message(" subj ", ""), "subj\n");
        assert_eq!(join_message("subj", "本文\n"), "subj\n\n本文\n");
        let (s, d) = split_message(&join_message("a", "b\n\nc"));
        assert_eq!((s.as_str(), d.as_str()), ("a", "b\n\nc"));
    }
}
