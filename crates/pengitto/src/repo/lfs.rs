//! Git LFS in a tab: whether the repository uses it (and git-lfs is
//! installed), and which of the listed files go through it (marked).

use std::collections::HashSet;

use bstr::BString;
use pengitto_core::{diff::DiffSource, lfs::LfsRow, model::Oid};

use super::{RepoTab, Side};
use crate::worker::Gen;

/// Which list a set of Git LFS files is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsTarget {
    Unstaged,
    Staged,
    Commit(Oid),
}

#[derive(Default)]
pub struct LfsState {
    /// Some `.gitattributes` in HEAD sends files through Git LFS (for the
    /// "isn't installed" banner; the marks go by each list's own
    /// attributes).
    pub used: bool,
    /// git-lfs is installed (`None`: not known yet).
    pub installed: Option<bool>,
    /// The "git-lfs isn't installed" banner was closed.
    pub banner_dismissed: bool,
    pub unstaged: HashSet<BString>,
    pub staged: HashSet<BString>,
    pub commit: Option<(Oid, HashSet<BString>)>,
    state_seq: Gen,
    unstaged_seq: Gen,
    staged_seq: Gen,
    commit_seq: Gen,
}

impl RepoTab {
    /// Looks again at whether the repository uses Git LFS (HEAD moved).
    pub(super) fn request_lfs_state(&mut self) {
        self.lfs.state_seq = self.gens.bump();
        self.worker.lfs_state(self.lfs.state_seq, self.git.clone(), self.lfs.installed.is_none());
    }

    pub(super) fn on_lfs_state(&mut self, seq: Gen, used: anyhow::Result<bool>, installed: Option<bool>) {
        if seq != self.lfs.state_seq {
            return;
        }
        if let Some(i) = installed {
            self.lfs.installed = Some(i);
        }
        match used {
            Ok(used) => self.lfs.used = used,
            // Not taken for "no LFS": the banner stays as it was.
            Err(e) => log::warn!("{}: couldn't tell whether Git LFS is used: {e:#}", self.title),
        }
    }

    /// Marks for the Local Changes lists: each file by the attributes of
    /// both its sides (unstaged: the index and the working tree; staged:
    /// HEAD and the index) — LFS may come in or go with these very changes.
    pub(super) fn request_lfs_local(&mut self) {
        let head = self.head_commit();
        let unstaged = LfsRow::rows(DiffSource::Unstaged, head, &self.list(Side::Unstaged));
        let staged = LfsRow::rows(DiffSource::Staged, head, &self.status.staged);
        self.lfs.unstaged_seq = self.gens.bump();
        self.worker.lfs_tracked(self.lfs.unstaged_seq, self.git.clone(), LfsTarget::Unstaged, unstaged);
        self.lfs.staged_seq = self.gens.bump();
        self.worker.lfs_tracked(self.lfs.staged_seq, self.git.clone(), LfsTarget::Staged, staged);
    }

    /// Marks for the commit shown: each file by the commit's attributes
    /// and its parent's (a deleted file's, a renamed one's old path).
    pub(super) fn request_lfs_commit(&mut self) {
        let Some(Ok(details)) = &self.commit.details else { return };
        let id = details.id;
        let source = DiffSource::Commit { id, parent: details.parents.first().copied() };
        let rows = LfsRow::rows(source, None, &self.commit.files);
        self.lfs.commit_seq = self.gens.bump();
        self.worker.lfs_tracked(self.lfs.commit_seq, self.git.clone(), LfsTarget::Commit(id), rows);
    }

    pub(super) fn on_lfs_tracked(&mut self, seq: Gen, target: LfsTarget, result: anyhow::Result<HashSet<BString>>) {
        let current = match target {
            LfsTarget::Unstaged => self.lfs.unstaged_seq,
            LfsTarget::Staged => self.lfs.staged_seq,
            LfsTarget::Commit(_) => self.lfs.commit_seq,
        };
        if seq != current {
            return;
        }
        let set = match result {
            Ok(set) => set,
            // Unknown isn't "not LFS": no marks, the reason in the log.
            Err(e) => {
                log::warn!("{}: couldn't tell which files use Git LFS: {e:#}", self.title);
                HashSet::new()
            }
        };
        match target {
            LfsTarget::Unstaged => self.lfs.unstaged = set,
            LfsTarget::Staged => self.lfs.staged = set,
            LfsTarget::Commit(id) => self.lfs.commit = Some((id, set)),
        }
    }

    /// A file in a Local Changes list goes through Git LFS.
    pub fn is_lfs(&self, side: Side, path: &[u8]) -> bool {
        match side {
            Side::Unstaged => self.lfs.unstaged.contains(path),
            Side::Staged => self.lfs.staged.contains(path),
        }
    }

    /// A file of the commit shown goes through Git LFS.
    pub fn is_lfs_in_commit(&self, path: &[u8]) -> bool {
        let shown = self.commit.details.as_ref().and_then(|d| d.as_ref().ok()).map(|d| d.id);
        self.lfs.commit.as_ref().is_some_and(|(id, set)| Some(*id) == shown && set.contains(path))
    }

    /// The repository uses Git LFS but git-lfs isn't installed here.
    pub fn lfs_missing(&self) -> bool {
        self.lfs.used && self.lfs.installed == Some(false) && !self.lfs.banner_dismissed
    }
}

/// Advice for an error that comes from git-lfs not being there: git (or
/// the shell it runs filters with) couldn't find or start it. A git-lfs
/// that runs and fails (a download, say) gets none.
pub fn lfs_hint(git_message: &str) -> Option<&'static str> {
    const MISSING: [&str; 4] =
        ["git-lfs: not found", "git-lfs: command not found", "git-lfs: No such file or directory", "cannot run git-lfs"];
    let missing = MISSING.iter().any(|m| git_message.contains(m))
        // The shell's "not found" exit code, for a git-lfs filter.
        || git_message.lines().any(|l| l.contains("filter 'git-lfs") && l.trim_end().ends_with("failed 127"));
    missing.then_some("Files here are stored with Git LFS, which isn't installed: install git-lfs (https://git-lfs.com).")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_needs_git_lfs_missing() {
        // As git 2.43 says it (through dash, and bash).
        for missing in [
            "git-lfs filter-process: 1: git-lfs: not found\nfatal: the remote end hung up unexpectedly",
            "git-lfs smudge -- 'a.bin': 1: git-lfs: not found\nerror: external filter 'git-lfs smudge -- %f' failed 127\nfatal: a.bin: smudge filter lfs failed",
            "bash: line 1: git-lfs: command not found",
            "error: external filter 'git-lfs clean -- %f' failed 127",
        ] {
            assert!(lfs_hint(missing).is_some(), "{missing}");
        }
        for other in [
            "Error downloading object: a.bin (ba7816b): Smudge error: batch request: missing credentials\nerror: external filter 'git-lfs smudge -- %f' failed 2\nerror: external filter 'git-lfs smudge -- %f' failed\nfatal: a.bin: smudge filter lfs failed",
            "git-lfs filter-process: Smudge error: Error downloading big.bin: not found on the server",
            "fatal: pathspec 'git-lfs' did not match any files",
        ] {
            assert!(lfs_hint(other).is_none(), "{other}");
        }
    }
}
