//! Worktrees in a tab (M5c): the list kept current (also from a linked
//! worktree's tab), adding, locking, pruning, and what removing asks and
//! refuses.

mod common;

use std::{os::unix::ffi::OsStrExt as _, path::Path, process::Command};

use common::{git, idle, poll_until};
use pengitto::repo::{RepoTab, dialog::Dialog, worktree::RemoveRequest};
use pengitto_core::worktree::{self, Force, NewWorktree, Worktree, same_place};

fn run(d: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git").current_dir(d).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap()
}

fn out(d: &Path, args: &[&str]) -> String {
    let o = run(d, args);
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// A repository in `<tmp>/repo` (worktrees go next to it).
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("repo");
    std::fs::create_dir(&d).unwrap();
    let mut t = 1_760_000_000;
    git(&d, &["init", "-q", "-b", "main"], &mut t);
    git(&d, &["config", "user.name", "T"], &mut t);
    git(&d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("f.txt"), "a\n").unwrap();
    git(&d, &["add", "-A"], &mut t);
    git(&d, &["commit", "-qm", "base"], &mut t);
    dir
}

fn open_tab(d: &Path) -> RepoTab {
    let mut tab = RepoTab::open(d, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading && !t.worktrees.is_empty()));
    tab
}

fn listed(tab: &RepoTab, path: &Path) -> bool {
    tab.worktrees.iter().any(|w| same_place(&w.path, path))
}

fn entry(tab: &RepoTab, path: &Path) -> Worktree {
    tab.worktrees.iter().find(|w| same_place(&w.path, path)).unwrap().clone()
}

/// Waits until the tab's background job `name` is over (its result, if
/// any, on its way), then takes in what arrived.
fn after_job(tab: &mut RepoTab, name: &str) {
    let running = tab.worker_mut().running_jobs();
    let start = std::time::Instant::now();
    while running.count(name) > 0 {
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{name} still running");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    tab.poll();
}

#[test]
fn the_list_follows_changes_made_anywhere() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut main = open_tab(&d);
    assert_eq!(main.worktrees.len(), 1);
    assert!(main.worktrees[0].main && main.is_this_worktree(&main.worktrees[0]));
    // Added on the command line: noticed (watching the shared git dir).
    let a = dir.path().join("a");
    out(&d, &["worktree", "add", "-q", a.to_str().unwrap(), "-b", "a"]);
    assert!(poll_until(&mut main, |t| listed(t, &a)));
    // A tab on the linked worktree sees the others change too.
    let mut linked = open_tab(&a);
    assert!(linked.is_this_worktree(linked.worktrees.iter().find(|w| same_place(&w.path, &a)).unwrap()));
    let b = dir.path().join("b");
    out(&d, &["worktree", "add", "-q", "--detach", b.to_str().unwrap()]);
    assert!(poll_until(&mut linked, |t| listed(t, &b)));
    out(&d, &["worktree", "lock", "--reason", "usb", b.to_str().unwrap()]);
    assert!(poll_until(&mut linked, |t| t.worktrees.iter().any(|w| same_place(&w.path, &b) && w.locked.as_deref() == Some("usb"))));
    // The main worktree switching branch, and a branch made there: seen
    // from the linked one.
    out(&d, &["switch", "-q", "-c", "other"]);
    assert!(poll_until(&mut linked, |t| t.worktrees[0].branch_name() == Some("other")));
    assert!(poll_until(&mut linked, |t| t.refs.refs.iter().any(|r| r.short_name == "other")));
    out(&d, &["worktree", "unlock", b.to_str().unwrap()]);
    out(&d, &["worktree", "remove", b.to_str().unwrap()]);
    assert!(poll_until(&mut linked, |t| !listed(t, &b)));
}

#[test]
fn adding_from_the_tab() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut tab = open_tab(&d);
    // The dialog's default place: next to the repository.
    tab.request_add_worktree(None);
    let Some(Dialog::AddWorktree { path, .. }) = &tab.dialog else { panic!() };
    assert_eq!(path, &dir.path().join("repo-worktree"));
    tab.dialog = None;
    let p = dir.path().join("feature");
    tab.add_worktree(p.clone(), NewWorktree::NewBranch { name: "feature".into(), start: "HEAD".into() }, true);
    assert!(poll_until(&mut tab, |t| idle(t) && listed(t, &p)));
    assert_eq!(tab.open_request.as_deref(), Some(p.as_path()), "opened as asked");
    assert!(tab.refs.refs.iter().any(|r| r.short_name == "feature"));
    // Failing after the branch was made: said so, and the branch shows.
    let taken = dir.path().join("taken");
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("x"), "x").unwrap();
    tab.add_worktree(taken, NewWorktree::NewBranch { name: "left".into(), start: "HEAD".into() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.iter().any(|e| e.contains("'left' was created anyway")), "{:?}", tab.errors);
    assert!(poll_until(&mut tab, |t| t.refs.refs.iter().any(|r| r.short_name == "left")));
    // A path with a line break in it: opened like any other.
    let odd = dir.path().join("new\nline");
    tab.add_worktree(odd.clone(), NewWorktree::Detached { commit: "HEAD".into() }, false);
    assert!(poll_until(&mut tab, |t| idle(t) && listed(t, &odd)));
    let other = open_tab(&odd);
    assert!(same_place(&other.paths.workdir, &odd));
    // A post-checkout hook failing: the worktree is there all the same —
    // said so, and opened as asked.
    let hook = d.join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\necho the hook failed >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let hooked = dir.path().join("hooked");
    tab.errors.clear();
    tab.open_request = None;
    tab.add_worktree(hooked.clone(), NewWorktree::NewBranch { name: "hooked".into(), start: "HEAD".into() }, true);
    assert!(poll_until(&mut tab, |t| idle(t) && listed(t, &hooked)));
    assert!(
        tab.errors.iter().any(|e| e.contains("was added, but git reported a failure") && e.contains("the hook failed")),
        "{:?}",
        tab.errors
    );
    assert!(!tab.errors.iter().any(|e| e.contains("created anyway")), "{:?}", tab.errors);
    assert_eq!(tab.open_request.as_deref(), Some(hooked.as_path()));
}

#[test]
fn the_dialog_keeps_any_path_and_takes_only_its_own_folder() {
    // The repository's folder isn't UTF-8: the default place keeps its
    // bytes (the text shown can't).
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir(&d).unwrap();
    let mut t = 1_760_000_000;
    git(&d, &["init", "-q", "-b", "main"], &mut t);
    std::fs::write(d.join("f.txt"), "a\n").unwrap();
    git(&d, &["add", "-A"], &mut t);
    git(&d, &["commit", "-qm", "base"], &mut t);
    let mut tab = open_tab(&d);
    tab.request_add_worktree(Some("topic".into()));
    let Some(Dialog::AddWorktree { path, path_text, .. }) = &tab.dialog else { panic!() };
    assert_eq!(path.as_os_str().as_bytes(), dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9-topic")).as_os_str().as_bytes());
    assert!(path_text.contains('\u{fffd}'));
    // A folder picked: in it, for the dialog it was picked for.
    let picked = dir.path().join(std::ffi::OsStr::from_bytes(b"pick\xff"));
    let (release, gate) = crossbeam_channel::unbounded::<()>();
    let p = picked.clone();
    tab.set_folder_dialog_for_test(move || {
        let _ = gate.recv();
        Some(p.clone())
    });
    tab.pick_worktree_folder();
    release.send(()).unwrap();
    assert!(poll_until(&mut tab, |t| matches!(&t.dialog, Some(Dialog::AddWorktree { path_edited: true, .. }))));
    let Some(Dialog::AddWorktree { path, .. }) = &tab.dialog else { panic!() };
    assert_eq!(path.as_os_str().as_bytes(), picked.join(std::ffi::OsStr::from_bytes(b"caf\xe9-topic")).as_os_str().as_bytes());
    // Picked for a dialog since closed: not for the one showing now.
    tab.request_add_worktree(Some("topic".into()));
    tab.pick_worktree_folder();
    tab.dialog = None;
    tab.request_add_worktree(None);
    release.send(()).unwrap();
    let running = std::time::Instant::now();
    while running.elapsed() < std::time::Duration::from_millis(300) {
        tab.poll();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let Some(Dialog::AddWorktree { path, path_edited, .. }) = &tab.dialog else { panic!() };
    assert!(!path_edited && !path.starts_with(&picked), "{}", path.display());
}

#[test]
fn locking_and_pruning() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut tab = open_tab(&d);
    let p = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", "x"]);
    assert!(poll_until(&mut tab, |t| listed(t, &p)));
    let w = tab.worktrees.iter().find(|w| same_place(&w.path, &p)).unwrap().clone();
    tab.lock_worktree(w.clone(), "on a usb disk".into());
    assert!(poll_until(&mut tab, |t| idle(t) && t.worktrees.iter().any(|w| w.locked.as_deref() == Some("on a usb disk"))));
    tab.unlock_worktree(w);
    assert!(poll_until(&mut tab, |t| idle(t) && t.worktrees.iter().all(|w| w.locked.is_none())));
    // Gone: pruning lists it first, then removes the record.
    std::fs::remove_dir_all(&p).unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.worktrees.iter().any(|w| w.prunable.is_some())));
    tab.request_prune_worktrees();
    assert!(poll_until(&mut tab, |t| matches!(&t.dialog, Some(Dialog::PruneWorktrees { lines }) if lines.len() == 1)));
    tab.dialog = None;
    // Another dialog came up while it was being looked at (showing, or
    // shown and closed): not replaced, and none comes up afterwards.
    tab.request_prune_worktrees();
    tab.request_lock_worktree(&tab.worktrees[0].clone());
    after_job(&mut tab, "prune-preview");
    assert!(matches!(tab.dialog, Some(Dialog::LockWorktree { .. })), "the lock dialog stays");
    tab.dialog = None;
    tab.request_prune_worktrees();
    tab.request_lock_worktree(&tab.worktrees[0].clone());
    tab.dialog = None;
    after_job(&mut tab, "prune-preview");
    assert!(tab.dialog.is_none());
    tab.prune_worktrees();
    assert!(poll_until(&mut tab, |t| idle(t) && !listed(t, &p)));
    // Nothing to prune: said, no dialog.
    tab.request_prune_worktrees();
    assert!(poll_until(&mut tab, |t| t.activity.iter().any(|a| a.contains("No worktree records to prune"))));
    assert!(tab.dialog.is_none());
}

#[test]
fn removing_shows_what_goes_and_refuses_what_would_be_lost() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut tab = open_tab(&d);
    let p = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", "x"]);
    std::fs::write(p.join("new.txt"), "x").unwrap();
    std::fs::write(p.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(p.join("a.log"), "l").unwrap();
    assert!(poll_until(&mut tab, |t| listed(t, &p)));
    let w = tab.worktrees.iter().find(|w| same_place(&w.path, &p)).unwrap().clone();
    // The main one: never.
    let main = tab.worktrees[0].clone();
    tab.request_remove_worktree(&main);
    assert!(tab.dialog.is_none() && tab.errors.iter().any(|e| e.contains("main worktree")));
    tab.request_remove_worktree(&w);
    assert!(poll_until(&mut tab, |t| matches!(&t.dialog, Some(Dialog::RemoveWorktree { losses: Some(_), .. }))));
    let Some(Dialog::RemoveWorktree { losses: Some(Ok(l)), .. }) = &tab.dialog else { panic!() };
    assert_eq!(l.ignored, ["a.log"]);
    assert_eq!(l.changed.len(), 2, "{:?}", l.changed);
    let l = l.clone();
    tab.dialog = None;
    // A tab on it with work of its own: removal refused.
    let mut there = open_tab(&p);
    assert_eq!(there.removal_block(), None);
    there.commit_subject = "half written".into();
    assert!(there.removal_block().is_some_and(|w| w.contains("commit message")));
    there.commit_subject.clear();
    // Confirmed: carried out (here as the app does it).
    tab.confirm_remove_worktree(w, Force { discard: true, locked: false }, l);
    let req = tab.remove_requested.take().unwrap();
    there.sleep_for_removal(p.clone());
    assert!(tab.remove_worktree(req).is_some());
    assert!(poll_until(&mut tab, |t| idle(t) && !listed(t, &p)));
    assert!(!p.exists());
}

#[test]
fn a_removal_goes_only_as_confirmed() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut tab = open_tab(&d);
    let p = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", "x"]);
    std::fs::write(p.join("draft.txt"), "x").unwrap();
    assert!(poll_until(&mut tab, |t| listed(t, &p)));
    let w = entry(&tab, &p);
    let losses = worktree::losses(&tab_git(&d), &w).unwrap();
    // A file made after the dialog showed what goes: nothing removed.
    std::fs::write(p.join("later.txt"), "y").unwrap();
    assert!(tab.remove_worktree(RemoveRequest { worktree: w, force: Force { discard: true, locked: false }, losses }).is_some());
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.iter().any(|e| e.contains("since the removal was confirmed") && e.contains("nothing was removed")), "{:?}", tab.errors);
    assert!(p.join("later.txt").exists() && p.join("draft.txt").exists());
}

fn tab_git(d: &Path) -> pengitto_core::GitRunner {
    pengitto_core::GitRunner::new(d)
}

#[test]
fn asleep_it_reads_nothing_and_wakes_reloaded() {
    use common::gated::{Gate, Gates, gated_tab};
    let dir = repo();
    let d = dir.path().join("repo");
    let (mut tab, gates) = gated_tab(&d, Gates { worktrees: Gate::closed(), ..Default::default() });
    // Listings on their way when it goes to sleep: dropped.
    let p = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", "x"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |_| gates.worktrees.arrived() >= 1));
    tab.sleep_for_removal(dir.path().join("elsewhere"));
    gates.worktrees.open();
    after_job(&mut tab, "worktrees");
    assert!(tab.worktrees.is_empty(), "a result read before it slept isn't taken");
    // Nothing more is read, nothing done, no dialog.
    let total = gates.worktrees.total();
    tab.refresh();
    tab.request_add_worktree(None);
    tab.request_prune_worktrees();
    tab.request_lock_worktree(&Worktree {
        path: p.clone(),
        head: None,
        branch: None,
        bare: false,
        detached: true,
        locked: None,
        prunable: None,
        main: false,
    });
    tab.open_stash_dialog();
    assert!(tab.dialog.is_none());
    assert_eq!(tab.block_reason(pengitto::repo::Needs::Idle), Some("This worktree is being removed"));
    std::thread::sleep(std::time::Duration::from_millis(150));
    tab.poll();
    assert_eq!(gates.worktrees.total(), total, "no listing while asleep");
    assert!(!tab.watching() && tab.dialog.is_none());
    // Woken: watching and reloading again.
    tab.wake();
    assert!(tab.watching() && !tab.asleep());
    assert!(poll_until(&mut tab, |t| idle(t) && listed(t, &p)));
}

#[test]
fn a_failed_removal_says_what_is_left() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut tab = open_tab(&d);
    let p = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", "x"]);
    std::fs::write(p.join("new.txt"), "x").unwrap();
    assert!(poll_until(&mut tab, |t| listed(t, &p)));
    let w = tab.worktrees.iter().find(|w| same_place(&w.path, &p)).unwrap().clone();
    // Changes there and no force: git refuses, nothing goes.
    let losses = worktree::losses(&tab_git(&d), &w).unwrap();
    assert!(tab.remove_worktree(RemoveRequest { worktree: w, force: Force::default(), losses }).is_some());
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.iter().any(|e| e.contains("nothing was removed")), "{:?}", tab.errors);
    assert!(p.exists() && listed(&tab, &p));
}
