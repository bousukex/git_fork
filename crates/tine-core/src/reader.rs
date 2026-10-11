//! Read-only repository access.
//!
//! The UI only talks to [`RepoReader`]. [`CliReader`] implements it with the
//! git CLI; a faster in-process backend can implement the same trait later
//! without touching callers. Write operations live elsewhere and always use
//! the CLI.

use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use bstr::ByteSlice;

use crate::{
    diff::{self, DiffOptions, DiffSource},
    log::{self, LogOptions},
    model::{ChangedFile, CommitDetails, CommitSummary, FileDiff, Oid, RefsSnapshot, WorkingStatus},
    refs,
    runner::{CancelToken, GitRunner},
    status,
};

/// Locations of a non-bare repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPaths {
    pub workdir: PathBuf,
    pub git_dir: PathBuf,
    /// Shared by all linked worktrees of a repository (refs, objects,
    /// config); equals `git_dir` for the main worktree.
    pub common_dir: PathBuf,
}

impl RepoPaths {
    /// Finds the repository containing `path`.
    pub fn discover(path: &Path) -> Result<RepoPaths> {
        Self::discover_with(&GitRunner::new(path))
    }

    pub fn discover_with(git: &GitRunner) -> Result<RepoPaths> {
        let out = git
            .output([
                "rev-parse",
                "--is-bare-repository",
                "--show-toplevel",
                "--absolute-git-dir",
                "--path-format=absolute",
                "--git-common-dir",
            ])
            .with_context(|| format!("{} is not a git repository", git.workdir().display()))?;
        let mut lines = out.lines();
        if lines.next() == Some(b"true") {
            bail!("bare repositories are not supported");
        }
        let path = |l: Option<&[u8]>| -> Result<PathBuf> {
            let l = l.filter(|l| !l.is_empty()).context("unexpected `git rev-parse` output")?;
            Ok(PathBuf::from(OsString::from_vec(l.to_vec())))
        };
        let workdir = path(lines.next())?;
        let git_dir = path(lines.next())?;
        let common_dir = path(lines.next())?;
        Ok(RepoPaths { workdir, git_dir, common_dir })
    }
}

pub trait RepoReader: Send + Sync {
    fn paths(&self) -> &RepoPaths;
    fn refs(&self) -> Result<RefsSnapshot>;
    fn status(&self) -> Result<WorkingStatus>;
    /// Streams history in batches; return `false` from `on_batch` to stop.
    fn log(
        &self,
        opts: &LogOptions,
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<CommitSummary>) -> bool,
    ) -> Result<()>;
    fn commit_details(&self, id: Oid) -> Result<CommitDetails>;
    fn commit_files(&self, id: Oid, parent: Option<Oid>) -> Result<Vec<ChangedFile>>;
    fn file_diff(&self, source: DiffSource, file: &ChangedFile, opts: DiffOptions, cancel: &CancelToken) -> Result<FileDiff>;
    /// Streams commits matching a search ([`crate::search::search`]).
    fn search(
        &self,
        kind: crate::search::SearchKind,
        query: &str,
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<Oid>) -> bool,
    ) -> Result<crate::search::SearchEnd>;
    /// Streams the history of one file ([`crate::file_history::file_history`]).
    fn file_history(
        &self,
        start: Oid,
        path: &[u8],
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<crate::file_history::FileHistoryEntry>) -> bool,
    ) -> Result<crate::search::SearchEnd>;
    /// [`crate::blame::blame`].
    fn blame(&self, path: &[u8], rev: Option<Oid>, ignore_whitespace: bool, cancel: &CancelToken) -> Result<crate::blame::Blame>;
    /// Every file in the index, once each (conflicted files have several
    /// stages), as raw paths.
    fn tracked_files(&self, cancel: &CancelToken) -> Result<Vec<bstr::BString>>;
    /// Every file path in commit `rev`.
    fn paths_at(&self, rev: Oid, cancel: &CancelToken) -> Result<Vec<bstr::BString>>;
}

#[derive(Debug, Clone)]
pub struct CliReader {
    git: GitRunner,
    paths: RepoPaths,
}

impl CliReader {
    pub fn open(path: &Path) -> Result<Self> {
        Self::with_runner(GitRunner::new(path))
    }

    /// Uses `git` (e.g. with extra env) to discover and read the repository.
    pub fn with_runner(git: GitRunner) -> Result<Self> {
        let paths = RepoPaths::discover_with(&git)?;
        // Run from the top level so paths in output are repo-relative.
        let git = git.with_workdir(&paths.workdir);
        Ok(Self { git, paths })
    }

    pub fn runner(&self) -> &GitRunner {
        &self.git
    }
}

impl RepoReader for CliReader {
    fn paths(&self) -> &RepoPaths {
        &self.paths
    }

    fn refs(&self) -> Result<RefsSnapshot> {
        refs::snapshot(&self.git)
    }

    fn status(&self) -> Result<WorkingStatus> {
        status::status(&self.git, &self.paths.git_dir)
    }

    fn log(
        &self,
        opts: &LogOptions,
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<CommitSummary>) -> bool,
    ) -> Result<()> {
        log::stream(&self.git, opts, 2000, cancel, on_batch)
    }

    fn commit_details(&self, id: Oid) -> Result<CommitDetails> {
        log::details(&self.git, &id)
    }

    fn commit_files(&self, id: Oid, parent: Option<Oid>) -> Result<Vec<ChangedFile>> {
        diff::commit_files(&self.git, id, parent)
    }

    fn file_diff(&self, source: DiffSource, file: &ChangedFile, opts: DiffOptions, cancel: &CancelToken) -> Result<FileDiff> {
        // Also around the steps without git (a conflicted file is read).
        let stopped = || -> Result<()> { if cancel.is_cancelled() { Err(crate::Cancelled.into()) } else { Ok(()) } };
        stopped()?;
        let diff = diff::file_diff(&self.git.clone().with_cancel(cancel.clone()), source, file, opts)?;
        stopped()?;
        Ok(diff)
    }

    fn search(
        &self,
        kind: crate::search::SearchKind,
        query: &str,
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<Oid>) -> bool,
    ) -> Result<crate::search::SearchEnd> {
        crate::search::search(&self.git, kind, query, cancel, on_batch)
    }

    fn file_history(
        &self,
        start: Oid,
        path: &[u8],
        cancel: &CancelToken,
        on_batch: &mut dyn FnMut(Vec<crate::file_history::FileHistoryEntry>) -> bool,
    ) -> Result<crate::search::SearchEnd> {
        crate::file_history::file_history(&self.git, start, path, cancel, on_batch)
    }

    fn blame(&self, path: &[u8], rev: Option<Oid>, ignore_whitespace: bool, cancel: &CancelToken) -> Result<crate::blame::Blame> {
        crate::blame::blame(&self.git, path, rev, ignore_whitespace, cancel)
    }

    fn tracked_files(&self, cancel: &CancelToken) -> Result<Vec<bstr::BString>> {
        let git = self.git.clone().with_cancel(cancel.clone());
        let out = git.output(["ls-files", "--cached", "--deduplicate", "-z"])?;
        Ok(out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(bstr::BString::from).collect())
    }

    fn paths_at(&self, rev: Oid, cancel: &CancelToken) -> Result<Vec<bstr::BString>> {
        let git = self.git.clone().with_cancel(cancel.clone());
        let out = git.output(["ls-tree", "-r", "-z", "--name-only", &rev.to_hex()])?;
        Ok(out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(bstr::BString::from).collect())
    }
}
