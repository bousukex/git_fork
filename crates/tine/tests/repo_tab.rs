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

/// A committed 30-line file edited in two far-apart places.
fn two_hunk_repo() -> tempfile::TempDir {
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_950_000_000;
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("long.txt"), &body).unwrap();
    git(d, &["add", "long.txt"], &mut t);
    git(d, &["commit", "-qm", "long file"], &mut t);
    let edited = body.replace("line 2\n", "line two\n").replace("line 28\n", "line 28\nextra after 28\n");
    std::fs::write(d.join("long.txt"), edited).unwrap();
    repo
}

fn select_file(tab: &mut RepoTab, side: Side, path: &str) {
    let f = tab.list(side).into_iter().find(|f| f.path == path).unwrap();
    tab.click_change(side, &f, false, false);
}

fn doc_rows(tab: &RepoTab) -> Vec<(tine::diff_model::RowKind, String)> {
    match &tab.changes_diff.doc {
        Some(Ok(d)) => d.rows.iter().map(|r| (r.kind, r.text.clone())).collect(),
        _ => Vec::new(),
    }
}

fn diff_ready(tab: &RepoTab, path: &str) -> bool {
    tab.changes_diff.file.as_ref().is_some_and(|f| f.path == path)
        && matches!(tab.changes_diff.doc, Some(Ok(_)))
        && !tab.changes_diff.stale
}

#[test]
fn stage_hunk_and_selected_lines() {
    use tine::diff_model::RowKind;
    use tine::repo::PatchTarget;
    use tine_core::patch::PatchMode;

    let repo = two_hunk_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "long.txt")));
    select_file(&mut tab, Side::Unstaged, "long.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    let actions = tab.line_actions().unwrap();
    assert_eq!(actions.disabled, None);

    // Stage the first hunk as a whole.
    tab.apply_lines(PatchMode::Stage, PatchTarget::Hunk(0));
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.iter().any(|f| f.path == "long.txt")));
    assert!(tab.status.unstaged.iter().any(|f| f.path == "long.txt"), "second hunk stays unstaged");

    // In the remaining diff, select just the added line and stage it.
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt") && !doc_rows(t).iter().any(|(_, x)| x == "line two")));
    let rows = doc_rows(&tab);
    let added = rows.iter().position(|(k, x)| *k == RowKind::Added && x == "extra after 28").unwrap();
    tab.select_diff_rows([added].into(), Some(added));
    tab.apply_lines(PatchMode::Stage, PatchTarget::Selection);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.unstaged.iter().any(|f| f.path == "long.txt")));

    // Unstage one line from the staged diff.
    select_file(&mut tab, Side::Staged, "long.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")
        && t.changes_selected.as_ref().is_some_and(|(s, _)| *s == Side::Staged)));
    let rows = doc_rows(&tab);
    let added = rows.iter().position(|(k, x)| *k == RowKind::Added && x == "extra after 28").unwrap();
    tab.select_diff_rows([added].into(), Some(added));
    tab.apply_lines(PatchMode::Unstage, PatchTarget::Selection);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "long.txt")));
    let staged = std::process::Command::new("git").current_dir(repo.path()).args(["show", ":long.txt"]).output().unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(staged.contains("line two\n") && !staged.contains("extra after 28"), "{staged}");
}

#[test]
fn discard_lines_needs_confirmation_and_respects_gating() {
    use tine::diff_model::RowKind;
    use tine::repo::PatchTarget;
    use tine_core::patch::PatchMode;

    let repo = two_hunk_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "long.txt")));

    // Untracked files can only be handled as a whole.
    select_file(&mut tab, Side::Unstaged, "新規ファイル.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "新規ファイル.txt")));
    assert!(tab.line_actions().unwrap().disabled.is_some());

    select_file(&mut tab, Side::Unstaged, "long.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    // "Ignore whitespace" changes what is shown, so line actions are off.
    let mut opts = tab.diff_opts;
    opts.ignore_whitespace = true;
    tab.set_diff_options(opts);
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    assert!(tab.line_actions().unwrap().disabled.unwrap().contains("Ignore whitespace"));
    opts.ignore_whitespace = false;
    tab.set_diff_options(opts);
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));

    let rows = doc_rows(&tab);
    let added = rows.iter().position(|(k, x)| *k == RowKind::Added && x == "extra after 28").unwrap();
    tab.select_diff_rows([added].into(), Some(added));
    tab.apply_lines(PatchMode::Discard, PatchTarget::Selection);
    assert!(tab.pending_line_discard.is_some(), "discard waits for confirmation");
    assert!(!tab.busy());
    tab.confirm_line_discard();
    assert!(poll_until(&mut tab, |t| idle(t) && !std::fs::read_to_string(repo.path().join("long.txt")).unwrap().contains("extra after 28")));
    let now = std::fs::read_to_string(repo.path().join("long.txt")).unwrap();
    assert!(now.contains("line two\n"), "the other change stays");
}

#[test]
fn hunk_button_acts_only_on_its_own_selection() {
    use tine::diff_model::RowKind;
    use tine::repo::PatchTarget;
    use tine_core::patch::PatchMode;

    let repo = two_hunk_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "long.txt")));
    select_file(&mut tab, Side::Unstaged, "long.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    let rows = doc_rows(&tab);
    let in_first = rows.iter().position(|(k, x)| *k == RowKind::Added && x == "line two").unwrap();
    let in_second = rows.iter().position(|(k, x)| *k == RowKind::Added && x == "extra after 28").unwrap();
    tab.select_diff_rows([in_first, in_second].into(), Some(in_first));

    // The second hunk's "Discard Lines (1)" button.
    tab.apply_lines(PatchMode::Discard, PatchTarget::SelectedIn(1));
    assert_eq!(tab.pending_line_discard.as_ref().unwrap().lines.len(), 1);
    tab.confirm_line_discard();
    assert!(poll_until(&mut tab, |t| idle(t)
        && !std::fs::read_to_string(repo.path().join("long.txt")).unwrap().contains("extra after 28")));
    let now = std::fs::read_to_string(repo.path().join("long.txt")).unwrap();
    assert!(now.contains("line two\n"), "the first hunk's selected line must be untouched: {now}");
}

#[test]
fn line_actions_are_disabled_for_unsupported_files() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_960_000_000;
    std::fs::write(d.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    std::fs::write(d.join("script.sh"), "echo hi\n").unwrap();
    git(d, &["add", "bin.dat", "script.sh"], &mut t);
    git(d, &["commit", "-qm", "binary and script"], &mut t);
    std::fs::write(d.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
    std::fs::set_permissions(d.join("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(d.join("script.sh"), "echo hello\n").unwrap();
    git(d, &["mv", "main.rs", "moved.rs"], &mut t);

    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(d, &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.iter().any(|f| f.path == "moved.rs")));
    for (side, path, why) in [
        (Side::Unstaged, "bin.dat", "Binary"),
        (Side::Unstaged, "script.sh", "mode change"),
        (Side::Staged, "moved.rs", "Only modified"),
        (Side::Unstaged, "新規ファイル.txt", "Only modified"),
    ] {
        select_file(&mut tab, side, path);
        assert!(poll_until(&mut tab, |t| diff_ready(t, path)), "{path}");
        let reason = tab.line_actions().unwrap().disabled.unwrap_or_else(|| panic!("{path} should be disabled"));
        assert!(reason.contains(why), "{path}: {reason}");
    }
}

#[test]
fn line_actions_are_disabled_for_conflicts() {
    let repo = conflicted_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.conflicted.is_empty()));
    select_file(&mut tab, Side::Unstaged, "README.md");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "README.md")));
    assert!(tab.line_actions().is_none(), "conflicts are resolved per file, not by hunk");
}

#[test]
fn line_actions_wait_for_the_refreshed_diff() {
    use tine::repo::PatchTarget;
    use tine_core::patch::PatchMode;
    let repo = two_hunk_repo();
    let ctx = egui::Context::default();
    let mut tab = RepoTab::open(repo.path(), &ctx).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "long.txt")));
    select_file(&mut tab, Side::Unstaged, "long.txt");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    tab.apply_lines(PatchMode::Stage, PatchTarget::Hunk(0));
    // As soon as the operation is done the old diff is marked outdated...
    assert!(poll_until(&mut tab, |t| !t.busy()));
    assert!(tab.changes_diff.stale);
    assert_eq!(tab.line_actions().unwrap().disabled, Some("Updating the diff"));
    // ...until the new one arrives.
    assert!(poll_until(&mut tab, |t| diff_ready(t, "long.txt")));
    assert_eq!(tab.line_actions().unwrap().disabled, None);
}
