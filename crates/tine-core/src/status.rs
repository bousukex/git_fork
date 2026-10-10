//! Working tree status via `git status --porcelain=v2 -z`.

use std::path::Path;

use anyhow::Result;
use bstr::{BString, ByteSlice};

use crate::{
    model::{ChangeKind, ChangedFile, RepoState, WorkingStatus},
    runner::GitRunner,
};

pub fn status(git: &GitRunner, git_dir: &Path) -> Result<WorkingStatus> {
    // Submodule handling follows the user's config (`submodule.*.ignore`,
    // `diff.ignoreSubmodules`), like `git status` on the command line.
    let out = git.output(["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
    let mut st = parse(&out);
    st.state = repo_state(git_dir);
    Ok(st)
}

fn kind(c: u8) -> Option<ChangeKind> {
    Some(match c {
        b'M' => ChangeKind::Modified,
        b'T' => ChangeKind::TypeChanged,
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        _ => return None,
    })
}

pub fn parse(out: &[u8]) -> WorkingStatus {
    let mut st = WorkingStatus::default();
    let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        match rec.first() {
            Some(b'1') | Some(b'2') => {
                let renamed = rec[0] == b'2';
                let n = if renamed { 10 } else { 9 };
                let fields: Vec<&[u8]> = rec.splitn(n, |&b| b == b' ').collect();
                if fields.len() < n {
                    continue;
                }
                let xy = fields[1];
                let is_submodule = fields[2].first() == Some(&b'S');
                let path = BString::from(fields[n - 1]);
                let orig = if renamed { records.next().map(BString::from) } else { None };
                for (side, list) in [(xy.first(), &mut st.staged), (xy.get(1), &mut st.unstaged)] {
                    if let Some(k) = side.copied().and_then(kind) {
                        let orig_path =
                            matches!(k, ChangeKind::Renamed | ChangeKind::Copied).then(|| orig.clone()).flatten();
                        list.push(ChangedFile { path: path.clone(), orig_path, kind: k, is_submodule });
                    }
                }
            }
            Some(b'u') => {
                let fields: Vec<&[u8]> = rec.splitn(11, |&b| b == b' ').collect();
                if let Some(path) = fields.get(10) {
                    st.conflicted.push(ChangedFile {
                        path: BString::from(*path),
                        orig_path: None,
                        kind: ChangeKind::Conflicted,
                        is_submodule: fields[2].first() == Some(&b'S'),
                    });
                }
            }
            Some(b'?') => {
                if let Some(path) = rec.get(2..).filter(|p| !p.is_empty()) {
                    st.unstaged.push(ChangedFile {
                        path: BString::from(path),
                        orig_path: None,
                        kind: ChangeKind::Untracked,
                        is_submodule: false,
                    });
                }
            }
            _ => {}
        }
    }
    // Sort like a file tree: case-insensitive path order.
    for list in [&mut st.staged, &mut st.unstaged, &mut st.conflicted] {
        list.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()).then(a.path.cmp(&b.path)));
    }
    st
}

/// Which of `paths` (relative to the work tree) git ignores.
pub fn ignored(git: &GitRunner, paths: &[BString]) -> Result<std::collections::HashSet<BString>> {
    if paths.is_empty() {
        return Ok(Default::default());
    }
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p);
        input.push(0);
    }
    // check-ignore takes plain path names and refuses literal-pathspec mode.
    let git = git.clone().with_env("GIT_LITERAL_PATHSPECS", "0");
    // Exit code 1 means "none of them are ignored".
    let out = git.output_with_input(["check-ignore", "-z", "--stdin"], &input, &[1])?;
    Ok(out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(BString::from).collect())
}

/// Detects an in-progress merge/rebase/etc. from marker files.
pub fn repo_state(git_dir: &Path) -> RepoState {
    let has = |p: &str| git_dir.join(p).exists();
    if has("rebase-merge") || has("rebase-apply") {
        RepoState::Rebasing
    } else if has("MERGE_HEAD") {
        RepoState::Merging
    } else if has("CHERRY_PICK_HEAD") {
        RepoState::CherryPicking
    } else if has("REVERT_HEAD") {
        RepoState::Reverting
    } else if has("BISECT_LOG") {
        RepoState::Bisecting
    } else {
        RepoState::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn parses_all_entry_types() {
        let out = [
            format!("1 M. N... 100644 100644 100644 {H} {H} staged only.txt"),
            format!("1 .M N... 100644 100644 100644 {H} {H} 日本語.txt"),
            format!("1 MM N... 100644 100644 100644 {H} {H} both.txt"),
            format!("2 R. N... 100644 100644 100644 {H} {H} R100 new name.txt"),
            "old name.txt".into(),
            format!("u UU N... 100644 100644 100644 100644 {H} {H} {H} conflict.txt"),
            "? untracked dir/file.txt".into(),
            "! ignored.txt".into(),
            format!("1 .M S.M. 160000 160000 160000 {H} {H} sub"),
        ]
        .join("\0")
            + "\0";
        let st = parse(out.as_bytes());
        let names = |l: &[ChangedFile]| l.iter().map(|f| f.path.to_string()).collect::<Vec<_>>();
        assert_eq!(names(&st.staged), ["both.txt", "new name.txt", "staged only.txt"]);
        assert_eq!(names(&st.unstaged), ["both.txt", "sub", "untracked dir/file.txt", "日本語.txt"]);
        assert_eq!(names(&st.conflicted), ["conflict.txt"]);
        let renamed = st.staged.iter().find(|f| f.kind == ChangeKind::Renamed).unwrap();
        assert_eq!(renamed.orig_path.as_ref().unwrap(), "old name.txt");
        assert!(st.unstaged.iter().find(|f| f.path == "sub").unwrap().is_submodule);
    }

    #[test]
    fn tolerates_truncated_output() {
        let st = parse(b"1 M. N... 100644\x00? \x00u UU\x002 R. N... 100644");
        assert!(st.staged.is_empty());
        assert!(st.unstaged.is_empty());
        assert!(st.conflicted.is_empty());
    }
}
