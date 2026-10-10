//! File history and blame against real repositories (M4b).

mod common;

use std::os::unix::ffi::OsStrExt as _;

use bstr::BString;
use common::TestRepo;
use tine_core::{
    CancelToken,
    blame::{self, Blame},
    file_history::{FileHistoryEntry, file_history},
    model::Oid,
    search::SearchEnd,
};

fn head(repo: &TestRepo) -> Oid {
    Oid::from_hex(repo.git(["rev-parse", "HEAD"]).as_bytes()).unwrap()
}

fn history(repo: &TestRepo, start: Oid, path: &[u8]) -> Vec<FileHistoryEntry> {
    let mut all = Vec::new();
    let end = file_history(&repo.runner(), start, path, &CancelToken::default(), |b| {
        all.extend(b);
        true
    })
    .unwrap();
    assert_eq!(end, SearchEnd::Complete);
    all
}

fn summary(entries: &[FileHistoryEntry]) -> Vec<(String, Option<char>, String)> {
    entries.iter().map(|e| (e.commit.subject.clone(), e.status, e.path.to_string())).collect()
}

fn commit_all(repo: &TestRepo, msg: &str) {
    repo.git(["add", "-A"]);
    repo.git(["commit", "-q", "-m", msg]);
}

#[test]
fn history_follows_renames_from_the_given_commit() {
    let repo = TestRepo::new();
    repo.commit_file("old.txt", "one\ntwo\nthree\nfour\n", "add");
    repo.commit_file("old.txt", "one\ntwo\nthree\nfour\nfive\n", "edit");
    repo.git(["mv", "old.txt", "new.txt"]);
    commit_all(&repo, "rename");
    repo.commit_file("new.txt", "one\ntwo\nthree\nfour\nfive\nsix\n", "edit again");
    repo.commit_file("unrelated.txt", "x", "unrelated");
    let e = history(&repo, head(&repo), b"new.txt");
    assert_eq!(
        summary(&e),
        [
            ("edit again".into(), Some('M'), "new.txt".into()),
            ("rename".into(), Some('R'), "new.txt".into()),
            ("edit".into(), Some('M'), "old.txt".into()),
            ("add".into(), Some('A'), "old.txt".into()),
        ]
    );
    assert_eq!(e[1].old_path, Some(BString::from("old.txt")));
    assert_eq!(e[1].score, Some(100));

    // From an older commit: only what came before it.
    let edit = e[2].commit.id;
    assert_eq!(summary(&history(&repo, edit, b"old.txt")), [("edit".into(), Some('M'), "old.txt".into()), ("add".into(), Some('A'), "old.txt".into())]);
}

#[test]
fn history_through_deletion_and_merges() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "1\n", "add");
    repo.git(["rm", "-q", "f.txt"]);
    commit_all(&repo, "delete");
    repo.commit_file("f.txt", "again\n", "re-add");
    repo.git(["checkout", "-q", "-b", "side"]);
    repo.commit_file("f.txt", "again\nside\n", "side edit");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("g.txt", "g\n", "main other");
    repo.git(["merge", "-q", "--no-edit", "side"]);
    let e = history(&repo, head(&repo), b"f.txt");
    let s: Vec<(String, Option<char>)> = e.iter().map(|e| (e.commit.subject.clone(), e.status)).collect();
    assert!(s.contains(&("Merge branch 'side'".into(), Some('M'))), "the merge changed it relative to its first parent: {s:?}");
    assert!(s.contains(&("side edit".into(), Some('M'))));
    assert!(s.contains(&("delete".into(), Some('D'))));
    assert_eq!(s.last(), Some(&("add".into(), Some('A'))));
}

#[test]
fn history_of_awkward_paths() {
    let repo = TestRepo::new();
    for name in [&b"line\nbreak.txt"[..], b"tab\there.txt", b"latin1-\xe9.txt", "日本語 ファイル.md".as_bytes()] {
        let path = repo.path().join(std::ffi::OsStr::from_bytes(name));
        std::fs::write(&path, "1\n").unwrap();
        commit_all(&repo, "add");
        std::fs::write(&path, "2\n").unwrap();
        commit_all(&repo, "edit");
        let e = history(&repo, head(&repo), name);
        assert_eq!(e.len(), 2, "{:?}", BString::from(name));
        assert!(e.iter().all(|e| e.path == name), "{:?}", e.iter().map(|e| e.path.clone()).collect::<Vec<_>>());
    }
}

fn blame_of(repo: &TestRepo, path: &[u8], rev: Option<Oid>, ws: bool) -> Blame {
    blame::blame(&repo.runner(), path, rev, ws, &CancelToken::default()).unwrap()
}

fn who(b: &Blame) -> Vec<(String, String)> {
    b.lines.iter().map(|l| (b.commit_of(l).summary.clone(), l.content.to_string())).collect()
}

#[test]
fn blame_lines_commits_and_work_tree() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "a\nb\nc\n", "first");
    repo.commit_file("f.txt", "a\nB\nc\n\tindented\n", "second");
    let b = blame_of(&repo, b"f.txt", Some(head(&repo)), false);
    assert_eq!(who(&b), [("first", "a"), ("second", "B"), ("first", "c"), ("second", "\tindented")].map(|(s, c)| (s.into(), c.into())));
    let first = b.commits.iter().find(|c| c.summary == "first").unwrap();
    assert!(first.boundary, "the root commit is a boundary");
    let second_origin = &b.origins[b.lines[1].origin];
    assert_eq!(second_origin.filename, "f.txt");
    assert_eq!(second_origin.previous.as_ref().map(|p| p.1.clone()), Some(BString::from("f.txt")));

    // Work tree: uncommitted lines, CRLF kept as is.
    repo.write("f.txt", "a\nB\nc\n\tindented\nnew\r\n");
    let b = blame_of(&repo, b"f.txt", None, false);
    let last = b.lines.last().unwrap();
    assert!(b.commit_of(last).is_uncommitted());
    assert_eq!(last.content, "new\r");

    // -w: a whitespace-only change keeps the older author.
    repo.commit_file("f.txt", "a\nB\nc\n    indented\n", "reindent");
    let plain = blame_of(&repo, b"f.txt", Some(head(&repo)), false);
    let ws = blame_of(&repo, b"f.txt", Some(head(&repo)), true);
    assert_eq!(plain.commit_of(&plain.lines[3]).summary, "reindent");
    assert_eq!(ws.commit_of(&ws.lines[3]).summary, "second");
}

#[test]
fn blame_follows_renames_and_quotes_odd_paths() {
    let repo = TestRepo::new();
    let odd = b"we\"ird\tname.txt";
    let path = repo.path().join(std::ffi::OsStr::from_bytes(odd));
    std::fs::write(&path, "kept\nchanged\n").unwrap();
    commit_all(&repo, "add odd");
    repo.git([std::ffi::OsStr::new("mv"), std::ffi::OsStr::from_bytes(odd), std::ffi::OsStr::new("plain.txt")]);
    commit_all(&repo, "rename");
    repo.commit_file("plain.txt", "kept\nchanged!\n", "edit");
    let b = blame_of(&repo, b"plain.txt", Some(head(&repo)), false);
    let kept = &b.origins[b.lines[0].origin];
    assert_eq!(kept.filename, odd, "the quoted origin path, unquoted");
    let edited = &b.origins[b.lines[1].origin];
    assert_eq!(edited.filename, "plain.txt");
    assert_eq!(edited.previous.as_ref().unwrap().1, "plain.txt");
}

#[test]
fn blame_in_a_sha256_repository() {
    let repo = TestRepo::new();
    let (ok, _, err) = repo.try_git(["init", "-q", "--object-format=sha256", "sha"]);
    assert!(ok, "{err}");
    let sub = repo.path().join("sha");
    std::fs::write(sub.join("x.txt"), "1\n").unwrap();
    let git = repo.runner().with_workdir(&sub);
    git.output(["add", "-A"]).unwrap();
    git.output(["-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "sha256"]).unwrap();
    let b = blame::blame(&git, b"x.txt", None, false, &CancelToken::default()).unwrap();
    assert_eq!(b.commits[0].id.to_hex().len(), 64);
    assert_eq!(b.commits[0].summary, "sha256");
}

#[test]
fn oversized_files_are_refused() {
    let repo = TestRepo::new();
    let big = vec![b'x'; (blame::MAX_BYTES + 1) as usize];
    std::fs::write(repo.path().join("big.bin"), &big).unwrap();
    let err = blame::blame(&repo.runner(), b"big.bin", None, false, &CancelToken::default()).unwrap_err();
    assert!(format!("{err:#}").contains("too large"), "{err:#}");
    let cancel = CancelToken::default();
    cancel.cancel();
    repo.commit_file("small.txt", "1\n", "small");
    assert!(blame::blame(&repo.runner(), b"small.txt", None, false, &cancel).is_err(), "cancelled");
}

#[test]
fn line_limit_is_checked_before_blaming() {
    let repo = TestRepo::new();
    let five = "1\n2\n3\n4\n5\n";
    repo.commit_file("five.txt", five, "five lines");
    repo.commit_file("six.txt", "1\n2\n3\n4\n5\n6", "six lines, no final newline");
    let limits = blame::Limits { max_bytes: blame::MAX_BYTES, max_lines: 5 };
    let at = |rev: Option<Oid>, name: &str| {
        blame::blame_with_limits(&repo.runner(), name.as_bytes(), rev, false, &CancelToken::default(), limits)
    };
    let head = head(&repo);
    // Exactly at the limit: fine, in the work tree and at a commit.
    assert_eq!(at(None, "five.txt").unwrap().lines.len(), 5);
    assert_eq!(at(Some(head), "five.txt").unwrap().lines.len(), 5);
    // One over: refused before git blame runs.
    for rev in [None, Some(head)] {
        let err = at(rev, "six.txt").unwrap_err();
        assert!(format!("{err:#}").contains("too many lines"), "{err:#}");
    }
}
