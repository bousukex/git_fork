//! RepoTab behaviour against real repositories.

mod common;

use std::time::{Duration, Instant};

use common::{git, sample_repo};
use tine::repo::{RepoTab, View};

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
