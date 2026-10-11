//! The merge editor's core (M5b): splitting a conflict with `git
//! merge-file`, putting it back together byte for byte, what is opened and
//! what isn't, and saving only over what was opened.

mod common;

use std::{
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::Command,
};

use common::TestRepo;
use tine_core::{
    CancelToken, GitRunner,
    integrate::{self, MergeMode, Step},
    merge_file::{self, Choice, Merge, Segment, Unsupported},
    model::RepoState,
};

/// `git merge-file --diff3` itself on the three versions (the reference;
/// without `--diff3`, `--union` and the like join nearby conflicts).
fn reference(ours: &[u8], base: &[u8], theirs: &[u8], marker: usize, how: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let files = ["ours", "base", "theirs"].map(|n| dir.path().join(n));
    for (f, b) in files.iter().zip([ours, base, theirs]) {
        std::fs::write(f, b).unwrap();
    }
    let mut cmd = Command::new("git");
    cmd.args(["merge-file", "-p", "--diff3", &format!("--marker-size={marker}"), "-L", "HEAD", "-L", "base", "-L", "theirs"]);
    if !how.is_empty() {
        cmd.arg(how);
    }
    let out = cmd.args(&files).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(out.status.code().is_some_and(|c| (0..=127).contains(&c)), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn split(ours: &[u8], base: &[u8], theirs: &[u8]) -> Merge {
    let dir = tempfile::tempdir().unwrap();
    merge_file::merge(&GitRunner::new(dir.path()), ours, base, theirs, ["HEAD", "base", "theirs"], &CancelToken::default()).unwrap()
}

/// Every way of resolving matches git's own (markers, --ours, --theirs,
/// --union), byte for byte.
fn check(ours: &[u8], base: &[u8], theirs: &[u8]) -> Merge {
    let m = split(ours, base, theirs);
    let n = m.conflict_count();
    let all = |c: Choice| m.assemble(&vec![c; n]);
    assert_eq!(all(Choice::Unresolved), reference(ours, base, theirs, m.marker, ""), "markers");
    assert_eq!(all(Choice::Ours), reference(ours, base, theirs, m.marker, "--ours"), "ours");
    assert_eq!(all(Choice::Theirs), reference(ours, base, theirs, m.marker, "--theirs"), "theirs");
    assert_eq!(all(Choice::OursThenTheirs), reference(ours, base, theirs, m.marker, "--union"), "union");
    // Theirs first: git's union with the sides swapped.
    assert_eq!(all(Choice::TheirsThenOurs), reference(theirs, base, ours, m.marker, "--union"), "union, theirs first");
    m
}

#[test]
fn splits_match_git_byte_for_byte() {
    // Common lines, one conflict, then several.
    let m = check(b"a\nB1\nc\nd\n", b"a\nb\nc\nd\n", b"a\nB2\nc\nD\n");
    assert_eq!(m.conflict_count(), 1);
    assert_eq!(m.marker, 7);
    let m = check(b"1\nx\n2\nx\n3\n", b"1\n-\n2\n-\n3\n", b"1\ny\n2\ny\n3\n");
    assert_eq!(m.conflict_count(), 2);
    // No conflict at all (changes apart; touching ones conflict in git).
    let m = check(b"a\nX\nc\nd\n", b"a\nb\nc\nd\n", b"a\nb\nc\nD\n");
    assert_eq!(m.conflict_count(), 0);
    // A conflict ending the file, the sides without a last line ending.
    let m = check(b"a\nx", b"a\nb", b"a\ny");
    let Some(Segment::Conflict(c)) = m.segments.last() else { panic!() };
    assert_eq!((c.ours.as_slice(), c.theirs.as_slice()), (&b"x"[..], &b"y"[..]));
    check(b"a\nx\n", b"a\nb", b"a\ny");
    // CRLF, also without the last one; mixed line endings.
    let m = check(b"a\r\nx\r\n", b"a\r\nb\r\n", b"a\r\ny\r\n");
    assert_eq!(m.eol, b"\r\n");
    check(b"a\r\nx", b"a\r\nb", b"a\r\ny");
    check(b"a\r\nb\nx\r\n", b"a\r\nb\nc\r\n", b"a\r\nb\ny\n");
    // Not UTF-8.
    check(b"caf\xe9\nx\n", b"caf\xe9\nb\n", b"caf\xe9\ny\n");
    // A lone CR ending a side is content (git adds the line ending after it).
    let m = check(b"\r", b"b\n", b"y\n");
    let Some(Segment::Conflict(c)) = m.segments.last() else { panic!() };
    assert_eq!(c.ours, b"\r");
    check(b"a\nx\r", b"a\nb\r", b"a\ny\r");
    check(b"a\r\nx\r", b"a\r\nb\r", b"a\r\ny\r");
    // Lines that look like markers: the markers grow past them.
    let m = check(b"<<<<<<< not one\nx\n=========\n", b"<<<<<<< not one\nb\n=========\n", b"<<<<<<< not one\ny\n=========\n");
    assert_eq!(m.marker, 10);
    // Added on both sides (no base); a side deleting the lines.
    check(b"one\n", b"", b"two\n");
    check(b"a\nc\n", b"a\nb\nc\n", b"a\nB\nc\n");
    // More conflicts than git's exit code counts.
    let ours: String = (0..300).map(|i| format!("o{i}\nkeep{i}\n")).collect();
    let base: String = (0..300).map(|i| format!("b{i}\nkeep{i}\n")).collect();
    let theirs: String = (0..300).map(|i| format!("t{i}\nkeep{i}\n")).collect();
    assert_eq!(check(ours.as_bytes(), base.as_bytes(), theirs.as_bytes()).conflict_count(), 300);
}

#[test]
fn choosing_the_base_and_mixing_choices() {
    let m = split(b"1\nx\n2\nx\n3\n", b"1\n-\n2\n-\n3\n", b"1\ny\n2\ny\n3\n");
    assert_eq!(m.assemble(&[Choice::Base, Choice::TheirsThenOurs]), b"1\n-\n2\ny\nx\n3\n");
    // An unresolved one keeps its markers among resolved ones.
    let out = m.assemble(&[Choice::Ours]);
    assert!(out.starts_with(b"1\nx\n2\n<<<<<<< HEAD\n"), "{}", String::from_utf8_lossy(&out));
    // Concatenating a side without a last line ending.
    let m = split(b"a\nx", b"a\nb", b"a\ny");
    assert_eq!(m.assemble(&[Choice::OursThenTheirs]), b"a\nx\ny");
}

#[test]
fn what_git_merge_file_cant_do_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let failing = dir.path().join("git");
    std::fs::write(&failing, "#!/bin/sh\necho broken >&2\nexit 255\n").unwrap();
    std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let git = GitRunner::new(dir.path()).with_program(failing);
    let e = merge_file::merge(&git, b"a\n", b"b\n", b"c\n", ["HEAD", "base", "theirs"], &CancelToken::default()).unwrap_err();
    assert!(matches!(e.downcast_ref::<Unsupported>(), Some(Unsupported::MergeFailed(_))), "{e:#}");
}

/// main and topic both change f.txt (also `extra` lines on each side);
/// returns the repository merging topic into main, with the conflict.
fn conflicted(repo: &TestRepo, base: &str, main: &str, topic: &str) {
    repo.commit_file("f.txt", base, "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("f.txt", topic, "topic change");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", main, "main change");
    let step = integrate::merge(&repo.runner(), &repo.path().join(".git"), "topic", MergeMode::Default).unwrap();
    assert_eq!(step, Step::Conflicts);
}

fn open(repo: &TestRepo, path: &str) -> anyhow::Result<merge_file::Opened> {
    merge_file::open(&repo.runner(), &repo.path().join(".git"), path.as_bytes(), &CancelToken::default())
}

fn unsupported(repo: &TestRepo, path: &str) -> Unsupported {
    let e = open(repo, path).unwrap_err();
    e.downcast_ref::<Unsupported>().unwrap_or_else(|| panic!("{e:#}")).clone()
}

#[test]
fn opens_a_merge_conflict_and_names_the_sides() {
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let o = open(&repo, "f.txt").unwrap();
    assert_eq!(o.state, RepoState::Merging);
    assert_eq!(o.merge.conflict_count(), 1);
    assert_eq!(o.merge.assemble(&[Choice::Theirs]), b"a\ntopic\nc\n");
    assert!(o.sides.ours.starts_with("HEAD — main"), "{}", o.sides.ours);
    assert!(o.sides.theirs.contains("topic change"), "{}", o.sides.theirs);
    assert_eq!(o.sides.labels[2], "topic");
}

#[test]
fn what_isnt_for_the_editor_says_why() {
    // Deleted on one side.
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "a\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("f.txt", "topic\n", "topic change");
    repo.git(["checkout", "-q", "main"]);
    repo.git(["rm", "-q", "f.txt"]);
    repo.git(["commit", "-q", "-m", "gone"]);
    integrate::merge(&repo.runner(), &repo.path().join(".git"), "topic", MergeMode::Default).unwrap();
    assert_eq!(unsupported(&repo, "f.txt"), Unsupported::OneSided);
    assert_eq!(unsupported(&repo, "nothing.txt"), Unsupported::NotConflicted);

    // Binary.
    let repo = TestRepo::new();
    conflicted(&repo, "a\0\n", "main\0\n", "topic\0\n");
    assert_eq!(unsupported(&repo, "f.txt"), Unsupported::Binary);

    // A symbolic link on both sides.
    let repo = TestRepo::new();
    std::os::unix::fs::symlink("one", repo.path().join("link")).unwrap();
    repo.git(["add", "-A"]);
    repo.git(["commit", "-q", "-m", "base"]);
    repo.git(["checkout", "-q", "-b", "topic"]);
    std::fs::remove_file(repo.path().join("link")).unwrap();
    std::os::unix::fs::symlink("two", repo.path().join("link")).unwrap();
    repo.git(["commit", "-q", "-am", "topic"]);
    repo.git(["checkout", "-q", "main"]);
    std::fs::remove_file(repo.path().join("link")).unwrap();
    std::os::unix::fs::symlink("three", repo.path().join("link")).unwrap();
    repo.git(["commit", "-q", "-am", "main"]);
    integrate::merge(&repo.runner(), &repo.path().join(".git"), "topic", MergeMode::Default).unwrap();
    assert_eq!(unsupported(&repo, "link"), Unsupported::Symlink);

    // Attributes: a filter, an encoding, a merge driver (picked as git
    // does: configured drivers, also under git's own names, and
    // merge.default).
    for (attr, config, expect) in [
        ("f.txt filter=lfs", None, Some(Unsupported::Filter("lfs".into()))),
        ("f.txt working-tree-encoding=UTF-16", None, Some(Unsupported::WorkingTreeEncoding("UTF-16".into()))),
        ("f.txt merge=mine", Some(("merge.mine.driver", "cat %A")), Some(Unsupported::Driver("mine".into()))),
        ("f.txt merge=text", Some(("merge.text.driver", "cat %A")), Some(Unsupported::Driver("text".into()))),
        ("", Some(("merge.default", "mine")), None),
        ("f.txt -merge", None, Some(Unsupported::Driver("binary".into()))),
        ("f.txt merge=binary", None, Some(Unsupported::Driver("binary".into()))),
        // Names git doesn't know merge as text; so does a set attribute.
        ("f.txt merge=unknown", None, None),
        ("f.txt merge", Some(("merge.default", "mine")), None),
    ] {
        let repo = TestRepo::new();
        conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
        std::fs::write(repo.path().join(".git/info/attributes"), format!("{attr}\n")).unwrap();
        if let Some((k, v)) = config {
            repo.git(["config", k, v]);
            if k == "merge.default" {
                repo.git(["config", "merge.mine.driver", "cat %A"]);
            }
        }
        match expect {
            Some(e) => assert_eq!(unsupported(&repo, "f.txt"), e, "{attr} {config:?}"),
            None if config.is_some_and(|(k, _)| k == "merge.default") && attr.is_empty() => {
                assert_eq!(unsupported(&repo, "f.txt"), Unsupported::Driver("mine".into()))
            }
            None => assert!(open(&repo, "f.txt").is_ok(), "{attr} {config:?}"),
        }
    }

    // Too large.
    let repo = TestRepo::new();
    let big = "x\n".repeat(merge_file::MAX_SIDE_LINES + 1);
    conflicted(&repo, &format!("{big}b\n"), &format!("{big}main\n"), &format!("{big}topic\n"));
    assert_eq!(unsupported(&repo, "f.txt"), Unsupported::TooLarge);
}

#[test]
fn both_added_has_no_base() {
    let repo = TestRepo::new();
    repo.commit_file("other", "x\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("f.txt", "topic\n", "topic adds");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", "main\n", "main adds");
    integrate::merge(&repo.runner(), &repo.path().join(".git"), "topic", MergeMode::Default).unwrap();
    let o = open(&repo, "f.txt").unwrap();
    assert!(o.stages.base.is_none());
    let Some(Segment::Conflict(c)) = o.merge.segments.first() else { panic!() };
    assert_eq!((c.ours.as_slice(), c.base.as_slice(), c.theirs.as_slice()), (&b"main\n"[..], &b""[..], &b"topic\n"[..]));
}

fn save(repo: &TestRepo, o: &merge_file::Opened, result: &[u8], resolved: bool) -> anyhow::Result<merge_file::Saved> {
    merge_file::save(&repo.runner(), &repo.path().join(".git"), &o.target(), &o.work, result, resolved)
}

fn conflicted_paths(repo: &TestRepo) -> String {
    repo.git(["diff", "--name-only", "--diff-filter=U"])
}

#[test]
fn saving_writes_what_git_would_check_out() {
    // Line endings converted as for a checkout; the executable bit kept.
    let repo = TestRepo::new();
    std::fs::write(repo.path().join(".gitattributes"), "f.txt text eol=crlf\n").unwrap();
    repo.git(["add", ".gitattributes"]);
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let o = open(&repo, "f.txt").unwrap();
    assert_eq!(o.merge.eol, b"\n", "the stages are normalized");
    let result = o.merge.assemble(&[Choice::OursThenTheirs]);
    let saved = save(&repo, &o, &result, false).unwrap();
    assert_eq!(saved.staged, None);
    assert_eq!(std::fs::read(repo.path().join("f.txt")).unwrap(), b"a\r\nmain\r\ntopic\r\nc\r\n");
    assert_eq!(conflicted_paths(&repo), "f.txt", "still conflicted");
    // Saving again goes from what was written.
    let saved = merge_file::save(&repo.runner(), &repo.path().join(".git"), &o.target(), &saved.work, &result, true).unwrap();
    assert_eq!(saved.staged, Some(Ok(())));
    assert_eq!(conflicted_paths(&repo), "");
    assert_eq!(repo.git(["show", ":f.txt"]), "a\nmain\ntopic\nc", "stored normalized");

    let repo = TestRepo::new();
    repo.write("run.sh", "a\nb\n");
    repo.git(["add", "-A"]);
    repo.git(["update-index", "--chmod=+x", "run.sh"]);
    repo.git(["commit", "-q", "-m", "base"]);
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("run.sh", "a\ntopic\n", "topic");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("run.sh", "a\nmain\n", "main");
    integrate::merge(&repo.runner(), &repo.path().join(".git"), "topic", MergeMode::Default).unwrap();
    let o = open(&repo, "run.sh").unwrap();
    std::fs::set_permissions(repo.path().join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let o = merge_file::Opened { work: merge_file::work_file(&repo.path().join("run.sh"), &CancelToken::default()).unwrap(), ..o };
    save(&repo, &o, b"a\nboth\n", true).unwrap();
    assert_ne!(std::fs::metadata(repo.path().join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(repo.git(["ls-files", "-s", "run.sh"]).split(' ').next(), Some("100755"));
}

#[test]
fn saving_over_something_changed_is_refused() {
    let fresh = || {
        let repo = TestRepo::new();
        conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
        let o = open(&repo, "f.txt").unwrap();
        (repo, o)
    };
    let refused = |r: anyhow::Result<merge_file::Saved>| {
        let e = r.unwrap_err();
        assert!(e.is::<merge_file::Changed>(), "{e:#}");
    };
    // Edited outside the editor.
    let (repo, o) = fresh();
    std::fs::write(repo.path().join("f.txt"), "edited elsewhere\n").unwrap();
    refused(save(&repo, &o, b"mine\n", false));
    assert_eq!(std::fs::read_to_string(repo.path().join("f.txt")).unwrap(), "edited elsewhere\n");
    // Resolved elsewhere (the stages are gone).
    let (repo, o) = fresh();
    repo.git(["add", "f.txt"]);
    refused(save(&repo, &o, b"mine\n", false));
    // The merge aborted.
    let (repo, o) = fresh();
    repo.git(["merge", "--abort"]);
    refused(save(&repo, &o, b"mine\n", false));
    // HEAD moved (same operation, same stages).
    let (repo, o) = fresh();
    repo.git(["update-ref", "HEAD", "HEAD~"]);
    refused(save(&repo, &o, b"mine\n", false));
    // Its permissions changed, or it became a link.
    let (repo, o) = fresh();
    std::fs::set_permissions(repo.path().join("f.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
    refused(save(&repo, &o, b"mine\n", false));
    let (repo, o) = fresh();
    std::fs::remove_file(repo.path().join("f.txt")).unwrap();
    std::os::unix::fs::symlink("elsewhere", repo.path().join("f.txt")).unwrap();
    refused(save(&repo, &o, b"mine\n", false));
    assert!(repo.path().join("f.txt").symlink_metadata().unwrap().file_type().is_symlink());
    // A link already when opened (and still the same one): never written
    // through, nor replaced.
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("target"), "outside\n").unwrap();
    std::fs::remove_file(repo.path().join("f.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("target"), repo.path().join("f.txt")).unwrap();
    let o = open(&repo, "f.txt").unwrap();
    refused(save(&repo, &o, b"mine\n", false));
    assert!(repo.path().join("f.txt").symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(outside.path().join("target")).unwrap(), "outside\n");
    // What applies to it changed after opening (now through a filter).
    let (repo, o) = fresh();
    std::fs::write(repo.path().join(".git/info/attributes"), "f.txt filter=lfs\n").unwrap();
    refused(save(&repo, &o, b"mine\n", false));
}

#[test]
fn saving_never_writes_through_files_already_there() {
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let o = open(&repo, "f.txt").unwrap();
    // Where a fixed temporary name would have been: a file and a link.
    let pid = std::process::id();
    std::fs::write(repo.path().join(format!(".f.txt.tine-merge-{pid}")), "keep\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("target"), "outside\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("target"), repo.path().join(".f.txt.tine-merge")).unwrap();
    save(&repo, &o, b"a\nmine\nc\n", false).unwrap();
    assert_eq!(std::fs::read_to_string(repo.path().join(format!(".f.txt.tine-merge-{pid}"))).unwrap(), "keep\n");
    assert_eq!(std::fs::read_to_string(outside.path().join("target")).unwrap(), "outside\n");
    assert_eq!(std::fs::read_to_string(repo.path().join("f.txt")).unwrap(), "a\nmine\nc\n");
    // Nothing of its own left behind.
    let names: Vec<String> = std::fs::read_dir(repo.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names.iter().filter(|n| n.contains("tine-merge")).count(), 2, "{names:?}");
}

#[test]
fn a_huge_working_tree_file_isnt_read() {
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let f = std::fs::File::options().write(true).open(repo.path().join("f.txt")).unwrap();
    f.set_len(merge_file::MAX_WORK_BYTES + 1).unwrap();
    assert_eq!(unsupported(&repo, "f.txt"), Unsupported::TooLarge);
}

#[test]
fn sides_outside_an_operation_and_of_merge_commits() {
    // A stash applied with conflicts: stage 2 is what was there.
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "a\n", "base");
    repo.write("f.txt", "stashed\n");
    repo.git(["stash", "push", "-q"]);
    repo.commit_file("f.txt", "committed\n", "later");
    let (ok, _, _) = repo.try_git(["stash", "apply"]);
    assert!(!ok);
    let o = open(&repo, "f.txt").unwrap();
    assert_eq!(o.state, RepoState::Clean);
    assert!(o.sides.ours.contains("stage 2") && o.sides.theirs.contains("stage 3"), "{:?}", o.sides);
    // A merge reverted with -m: git's chosen parent, not REVERT_HEAD^.
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "a\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("f.txt", "t\n", "topic");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("g.txt", "g\n", "other");
    repo.git(["merge", "-q", "--no-edit", "topic"]);
    let merge = repo.git(["rev-parse", "HEAD"]);
    repo.commit_file("f.txt", "m\n", "after");
    let (ok, _, _) = repo.try_git(["revert", "--no-edit", "-m", "1", &merge]);
    assert!(!ok);
    let o = open(&repo, "f.txt").unwrap();
    assert!(o.sides.theirs.contains("chosen with -m"), "{}", o.sides.theirs);
}

#[test]
fn a_failed_add_still_saves() {
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let o = open(&repo, "f.txt").unwrap();
    let lock = repo.path().join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    let saved = save(&repo, &o, b"a\nmine\nc\n", true).unwrap();
    assert!(matches!(&saved.staged, Some(Err(e)) if e.contains("index.lock")), "{:?}", saved.staged);
    assert_eq!(std::fs::read_to_string(repo.path().join("f.txt")).unwrap(), "a\nmine\nc\n");
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(conflicted_paths(&repo), "f.txt");
}

#[test]
fn an_unresolved_save_keeps_the_markers() {
    let repo = TestRepo::new();
    conflicted(&repo, "a\nb\nc\n", "a\nmain\nc\n", "a\ntopic\nc\n");
    let o = open(&repo, "f.txt").unwrap();
    save(&repo, &o, &o.merge.assemble(&[]), false).unwrap();
    let text = std::fs::read_to_string(repo.path().join("f.txt")).unwrap();
    assert!(text.contains("<<<<<<< HEAD\nmain\n||||||| base\nb\n=======\ntopic\n>>>>>>> topic\n"), "{text}");
    assert_eq!(merge_file::marker_lines(text.as_bytes(), o.merge.marker), 4);
    let _ = Path::new("");
}
