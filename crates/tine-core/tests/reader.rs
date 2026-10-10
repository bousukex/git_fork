mod common;

use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

use common::TestRepo;
use tine_core::{
    CancelToken, RepoReader,
    diff::{DiffOptions, DiffSource},
    graph::Graph,
    log::LogOptions,
    model::{ChangeKind, Head, LineKind, Oid, RefKind, RepoState, Tracking},
};

fn oid(s: &str) -> Oid {
    Oid::from_hex(s.as_bytes()).unwrap()
}

fn history(repo: &TestRepo) -> Vec<tine_core::model::CommitSummary> {
    let mut all = Vec::new();
    repo.reader()
        .log(&LogOptions::default(), &CancelToken::default(), &mut |b| {
            all.extend(b);
            true
        })
        .unwrap();
    all
}

#[test]
fn empty_repository() {
    let repo = TestRepo::new();
    let r = repo.reader();
    assert!(history(&repo).is_empty());
    let refs = r.refs().unwrap();
    assert_eq!(refs.head, Some(Head::Unborn { name: "main".into() }));
    assert!(refs.refs.is_empty());
    assert_eq!(r.status().unwrap().change_count(), 0);
}

#[test]
fn discovers_from_subdirectory() {
    let repo = TestRepo::new();
    repo.commit_file("a/b/c.txt", "x", "init");
    let r = tine_core::CliReader::with_runner(repo.runner().with_workdir(repo.path().join("a/b"))).unwrap();
    assert_eq!(r.paths().workdir.canonicalize().unwrap(), repo.path().canonicalize().unwrap());
    assert!(r.paths().git_dir.ends_with(".git"));
    assert!(r.status().unwrap().unstaged.is_empty());
}

#[test]
fn history_with_branches_merges_and_graph() {
    let repo = TestRepo::new();
    let c1 = repo.commit_file("f.txt", "1\n", "first");
    repo.git(["checkout", "-q", "-b", "feature"]);
    let c2 = repo.commit_file("g.txt", "g\n", "feature work");
    repo.git(["checkout", "-q", "main"]);
    let c3 = repo.commit_file("f.txt", "1\n2\n", "main work");
    repo.git(["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"]);
    let merge = repo.git(["rev-parse", "HEAD"]);

    let commits = history(&repo);
    let ids: Vec<String> = commits.iter().map(|c| c.id.to_hex()).collect();
    assert_eq!(ids, [merge.clone(), c3.clone(), c2.clone(), c1.clone()]);
    assert_eq!(commits[0].parents, [oid(&c3), oid(&c2)]);
    assert_eq!(commits[0].subject, "Merge feature");
    assert_eq!(&*commits[0].author_name, "Test User");

    let mut g = Graph::new();
    for c in &commits {
        g.push(c.id, &c.parents);
    }
    assert_eq!(g.rows().iter().map(|r| r.lane).collect::<Vec<_>>(), [0, 0, 1, 0]);

    let d = repo.reader().commit_details(oid(&merge)).unwrap();
    assert_eq!(d.message, "Merge feature");
    assert_eq!(d.parents.len(), 2);
}

#[test]
fn refs_tags_stashes_and_tracking() {
    let origin = TestRepo::new();
    origin.commit_file("a.txt", "a\n", "one");
    let repo = origin.clone_repo();
    repo.commit_file("a.txt", "a\nb\n", "local change");
    repo.git(["tag", "light"]);
    repo.git(["tag", "-a", "v1.0", "-m", "release 1.0"]);
    repo.git(["branch", "topic"]);
    repo.write("a.txt", "dirty\n");
    repo.git(["stash", "push", "-q", "-m", "作業中"]);

    let refs = repo.reader().refs().unwrap();
    assert_eq!(refs.head, Some(Head::Branch { name: "main".into(), target: refs.head_target() }));
    let head = refs.head_target().unwrap();
    let find = |name: &str| refs.refs.iter().find(|r| r.short_name == name).unwrap_or_else(|| panic!("{name}"));
    let main = find("main");
    assert!(main.is_head);
    assert_eq!(main.upstream.as_deref(), Some("origin/main"));
    assert_eq!(main.track, Tracking::Diverged { ahead: 1, behind: 0 });
    assert_eq!(find("origin/main").kind, RefKind::RemoteBranch);
    assert!(refs.refs.iter().all(|r| r.short_name != "origin/HEAD"));
    assert_eq!(find("v1.0").kind, RefKind::Tag);
    assert_eq!(find("v1.0").target, head, "annotated tag must be peeled");
    assert_eq!(find("light").target, head);
    assert!(!find("topic").is_head);
    assert_eq!(refs.remotes, ["origin"]);
    assert_eq!(refs.stashes.len(), 1);
    assert!(refs.stashes[0].message.contains("作業中"), "{}", refs.stashes[0].message);

    // Stash commits are not part of the history list.
    assert_eq!(history(&repo).len(), 2);

    repo.git(["checkout", "-q", "--detach", "HEAD~1"]);
    let refs = repo.reader().refs().unwrap();
    assert!(matches!(refs.head, Some(Head::Detached(_))));
}

#[test]
fn status_with_unusual_file_names() {
    let repo = TestRepo::new();
    repo.commit_file("keep.txt", "1\n", "init");
    repo.commit_file("old name.txt", "same content here\nline2\nline3\n", "add old");

    repo.write("日本語 ファイル.txt", "こんにちは\n");
    repo.write("glob*[x].txt", "g\n");
    let latin1 = OsStr::from_bytes(b"latin\xe9.txt");
    std::fs::write(repo.path().join(latin1), "x\n").unwrap();
    repo.write("keep.txt", "1\n2\n");
    repo.git(["mv", "old name.txt", "new name.txt"]);
    repo.write("staged.txt", "s\n");
    repo.git(["add", "staged.txt"]);

    let st = repo.reader().status().unwrap();
    assert_eq!(st.state, RepoState::Clean);
    let staged: Vec<_> = st.staged.iter().map(|f| (f.path.to_string(), f.kind)).collect();
    assert_eq!(staged, [("new name.txt".into(), ChangeKind::Renamed), ("staged.txt".into(), ChangeKind::Added)]);
    assert_eq!(st.staged[0].orig_path.as_ref().unwrap(), "old name.txt");

    let unstaged: Vec<_> = st.unstaged.iter().map(|f| (f.path.as_slice().to_vec(), f.kind)).collect();
    assert!(unstaged.contains(&("日本語 ファイル.txt".as_bytes().to_vec(), ChangeKind::Untracked)));
    assert!(unstaged.contains(&(b"glob*[x].txt".to_vec(), ChangeKind::Untracked)));
    assert!(unstaged.contains(&(b"latin\xe9.txt".to_vec(), ChangeKind::Untracked)), "{unstaged:?}");
    assert!(unstaged.contains(&(b"keep.txt".to_vec(), ChangeKind::Modified)));

    // Diffs for each source resolve the right file, even with glob characters.
    let r = repo.reader();
    let file = |list: &[tine_core::model::ChangedFile], name: &[u8]| list.iter().find(|f| f.path == name).unwrap().clone();
    let d = r.file_diff(DiffSource::Unstaged, &file(&st.unstaged, b"keep.txt"), DiffOptions::default()).unwrap();
    assert_eq!(d.stats(), (1, 0));
    let d = r.file_diff(DiffSource::Unstaged, &file(&st.unstaged, b"glob*[x].txt"), DiffOptions::default()).unwrap();
    assert_eq!(d.hunks[0].lines[0].content, "g");
    let d = r.file_diff(DiffSource::Unstaged, &file(&st.unstaged, "日本語 ファイル.txt".as_bytes()), DiffOptions::default()).unwrap();
    assert_eq!(d.hunks[0].lines[0].content, "こんにちは");
    let d = r.file_diff(DiffSource::Unstaged, &file(&st.unstaged, b"latin\xe9.txt"), DiffOptions::default()).unwrap();
    assert_eq!(d.stats(), (1, 0));
    let d = r.file_diff(DiffSource::Staged, &st.staged[0], DiffOptions::default()).unwrap();
    assert!(d.header.iter().any(|h| h.starts_with(b"rename from")), "{:?}", d.header);
    assert!(d.hunks.is_empty());
}

#[test]
fn commit_files_and_diffs() {
    let repo = TestRepo::new();
    let root = repo.commit_file("a.txt", "one\ntwo\nthree\n", "root");
    repo.write("bin.dat", "");
    std::fs::write(repo.path().join("bin.dat"), [0u8, 1, 2, 255]).unwrap();
    let c2 = repo.commit_file("a.txt", "one\nTWO\nthree\n", "edit and add binary");

    let r = repo.reader();
    let files = r.commit_files(oid(&root), None).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].kind, ChangeKind::Added);
    let d = r.file_diff(DiffSource::Commit { id: oid(&root), parent: None }, &files[0], DiffOptions::default()).unwrap();
    assert_eq!(d.stats(), (3, 0));

    let files = r.commit_files(oid(&c2), Some(oid(&root))).unwrap();
    let names: Vec<_> = files.iter().map(|f| (f.path.to_string(), f.kind)).collect();
    assert_eq!(names, [("a.txt".into(), ChangeKind::Modified), ("bin.dat".into(), ChangeKind::Added)]);
    let src = DiffSource::Commit { id: oid(&c2), parent: Some(oid(&root)) };
    let d = r.file_diff(src, &files[0], DiffOptions::default()).unwrap();
    let changed: Vec<_> = d.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| (l.kind, l.content.to_string()))
        .collect();
    assert_eq!(changed, [(LineKind::Deleted, "two".into()), (LineKind::Added, "TWO".into())]);
    assert!(r.file_diff(src, &files[1], DiffOptions::default()).unwrap().binary);
}

#[test]
fn merge_conflict_state() {
    let repo = TestRepo::new();
    repo.commit_file("c.txt", "base\n", "base");
    repo.git(["checkout", "-q", "-b", "other"]);
    repo.commit_file("c.txt", "theirs\n", "theirs");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("c.txt", "ours\n", "ours");
    let (ok, stdout, _) = repo.try_git(["merge", "other"]);
    assert!(!ok);
    assert!(stdout.contains("CONFLICT"), "{stdout}");
    let st = repo.reader().status().unwrap();
    assert_eq!(st.state, RepoState::Merging);
    assert_eq!(st.conflicted.len(), 1);
    assert_eq!(st.conflicted[0].path, "c.txt");
}

#[test]
fn log_can_be_cancelled_and_limited() {
    let repo = TestRepo::new();
    for i in 0..30 {
        repo.commit_file("n.txt", &i.to_string(), &format!("c{i}"));
    }
    let r = repo.reader();
    let mut batches = 0;
    r.log(&LogOptions { max_count: Some(10), ..Default::default() }, &CancelToken::default(), &mut |b| {
        assert_eq!(b.len(), 10);
        batches += 1;
        true
    })
    .unwrap();
    assert_eq!(batches, 1);
    let cancel = CancelToken::default();
    cancel.cancel();
    let mut seen = 0;
    r.log(&LogOptions::default(), &cancel, &mut |b| {
        seen += b.len();
        true
    })
    .unwrap();
    assert_eq!(seen, 0);
}

#[test]
fn commit_graph_detection_and_write() {
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    let git = repo.runner();
    assert!(!tine_core::maintenance::has_commit_graph(&git));
    tine_core::maintenance::write_commit_graph(&git).unwrap();
    assert!(tine_core::maintenance::has_commit_graph(&git));
}

#[test]
fn ignored_paths_and_submodules() {
    let repo = TestRepo::new();
    repo.commit_file(".gitignore", "/target/\n*.log\n", "ignore");
    let git = repo.runner();
    let paths: Vec<bstr::BString> =
        ["target/debug/foo", "src/a.log", "src/main.rs", "日本語.txt"].iter().map(|p| (*p).into()).collect();
    let ignored = tine_core::status::ignored(&git, &paths).unwrap();
    assert!(ignored.contains(&bstr::BString::from("target/debug/foo")), "{ignored:?}");
    assert!(ignored.contains(&bstr::BString::from("src/a.log")));
    assert_eq!(ignored.len(), 2);
    assert!(tine_core::status::ignored(&git, &paths[2..]).unwrap().is_empty());

    let sub = TestRepo::new();
    sub.commit_file("x", "1", "sub");
    let url = sub.path().to_str().unwrap().to_owned();
    repo.git(["-c", "protocol.file.allow=always", "submodule", "add", "-q", &url, "libs/sub"]);
    repo.git(["commit", "-qm", "add submodule"]);
    let refs = repo.reader().refs().unwrap();
    assert_eq!(refs.submodules, [bstr::BString::from("libs/sub")]);
}
