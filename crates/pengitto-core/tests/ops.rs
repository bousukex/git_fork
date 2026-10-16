mod common;

use std::os::unix::fs::PermissionsExt as _;

use common::TestRepo;
use pengitto_core::{
    RepoReader,
    model::{ChangeKind, ChangedFile},
    ops::{self, CommitOptions},
};

fn find<'a>(list: &'a [ChangedFile], path: &str) -> &'a ChangedFile {
    list.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not in {list:?}"))
}

#[test]
fn stage_and_unstage_files() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "1\n", "init");
    repo.commit_file("old.txt", "same\ncontent\nhere\n", "old");
    repo.write("a.txt", "2\n");
    repo.write("新規 ファイル.txt", "new\n");
    repo.write("glob*.txt", "g\n");
    std::fs::remove_file(repo.path().join("old.txt")).unwrap();
    repo.write("moved.txt", "same\ncontent\nhere\n");
    let git = repo.runner();
    let r = repo.reader();

    let st = r.status().unwrap();
    ops::stage(&git, &st.unstaged).unwrap();
    let st = r.status().unwrap();
    assert!(st.unstaged.is_empty(), "{:?}", st.unstaged);
    assert_eq!(find(&st.staged, "a.txt").kind, ChangeKind::Modified);
    assert_eq!(find(&st.staged, "新規 ファイル.txt").kind, ChangeKind::Added);
    assert_eq!(find(&st.staged, "glob*.txt").kind, ChangeKind::Added);
    let moved = find(&st.staged, "moved.txt");
    assert_eq!(moved.kind, ChangeKind::Renamed);

    // Unstaging a rename unstages both sides.
    ops::unstage(&git, &[moved.clone(), find(&st.staged, "glob*.txt").clone()]).unwrap();
    let st = r.status().unwrap();
    assert!(st.staged.iter().all(|f| f.path != "moved.txt" && f.path != "old.txt" && f.path != "glob*.txt"));
    assert_eq!(find(&st.unstaged, "old.txt").kind, ChangeKind::Deleted);
    assert_eq!(find(&st.unstaged, "moved.txt").kind, ChangeKind::Untracked);
    assert_eq!(find(&st.staged, "a.txt").kind, ChangeKind::Modified);
}

#[test]
fn unstage_without_any_commit() {
    let repo = TestRepo::new();
    repo.write("first.txt", "x\n");
    let git = repo.runner();
    let r = repo.reader();
    ops::stage(&git, &r.status().unwrap().unstaged).unwrap();
    let st = r.status().unwrap();
    assert_eq!(st.staged.len(), 1);
    ops::unstage(&git, &st.staged).unwrap();
    let st = r.status().unwrap();
    assert!(st.staged.is_empty());
    assert_eq!(st.unstaged[0].kind, ChangeKind::Untracked);
    assert!(repo.path().join("first.txt").exists(), "unstaging must keep the file");
}

#[test]
fn discard_restores_tracked_and_hands_over_untracked() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "original\n", "init");
    repo.commit_file("gone.txt", "keep me\n", "gone");
    repo.write("a.txt", "edited\n");
    std::fs::remove_file(repo.path().join("gone.txt")).unwrap();
    repo.write("junk.txt", "junk\n");
    // Staged change survives a discard of the work tree change.
    repo.write("b.txt", "staged\n");
    repo.git(["add", "b.txt"]);
    repo.write("b.txt", "staged then edited\n");
    let git = repo.runner();
    let r = repo.reader();

    let st = r.status().unwrap();
    let mut handed = Vec::new();
    let report = ops::discard(&git, &st.unstaged, |p| {
        handed.push(p.to_path_buf());
        std::fs::remove_file(p)?;
        Ok(())
    })
    .unwrap();
    assert!(report.is_complete());
    assert_eq!(report.removed, handed);
    assert_eq!(report.restored.len(), 3);
    assert_eq!(handed, [repo.path().join("junk.txt")]);
    assert_eq!(std::fs::read_to_string(repo.path().join("a.txt")).unwrap(), "original\n");
    assert_eq!(std::fs::read_to_string(repo.path().join("gone.txt")).unwrap(), "keep me\n");
    assert_eq!(std::fs::read_to_string(repo.path().join("b.txt")).unwrap(), "staged\n");
    let st = r.status().unwrap();
    assert!(st.unstaged.is_empty(), "{:?}", st.unstaged);
    assert_eq!(st.staged.len(), 1);
}

#[test]
fn commit_amend_and_messages() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    let r = repo.reader();
    ops::stage(&git, &r.status().unwrap().unstaged).unwrap();

    let msg = ops::join_message("最初のコミット", "本文の1行目\n\n# 行頭のシャープも残る");
    let id = ops::commit(&git, &msg, &CommitOptions::default()).unwrap();
    assert_eq!(r.refs().unwrap().head_target(), Some(id));
    assert_eq!(ops::head_message(&git).unwrap(), msg.trim_end());

    // Amend only the message.
    let id2 = ops::commit(&git, "書き直し\n", &CommitOptions { amend: true, ..Default::default() }).unwrap();
    assert_ne!(id, id2);
    assert_eq!(ops::head_message(&git).unwrap(), "書き直し");
    assert_eq!(repo.git(["rev-list", "--count", "HEAD"]), "1");

    // Nothing staged and not amending: git refuses and we report it.
    let err = ops::commit(&git, "empty", &CommitOptions::default()).unwrap_err();
    assert!(format!("{err:#}").contains("nothing"), "{err:#}");
    assert!(ops::commit(&git, "  \n", &CommitOptions::default()).is_err());
}

#[test]
fn failing_hook_is_reported_and_nothing_is_committed() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "1\n", "init");
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'lint failed: 日本語' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.write("a.txt", "2\n");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    ops::stage(&git, &repo.reader().status().unwrap().unstaged).unwrap();
    let err = ops::commit(&git, "change", &CommitOptions::default()).unwrap_err();
    assert!(format!("{err:#}").contains("lint failed: 日本語"), "{err:#}");
    assert_eq!(repo.git(["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn discard_unstaged_rename() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "line1\nline2\nline3\nline4\n", "init");
    std::fs::rename(repo.path().join("a.txt"), repo.path().join("b.txt")).unwrap();
    repo.git(["add", "-N", "b.txt"]);
    let git = repo.runner();
    let r = repo.reader();
    let st = r.status().unwrap();
    let renamed = find(&st.unstaged, "b.txt").clone();
    assert_eq!(renamed.kind, ChangeKind::Renamed);
    assert_eq!(renamed.orig_path.as_ref().unwrap(), "a.txt");

    let report = ops::discard(&git, &[renamed], |p| Ok(std::fs::remove_file(p)?)).unwrap();
    assert!(report.is_complete());
    assert!(repo.path().join("a.txt").exists());
    assert!(!repo.path().join("b.txt").exists());
    let st = r.status().unwrap();
    assert!(st.unstaged.is_empty() && st.staged.is_empty(), "{st:?}");
}

#[test]
fn discard_reports_partial_failure() {
    let repo = TestRepo::new();
    repo.commit_file("t.txt", "1\n", "init");
    repo.write("t.txt", "2\n");
    repo.write("ok.txt", "x");
    repo.write("stuck.txt", "x");
    let git = repo.runner();
    let st = repo.reader().status().unwrap();
    let report = ops::discard(&git, &st.unstaged, |p| {
        if p.ends_with("stuck.txt") {
            anyhow::bail!("trash is full");
        }
        Ok(std::fs::remove_file(p)?)
    })
    .unwrap();
    assert!(!report.is_complete());
    assert_eq!(report.restored, [bstr::BString::from("t.txt")]);
    assert_eq!(report.removed, [repo.path().join("ok.txt")]);
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].0.ends_with("stuck.txt"));
    assert!(report.failed[0].1.contains("trash is full"));
    assert_eq!(std::fs::read_to_string(repo.path().join("t.txt")).unwrap(), "1\n");
}
