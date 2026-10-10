//! Fetch, pull and push against local bare repositories (M3b).

mod common;

use common::TestRepo;
use tine_core::{
    RepoReader,
    integrate::Step,
    model::{Oid, RepoState},
    remote::{self, FetchTarget, PushOutcome, PushSpec, RefUpdate},
};

/// A bare remote with one commit on main, and two clones of it.
fn setup() -> (TestRepo, TestRepo, TestRepo) {
    let bare = TestRepo::new_bare();
    let seed = TestRepo::clone_from(bare.path());
    seed.commit_file("f.txt", "base\n", "base");
    seed.git(["push", "-q", "origin", "main"]);
    let a = TestRepo::clone_from(bare.path());
    let b = TestRepo::clone_from(bare.path());
    (bare, a, b)
}

fn spec(branch: &str) -> PushSpec {
    PushSpec {
        remote: "origin".into(),
        branch: branch.into(),
        remote_branch: branch.into(),
        set_upstream: false,
        tags: Vec::new(),
        force_lease: None,
    }
}

fn reported(out: PushOutcome) -> Vec<(RefUpdate, String)> {
    match out {
        PushOutcome::Reported { refs, failed: None } => refs.into_iter().map(|r| (r.update, r.short().to_owned())).collect(),
        other => panic!("{other:?}"),
    }
}

fn oid(repo: &TestRepo, rev: &str) -> Oid {
    Oid::from_hex(repo.git(["rev-parse", rev]).as_bytes()).unwrap()
}

#[test]
fn fetch_all_and_prune() {
    let (_bare, a, b) = setup();
    b.git(["switch", "-q", "-c", "x"]);
    b.commit_file("x.txt", "x\n", "x");
    b.git(["push", "-q", "origin", "x"]);
    let mut lines = Vec::new();
    remote::fetch(&a.runner(), &FetchTarget::All, false, &mut |l| lines.push(l.to_owned())).unwrap();
    assert_eq!(a.git(["rev-parse", "origin/x"]), b.git(["rev-parse", "x"]));

    b.git(["push", "-q", "origin", "--delete", "x"]);
    remote::fetch(&a.runner(), &FetchTarget::Remote("origin".into()), false, &mut |_| {}).unwrap();
    assert!(a.try_git(["rev-parse", "--verify", "-q", "origin/x"]).0, "kept without prune");
    remote::fetch(&a.runner(), &FetchTarget::Remote("origin".into()), true, &mut |_| {}).unwrap();
    assert!(!a.try_git(["rev-parse", "--verify", "-q", "origin/x"]).0, "pruned");
    assert!(remote::fetch(&a.runner(), &FetchTarget::Remote("nope".into()), false, &mut |_| {}).is_err());
    assert!(remote::fetch(&a.runner(), &FetchTarget::Remote("--all".into()), false, &mut |_| {}).is_err());
}

#[test]
fn pull_merges_or_rebases_as_asked() {
    let (_bare, a, b) = setup();
    b.commit_file("b.txt", "b\n", "from b");
    b.git(["push", "-q", "origin", "main"]);
    a.commit_file("a.txt", "a\n", "from a");
    let start = a.git(["rev-parse", "HEAD"]);
    let gd = a.path().join(".git");
    let git = a.runner();

    assert_eq!(remote::pull(&git, &gd, "origin", "main", false, &mut |_| {}).unwrap(), Step::Done);
    assert_eq!(a.git(["rev-list", "--count", "--merges", "HEAD"]), "1", "merged");

    a.git(["reset", "-q", "--hard", &start]);
    assert_eq!(remote::pull(&git, &gd, "origin", "main", true, &mut |_| {}).unwrap(), Step::Done);
    assert_eq!(a.git(["rev-list", "--count", "--merges", "HEAD"]), "0", "rebased");
    assert_eq!(a.git(["log", "-1", "--format=%s"]), "from a");
    assert_eq!(a.git(["log", "-1", "--format=%s", "HEAD~1"]), "from b");

    assert!(!remote::pull_rebases_by_default(&git));
    a.git(["config", "pull.rebase", "true"]);
    assert!(remote::pull_rebases_by_default(&git));
    a.git(["config", "pull.rebase", "false"]);
    assert!(!remote::pull_rebases_by_default(&git));
}

#[test]
fn pull_conflicts_leave_the_matching_state() {
    for (rebase, state) in [(false, RepoState::Merging), (true, RepoState::Rebasing)] {
        let (_bare, a, b) = setup();
        b.commit_file("f.txt", "theirs\n", "theirs");
        b.git(["push", "-q", "origin", "main"]);
        a.commit_file("f.txt", "ours\n", "ours");
        let gd = a.path().join(".git");
        assert_eq!(remote::pull(&a.runner(), &gd, "origin", "main", rebase, &mut |_| {}).unwrap(), Step::Conflicts);
        assert_eq!(a.reader().status().unwrap().state, state);
    }
}

#[test]
fn push_new_branch_with_upstream_and_tags() {
    let (bare, a, _b) = setup();
    a.git(["switch", "-q", "-c", "feat"]);
    a.commit_file("feat.txt", "f\n", "feat");
    a.git(["tag", "v1"]);
    let mut lines = Vec::new();
    let out = remote::push(
        &a.runner(),
        &PushSpec { set_upstream: true, tags: vec!["v1".into()], ..spec("feat") },
        &mut |l| lines.push(l.to_owned()),
    )
    .unwrap();
    assert_eq!(reported(out), [(RefUpdate::New, "feat".to_owned()), (RefUpdate::New, "v1".to_owned())]);
    assert_eq!(a.git(["rev-parse", "--abbrev-ref", "feat@{upstream}"]), "origin/feat");
    assert_eq!(bare.git(["rev-parse", "v1"]), a.git(["rev-parse", "v1"]));
    assert!(lines.iter().any(|l| l.contains("Writing objects")), "{lines:?}");

    // Pushing again: nothing new.
    let out = remote::push(&a.runner(), &spec("feat"), &mut |_| {}).unwrap();
    assert_eq!(reported(out), [(RefUpdate::UpToDate, "feat".to_owned())]);
}

#[test]
fn push_is_rejected_when_the_remote_moved() {
    let (_bare, a, b) = setup();
    b.commit_file("b.txt", "b\n", "from b");
    b.git(["push", "-q", "origin", "main"]);
    a.commit_file("a.txt", "a\n", "from a");
    let out = remote::push(&a.runner(), &spec("main"), &mut |_| {}).unwrap();
    let PushOutcome::Reported { refs, failed: None } = out else { panic!("{out:?}") };
    assert_eq!(refs[0].update, RefUpdate::Rejected);
    assert!(refs[0].summary.contains("fetch first") || refs[0].summary.contains("non-fast-forward"), "{}", refs[0].summary);
}

#[test]
fn force_with_lease_uses_the_confirmed_value() {
    let (bare, a, b) = setup();
    a.commit_file("a.txt", "a\n", "from a");
    a.git(["push", "-q", "origin", "main"]);
    a.git(["commit", "-q", "--amend", "-m", "rewritten"]);
    // The user confirms the force push while origin/main is here…
    let confirmed = oid(&a, "origin/main");
    // …then someone pushes, and a background fetch updates origin/main.
    b.git(["pull", "-q", "--rebase=false", "origin", "main"]);
    b.commit_file("b.txt", "b\n", "from b");
    b.git(["push", "-q", "origin", "main"]);
    a.git(["fetch", "-q"]);
    let out = remote::push(&a.runner(), &PushSpec { force_lease: Some(Some(confirmed)), ..spec("main") }, &mut |_| {}).unwrap();
    let PushOutcome::Reported { refs, failed: None } = out else { panic!("{out:?}") };
    assert_eq!(refs[0].update, RefUpdate::Rejected, "b's commit is not overwritten");
    assert!(refs[0].summary.contains("stale info"), "{}", refs[0].summary);
    assert_eq!(bare.git(["log", "-1", "--format=%s", "main"]), "from b");

    // Confirmed against the current value, it goes through.
    let current = oid(&a, "origin/main");
    let out = remote::push(&a.runner(), &PushSpec { force_lease: Some(Some(current)), ..spec("main") }, &mut |_| {}).unwrap();
    assert_eq!(reported(out), [(RefUpdate::Forced, "main".to_owned())]);
    assert_eq!(bare.git(["log", "-1", "--format=%s", "main"]), "rewritten");

    // "Must not exist yet" for a branch that does exist.
    a.git(["commit", "-q", "--amend", "-m", "again"]);
    let out = remote::push(&a.runner(), &PushSpec { force_lease: Some(None), ..spec("main") }, &mut |_| {}).unwrap();
    let PushOutcome::Reported { refs, failed: None } = out else { panic!("{out:?}") };
    assert_eq!(refs[0].update, RefUpdate::Rejected);
}

fn hook(repo: &TestRepo, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = repo.path().join("hooks").join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn partial_success_reports_each_ref() {
    let (bare, a, _b) = setup();
    hook(&bare, "update", r#"case "$1" in refs/tags/*) echo "no tags here" >&2; exit 1;; esac"#);
    a.commit_file("a.txt", "a\n", "from a");
    a.git(["tag", "v2"]);
    let out = remote::push(&a.runner(), &PushSpec { tags: vec!["v2".into()], ..spec("main") }, &mut |_| {}).unwrap();
    let PushOutcome::Reported { refs, failed: None } = out else { panic!("{out:?}") };
    assert_eq!(refs.len(), 2);
    assert_eq!((refs[0].update, refs[0].short()), (RefUpdate::FastForward, "main"));
    assert_eq!((refs[1].update, refs[1].short()), (RefUpdate::Rejected, "v2"));
    assert!(refs[1].summary.contains("hook declined"), "{}", refs[1].summary);
    assert_eq!(bare.git(["rev-parse", "main"]), a.git(["rev-parse", "main"]));
}

#[test]
fn unreachable_remote_is_a_plain_failure() {
    let (_bare, a, _b) = setup();
    a.git(["remote", "add", "gone", "/nonexistent/repo.git"]);
    let err = remote::push(&a.runner(), &PushSpec { remote: "gone".into(), ..spec("main") }, &mut |_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("does not appear to be a git repository"), "{err:#}");
}

#[test]
fn connection_lost_mid_push_is_checked_with_ls_remote() {
    // The remote dies before (prepared) or after (committed) updating refs.
    for (state, applied) in [("prepared", false), ("committed", true)] {
        let (bare, a, _b) = setup();
        hook(&bare, "reference-transaction", &format!(r#"[ "$1" = {state} ] && kill -9 $PPID; exit 0"#));
        a.commit_file("a.txt", "a\n", "from a");
        let out = remote::push(&a.runner(), &spec("main"), &mut |_| {}).unwrap();
        let PushOutcome::Unknown { verified, detail } = out else { panic!("{state}: {out:?}") };
        std::fs::remove_file(bare.path().join("hooks/reference-transaction")).unwrap();
        if applied {
            assert_eq!(verified, ["refs/heads/main"], "{detail}");
            assert!(detail.contains("has all pushed refs"), "{detail}");
        } else {
            assert!(verified.is_empty(), "{detail}");
        }
    }
}

#[test]
fn delete_remote_branch() {
    let (bare, a, _b) = setup();
    a.git(["push", "-q", "origin", "main:refs/heads/tmp"]);
    remote::delete_remote_ref(&a.runner(), "origin", "refs/heads/tmp", &mut |_| {}).unwrap();
    assert!(!bare.try_git(["rev-parse", "--verify", "-q", "refs/heads/tmp"]).0);
    let have = remote::ls_remote(&a.runner(), "origin", &["refs/heads/main", "refs/heads/tmp"]).unwrap();
    assert_eq!(have.keys().collect::<Vec<_>>(), ["refs/heads/main"]);
    // Already gone: git reports it deleted (with a warning).
    remote::delete_remote_ref(&a.runner(), "origin", "refs/heads/tmp", &mut |_| {}).unwrap();
    assert!(remote::delete_remote_ref(&a.runner(), "origin", "main", &mut |_| {}).is_err(), "full names only");
}

#[test]
fn auth_failures_get_advice() {
    let (_bare, a, _b) = setup();
    let git = a.runner();
    let msg = "fatal: could not read Username for 'https://example.com': terminal prompts disabled";
    assert!(remote::auth_hint(&git, msg).unwrap().contains("credential.helper"));
    a.git(["config", "credential.helper", "store"]);
    assert_eq!(remote::auth_hint(&git, msg), None, "a helper is set up");
    assert!(remote::auth_hint(&git, "git@host: Permission denied (publickey).").unwrap().contains("ssh-add"));
    assert_eq!(remote::auth_hint(&git, "fatal: not a git repository"), None);
}

#[test]
fn a_failing_second_push_url_is_not_a_success() {
    let (bare, a, _b) = setup();
    let url = a.git(["remote", "get-url", "origin"]);
    assert!(url.starts_with("file://"), "{url}");
    a.git(["remote", "set-url", "--add", "--push", "origin", &url]);
    a.git(["remote", "set-url", "--add", "--push", "origin", "file:///nonexistent/mirror.git"]);
    a.commit_file("a.txt", "a\n", "from a");
    let out = remote::push(&a.runner(), &spec("main"), &mut |_| {}).unwrap();
    let PushOutcome::Reported { refs, failed } = out else { panic!("{out:?}") };
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].update, RefUpdate::FastForward, "the first URL took it");
    assert!(refs[0].destination.contains(bare.path().to_str().unwrap()), "{}", refs[0].destination);
    let failed = failed.expect("the second URL's failure is reported");
    assert!(failed.contains("does not appear to be a git repository"), "{failed}");
}

#[test]
fn cancelling_while_the_connection_breaks_is_a_cancel() {
    let (bare, a, _b) = setup();
    hook(&bare, "reference-transaction", r#"[ "$1" = committed ] && kill -9 $PPID; exit 0"#);
    a.commit_file("a.txt", "a\n", "from a");
    let cancel = tine_core::CancelToken::default();
    let git = a.runner().with_cancel(cancel.clone());
    // The user cancels as the push falls apart (e.g. a prompt while
    // checking the remote): the result is a cancel, not "unknown".
    let err = remote::push(&git, &spec("main"), &mut |line| {
        if line.contains("hung up") || line.contains("disconnect") {
            cancel.cancel();
        }
    })
    .unwrap_err();
    assert!(err.downcast_ref::<tine_core::Cancelled>().is_some(), "{err:#}");
}
