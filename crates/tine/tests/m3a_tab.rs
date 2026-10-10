//! M3a flows through RepoTab: checkout, branches, merge/rebase with local
//! changes, conflicts and stash conflicts.

mod common;

use common::{diverged_repo, git, idle, poll_until};
use tine::repo::{
    RepoTab, View,
    dialog::{CheckoutTarget, Dialog},
};
use tine_core::{
    model::{RefKind, RepoState},
    sequencer::Resolution,
};

fn open(dir: &std::path::Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, idle));
    tab
}

fn head_name(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["symbolic-ref", "--short", "HEAD"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn read(dir: &std::path::Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap()
}

#[test]
fn checkout_and_stash_and_switch() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.checkout_branch("topic");
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "topic"));

    // A local edit to f.txt blocks switching back: offer Stash & Switch.
    std::fs::write(d.join("f.txt"), "local edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    tab.checkout_branch("main");
    assert!(poll_until(&mut tab, |t| idle(t) && t.dialog.is_some()));
    assert_eq!(tab.dialog, Some(Dialog::StashAndSwitch { target: CheckoutTarget::Branch("main".into()) }));
    assert!(tab.errors.is_empty(), "a decision, not an error: {:?}", tab.errors);
    tab.dialog = None;
    tab.stash_and_switch(CheckoutTarget::Branch("main".into()));
    // Switched; re-applying conflicts with main's f.txt, so the stash is kept.
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "main"));
    assert!(poll_until(&mut tab, |t| t.stash_conflict.is_some()));
    assert_eq!(tab.view, View::LocalChanges);
    assert!(tab.block_reason(tine::repo::Needs::CleanState).is_some());
}

#[test]
fn delete_unmerged_branch_asks_for_force() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.delete_branch("topic", false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.dialog.is_some()));
    assert_eq!(tab.dialog, Some(Dialog::DeleteBranch { name: "topic".into(), force: true }));
    tab.dialog = None;
    tab.delete_branch("topic", true);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.refs.refs.iter().any(|r| r.short_name == "topic")));
    // Deleting the current branch is refused with a clear reason.
    tab.delete_branch("main", false);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.last().unwrap().contains("current branch"), "{:?}", tab.errors);
}

#[test]
fn create_rename_tag() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.create_branch("feature/新機能", "HEAD", true);
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "feature/新機能"));
    tab.rename_branch("feature/新機能", "feature/renamed");
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "feature/renamed"));
    tab.create_tag("v1.0", "HEAD", "release");
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.refs.iter().any(|r| r.kind == RefKind::Tag && r.short_name == "v1.0")));
    // Every operation left a record of its git commands.
    let ops = tine::ops_registry::snapshots_for(tab.repo_key());
    assert!(ops.len() >= 3);
    assert!(ops.iter().all(|o| !o.commands.is_empty()));
}

#[test]
fn merge_with_local_changes_stashes_and_recovers() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("g.txt"), "my local work\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));

    // Without stashing, merging with local changes is refused.
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() }, false);
    assert!(tab.errors.last().unwrap().contains("Commit or stash"));
    tab.errors.clear();
    tab.request_merge("topic", "topic");
    assert!(matches!(tab.dialog, Some(Dialog::Merge { stash_first: true, .. })));
    tab.dialog = None;

    // With stashing: the merge conflicts, the changes stay safely stashed.
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() }, true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Merging));
    assert!(poll_until(&mut tab, |t| t.pending_autostash.is_some()));
    assert_eq!(tab.view, View::LocalChanges);
    assert_eq!(read(d, "g.txt"), "g\n", "local work is stashed, not mixed into the merge");

    // Abort, then re-apply the stashed work.
    tab.request_abort();
    assert_eq!(tab.dialog, Some(Dialog::Abort { state: RepoState::Merging }));
    tab.dialog = None;
    tab.abort(RepoState::Merging);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(read(d, "f.txt"), "main\n");
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && read(d, "g.txt") == "my local work\n"));
    assert!(tab.refs.stashes.is_empty() || poll_until(&mut tab, |t| t.refs.stashes.is_empty()));
}

#[test]
fn merge_conflict_resolved_with_ours_commits_unchanged_tree() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Merging && !t.status.conflicted.is_empty()));
    assert!(!tab.can_commit());
    assert_eq!(tab.commit_subject, "Merge branch 'topic'", "MERGE_MSG prefilled");
    tab.resolve(vec!["f.txt".into()], Resolution::Ours);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty()));
    assert!(tab.status.staged.is_empty(), "the tree equals HEAD");
    assert!(tab.can_commit(), "a merge can be committed with nothing staged");
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(tab.history.commits[0].parents.len(), 2, "merge commit created");
}

#[test]
fn mark_resolved_with_markers_asks_first() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.conflicted.is_empty()));
    tab.resolve(vec!["f.txt".into()], Resolution::MarkResolved);
    assert!(matches!(tab.dialog, Some(Dialog::MarkResolved { .. })));
}

#[test]
fn rebase_conflict_continue_and_empty_cherry_pick() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.checkout_branch("topic");
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "topic"));
    tab.integrate(tine::repo::dialog::Integration::Rebase { onto: "main".into() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Rebasing));
    assert_eq!(tab.rebase_progress, Some((1, 1)));
    assert!(!tab.can_commit(), "normal commits are off during a rebase");
    tab.resolve(vec!["f.txt".into()], Resolution::Theirs);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty()));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(read(d, "f.txt"), "topic\n");

    // Cherry-picking main's tip onto topic... is now already an ancestor:
    // pick topic's own change again onto main to get an empty step instead.
    let mut t = 1_900_000_000;
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("f.txt"), "topic\n").unwrap();
    git(d, &["commit", "-qam", "same as topic"], &mut t);
    assert!(poll_until(&mut tab, |t| idle(t) && head_name(d) == "main"));
    let topic_tip = tab.refs.refs.iter().find(|r| r.short_name == "topic").unwrap().target.to_hex();
    tab.request_cherry_pick(&topic_tip);
    assert!(poll_until(&mut tab, |t| idle(t) && t.dialog.is_some()));
    assert_eq!(tab.dialog, Some(Dialog::EmptyStep { state: RepoState::CherryPicking }));
    tab.dialog = None;
    tab.skip();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
}

#[test]
fn stash_conflict_done_and_drop() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("f.txt"), "stashed edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    tab.stash_push("my stash", false, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.stashes.len() == 1));
    let oid = tab.refs.stashes[0].id;
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "conflicting commit\n").unwrap();
    git(d, &["commit", "-qam", "conflicting"], &mut t);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.has_tracked_changes()));

    tab.stash_apply(oid, true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));
    assert!(tab.stash_conflict.as_ref().unwrap().was_clean_before);
    assert_eq!(tab.refs.stashes.len(), 1, "pop keeps the stash when it conflicts");
    tab.resolve(vec!["f.txt".into()], Resolution::Theirs);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty()));
    tab.finish_stash_conflict(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.stashes.is_empty()));
    assert!(tab.stash_conflict.is_none());
    assert!(tab.status.staged.is_empty(), "applied changes end up unstaged");
    assert_eq!(read(d, "f.txt"), "stashed edit\n");
}

#[test]
fn undo_stash_apply_restores_clean_tree() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("f.txt"), "stashed edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    tab.stash_push("", false, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.stashes.len() == 1));
    let oid = tab.refs.stashes[0].id;
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "conflicting commit\n").unwrap();
    git(d, &["commit", "-qam", "conflicting"], &mut t);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.has_tracked_changes()));
    tab.stash_apply(oid, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));
    tab.request_undo_stash_apply();
    assert_eq!(tab.dialog, Some(Dialog::UndoStashApply { oid }));
    tab.dialog = None;
    tab.undo_stash_apply();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty() && !t.status.has_tracked_changes()));
    assert_eq!(read(d, "f.txt"), "conflicting commit\n");
    assert_eq!(tab.refs.stashes.len(), 1, "the stash itself is kept");
}

#[test]
fn reset_dialog_lists_what_hard_reset_loses() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("g.txt"), "unsaved work\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    let base = tab.history.commits.iter().find(|c| c.subject == "base").unwrap().id;
    tab.request_reset(base, "base");
    match &tab.dialog {
        Some(Dialog::Reset { lost, .. }) => assert_eq!(lost, &vec!["g.txt".to_string()]),
        d => panic!("{d:?}"),
    }
    tab.dialog = None;
    tab.reset(base, tine_core::integrate::ResetMode::Soft, Vec::new());
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit() == Some(base)));
    assert_eq!(read(d, "g.txt"), "unsaved work\n", "soft keeps the work tree");
}


fn set_hook(dir: &std::path::Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let hook = dir.join(".git/hooks").join(name);
    std::fs::write(&hook, script).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn commit_count(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["rev-list", "--count", "HEAD"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn cancelling_a_commit_stops_the_running_hook() {
    let repo = diverged_repo();
    let d = repo.path();
    set_hook(d, "pre-commit", "#!/bin/sh\nsleep 30\n");
    let mut tab = open(d);
    std::fs::write(d.join("g.txt"), "edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    let all = tab.list(tine::repo::Side::Unstaged);
    tab.stage(all);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.staged.is_empty()));
    let before = commit_count(d);
    tab.commit_subject = "never committed".into();
    tab.commit();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let running = tine::ops_registry::snapshots_for(tab.repo_key()).into_iter().find(|s| s.state.is_active()).unwrap();
    let start = std::time::Instant::now();
    tine::ops_registry::cancel(running.id);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    assert!(tab.errors.last().unwrap().contains("cancelled"), "{:?}", tab.errors);
    assert_eq!(commit_count(d), before, "the commit did not happen");
    assert_eq!(tab.commit_subject, "never committed", "the message is kept");
}

#[test]
fn local_changes_found_right_before_running_become_a_question() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    assert!(!tab.status.has_tracked_changes());
    let head_before = tab.head_commit();
    // Edited after the UI last looked: its status still says clean.
    std::fs::write(d.join("g.txt"), "fresh edit\n").unwrap();
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "refs/heads/topic".into(), mode: Default::default() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.dialog.is_some()));
    assert!(matches!(tab.dialog, Some(Dialog::DirtyTree { .. })), "{:?}", tab.dialog);
    assert_eq!(tab.head_commit(), head_before, "nothing was merged");
    assert_eq!(read(d, "g.txt"), "fresh edit\n");
}

#[test]
fn reopened_tab_takes_over_a_stash_conflict() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut a = open(d);
    std::fs::write(d.join("f.txt"), "stashed edit\n").unwrap();
    assert!(poll_until(&mut a, |t| idle(t) && t.status.has_tracked_changes()));
    a.stash_push("", false, false);
    assert!(poll_until(&mut a, |t| idle(t) && t.refs.stashes.len() == 1));
    let oid = a.refs.stashes[0].id;
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "conflicting\n").unwrap();
    git(d, &["commit", "-qam", "conflicting"], &mut t);
    assert!(poll_until(&mut a, |t| idle(t) && !t.status.has_tracked_changes()));
    a.stash_apply(oid, false);
    assert!(poll_until(&mut a, |t| idle(t) && t.stash_conflict.is_some()));
    drop(a);

    let mut b = open(d);
    let workdir = b.paths.workdir.clone();
    for o in tine::worker::take_orphaned_results().into_iter().filter(|o| o.origin == workdir) {
        b.on_external_op_done(o.kind, o.result);
    }
    let sc = b.stash_conflict.clone().expect("taken over");
    assert_eq!(sc.oid, oid);
    assert!(sc.was_clean_before);
    assert!(b.block_reason(tine::repo::Needs::CleanState).is_some());
}

#[test]
fn other_worktree_tab_waits_for_its_refresh() {
    let repo = diverged_repo();
    let d = repo.path();
    let wt_root = tempfile::tempdir().unwrap();
    let wt = wt_root.path().join("wt");
    let mut t = 1_900_000_000;
    git(d, &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()], &mut t);
    let mut main = open(d);
    let mut other = open(&wt);
    assert_eq!(main.repo_key(), other.repo_key(), "shared queue key");
    main.create_tag("from-main", "HEAD", "");
    assert!(poll_until(&mut main, |t| idle(t) && t.refs.refs.iter().any(|r| r.short_name == "from-main")));
    // The other tab hasn't polled yet: it must not offer writes on stale state.
    assert!(other.busy());
    assert!(poll_until(&mut other, idle));
    assert!(other.refs.refs.iter().any(|r| r.short_name == "from-main"));
}

#[test]
fn amend_is_turned_off_and_draft_kept_when_a_merge_starts() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.commit_subject = "my draft".into();
    tab.set_amend(true);
    assert!(poll_until(&mut tab, |t| t.commit_subject == "main change"));
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "refs/heads/topic".into(), mode: Default::default() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Merging));
    assert!(!tab.amend);
    assert_eq!(tab.commit_subject, "Merge branch 'topic'");
    tab.abort(RepoState::Merging);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(tab.commit_subject, "my draft", "the draft is back after the merge");
}

#[test]
fn conflicts_of_unknown_origin_can_be_finished() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "stashed\n").unwrap();
    git(d, &["stash", "-q"], &mut t);
    std::fs::write(d.join("f.txt"), "committed\n").unwrap();
    git(d, &["commit", "-qam", "conflicting"], &mut t);
    // Applied on the command line, outside Tine.
    let _ = std::process::Command::new("git").current_dir(d).args(["stash", "apply"]).env("GIT_CONFIG_GLOBAL", "/dev/null").output();
    let mut tab = open(d);
    assert!(poll_until(&mut tab, |t| idle(t) && t.generic_conflict.is_some()));
    tab.resolve(vec!["f.txt".into()], Resolution::Theirs);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty()));
    assert!(!tab.status.staged.is_empty(), "resolving staged it");
    tab.finish_generic_conflict(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.generic_conflict.is_none()));
    assert!(tab.status.staged.is_empty());
    assert_eq!(read(d, "f.txt"), "stashed\n");
}

#[test]
fn destructive_ref_actions_are_blocked_during_conflicts() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "refs/heads/topic".into(), mode: Default::default() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Merging));
    tab.request_delete_branch("topic");
    assert!(tab.dialog.is_none());
    assert!(tab.errors.last().unwrap().contains("Finish or abort"));
    tab.request_rename_branch("topic");
    assert!(tab.dialog.is_none());
}

#[test]
fn branch_merge_uses_the_full_ref_even_with_a_same_named_tag() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["tag", "topic", "HEAD~1"], &mut t); // a tag named like the branch, at base
    let mut tab = open(d);
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "refs/heads/topic".into(), mode: Default::default() }, false);
    // Merging the branch conflicts; merging the tag (base) would do nothing.
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Merging));
}

#[test]
fn branch_created_but_not_switched_still_shows_up() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("f.txt"), "local edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    tab.create_branch("nb", "refs/heads/topic", true);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.last().unwrap().contains("could not switch"), "{:?}", tab.errors);
    assert!(tab.refs.refs.iter().any(|r| r.short_name == "nb"), "refs were re-read after the failure");
    assert_eq!(head_name(d), "main");
    assert_eq!(read(d, "f.txt"), "local edit\n");
}

#[test]
fn failed_undo_keeps_the_stash_conflict_for_another_try() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut tab = open(d);
    std::fs::write(d.join("f.txt"), "stashed edit\n").unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.has_tracked_changes()));
    tab.stash_push("", false, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.stashes.len() == 1));
    let oid = tab.refs.stashes[0].id;
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "conflicting\n").unwrap();
    git(d, &["commit", "-qam", "conflicting"], &mut t);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.has_tracked_changes()));
    tab.stash_apply(oid, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));

    // Someone else holds the index: the undo fails.
    let lock = d.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    let errors = tab.errors.len();
    tab.undo_stash_apply();
    assert!(poll_until(&mut tab, |t| idle(t) && t.errors.len() > errors));
    assert!(tab.stash_conflict.is_some(), "Undo/Done stay available");
    assert!(!tab.status.conflicted.is_empty());

    std::fs::remove_file(&lock).unwrap();
    tab.undo_stash_apply();
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_none()));
    assert_eq!(read(d, "f.txt"), "conflicting\n");
    assert_eq!(tab.refs.stashes.len(), 1, "the stash is kept");
}

#[test]
fn stash_and_switch_to_a_commit_moves_blocking_untracked_files_away() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["checkout", "-q", "topic"], &mut t);
    std::fs::write(d.join("n.txt"), "theirs\n").unwrap();
    git(d, &["add", "n.txt"], &mut t);
    git(d, &["commit", "-qm", "add n"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("n.txt"), "mine\n").unwrap(); // untracked here, tracked on topic
    let mut tab = open(d);
    tab.checkout_detached("refs/heads/topic", "topic");
    assert!(poll_until(&mut tab, |t| idle(t) && t.dialog.is_some()));
    let Some(Dialog::StashAndSwitch { target }) = tab.dialog.take() else { panic!("{:?}", tab.dialog) };
    assert!(matches!(target, CheckoutTarget::Detached { .. }), "{target:?}");
    tab.stash_and_switch(target);
    assert!(poll_until(&mut tab, |t| idle(t) && t.pending_autostash.is_some()));
    assert_eq!(head_name(d), "", "detached");
    assert_eq!(read(d, "n.txt"), "theirs\n");
    // The untracked file can't come back over the tracked one: it stays stashed.
    let out = std::process::Command::new("git").current_dir(d).args(["show", "stash@{0}^3:n.txt"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "mine\n");
}

#[test]
fn cancelled_checkout_still_shows_where_head_went() {
    let repo = diverged_repo();
    let d = repo.path();
    set_hook(d, "post-checkout", "#!/bin/sh\nsleep 30\n");
    let mut tab = open(d);
    tab.checkout_branch("topic");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let running = tine::ops_registry::snapshots_for(tab.repo_key()).into_iter().find(|s| s.state.is_active()).unwrap();
    let start = std::time::Instant::now();
    tine::ops_registry::cancel(running.id);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    assert!(tab.errors.last().unwrap().contains("cancelled"), "{:?}", tab.errors);
    // The switch itself happened before the hook: the tab must show it.
    assert_eq!(head_name(d), "topic");
    assert!(matches!(&tab.refs.head, Some(tine_core::model::Head::Branch { name, .. }) if name == "topic"), "{:?}", tab.refs.head);
    assert!(tine_core::maintenance::leftover_locks(&tab.paths).is_empty());
}

#[test]
fn stash_conflict_after_a_successful_merge_keeps_the_stash() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["checkout", "-q", "-b", "side"], &mut t);
    std::fs::write(d.join("g.txt"), "side\n").unwrap();
    git(d, &["commit", "-qam", "side edits g"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("g.txt"), "local edit\n").unwrap();
    let mut tab = open(d);
    tab.integrate(tine::repo::dialog::Integration::Merge { target: "refs/heads/side".into(), mode: Default::default() }, true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));
    assert_eq!(tab.refs.stashes.len(), 1, "the stash is kept");
    assert_eq!(tab.stash_conflict.as_ref().unwrap().oid, tab.refs.stashes[0].id);
    assert_eq!(tab.status.state, RepoState::Clean);
    assert_eq!(tab.status.conflicted.len(), 1);
}
