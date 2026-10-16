//! M3b flows through RepoTab against local bare remotes: fetch, pull, push,
//! remote deletes.

mod common;

use std::path::{Path, PathBuf};

use common::{git, idle, poll_until};
use pengitto::repo::{RepoTab, dialog::Dialog};
use pengitto_core::model::{RefKind, RepoState};

/// A bare remote with one commit, the clone the tab works in, and another
/// clone standing for a colleague.
struct Setup {
    _dirs: Vec<tempfile::TempDir>,
    bare: PathBuf,
    mine: PathBuf,
    other: PathBuf,
    t: i64,
}

fn setup() -> Setup {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (bare, mine, other) = (dirs[0].path().to_path_buf(), dirs[1].path().to_path_buf(), dirs[2].path().to_path_buf());
    let mut t = 1_800_000_000;
    git(&bare, &["init", "-q", "--bare", "-b", "main"], &mut t);
    for d in [&mine, &other] {
        git(d, &["clone", "-q", &format!("file://{}", bare.display()), "."], &mut t);
        git(d, &["config", "user.name", "山田 太郎"], &mut t);
        git(d, &["config", "user.email", "taro@example.com"], &mut t);
    }
    std::fs::write(other.join("f.txt"), "base\n").unwrap();
    git(&other, &["add", "-A"], &mut t);
    git(&other, &["commit", "-qm", "base"], &mut t);
    git(&other, &["push", "-q", "origin", "main"], &mut t);
    git(&mine, &["pull", "-q", "origin", "main"], &mut t);
    Setup { _dirs: dirs, bare, mine, other, t }
}

impl Setup {
    fn other_commits(&mut self, file: &str, body: &str, msg: &str) {
        std::fs::write(self.other.join(file), body).unwrap();
        git(&self.other, &["add", "-A"], &mut self.t);
        git(&self.other, &["commit", "-qm", msg], &mut self.t);
        git(&self.other, &["push", "-q", "origin", "HEAD"], &mut self.t);
    }

    fn mine_commits(&mut self, file: &str, body: &str, msg: &str) {
        std::fs::write(self.mine.join(file), body).unwrap();
        git(&self.mine, &["add", "-A"], &mut self.t);
        git(&self.mine, &["commit", "-qm", msg], &mut self.t);
    }
}

fn rev(dir: &Path, rev: &str) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["rev-parse", "--verify", "-q", rev]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn open(dir: &Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, idle));
    tab
}

fn has_ref(tab: &RepoTab, short: &str) -> bool {
    tab.refs.refs.iter().any(|r| r.short_name == short)
}

fn last_error(tab: &RepoTab) -> &str {
    tab.errors.last().map(String::as_str).unwrap_or_default()
}

#[test]
fn fetch_pull_and_push() {
    let mut s = setup();
    let mut tab = open(&s.mine);
    s.other_commits("g.txt", "g\n", "from other");
    tab.open_fetch_dialog(None);
    assert_eq!(tab.dialog, Some(Dialog::Fetch { remote: None, prune: false }));
    tab.dialog = None;
    tab.fetch(None, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.refs.refs.iter().any(|r| r.short_name == "origin/main" && r.target.to_hex() == rev(&s.other, "HEAD"))));
    let fetched = pengitto::ops_registry::snapshots_for(tab.repo_key()).into_iter().next().unwrap();
    assert!(fetched.commands.iter().any(|c| c.args.iter().any(|a| a == "fetch")), "recorded for the Output panel");

    tab.open_pull_dialog(None);
    let Some(Dialog::Pull { remote_branch, rebase, stash_first }) = tab.dialog.take() else { panic!("{:?}", tab.dialog) };
    assert_eq!((remote_branch.as_str(), rebase, stash_first), ("origin/main", false, false));
    tab.pull(&remote_branch, rebase, stash_first);
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit().map(|o| o.to_hex()) == Some(rev(&s.other, "HEAD"))));
    assert!(tab.activity.last().unwrap().contains("Pulled origin/main"), "{:?}", tab.activity);

    tab.pull("origin/main", false, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.activity.last().unwrap().contains("already up to date")));

    s.mine_commits("h.txt", "h\n", "from me");
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit().map(|o| o.to_hex()) == Some(rev(&s.mine, "HEAD"))));
    tab.open_push_dialog(None);
    let Some(Dialog::Push { branch, remote, remote_branch, set_upstream, tags, force, .. }) = tab.dialog.take() else { panic!() };
    assert!(!force);
    assert_eq!((branch.as_str(), remote.as_str(), remote_branch.as_str()), ("main", "origin", "main"));
    assert!(!set_upstream, "main already tracks origin/main");
    tab.push(&branch, &remote, &remote_branch, set_upstream, tags, None);
    assert!(poll_until(&mut tab, |t| idle(t) && t.activity.last().unwrap().contains("pushed main")));
    assert_eq!(rev(&s.bare, "main"), rev(&s.mine, "HEAD"));
    assert!(tab.errors.is_empty(), "{:?}", tab.errors);
}

#[test]
fn new_branch_push_sets_upstream_and_pushes_tags() {
    let mut s = setup();
    git(&s.mine, &["switch", "-q", "-c", "feature/日本語"], &mut s.t);
    s.mine_commits("x.txt", "x\n", "feature");
    git(&s.mine, &["tag", "v1.0"], &mut s.t);
    let mut tab = open(&s.mine);
    tab.open_push_dialog(None);
    let Some(Dialog::Push { set_upstream, remote_branch, .. }) = tab.dialog.take() else { panic!() };
    assert!(set_upstream);
    assert_eq!(remote_branch, "feature/日本語");
    tab.push("feature/日本語", "origin", &remote_branch, true, true, None);
    assert!(poll_until(&mut tab, |t| idle(t) && has_ref(t, "origin/feature/日本語")));
    let local = tab.refs.refs.iter().find(|r| r.kind == RefKind::LocalBranch && r.short_name == "feature/日本語").unwrap();
    assert_eq!(local.upstream.as_deref(), Some("origin/feature/日本語"));
    assert_eq!(rev(&s.bare, "refs/tags/v1.0"), rev(&s.mine, "v1.0"));
}

#[test]
fn rejected_push_says_what_to_do() {
    let mut s = setup();
    let mut tab = open(&s.mine);
    s.other_commits("g.txt", "g\n", "from other");
    s.mine_commits("h.txt", "h\n", "from me");
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit().map(|o| o.to_hex()) == Some(rev(&s.mine, "HEAD"))));
    tab.push("main", "origin", "main", false, false, None);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    let e = last_error(&tab);
    assert!(e.contains("rejected main") && e.contains("pull"), "{e}");
    assert_eq!(rev(&s.bare, "main"), rev(&s.other, "HEAD"), "nothing overwritten");
}

#[test]
fn force_push_only_replaces_what_the_user_saw() {
    let mut s = setup();
    s.mine_commits("h.txt", "h\n", "mine");
    git(&s.mine, &["push", "-q", "origin", "main"], &mut s.t);
    git(&s.mine, &["commit", "-q", "--amend", "-m", "mine, rewritten"], &mut s.t);
    let mut tab = open(&s.mine);
    // A colleague pushes after our last fetch; we haven't seen it.
    git(&s.other, &["pull", "-q", "--rebase=false", "origin", "main"], &mut s.t);
    s.other_commits("g.txt", "g\n", "theirs");
    let seen = tab.tracking_target("origin", "main");
    tab.push("main", "origin", "main", false, false, Some(seen));
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(last_error(&tab).contains("moved since you confirmed"), "{}", last_error(&tab));
    assert_eq!(rev(&s.bare, "main"), rev(&s.other, "HEAD"), "their commit survives");

    // After fetching (and looking), the force push goes through.
    tab.errors.clear();
    tab.fetch(Some("origin".into()), false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.tracking_target("origin", "main").map(|o| o.to_hex()) == Some(rev(&s.other, "HEAD"))));
    let seen = tab.tracking_target("origin", "main");
    tab.push("main", "origin", "main", false, false, Some(seen));
    assert!(poll_until(&mut tab, |t| idle(t) && rev(&s.bare, "main") == rev(&s.mine, "HEAD")));
    assert!(tab.errors.is_empty(), "{:?}", tab.errors);
}

#[test]
fn pull_with_local_changes() {
    let mut s = setup();
    s.other_commits("g.txt", "g\n", "from other");
    std::fs::write(s.mine.join("f.txt"), "my edit\n").unwrap();
    let mut tab = open(&s.mine);
    assert!(tab.status.has_tracked_changes());
    tab.open_pull_dialog(None);
    let Some(Dialog::Pull { stash_first, .. }) = tab.dialog.take() else { panic!() };
    assert!(stash_first, "offered by default");
    tab.pull("origin/main", false, false);
    assert!(last_error(&tab).contains("Commit or stash"), "{:?}", tab.errors);
    tab.pull("origin/main", true, true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit().map(|o| o.to_hex()) == Some(rev(&s.other, "HEAD"))));
    assert_eq!(std::fs::read_to_string(s.mine.join("f.txt")).unwrap(), "my edit\n");
    assert_eq!(std::fs::read_to_string(s.mine.join("g.txt")).unwrap(), "g\n");
    assert!(tab.refs.stashes.is_empty(), "the stash was re-applied and dropped");
}

#[test]
fn pull_conflict_shows_the_merge_state() {
    let mut s = setup();
    s.other_commits("f.txt", "theirs\n", "theirs");
    s.mine_commits("f.txt", "mine\n", "mine");
    let mut tab = open(&s.mine);
    tab.pull("origin/main", true, false);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Rebasing));
    assert_eq!(tab.status.conflicted.len(), 1);
    tab.abort(RepoState::Rebasing);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
}

#[test]
fn delete_on_remote_and_push_a_tag() {
    let mut s = setup();
    git(&s.other, &["push", "-q", "origin", "main:refs/heads/tmp"], &mut s.t);
    git(&s.mine, &["fetch", "-q"], &mut s.t);
    git(&s.mine, &["tag", "v9"], &mut s.t);
    let mut tab = open(&s.mine);
    assert!(has_ref(&tab, "origin/tmp"));
    tab.request_delete_remote_ref("origin", "refs/heads/tmp");
    assert_eq!(tab.dialog, Some(Dialog::DeleteRemoteRef { remote: "origin".into(), full_ref: "refs/heads/tmp".into() }));
    tab.dialog = None;
    tab.delete_remote_ref("origin", "refs/heads/tmp");
    assert!(poll_until(&mut tab, |t| idle(t) && !has_ref(t, "origin/tmp")));
    assert_eq!(rev(&s.bare, "refs/heads/tmp"), "");

    tab.push_tag("v9", "origin");
    assert!(poll_until(&mut tab, |t| idle(t) && t.activity.last().unwrap().contains("pushed v9")));
    assert_eq!(rev(&s.bare, "refs/tags/v9"), rev(&s.mine, "v9"));
}

#[test]
fn remote_actions_need_a_remote_and_a_branch() {
    let repo = common::diverged_repo();
    let tab = open(repo.path());
    assert_eq!(tab.remote_block(pengitto::repo::Needs::Idle, false), Some("This repository has no remotes"));

    let mut s = setup();
    git(&s.mine, &["switch", "-q", "--detach"], &mut s.t);
    let mut tab = open(&s.mine);
    assert_eq!(tab.remote_block(pengitto::repo::Needs::Idle, true), Some("Check out a branch first"));
    assert_eq!(tab.remote_block(pengitto::repo::Needs::Idle, false), None, "fetching works detached");
    tab.open_pull_dialog(None);
    assert!(tab.dialog.is_none());
}
