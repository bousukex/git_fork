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
    fn file_diff(&self, source: DiffSource, file: &ChangedFile, opts: DiffOptions) -> Result<FileDiff>;
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

    fn file_diff(&self, source: DiffSource, file: &ChangedFile, opts: DiffOptions) -> Result<FileDiff> {
        diff::file_diff(&self.git, source, file, opts)
    }
}
