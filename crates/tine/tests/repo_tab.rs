//! RepoTab behaviour against real repositories.

mod common;

use std::time::{Duration, Instant};

use common::{git, sample_repo};
use tine::repo::{RepoTab, Side, View};

/// Polls until `done` or timeout; returns whether `done` became true.
fn poll_until(tab: &mut RepoTab, mut done: impl FnMut(&RepoTab) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        tab.poll();
        if done(tab) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn loaded(tab: &RepoTab) -> bool {
    !tab.history.loading && tab.history.elapsed.is_some() && tab.status_loaded && tab.refs.head.is_some()
}

#[test]
fn history_reloads_only_when_refs_move() {
    let repo = sample_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, loaded));
    let before = tab.history.commits.len();

    // Nothing changed: refs and status reload, history does not.
    tab.refresh_if_changed();
    let mut reloaded = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(800) {
        tab.poll();
        reloaded |= tab.history.loading;
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!reloaded, "history must not reload when refs did not move");

    // A commit made outside Tine moves the branch: history reloads.
    let mut t = 1_800_000_000;
    git(repo.path(), &["commit", "-q", "--allow-empty", "-m", "外部からのコミット"], &mut t);
    tab.refresh_if_changed();
    assert!(poll_until(&mut tab, |t| loaded(t) && t.history.commits.len() == before + 1));
    assert_eq!(tab.history.commits[0].subject, "外部からのコミット");
}

#[test]
fn selecting_same_commit_switches_back_to_history() {
    let repo = sample_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, loaded));
    let id = tab.history.commits[0].id;
    tab.select_commit(id);
    tab.view = View::LocalChanges;
    tab.select_commit(id);
    assert_eq!(tab.view, View::History);
}

fn idle(tab: &RepoTab) -> bool {
    loaded(tab) && !tab.busy()
}

#[test]
fn stage_commit_amend_flow() {
    let repo = sample_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.len() == 2));
    let commits_before = tab.history.commits.len();

    // Multi-select both unstaged files with Shift and stage them.
    let list = tab.list(Side::Unstaged);
    tab.click_change(Side::Unstaged, &list[0], false, false);
    tab.click_change(Side::Unstaged, &list[1], false, true);
    let targets = tab.action_targets(Side::Unstaged, &list[1]);
    assert_eq!(targets.len(), 2);
    tab.stage(targets);
    assert!(tab.busy());
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.len() == 2 && t.status.unstaged.is_empty()));

    // Ctrl-click toggles a single file out of the selection; unstage one.
    let staged = tab.list(Side::Staged);
    tab.click_change(Side::Staged, &staged[0], false, false);
    tab.click_change(Side::Staged, &staged[1], true, false);
    assert_eq!(tab.selected_on(Side::Staged).len(), 2);
    tab.click_change(Side::Staged, &staged[1], true, false);
    assert_eq!(tab.selected_on(Side::Staged).len(), 1);
    tab.unstage(tab.selected_on(Side::Staged));
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.len() == 1 && t.status.unstaged.len() == 1));
    let to_stage = tab.list(Side::Unstaged);
    tab.stage(to_stage);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.len() == 2));

    // Commit.
    tab.commit_subject = "ステージした変更をコミット".into();
    tab.commit_body = "本文".into();
    assert!(tab.can_commit());
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && t.history.commits.len() == commits_before + 1));
    assert_eq!(tab.history.commits[0].subject, "ステージした変更をコミット");
    assert!(tab.commit_subject.is_empty() && tab.commit_body.is_empty());
    assert_eq!(tab.committed_messages, ["ステージした変更をコミット\n\n本文\n"]);
    assert_eq!(tab.status.change_count(), 0);

    // Amend prefills the last message; rewrite the subject only.
    tab.set_amend(true);
    assert!(poll_until(&mut tab, |t| t.commit_subject == "ステージした変更をコミット"));
    assert_eq!(tab.commit_body, "本文");
    assert!(tab.can_commit(), "amending needs no staged changes");
    tab.commit_subject = "書き直したコミット".into();
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && t.history.commits.first().is_some_and(|c| c.subject == "書き直したコミット")));
    assert_eq!(tab.history.commits.len(), commits_before + 1, "amend must not add a commit");
    assert!(!tab.amend);
}

#[test]
fn discard_tracked_change_after_confirmation() {
    let repo = sample_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.len() == 2));
    let readme = tab.list(Side::Unstaged).into_iter().find(|f| f.path == "README.md").unwrap();
    let before = std::fs::read_to_string(repo.path().join("README.md")).unwrap();

    tab.request_discard(vec![readme.clone()]);
    assert!(tab.pending_discard.is_some());
    // Cancelling leaves the file alone.
    tab.pending_discard = None;
    std::thread::sleep(Duration::from_millis(200));
    tab.poll();
    assert_eq!(std::fs::read_to_string(repo.path().join("README.md")).unwrap(), before);

    tab.request_discard(vec![readme]);
    tab.confirm_discard();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.len() == 1));
    let after = std::fs::read_to_string(repo.path().join("README.md")).unwrap();
    assert_ne!(after, before);
    assert!(after.contains("Run it."), "{after}");
}

#[test]
fn failed_commit_keeps_message_and_reports_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = sample_repo();
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'フックで拒否' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.len() == 2));
    let all = tab.list(Side::Unstaged);
    tab.stage(all);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.len() == 2));
    tab.commit_subject = "拒否されるコミット".into();
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.last().unwrap().contains("フックで拒否"), "{:?}", tab.errors);
    assert_eq!(tab.commit_subject, "拒否されるコミット", "message must survive a failed commit");
    assert!(tab.committed_messages.is_empty());
}

#[test]
fn merge_message_is_prefilled_during_conflict() {
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["stash", "-u", "-q"], &mut t);
    git(d, &["checkout", "-q", "-b", "other", "HEAD~1"], &mut t);
    std::fs::write(d.join("README.md"), "conflicting\n").unwrap();
    git(d, &["commit", "-qam", "other change"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("README.md"), "main side\n").unwrap();
    git(d, &["commit", "-qam", "main change"], &mut t);
    let out = std::process::Command::new("git")
        .current_dir(d)
        .args(["merge", "other"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!out.status.success());

    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(d, &ctx).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.conflicted.is_empty()));
    assert_eq!(tab.commit_subject, "Merge branch 'other'");
    assert!(!tab.can_commit(), "conflicts block committing");
}

/// sample_repo with an unresolved merge conflict on README.md.
fn conflicted_repo() -> tempfile::TempDir {
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["checkout", "-q", "--", "README.md"], &mut t);
    git(d, &["checkout", "-q", "-b", "other", "HEAD~1"], &mut t);
    std::fs::write(d.join("README.md"), "conflicting\n").unwrap();
    git(d, &["commit", "-qam", "other change"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("README.md"), "main side\n").unwrap();
    git(d, &["commit", "-qam", "main change"], &mut t);
    let out = std::process::Command::new("git")
        .current_dir(d)
        .args(["merge", "other"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!out.status.success());
    repo
}

#[test]
fn conflicted_files_are_never_staged_in_bulk() {
    let repo = conflicted_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.len() == 1 && t.status.unstaged.len() == 1));
    let all = tab.list(Side::Unstaged);
    assert_eq!(all.len(), 2);
    tab.stage(all);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.iter().any(|f| f.path == "新規ファイル.txt")));
    assert_eq!(tab.status.conflicted.len(), 1, "the conflict must stay unresolved");
    assert!(tab.activity.iter().any(|a| a.contains("Skipped 1 conflicted")), "{:?}", tab.activity);
    assert!(!tab.can_commit());
}

#[test]
fn amend_sets_draft_aside_and_restores_it() {
    let repo = sample_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut tab, idle));
    let head_subject = tab.history.commits.iter().find(|c| Some(c.id) == tab.head_commit()).unwrap().subject.clone();

    tab.commit_subject = "書きかけの件名".into();
    tab.commit_body = "書きかけの本文".into();
    tab.set_amend(true);
    assert!(tab.commit_subject.is_empty(), "draft is set aside immediately");
    assert!(poll_until(&mut tab, |t| t.commit_subject == head_subject));

    // Turning amend off brings the draft back.
    tab.set_amend(false);
    assert_eq!((tab.commit_subject.as_str(), tab.commit_body.as_str()), ("書きかけの件名", "書きかけの本文"));

    // A completed amend also restores the draft afterwards.
    tab.set_amend(true);
    assert!(poll_until(&mut tab, |t| t.commit_subject == head_subject));
    tab.commit_subject = "直前のコミットを修正".into();
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && t.history.commits.first().is_some_and(|c| c.subject == "直前のコミットを修正")));
    assert!(!tab.amend);
    assert_eq!((tab.commit_subject.as_str(), tab.commit_body.as_str()), ("書きかけの件名", "書きかけの本文"));
}

#[test]
fn discard_untracked_and_partial_failure() {
    let repo = sample_repo();
    std::fs::write(repo.path().join("stuck.txt"), "x").unwrap();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    // Never touch the real trash in tests.
    tab.set_untracked_remover(|p| {
        if p.ends_with("stuck.txt") {
            anyhow::bail!("permission denied");
        }
        Ok(std::fs::remove_file(p)?)
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.len() == 3));
    let all = tab.list(Side::Unstaged);
    tab.request_discard(all);
    tab.confirm_discard();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    let err = tab.errors.last().unwrap();
    assert!(err.contains("restored 1 file(s), moved 1 untracked"), "{err}");
    assert!(err.contains("stuck.txt (permission denied)"), "{err}");
    assert!(!repo.path().join("新規ファイル.txt").exists());
    assert!(repo.path().join("stuck.txt").exists());
    assert!(poll_until(&mut tab, |t| t.status.unstaged.len() == 1));
}

#[test]
fn operations_survive_closing_and_reopening_the_tab() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = sample_repo();
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ctx = egui::Context::default();

    let mut a = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut a, |t| idle(t) && t.status.unstaged.len() == 2));
    let readme = a.list(Side::Unstaged).into_iter().find(|f| f.path == "README.md").unwrap();
    a.stage(vec![readme]);
    assert!(poll_until(&mut a, |t| idle(t) && t.status.staged.len() == 1));
    a.commit_subject = "閉じたタブからのコミット".into();
    a.commit();
    let key = a.repo_key().to_path_buf();
    drop(a); // closed while the (slow) commit runs

    // Reopen and queue another write right away: it must wait for the commit.
    let mut b = RepoTab::open(repo.path(), &ctx).unwrap();
    assert!(poll_until(&mut b, |t| idle(t) && !t.status.unstaged.is_empty()));
    let new_file = b.list(Side::Unstaged).into_iter().find(|f| f.path == "新規ファイル.txt").unwrap();
    b.stage(vec![new_file]);
    assert!(poll_until(&mut b, |t| idle(t) && t.status.staged.iter().any(|f| f.path == "新規ファイル.txt")));

    // The commit finished first and its result was kept for the app.
    let orphans: Vec<_> = tine::worker::take_orphaned_results().into_iter().filter(|o| o.repo == key).collect();
    assert_eq!(orphans.len(), 1);
    assert!(orphans[0].result.as_ref().is_ok_and(|o| o.summary.contains("閉じたタブからのコミット")));
    b.on_external_op_done(orphans[0].kind.clone(), Ok(tine::worker::OpOutcome::default()));
    assert!(poll_until(&mut b, |t| idle(t) && t.history.commits.first().is_some_and(|c| c.subject == "閉じたタブからのコミット")));
}
