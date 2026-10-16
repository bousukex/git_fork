//! Commit search (M4a).

mod common;

use common::TestRepo;
use pengitto_core::{
    CancelToken,
    log::default_revisions,
    model::Oid,
    search::{SearchEnd, SearchKind, hash_prefix_matches, search},
};

fn find(repo: &TestRepo, kind: SearchKind, q: &str) -> Vec<String> {
    let mut found = Vec::new();
    let end = search(&repo.runner(), kind, q, &CancelToken::default(), |b| {
        found.extend(b);
        true
    })
    .unwrap();
    assert_eq!(end, SearchEnd::Complete);
    found.iter().map(|id| repo.git(["log", "-1", "--format=%s", &id.to_hex()])).collect()
}

fn commit_msg(repo: &TestRepo, file: &str, body: &str, msg: &str) {
    repo.write(file, body);
    repo.git(["add", "-A"]);
    repo.git(["commit", "-q", "-m", msg]);
}

#[test]
fn message_and_author() {
    let repo = TestRepo::new();
    commit_msg(&repo, "a", "1", "Fix parser\n\nThe Borrow checker was wrong.");
    commit_msg(&repo, "a", "2", "Add abc");
    commit_msg(&repo, "a", "3", "日本語のコミット");
    assert_eq!(find(&repo, SearchKind::Message, "borrow CHECKER"), ["Fix parser"], "body, any case");
    assert_eq!(find(&repo, SearchKind::Message, "a.c"), Vec::<String>::new(), "literal, not a regex");
    assert_eq!(find(&repo, SearchKind::Message, "日本語"), ["日本語のコミット"]);
    assert_eq!(find(&repo, SearchKind::Author, "TEST@example"), ["日本語のコミット", "Add abc", "Fix parser"]);
    assert!(find(&repo, SearchKind::Author, "nobody").is_empty());
}

#[test]
fn changes_count_occurrences_and_include_merge_resolutions() {
    let repo = TestRepo::new();
    commit_msg(&repo, "f.txt", "base\n", "base");
    commit_msg(&repo, "f.txt", "base\nneedle\n", "add needle");
    commit_msg(&repo, "f.txt", "base\nNEEDLE\n", "change case");
    commit_msg(&repo, "f.txt", "base\nneedle needle\n", "two needles");
    commit_msg(&repo, "f.txt", "needle needle\nbase\n", "move lines");
    assert_eq!(find(&repo, SearchKind::Changes, "needle"), ["two needles", "change case", "add needle"], "count changes only, case-sensitive");

    // A merge whose conflict resolution introduces the text.
    repo.git(["checkout", "-q", "-b", "side"]);
    commit_msg(&repo, "g.txt", "side\n", "side");
    repo.git(["checkout", "-q", "main"]);
    commit_msg(&repo, "g.txt", "main\n", "main g");
    let (ok, _, _) = repo.try_git(["merge", "-q", "side"]);
    assert!(!ok, "conflicts");
    repo.write("g.txt", "resolved with haystack\n");
    repo.git(["add", "g.txt"]);
    repo.git(["commit", "-q", "--no-edit"]);
    assert_eq!(find(&repo, SearchKind::Changes, "haystack"), ["Merge branch 'side'"]);
}

#[test]
fn path_search_is_literal_and_case_insensitive() {
    let repo = TestRepo::new();
    commit_msg(&repo, "src/Parser.rs", "1", "parser");
    commit_msg(&repo, "docs/日本語.md", "1", "japanese doc");
    commit_msg(&repo, "weird[1].txt", "1", "brackets");
    commit_msg(&repo, "top.txt", "1", "root file");
    commit_msg(&repo, "src/other.rs", "1", "other");
    assert_eq!(find(&repo, SearchKind::Path, "parser"), ["parser"]);
    assert_eq!(find(&repo, SearchKind::Path, "日本"), ["japanese doc"]);
    assert_eq!(find(&repo, SearchKind::Path, "[1]"), ["brackets"], "glob characters match literally");
    assert_eq!(find(&repo, SearchKind::Path, "top"), ["root file"], "files at the root");
    assert_eq!(find(&repo, SearchKind::Path, "*"), Vec::<String>::new(), "a star is just a star");
    // The pathspec override is local to the search.
    assert_eq!(repo.runner().output(["log", "--format=%s", "--", "*.rs"]).unwrap(), b"", "other commands stay literal");
}

#[test]
fn cancelled_search_reports_stopped() {
    let repo = TestRepo::new();
    for i in 0..30 {
        commit_msg(&repo, "a", &i.to_string(), &format!("match {i}"));
    }
    let cancel = CancelToken::default();
    cancel.cancel();
    let end = search(&repo.runner(), SearchKind::Message, "match", &cancel, |_| true).unwrap();
    assert_eq!(end, SearchEnd::Stopped);
    // The receiver stopping also counts as stopped.
    let end = search(&repo.runner(), SearchKind::Message, "match", &CancelToken::default(), |_| false).unwrap();
    assert_eq!(end, SearchEnd::Stopped);
}

#[test]
fn revision_set_edge_cases() {
    // Empty repository: nothing, no error.
    let empty = TestRepo::new();
    assert!(find(&empty, SearchKind::Message, "x").is_empty());
    assert!(!default_revisions(&empty.runner()).contains(&"HEAD".to_owned()));

    // Unborn HEAD but another branch exists.
    let repo = TestRepo::new();
    commit_msg(&repo, "a", "1", "on main");
    repo.git(["checkout", "-q", "--orphan", "fresh"]);
    assert_eq!(find(&repo, SearchKind::Message, "on main"), ["on main"]);
    assert!(!default_revisions(&repo.runner()).contains(&"HEAD".to_owned()));

    // A commit reachable only from a detached HEAD is found.
    let repo = TestRepo::new();
    commit_msg(&repo, "a", "1", "base");
    repo.git(["checkout", "-q", "--detach"]);
    commit_msg(&repo, "a", "2", "detached only");
    assert_eq!(find(&repo, SearchKind::Message, "detached"), ["detached only"]);
}

#[test]
fn hash_prefixes() {
    let a = Oid::from_hex(b"abcdef0123456789abcdef0123456789abcdef01").unwrap();
    let b = Oid::from_hex(b"abcd000000000000000000000000000000000000").unwrap();
    let ids = [a, b];
    assert_eq!(hash_prefix_matches("ABCD", ids.iter()), vec![a, b]);
    assert_eq!(hash_prefix_matches("abcde", ids.iter()), vec![a]);
    assert!(hash_prefix_matches("abc", ids.iter()).is_empty(), "at least 4 digits");
    assert!(hash_prefix_matches("abcz", ids.iter()).is_empty(), "hex only");
}

#[test]
fn path_search_matches_directories_and_partial_paths() {
    let repo = TestRepo::new();
    commit_msg(&repo, "src/deep/nested/file.rs", "1", "deep file");
    commit_msg(&repo, "docs/readme.md", "1", "docs");
    commit_msg(&repo, "srcfile.txt", "1", "root srcfile");
    assert_eq!(find(&repo, SearchKind::Path, "src"), ["root srcfile", "deep file"], "a directory name and a file name");
    assert_eq!(find(&repo, SearchKind::Path, "deep/nes"), ["deep file"], "several components");
    assert_eq!(find(&repo, SearchKind::Path, "NESTED"), ["deep file"], "a directory deep down, any case");
    assert_eq!(find(&repo, SearchKind::Path, "c/deep"), ["deep file"]);
}

#[test]
fn changes_search_includes_binary_files() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("data.bin"), b"a\0b needle\0").unwrap();
    repo.git(["add", "-A"]);
    repo.git(["commit", "-q", "-m", "binary with needle"]);
    commit_msg(&repo, "t.txt", "needle\n", "text with needle");
    assert_eq!(find(&repo, SearchKind::Changes, "needle"), ["text with needle", "binary with needle"]);
}

#[test]
fn cancelling_before_or_during_preparation_is_stopped() {
    // An empty repository returns before any git log: still "stopped".
    let empty = TestRepo::new();
    let cancel = CancelToken::default();
    cancel.cancel();
    let end = search(&empty.runner(), SearchKind::Message, "x", &cancel, |_| true).unwrap();
    assert_eq!(end, SearchEnd::Stopped);
    let repo = TestRepo::new();
    commit_msg(&repo, "a", "1", "one");
    let end = search(&repo.runner(), SearchKind::Changes, "x", &cancel, |_| panic!("no results after a cancel")).unwrap();
    assert_eq!(end, SearchEnd::Stopped);
}
