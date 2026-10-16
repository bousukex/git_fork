mod common;

use bstr::BString;
use common::TestRepo;
use pengitto_core::{
    RepoReader,
    diff::{DiffOptions, DiffSource},
    model::{ChangedFile, FileDiff, LineKind},
    patch::{self, PatchMode},
};

fn file(path: &str) -> ChangedFile {
    ChangedFile { path: path.into(), orig_path: None, kind: pengitto_core::model::ChangeKind::Modified, is_submodule: false }
}

fn diff(repo: &TestRepo, source: DiffSource, path: &str) -> FileDiff {
    repo.reader().file_diff(source, &file(path), DiffOptions::default(), &pengitto_core::CancelToken::default()).unwrap()
}

/// (hunk, line) of the changed line whose content is `text`.
fn line_of(d: &FileDiff, kind: LineKind, text: &str) -> (usize, usize) {
    for (h, hunk) in d.hunks.iter().enumerate() {
        for (l, line) in hunk.lines.iter().enumerate() {
            if line.kind == kind && line.content == text {
                return (h, l);
            }
        }
    }
    panic!("{kind:?} {text:?} not in diff");
}

fn changed(d: &FileDiff) -> Vec<(LineKind, String)> {
    d.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| matches!(l.kind, LineKind::Added | LineKind::Deleted))
        .map(|l| (l.kind, String::from_utf8_lossy(&l.content).into_owned()))
        .collect()
}

fn base(repo: &TestRepo, name: &str, lines: usize) {
    let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
    repo.commit_file(name, &body, "base");
}

#[test]
fn stage_unstage_and_discard_single_lines() {
    let repo = TestRepo::new();
    base(&repo, "f.txt", 30);
    // Two separate hunks: change line 2 and line 25.
    let mut text = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    text = text.replace("line 2\n", "line two\n").replace("line 25\n", "line 25\nadded after 25\n");
    repo.write("f.txt", &text);
    let git = repo.runner();

    // Stage only the addition in the second hunk.
    let d = diff(&repo, DiffSource::Unstaged, "f.txt");
    assert_eq!(d.hunks.len(), 2);
    let target = line_of(&d, LineKind::Added, "added after 25");
    patch::apply(&git, &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, l| (h, l) == target).unwrap();
    assert_eq!(changed(&diff(&repo, DiffSource::Staged, "f.txt")), [(LineKind::Added, "added after 25".into())]);
    assert_eq!(
        changed(&diff(&repo, DiffSource::Unstaged, "f.txt")),
        [(LineKind::Deleted, "line 2".into()), (LineKind::Added, "line two".into())]
    );

    // Stage just the deletion of "line 2" (the replacement stays unstaged).
    let d = diff(&repo, DiffSource::Unstaged, "f.txt");
    let del = line_of(&d, LineKind::Deleted, "line 2");
    patch::apply(&git, &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, l| (h, l) == del).unwrap();
    assert_eq!(changed(&diff(&repo, DiffSource::Staged, "f.txt")).len(), 2);

    // Unstage the addition again.
    let d = diff(&repo, DiffSource::Staged, "f.txt");
    let add = line_of(&d, LineKind::Added, "added after 25");
    patch::apply(&git, &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Unstage, |h, l| (h, l) == add).unwrap();
    assert_eq!(changed(&diff(&repo, DiffSource::Staged, "f.txt")), [(LineKind::Deleted, "line 2".into())]);

    // Discard the addition from the work tree; the rest stays.
    let d = diff(&repo, DiffSource::Unstaged, "f.txt");
    let add = line_of(&d, LineKind::Added, "added after 25");
    patch::apply(&git, &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Discard, |h, l| (h, l) == add).unwrap();
    let now = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    assert!(!now.contains("added after 25") && now.contains("line two\n"), "{now}");
}

#[test]
fn whole_hunk_of_two() {
    let repo = TestRepo::new();
    base(&repo, "f.txt", 30);
    let text = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    repo.write("f.txt", &text.replace("line 3\n", "LINE 3\n").replace("line 28\n", "LINE 28\n"));
    let d = diff(&repo, DiffSource::Unstaged, "f.txt");
    patch::apply(&repo.runner(), &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, _| h == 0).unwrap();
    assert_eq!(
        changed(&diff(&repo, DiffSource::Staged, "f.txt")),
        [(LineKind::Deleted, "line 3".into()), (LineKind::Added, "LINE 3".into())]
    );
    assert_eq!(diff(&repo, DiffSource::Unstaged, "f.txt").hunks.len(), 1);
}

#[test]
fn missing_trailing_newline() {
    let repo = TestRepo::new();
    repo.commit_file("n.txt", "a\nb\nlast", "no newline");
    repo.write("n.txt", "a\nB\nlast changed");
    let d = diff(&repo, DiffSource::Unstaged, "n.txt");
    // Stage only the change of the last line (both its - and + lines).
    let del = line_of(&d, LineKind::Deleted, "last");
    let add = line_of(&d, LineKind::Added, "last changed");
    patch::apply(&repo.runner(), &"n.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, l| (h, l) == del || (h, l) == add).unwrap();
    let staged = repo.git(["show", ":n.txt"]);
    assert_eq!(staged, "a\nb\nlast changed");
    assert_eq!(
        changed(&diff(&repo, DiffSource::Unstaged, "n.txt")),
        [(LineKind::Deleted, "b".into()), (LineKind::Added, "B".into())]
    );
}

#[test]
fn crlf_and_non_utf8_content_with_unusual_name() {
    let repo = TestRepo::new();
    let name = "日本語 ファイル.txt";
    let path = repo.path().join(name);
    std::fs::write(&path, b"one\r\ntwo\r\ncaf\xe9\r\n").unwrap();
    repo.git(["add", "-A"]);
    repo.git(["commit", "-qm", "crlf"]);
    std::fs::write(&path, b"one\r\nTWO\r\ncaf\xe9!\r\n").unwrap();
    let d = diff(&repo, DiffSource::Unstaged, name);
    // Content is raw bytes (Latin-1 here), so match by prefix.
    let mut target = None;
    for (h, hunk) in d.hunks.iter().enumerate() {
        for (l, line) in hunk.lines.iter().enumerate() {
            if line.kind != LineKind::Context && line.content.starts_with(b"caf") {
                target.get_or_insert_with(Vec::new).push((h, l));
            }
        }
    }
    let target = target.unwrap();
    patch::apply(&repo.runner(), &BString::from(name), &d, DiffOptions::default(), PatchMode::Stage, |h, l| target.contains(&(h, l))).unwrap();
    let out = std::process::Command::new("git")
        .current_dir(repo.path())
        .args(["show", &format!(":{name}")])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"one\r\ntwo\r\ncaf\xe9!\r\n");
}

#[test]
fn stale_selection_fails_without_changes() {
    let repo = TestRepo::new();
    base(&repo, "f.txt", 10);
    let text = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    repo.write("f.txt", &text.replace("line 5\n", "line five\n"));
    let d = diff(&repo, DiffSource::Unstaged, "f.txt");
    // The file changes again before the user applies the old selection, and
    // the index moves so the patch's context no longer matches.
    repo.write("f.txt", "completely\ndifferent\n");
    repo.git(["add", "f.txt"]);
    let before = repo.git(["show", ":f.txt"]);
    let err = patch::apply(&repo.runner(), &"f.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |_, _| true).unwrap_err();
    assert!(format!("{err:#}").contains("changed since its diff was shown"), "{err:#}");
    assert_eq!(repo.git(["show", ":f.txt"]), before, "index must be unchanged");
}

fn index_of(repo: &TestRepo, path: &str) -> String {
    repo.git(["show", &format!(":{path}")])
}

#[test]
fn one_line_file_in_all_three_modes() {
    let repo = TestRepo::new();
    repo.commit_file("one.txt", "old\n", "one line");
    repo.write("one.txt", "new\n");
    let git = repo.runner();

    let d = diff(&repo, DiffSource::Unstaged, "one.txt");
    assert!(d.hunks[0].lines.iter().all(|l| l.kind != LineKind::Context), "no context available");
    patch::apply(&git, &"one.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |_, _| true).unwrap();
    assert_eq!(index_of(&repo, "one.txt"), "new");

    let d = diff(&repo, DiffSource::Staged, "one.txt");
    patch::apply(&git, &"one.txt".into(), &d, DiffOptions::default(), PatchMode::Unstage, |_, _| true).unwrap();
    assert_eq!(index_of(&repo, "one.txt"), "old");

    let d = diff(&repo, DiffSource::Unstaged, "one.txt");
    patch::apply(&git, &"one.txt".into(), &d, DiffOptions::default(), PatchMode::Discard, |_, _| true).unwrap();
    assert_eq!(std::fs::read_to_string(repo.path().join("one.txt")).unwrap(), "old\n");
}

#[test]
fn append_to_empty_tracked_file_line_by_line() {
    let repo = TestRepo::new();
    repo.commit_file("empty.txt", "", "empty");
    repo.write("empty.txt", "first\nsecond\n");
    let git = repo.runner();
    let d = diff(&repo, DiffSource::Unstaged, "empty.txt");
    let first = line_of(&d, LineKind::Added, "first");
    patch::apply(&git, &"empty.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, l| (h, l) == first).unwrap();
    assert_eq!(index_of(&repo, "empty.txt"), "first");
    assert_eq!(changed(&diff(&repo, DiffSource::Unstaged, "empty.txt")), [(LineKind::Added, "second".into())]);
}

#[test]
fn stale_unstage_and_discard_change_nothing() {
    let repo = TestRepo::new();
    base(&repo, "f.txt", 10);
    let text = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    repo.write("f.txt", &text.replace("line 3\n", "line three\n"));
    repo.git(["add", "f.txt"]);
    repo.write("f.txt", &text.replace("line 3\n", "line three\n").replace("line 8\n", "line eight\n"));
    let git = repo.runner();
    let staged = diff(&repo, DiffSource::Staged, "f.txt");
    let unstaged = diff(&repo, DiffSource::Unstaged, "f.txt");

    // Both the index and the work tree move on before the user acts.
    repo.write("f.txt", &text.replace("line 3\n", "line THREE\n").replace("line 8\n", "line EIGHT\n"));
    repo.git(["add", "f.txt"]);
    repo.write("f.txt", &text.replace("line 3\n", "line THREE\n").replace("line 8\n", "line 8 again\n"));
    let (index_before, tree_before) = (index_of(&repo, "f.txt"), std::fs::read_to_string(repo.path().join("f.txt")).unwrap());

    let err = patch::apply(&git, &"f.txt".into(), &staged, DiffOptions::default(), PatchMode::Unstage, |_, _| true).unwrap_err();
    assert!(format!("{err:#}").contains("changed since its diff was shown"), "{err:#}");
    let err = patch::apply(&git, &"f.txt".into(), &unstaged, DiffOptions::default(), PatchMode::Discard, |_, _| true).unwrap_err();
    assert!(format!("{err:#}").contains("changed since its diff was shown"), "{err:#}");
    assert_eq!(index_of(&repo, "f.txt"), index_before);
    assert_eq!(std::fs::read_to_string(repo.path().join("f.txt")).unwrap(), tree_before);
}

#[test]
fn no_newline_partial_selection_is_applied_or_refused_cleanly() {
    let repo = TestRepo::new();
    repo.commit_file("n.txt", "a\nb", "no newline at end");
    repo.write("n.txt", "a\nb\nc");
    let git = repo.runner();
    let d = diff(&repo, DiffSource::Unstaged, "n.txt");
    // Staging only "+c" would need b's newline, which is part of the -b/+b pair.
    let c = line_of(&d, LineKind::Added, "c");
    let (index_before, tree_before) = (index_of(&repo, "n.txt"), std::fs::read_to_string(repo.path().join("n.txt")).unwrap());
    let err = patch::apply(&git, &"n.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |h, l| (h, l) == c)
        .unwrap_err();
    assert!(format!("{err:#}").contains("missing newline"), "{err:#}");
    assert_eq!(index_of(&repo, "n.txt"), index_before, "must not glue lines together");
    assert_eq!(std::fs::read_to_string(repo.path().join("n.txt")).unwrap(), tree_before);
    // Selecting the whole change always works.
    let d = diff(&repo, DiffSource::Unstaged, "n.txt");
    patch::apply(&git, &"n.txt".into(), &d, DiffOptions::default(), PatchMode::Stage, |_, _| true).unwrap();
    assert_eq!(index_of(&repo, "n.txt"), "a\nb\nc");
}
