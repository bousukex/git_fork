//! Local branch operations, history operations and recovery (M3a).

mod common;

use common::TestRepo;
use tine_core::{
    RepoReader,
    autostash::{self, AutoStash},
    branches::{self, Created, DeleteCheck, RemoteCheckout},
    integrate::{self, MergeMode, ResetMode, Step},
    model::{ConflictKind, Head, RefKind, RepoState},
    ops, sequencer,
    sequencer::Resolution,
    stash,
};

fn head_name(repo: &TestRepo) -> String {
    repo.git(["symbolic-ref", "--short", "-q", "HEAD"])
}

fn head(repo: &TestRepo) -> String {
    repo.git(["rev-parse", "HEAD"])
}

fn read(repo: &TestRepo, f: &str) -> String {
    std::fs::read_to_string(repo.path().join(f)).unwrap()
}

/// main: base → main change; topic: base → topic change, both editing f.txt.
fn diverged(repo: &TestRepo) {
    repo.commit_file("f.txt", "base\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.commit_file("f.txt", "topic\n", "topic change");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", "main\n", "main change");
}

fn git_dir(repo: &TestRepo) -> std::path::PathBuf {
    repo.path().join(".git")
}

#[test]
fn checkout_variants() {
    let origin = TestRepo::new();
    origin.commit_file("a", "1", "one");
    origin.git(["branch", "feature"]);
    origin.git(["branch", "other"]);
    let repo = origin.clone_repo();
    let git = repo.runner();
    let refs = repo.reader().refs().unwrap();
    let remote = |name: &str| refs.refs.iter().find(|r| r.kind == RefKind::RemoteBranch && r.short_name == name).unwrap().clone();

    // No local "feature" yet: create a tracking branch.
    assert_eq!(branches::plan_remote_checkout(&refs, &remote("origin/feature")), RemoteCheckout::CreateTracking("feature".into()));
    branches::create_tracking(&git, "feature", "origin/feature").unwrap();
    assert_eq!(head_name(&repo), "feature");
    let refs = repo.reader().refs().unwrap();
    assert_eq!(branches::plan_remote_checkout(&refs, &remote("origin/feature")), RemoteCheckout::SwitchExisting("feature".into()));

    // A local "other" that tracks nothing: name taken, a free name suggested.
    repo.git(["branch", "--no-track", "other", "HEAD"]);
    let refs = repo.reader().refs().unwrap();
    match branches::plan_remote_checkout(&refs, &remote("origin/other")) {
        RemoteCheckout::NameTaken { local, upstream, suggestion } => {
            assert_eq!(local, "other");
            assert_eq!(upstream, None);
            assert_eq!(suggestion, "other-2");
        }
        x => panic!("{x:?}"),
    }

    branches::switch(&git, "main").unwrap();
    assert_eq!(head_name(&repo), "main");
    let c = head(&repo);
    branches::switch_detached(&git, &c).unwrap();
    assert!(matches!(repo.reader().refs().unwrap().head, Some(Head::Detached(_))));
}

#[test]
fn branch_in_another_worktree_is_protected() {
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    let wt = tempfile::tempdir().unwrap();
    let wt_path = wt.path().join("wt");
    repo.git(["worktree", "add", "-q", "-b", "side", wt_path.to_str().unwrap()]);
    let git = repo.runner();
    let err = branches::switch(&git, "side").unwrap_err();
    assert!(format!("{err:#}").contains("another worktree"), "{err:#}");
    let refs = repo.reader().refs().unwrap();
    assert_eq!(branches::check_delete(&git, &refs, "side").unwrap(), DeleteCheck::InOtherWorktree);
}

#[test]
fn create_rename_delete_and_tags() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "one\n", "one");
    let first = head(&repo);
    repo.commit_file("f.txt", "two\n", "two");
    let git = repo.runner();

    assert!(branches::validate_branch_name(&git, "bad..name").is_err());
    assert!(branches::validate_branch_name(&git, "").is_err());
    assert_eq!(branches::create_branch(&git, "old-start", &first, false).unwrap(), Created::Created);
    assert_eq!(head_name(&repo), "main");

    // Local changes that switching would overwrite: created, not switched.
    repo.write("f.txt", "local edit\n");
    match branches::create_branch(&git, "from-first", &first, true).unwrap() {
        Created::CreatedNotSwitched(why) => assert!(why.contains("overwritten") || why.contains("local changes"), "{why}"),
        x => panic!("{x:?}"),
    }
    assert_eq!(head_name(&repo), "main");
    repo.git(["checkout", "--", "f.txt"]);
    assert_eq!(branches::create_branch(&git, "new-work", "HEAD", true).unwrap(), Created::CreatedAndSwitched);
    assert_eq!(head_name(&repo), "new-work");

    branches::rename_branch(&git, "new-work", "renamed").unwrap();
    assert_eq!(head_name(&repo), "renamed");
    let refs = repo.reader().refs().unwrap();
    assert_eq!(branches::check_delete(&git, &refs, "renamed").unwrap(), DeleteCheck::Current);
    assert_eq!(branches::check_delete(&git, &refs, "nope").unwrap(), DeleteCheck::Missing);
    assert_eq!(branches::check_delete(&git, &refs, "old-start").unwrap(), DeleteCheck::Safe);

    repo.git(["checkout", "-q", "-b", "unmerged"]);
    repo.commit_file("u.txt", "u", "unmerged work");
    repo.git(["checkout", "-q", "main"]);
    let refs = repo.reader().refs().unwrap();
    assert_eq!(branches::check_delete(&git, &refs, "unmerged").unwrap(), DeleteCheck::Unmerged);
    assert!(branches::delete_branch(&git, "unmerged", false).is_err(), "git refuses -d");
    branches::delete_branch(&git, "unmerged", true).unwrap();
    branches::delete_branch(&git, "old-start", false).unwrap();

    branches::create_tag(&git, "v1", &first, None).unwrap();
    branches::create_tag(&git, "v2", "HEAD", Some("リリース 2")).unwrap();
    assert!(branches::create_tag(&git, "bad tag", "HEAD", None).is_err());
    assert_eq!(repo.git(["cat-file", "-t", "v2"]), "tag", "annotated");
    assert_eq!(repo.git(["cat-file", "-t", "v1"]), "commit", "lightweight");
    branches::delete_tag(&git, "v1").unwrap();
    assert!(!repo.try_git(["rev-parse", "-q", "--verify", "refs/tags/v1"]).0);
}

#[test]
fn reset_modes() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "one\n", "one");
    let first = head(&repo);
    repo.commit_file("f.txt", "two\n", "two");
    let git = repo.runner();
    let second = head(&repo);

    integrate::reset(&git, &first, ResetMode::Soft).unwrap();
    assert_eq!(head(&repo), first);
    assert_eq!(repo.reader().status().unwrap().staged.len(), 1, "soft keeps the change staged");
    integrate::reset(&git, &second, ResetMode::Mixed).unwrap();
    repo.write("f.txt", "edited\n");
    integrate::reset(&git, &first, ResetMode::Mixed).unwrap();
    let st = repo.reader().status().unwrap();
    assert!(st.staged.is_empty() && st.unstaged.len() == 1, "mixed keeps the work tree");
    integrate::reset(&git, &first, ResetMode::Hard).unwrap();
    assert_eq!(read(&repo, "f.txt"), "one\n");
    assert!(integrate::reset(&git, "--hard", ResetMode::Soft).is_err());
}

#[test]
fn merge_conflict_abort_restores_everything() {
    let repo = TestRepo::new();
    diverged(&repo);
    let git = repo.runner();
    let before = head(&repo);
    assert_eq!(integrate::merge(&git, &git_dir(&repo), "topic", MergeMode::Default).unwrap(), Step::Conflicts);
    let st = repo.reader().status().unwrap();
    assert_eq!(st.state, RepoState::Merging);
    assert_eq!(st.conflict_kinds.get(&bstr::BString::from("f.txt")), Some(&ConflictKind::BothModified));
    assert!(sequencer::has_conflict_markers(repo.path(), &"f.txt".into()));
    sequencer::abort(&git, RepoState::Merging).unwrap();
    let st = repo.reader().status().unwrap();
    assert_eq!(st.state, RepoState::Clean);
    assert_eq!(head(&repo), before);
    assert_eq!(read(&repo, "f.txt"), "main\n");
    assert!(st.staged.is_empty() && st.unstaged.is_empty());
}

#[test]
fn merge_resolved_with_ours_completes_with_an_unchanged_tree() {
    let repo = TestRepo::new();
    diverged(&repo);
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    integrate::merge(&git, &git_dir(&repo), "topic", MergeMode::Default).unwrap();
    sequencer::resolve(&git, &["f.txt".into()], Resolution::Ours).unwrap();
    let st = repo.reader().status().unwrap();
    assert!(st.conflicted.is_empty() && st.staged.is_empty(), "index equals HEAD now");
    assert_eq!(st.state, RepoState::Merging);
    // Finishing the merge commits even though nothing differs from HEAD.
    ops::commit(&git, "Merge branch 'topic'\n", &ops::CommitOptions { amend: false, allow_empty: true }).unwrap();
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean);
    assert_eq!(repo.git(["rev-list", "--count", "--merges", "HEAD"]), "1");
    assert_eq!(read(&repo, "f.txt"), "main\n");
}

#[test]
fn merge_modes() {
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    repo.git(["checkout", "-q", "-b", "ahead"]);
    repo.commit_file("b", "2", "two");
    repo.git(["checkout", "-q", "main"]);
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    assert_eq!(integrate::merge(&git, &git_dir(&repo), "ahead", MergeMode::NoFastForward).unwrap(), Step::Done);
    assert_eq!(repo.git(["rev-list", "--count", "--merges", "HEAD"]), "1");
    diverged(&repo);
    assert!(integrate::merge(&git, &git_dir(&repo), "topic", MergeMode::FastForwardOnly).is_err());
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean, "ff-only failure leaves no merge");
}

#[test]
fn rebase_skip_and_continue() {
    let repo = TestRepo::new();
    diverged(&repo);
    repo.git(["checkout", "-q", "topic"]);
    repo.commit_file("g.txt", "g\n", "topic second");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    assert_eq!(integrate::rebase(&git, &git_dir(&repo), "main").unwrap(), Step::Conflicts);
    assert_eq!(tine_core::status::rebase_progress(&git_dir(&repo)), Some((1, 2)));
    // Skip the conflicting commit; the second one applies cleanly.
    assert_eq!(sequencer::skip(&git, &git_dir(&repo), RepoState::Rebasing).unwrap(), Step::Done);
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean);
    assert_eq!(read(&repo, "f.txt"), "main\n");
    assert_eq!(read(&repo, "g.txt"), "g\n");

    // Again, resolving instead of skipping.
    let repo = TestRepo::new();
    diverged(&repo);
    repo.git(["checkout", "-q", "topic"]);
    integrate::rebase(&git.with_workdir(repo.path()), &git_dir(&repo), "main").unwrap();
    let git = git.with_workdir(repo.path());
    sequencer::resolve(&git, &["f.txt".into()], Resolution::Theirs).unwrap();
    assert_eq!(sequencer::continue_op(&git, &git_dir(&repo), RepoState::Rebasing).unwrap(), Step::Done);
    assert_eq!(read(&repo, "f.txt"), "topic\n");
    assert_eq!(repo.git(["log", "-1", "--format=%s"]), "topic change");
}

#[test]
fn cherry_pick_that_becomes_empty() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    let picked = repo.commit_file("f.txt", "same\n", "make it same");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", "same\n", "already same on main");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    let step = integrate::cherry_pick(&git, &git_dir(&repo), &picked).unwrap();
    assert_eq!(step, Step::Empty);
    assert_eq!(repo.reader().status().unwrap().state, RepoState::CherryPicking);
    sequencer::skip(&git, &git_dir(&repo), RepoState::CherryPicking).unwrap();
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean);

    // Or keep it as an empty commit.
    let before = repo.git(["rev-list", "--count", "HEAD"]);
    assert_eq!(integrate::cherry_pick(&git, &git_dir(&repo), &picked).unwrap(), Step::Empty);
    sequencer::commit_empty(&git).unwrap();
    assert_eq!(repo.git(["rev-list", "--count", "HEAD"]), (before.parse::<u32>().unwrap() + 1).to_string());
}

#[test]
fn revert_conflict_then_continue() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "one\n", "one");
    let target = repo.commit_file("f.txt", "two\n", "two");
    repo.commit_file("f.txt", "three\n", "three");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    assert_eq!(integrate::revert(&git, &git_dir(&repo), &target).unwrap(), Step::Conflicts);
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Reverting);
    repo.write("f.txt", "resolved by hand\n");
    sequencer::resolve(&git, &["f.txt".into()], Resolution::MarkResolved).unwrap();
    assert_eq!(sequencer::continue_op(&git, &git_dir(&repo), RepoState::Reverting).unwrap(), Step::Done);
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean);
    assert!(repo.git(["log", "-1", "--format=%s"]).starts_with("Revert \"two\""));
}

#[test]
fn deleted_by_them_resolves_by_deletion() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    repo.git(["rm", "-q", "f.txt"]);
    repo.git(["commit", "-qm", "delete"]);
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", "edited\n", "edit");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    integrate::merge(&git, &git_dir(&repo), "topic", MergeMode::Default).unwrap();
    let st = repo.reader().status().unwrap();
    let kind = st.conflict_kinds[&bstr::BString::from("f.txt")];
    assert_eq!(kind, ConflictKind::DeletedByThem);
    assert!(kind.can_delete() && kind.has_ours() && !kind.has_theirs());
    sequencer::resolve(&git, &["f.txt".into()], Resolution::Delete).unwrap();
    assert!(repo.reader().status().unwrap().conflicted.is_empty());
    assert!(!repo.path().join("f.txt").exists());
}

#[test]
fn stash_by_oid_survives_index_shifts() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    let git = repo.runner();
    repo.write("f.txt", "first change\n");
    let first = stash::push(&git, "first", false, false).unwrap().unwrap();
    repo.write("f.txt", "second change\n");
    let second = stash::push(&git, "second", false, false).unwrap().unwrap();
    assert_eq!(stash::index_of(&git, first).unwrap(), Some(1));
    assert!(stash::push(&git, "nothing", false, false).unwrap().is_none(), "clean tree: no stash");

    stash::drop(&git, first).unwrap();
    assert_eq!(stash::index_of(&git, second).unwrap(), Some(0), "the right one was dropped");
    assert!(stash::drop(&git, first).is_err(), "gone");

    // Staged changes come back staged.
    repo.write("f.txt", "staged\n");
    repo.git(["add", "f.txt"]);
    let staged = stash::push(&git, "staged", false, false).unwrap().unwrap();
    let applied = stash::pop(&git, staged).unwrap();
    assert!(applied.index_restored && !applied.conflicts);
    assert_eq!(repo.reader().status().unwrap().staged.len(), 1);
    assert_eq!(stash::index_of(&git, staged).unwrap(), None, "pop dropped it");
}

#[test]
fn stash_conflict_keeps_stash_and_undo_removes_only_created_files() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    let git = repo.runner();
    repo.write("f.txt", "stashed\n");
    repo.write("new.txt", "untracked from stash\n");
    let oid = stash::push(&git, "with untracked", true, false).unwrap().unwrap();
    assert_eq!(stash::untracked_files(&git, oid).unwrap(), [bstr::BString::from("new.txt")]);
    repo.commit_file("f.txt", "committed meanwhile\n", "conflicting commit");
    // An unrelated untracked file that existed before the apply.
    repo.write("mine.txt", "keep me\n");

    let applied = stash::pop(&git, oid).unwrap();
    assert!(applied.conflicts && !applied.complete());
    assert!(applied.was_clean_before);
    assert_eq!(applied.created_untracked, [bstr::BString::from("new.txt")]);
    assert_eq!(stash::index_of(&git, oid).unwrap(), Some(0), "kept on conflict");
    assert_eq!(repo.reader().status().unwrap().state, RepoState::Clean, "no git marker for stash conflicts");

    stash::undo_apply(&git, &git_dir(&repo), &applied.created_untracked).unwrap();
    let st = repo.reader().status().unwrap();
    assert!(st.conflicted.is_empty() && st.staged.is_empty());
    assert_eq!(read(&repo, "f.txt"), "committed meanwhile\n");
    assert!(!repo.path().join("new.txt").exists(), "created by the apply: removed");
    assert_eq!(read(&repo, "mine.txt"), "keep me\n", "pre-existing untracked file: untouched");
}

#[test]
fn stash_apply_refuses_to_overwrite_existing_untracked_file() {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    let git = repo.runner();
    repo.write("new.txt", "from stash\n");
    let oid = stash::push(&git, "", true, false).unwrap().unwrap();
    repo.write("new.txt", "the user's own file\n");
    assert!(stash::apply(&git, oid).is_err());
    assert_eq!(read(&repo, "new.txt"), "the user's own file\n");
}

/// f.txt has 5 lines; `moved` changes line 3 on top.
fn five_lines(repo: &TestRepo) {
    repo.commit_file("f.txt", "1\n2\n3\n4\n5\n", "five");
    repo.git(["branch", "moved"]);
    repo.git(["checkout", "-q", "moved"]);
    repo.commit_file("f.txt", "1\n2\nTHREE\n4\n5\n", "line 3");
    repo.git(["checkout", "-q", "main"]);
}

#[test]
fn autostash_paths() {
    let repo = TestRepo::new();
    repo.commit_file("g.txt", "g\n", "g");
    repo.git(["checkout", "-q", "-b", "side"]);
    repo.commit_file("h.txt", "h\n", "side adds h");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("i.txt", "i\n", "main adds i");
    let git = repo.runner();
    let gd = git_dir(&repo);

    // Clean tree: nothing stashed.
    let (r, report) = autostash::run(&git, &gd, "test", false, || Ok(()));
    assert!(r.is_ok());
    assert_eq!(report, AutoStash::NotNeeded);

    // A real merge succeeds and the changes come back.
    repo.write("other.txt", "untracked\n");
    repo.write("g.txt", "local edit\n");
    let (r, report) =
        autostash::run(&git, &gd, "merge", false, || integrate::merge(&git, &gd, "side", MergeMode::Default));
    assert_eq!(r.unwrap(), Step::Done);
    assert_eq!(report, AutoStash::Restored);
    assert_eq!(read(&repo, "h.txt"), "h\n", "merged");
    assert_eq!(read(&repo, "g.txt"), "local edit\n");
    assert_eq!(read(&repo, "other.txt"), "untracked\n");
    assert_eq!(repo.git(["stash", "list"]), "");

    // A failing operation that left HEAD alone gives the changes back.
    let (r, report) = autostash::run(&git, &gd, "fail", false, || anyhow::bail!("boom") as anyhow::Result<()>);
    assert!(r.is_err());
    assert_eq!(report, AutoStash::Restored);
    assert_eq!(read(&repo, "g.txt"), "local edit\n");

    // A failing operation that moved HEAD: the changes stay stashed.
    let (r, report) = autostash::run(&git, &gd, "switch", false, || {
        repo.git(["switch", "-q", "side"]);
        anyhow::bail!("post-checkout hook failed") as anyhow::Result<()>
    });
    assert!(r.is_err());
    let AutoStash::NotRestored { oid, reason } = &report else { panic!("{report:?}") };
    assert!(reason.contains("refs/heads/side"), "{reason}");
    assert_eq!(read(&repo, "g.txt"), "g\n", "not applied onto the wrong branch");
    assert_eq!(stash::index_of(&git, *oid).unwrap(), Some(0));
}

#[test]
fn autostash_keeps_stash_when_operation_stops() {
    let repo = TestRepo::new();
    diverged(&repo);
    let git = repo.runner();
    let gd = git_dir(&repo);
    repo.commit_file("g.txt", "g\n", "g");
    repo.write("g.txt", "local edit\n");
    let (r, report) =
        autostash::run(&git, &gd, "merge", false, || integrate::merge(&git, &gd, "topic", MergeMode::Default));
    assert_eq!(r.unwrap(), Step::Conflicts);
    let AutoStash::Kept(oid) = report else { panic!("{report:?}") };
    assert!(report.describe().unwrap().contains(&oid.short()));
    assert_eq!(read(&repo, "g.txt"), "g\n", "changes are in the stash, not mixed into the merge");
    sequencer::abort(&git, RepoState::Merging).unwrap();
    assert!(stash::pop(&git, oid).unwrap().complete());
    assert_eq!(read(&repo, "g.txt"), "local edit\n");
}

#[test]
fn autostash_keeps_stash_when_staging_cannot_be_restored() {
    let repo = TestRepo::new();
    five_lines(&repo);
    let git = repo.runner();
    let gd = git_dir(&repo);
    // Line 1 staged, line 5 changed on top of that in the work tree.
    repo.write("f.txt", "ONE\n2\n3\n4\n5\n");
    repo.git(["add", "f.txt"]);
    repo.write("f.txt", "ONE\n2\n3\n4\nFIVE\n");
    let (r, report) =
        autostash::run(&git, &gd, "merge", false, || integrate::merge(&git, &gd, "moved", MergeMode::Default));
    assert_eq!(r.unwrap(), Step::Done);
    let AutoStash::RestoredUnstaged(oid) = report else { panic!("{report:?}") };
    assert_eq!(read(&repo, "f.txt"), "ONE\n2\nTHREE\n4\nFIVE\n", "content is back");
    assert_eq!(stash::index_of(&git, oid).unwrap(), Some(0), "stash kept as the record of the staging");
}

#[test]
fn autostash_for_checkout_also_moves_untracked_files() {
    let repo = TestRepo::new();
    repo.commit_file("a.txt", "a\n", "a");
    repo.git(["checkout", "-q", "-b", "other"]);
    repo.commit_file("blocker.txt", "tracked on other\n", "other adds blocker");
    repo.git(["checkout", "-q", "main"]);
    repo.write("blocker.txt", "my untracked file\n");
    let git = repo.runner();
    let gd = git_dir(&repo);
    assert!(branches::switch(&git, "other").is_err(), "the untracked file blocks the switch");
    let (r, report) = autostash::run(&git, &gd, "switch", true, || branches::switch(&git, "other"));
    r.unwrap();
    // Re-applying the untracked file onto `other` (where it is tracked) fails
    // safely: it stays in the stash.
    assert!(report.stash().is_some(), "{report:?}");
    assert_eq!(read(&repo, "blocker.txt"), "tracked on other\n");
}

#[test]
fn hard_reset_lists_untracked_files_it_would_replace() {
    let repo = TestRepo::new();
    repo.commit_file("f", "tracked\n", "has f");
    let with_f = repo.git(["rev-parse", "HEAD"]);
    repo.git(["rm", "-q", "f"]);
    repo.git(["commit", "-qm", "remove f"]);
    repo.write("f", "untracked now\n");
    repo.write("g.txt", "unrelated untracked\n");
    let lost = integrate::hard_reset_losses(&repo.runner(), &with_f).unwrap();
    assert_eq!(lost, ["f (untracked)"]);
}

#[test]
fn continue_failing_in_a_hook_is_an_error_not_conflicts() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "base\n", "base");
    repo.git(["checkout", "-q", "-b", "topic"]);
    let pick = repo.commit_file("f.txt", "topic\n", "topic");
    repo.git(["checkout", "-q", "main"]);
    repo.commit_file("f.txt", "main\n", "main");
    let git = repo.runner();
    assert_eq!(integrate::cherry_pick(&git, &git_dir(&repo), &pick).unwrap(), Step::Conflicts);
    sequencer::resolve(&git, &["f.txt".into()], Resolution::Theirs).unwrap();
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'hook says no' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = sequencer::continue_op(&git, &git_dir(&repo), RepoState::CherryPicking).unwrap_err();
    assert!(format!("{err:#}").contains("hook says no"), "{err:#}");
}

#[test]
fn delete_check_follows_git_rules_for_upstreams() {
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    repo.git(["branch", "base"]);
    repo.git(["checkout", "-q", "-b", "feature", "--track", "base"]);
    repo.commit_file("b", "2", "feature work");
    repo.git(["checkout", "-q", "main"]);
    repo.git(["merge", "-q", "--ff-only", "feature"]);
    let git = repo.runner();
    let refs = repo.reader().refs().unwrap();
    // Merged into HEAD but not into its upstream `base`: git -d refuses.
    assert_eq!(branches::check_delete(&git, &refs, "feature").unwrap(), DeleteCheck::Unmerged);
    assert!(branches::delete_branch(&git, "feature", false).is_err());
    // Once its upstream contains it, it is safe.
    repo.git(["branch", "-f", "base", "feature"]);
    assert_eq!(branches::check_delete(&git, &refs, "feature").unwrap(), DeleteCheck::Safe);
    branches::delete_branch(&git, "feature", false).unwrap();
}

#[test]
fn ensure_ready_rejects_dirty_and_in_progress() {
    let repo = TestRepo::new();
    diverged(&repo);
    let git = repo.runner();
    let gd = git_dir(&repo);
    integrate::ensure_ready(&git, &gd, false).unwrap();
    repo.write("f.txt", "dirty\n");
    let err = integrate::ensure_ready(&git, &gd, false).unwrap_err();
    assert!(err.downcast_ref::<integrate::DirtyWorkTree>().is_some());
    integrate::ensure_ready(&git, &gd, true).unwrap();
    repo.git(["checkout", "--", "f.txt"]);
    integrate::merge(&git, &gd, "topic", MergeMode::Default).unwrap();
    assert!(integrate::ensure_ready(&git, &gd, true).is_err());
}

#[test]
fn stash_with_untracked_really_removes_them() {
    // Regression: with GIT_LITERAL_PATHSPECS=1 (set for every command), git
    // stash -u saved untracked files but left them in the work tree.
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    repo.write("u.txt", "untracked\n");
    let git = repo.runner();
    let oid = stash::push(&git, "", true, false).unwrap().unwrap();
    assert!(!repo.path().join("u.txt").exists());
    assert!(stash::pop(&git, oid).unwrap().complete());
    assert_eq!(read(&repo, "u.txt"), "untracked\n");
}

#[test]
fn merging_full_ref_names_reads_like_short_names() {
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    let git = repo.runner().with_env("GIT_COMMITTER_NAME", "T").with_env("GIT_COMMITTER_EMAIL", "t@e");
    let subject = |repo: &TestRepo| repo.git(["log", "-1", "--format=%s"]);
    for (i, (target, expected)) in [
        ("refs/heads/b1", "Merge branch 'b1'"),
        ("refs/tags/t1", "Merge tag 't1'"),
        ("refs/remotes/origin/r1", "Merge remote-tracking branch 'origin/r1'"),
    ]
    .into_iter()
    .enumerate()
    {
        repo.git(["checkout", "-q", "--detach"]);
        repo.commit_file(&format!("side{i}"), "x", "side commit");
        repo.git(["update-ref", target, "HEAD"]);
        repo.git(["checkout", "-q", "main"]);
        assert_eq!(integrate::merge(&git, &git_dir(&repo), target, MergeMode::NoFastForward).unwrap(), Step::Done);
        assert_eq!(subject(&repo), expected);
    }
    // A tag with the branch's name doesn't change what is merged.
    repo.git(["branch", "dup", "HEAD~1"]);
    repo.git(["tag", "dup", "HEAD"]);
    let before = head(&repo);
    integrate::merge(&git, &git_dir(&repo), "refs/heads/dup", MergeMode::Default).unwrap();
    assert_eq!(head(&repo), before, "the branch is already merged");
}

#[test]
fn cancelled_commit_stops_the_hook_and_leaves_no_lock() {
    use std::{os::unix::fs::PermissionsExt as _, time::Duration};
    let repo = TestRepo::new();
    repo.commit_file("a", "1", "one");
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.write("a", "2");
    repo.git(["add", "a"]);
    let before = head(&repo);
    let cancel_when = |cancel: tine_core::CancelToken, ready: Box<dyn Fn() -> bool + Send>| {
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while !ready() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            cancel.cancel();
        })
    };

    // Our commit (message on stdin), stopped while the hook runs.
    let cancel = tine_core::CancelToken::default();
    let git = repo.runner().with_cancel(cancel.clone());
    let start = std::time::Instant::now();
    let waiter = cancel_when(cancel, Box::new(move || start.elapsed() > Duration::from_millis(300)));
    let err = ops::commit(&git, "never", &Default::default()).unwrap_err();
    waiter.join().unwrap();
    assert!(err.downcast_ref::<tine_core::Cancelled>().is_some(), "{err:#}");
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    assert_eq!(head(&repo), before);
    assert_eq!(repo.git(["diff", "--cached", "--name-only"]), "a", "the staged change is untouched");

    // `commit -a` holds index.lock while its hook runs: SIGTERM lets git
    // remove it (SIGKILL would leave it behind).
    let cancel = tine_core::CancelToken::default();
    let git = repo.runner().with_cancel(cancel.clone());
    let lock = repo.path().join(".git/index.lock");
    let waiter = cancel_when(cancel, Box::new(move || lock.exists()));
    let err = git.output(["commit", "-a", "-q", "-m", "never"]).unwrap_err();
    waiter.join().unwrap();
    assert!(err.downcast_ref::<tine_core::Cancelled>().is_some(), "{err:#}");
    assert!(tine_core::maintenance::leftover_locks(repo.reader().paths()).is_empty());
    assert_eq!(head(&repo), before);
}
