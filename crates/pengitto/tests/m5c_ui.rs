//! Worktrees through the UI (M5c): the sidebar section, adding one (it
//! opens in a tab), and removing one — the app checks and puts to sleep the
//! tabs on it (or inside it), closes them once it is gone, and wakes them
//! only when it is known to be all there after a failure.

mod common;

use std::{os::unix::ffi::OsStrExt as _, path::Path, process::Command};

use common::{git, settle};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use pengitto::{app::PengittoApp, repo::worktree::RemoveRequest};
use pengitto_core::{
    GitRunner,
    worktree::{self, Force, Worktree, same_place},
};

fn out(d: &Path, args: &[&str]) -> String {
    let o = Command::new("git").current_dir(d).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// A repository in `<tmp>/repo` with a branch `topic`.
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
    git(&d, &["branch", "topic"], &mut t);
    dir
}

fn app(paths: Vec<std::path::PathBuf>) -> Harness<'static, PengittoApp> {
    let mut h = Harness::builder().with_size([1300.0, 820.0]).build_eframe(move |cc| PengittoApp::new(cc, paths));
    settle(&mut h);
    h.state_mut().activate(0);
    settle(&mut h);
    h
}

fn tab_on<'a>(h: &'a mut Harness<'static, PengittoApp>, path: &Path) -> Option<&'a mut pengitto::repo::RepoTab> {
    h.state_mut().tabs_mut().iter_mut().find(|t| same_place(&t.paths.workdir, path))
}

fn entry(h: &mut Harness<'static, PengittoApp>, path: &Path) -> Worktree {
    h.state_mut().tabs_mut()[0].worktrees.iter().find(|w| same_place(&w.path, path)).unwrap().clone()
}

/// Steps the app until `done` (10s timeout).
fn until(h: &mut Harness<'static, PengittoApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, PengittoApp>) -> bool) {
    let start = std::time::Instant::now();
    while !done(h) {
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "timed out: {what}");
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn remove_from_sidebar(h: &mut Harness<'static, PengittoApp>, name: &str) {
    // (Clicked where it is now: what is under way done first — a refresh
    // or a banner moves it — and the pointer over it a frame before.)
    settle(h);
    let item = format!("worktree {name}");
    h.get_by_label(&item).hover();
    h.step();
    h.get_by_label(&item).click_secondary();
    until(h, "its menu", |h| h.query_by_label("Remove…").is_some());
    h.get_by_label("Remove…").click();
    settle(h);
    h.get_by_label("Remove it anyway, losing these").click();
    h.run_steps(2);
    h.get_by_label("Remove").click();
    settle(h);
}

/// Holds every `git status` of the repository at `d` while `gate` exists
/// (a core.fsmonitor hook that waits).
fn hold_status_while(d: &Path, gate: &Path) {
    let hook = d.join(".git/hold-status.sh");
    std::fs::write(&hook, format!("#!/bin/sh\nwhile [ -e '{}' ]; do sleep 0.02; done\nexit 1\n", gate.display())).unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    out(d, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
}

#[test]
fn removing_closes_its_tab() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    std::fs::write(x.join("scratch.txt"), "x").unwrap();
    let mut h = app(vec![d.clone(), x.clone()]);
    assert_eq!(h.state().tab_count(), 2);
    assert!(h.query_by_label("worktree x").is_some());
    remove_from_sidebar(&mut h, "x");
    assert!(!x.exists());
    assert_eq!(h.state().tab_count(), 1, "its tab closed");
    assert!(h.query_by_label("worktree x").is_none());
}

#[test]
fn work_in_its_tab_stops_a_removal() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    std::fs::write(x.join("scratch.txt"), "x").unwrap();
    let mut h = app(vec![d.clone(), x.clone()]);
    tab_on(&mut h, &x).unwrap().commit_subject = "half written".into();
    remove_from_sidebar(&mut h, "x");
    assert!(x.exists());
    assert_eq!(h.state().tab_count(), 2);
    let errors = h.state_mut().active_tab_mut().unwrap().errors.clone();
    assert!(errors.iter().any(|e| e.contains("a commit message is being written there")), "{errors:?}");
    // Its tab still follows its files.
    assert!(!tab_on(&mut h, &x).unwrap().asleep());
    std::fs::write(x.join("more.txt"), "x").unwrap();
    until(&mut h, "the tab sees the new file", |h| tab_on(h, &x).unwrap().status.unstaged.iter().any(|f| f.path == "more.txt"));
}

#[test]
fn tabs_inside_the_worktree_are_checked_and_go_with_it() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    // A repository of its own inside the worktree, open in a tab.
    let nested = x.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let mut t = 1_760_000_000;
    git(&nested, &["init", "-q", "-b", "main"], &mut t);
    std::fs::write(nested.join("n.txt"), "n\n").unwrap();
    git(&nested, &["add", "-A"], &mut t);
    git(&nested, &["commit", "-qm", "nested"], &mut t);
    let mut h = app(vec![d.clone(), x.clone(), nested.clone()]);
    assert_eq!(h.state().tab_count(), 3);
    // A message being written there stops it, and says where.
    tab_on(&mut h, &nested).unwrap().commit_subject = "half written".into();
    remove_from_sidebar(&mut h, "x");
    assert!(x.exists() && nested.exists());
    let errors = h.state_mut().tabs_mut()[0].errors.clone();
    assert!(
        errors.iter().any(|e| e.contains("a commit message is being written there") && e.contains("nested, open in a tab")),
        "{errors:?}"
    );
    assert!(h.state_mut().tabs_mut().iter().all(|t| !t.asleep()));
    // Without it: both tabs go with the worktree.
    tab_on(&mut h, &nested).unwrap().commit_subject.clear();
    remove_from_sidebar(&mut h, "x");
    assert!(!x.exists());
    assert_eq!(h.state().tab_count(), 1);
    assert!(same_place(&h.state_mut().tabs_mut()[0].paths.workdir, &d));
}

#[test]
fn while_it_goes_its_tab_sleeps_and_the_end_is_seen_without_the_tab_that_asked() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    std::fs::create_dir(x.join("sub")).unwrap();
    std::fs::write(x.join("sub/scratch.txt"), "x").unwrap();
    let mut h = app(vec![d.clone(), x.clone()]);
    let w = entry(&mut h, &x);
    let losses = worktree::losses(&GitRunner::new(&d), &w).unwrap();
    // The removal held half-way (its look at what goes waits).
    let gate = dir.path().join("gate");
    hold_status_while(&d, &gate);
    std::fs::write(&gate, "").unwrap();
    h.state_mut().tabs_mut()[0].remove_requested = Some(RemoveRequest { worktree: w, force: Force { discard: true, locked: false }, losses });
    h.run_steps(2);
    // Asleep: nothing to do there, no dialog, shown as such.
    let there = tab_on(&mut h, &x).unwrap();
    assert!(there.asleep());
    there.request_lock_worktree(&there.worktrees[0].clone());
    assert!(there.dialog.is_none());
    assert_eq!(there.block_reason(pengitto::repo::Needs::Idle), Some("This worktree is being removed"));
    let i = h.state_mut().tabs_mut().iter().position(|t| same_place(&t.paths.workdir, &x)).unwrap();
    h.state_mut().activate(i);
    h.run_steps(2);
    assert!(h.query_by_label_contains("This worktree is being removed").is_some());
    // Nothing in it opens meanwhile.
    let ctx = h.ctx.clone();
    h.state_mut().open(&x.join("sub"), &ctx);
    assert!(h.state().open_error().is_some_and(|e| e.contains("is being removed")), "{:?}", h.state().open_error());
    // The tab that asked closes; the end is still seen.
    h.state_mut().close_tab(0);
    h.run_steps(2);
    assert_eq!(h.state().tab_count(), 1);
    std::fs::remove_file(&gate).unwrap();
    until(&mut h, "the worktree's tab closes", |h| h.state().tab_count() == 0);
    assert!(!x.exists());
    // (The closed tab's result goes to the process-wide orphans, which
    // other tests' apps in this binary may take: not checked here.)
}

#[test]
fn a_failed_removal_wakes_its_tab_again() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    let mut h = app(vec![d.clone(), x.clone()]);
    // Changes there, and no force: git refuses.
    std::fs::write(x.join("scratch.txt"), "x").unwrap();
    let w = entry(&mut h, &x);
    let losses = worktree::losses(&GitRunner::new(&d), &w).unwrap();
    h.state_mut().tabs_mut()[0].remove_requested = Some(RemoveRequest { worktree: w, force: Force::default(), losses });
    settle(&mut h);
    assert!(x.exists());
    assert_eq!(h.state().tab_count(), 2);
    let errors = h.state_mut().tabs_mut()[0].errors.clone();
    assert!(errors.iter().any(|e| e.contains("Remove worktree failed") && e.contains("nothing was removed")), "{errors:?}");
    // Awake, watching, and reloaded — then a change shows (only watching
    // can bring it).
    until(&mut h, "the tab wakes", |h| {
        let t = tab_on(h, &x).unwrap();
        !t.asleep() && t.watching() && t.status_loaded && !t.busy()
    });
    std::fs::write(x.join("later.txt"), "x").unwrap();
    until(&mut h, "the tab sees the new file", |h| tab_on(h, &x).unwrap().status.unstaged.iter().any(|f| f.path == "later.txt"));
}

#[test]
fn a_tab_left_without_its_repository_closes_instead_of_waking() {
    let dir = repo();
    let d = dir.path().join("repo");
    // A worktree inside the main one's folder: without its own `.git`,
    // git would find the main repository from it.
    let x = d.join("inside");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    let mut h = app(vec![d.clone(), x.clone()]);
    let w = entry(&mut h, &x);
    let losses = worktree::losses(&GitRunner::new(&d), &w).unwrap();
    let gate = dir.path().join("gate");
    hold_status_while(&d, &gate);
    std::fs::write(&gate, "").unwrap();
    h.state_mut().tabs_mut()[0].remove_requested = Some(RemoveRequest { worktree: w, force: Force::default(), losses });
    h.run_steps(2);
    assert!(tab_on(&mut h, &x).unwrap().asleep());
    // Meanwhile it loses its `.git` (as a removal failing part-way could)
    // and gains a file: the removal refuses, and the tab can't go back to
    // a repository that isn't there.
    std::fs::remove_file(x.join(".git")).unwrap();
    std::fs::write(x.join("new.txt"), "n").unwrap();
    std::fs::remove_file(&gate).unwrap();
    until(&mut h, "the tab closes", |h| h.state().tab_count() == 1);
    assert!(h.state().notices().iter().any(|n| n.contains("isn't there as it was")), "{:?}", h.state().notices());
    assert!(same_place(&h.state_mut().tabs_mut()[0].paths.workdir, &d));
}

#[test]
fn what_was_confirmed_is_all_that_goes() {
    let dir = repo();
    let d = dir.path().join("repo");
    let x = dir.path().join("x");
    out(&d, &["worktree", "add", "-q", x.to_str().unwrap(), "-b", "x"]);
    std::fs::write(x.join("scratch.txt"), "x").unwrap();
    let mut h = app(vec![d.clone(), x.clone()]);
    h.get_by_label("worktree x").click_secondary();
    h.run_steps(2);
    h.get_by_label("Remove…").click();
    settle(&mut h);
    assert!(h.query_by_label_contains("scratch.txt").is_some());
    // Written after the dialog said what goes.
    std::fs::write(x.join("later.txt"), "y").unwrap();
    h.get_by_label("Remove it anyway, losing these").click();
    h.run_steps(2);
    h.get_by_label("Remove").click();
    settle(&mut h);
    assert!(x.join("later.txt").exists() && x.join("scratch.txt").exists());
    let errors = h.state_mut().tabs_mut()[0].errors.clone();
    assert!(errors.iter().any(|e| e.contains("since the removal was confirmed")), "{errors:?}");
    until(&mut h, "its tab wakes", |h| tab_on(h, &x).is_some_and(|t| !t.asleep()));
}

#[test]
fn browse_keeps_the_dialog_and_any_path_goes_as_it_is() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut h = app(vec![d.clone()]);
    // A folder that isn't UTF-8, picked with Browse… (a stand-in for the
    // desktop's dialog).
    let picked = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir(&picked).unwrap();
    let p = picked.clone();
    h.state_mut().active_tab_mut().unwrap().set_folder_dialog_for_test(move || Some(p.clone()));
    h.get_by_label("New worktree").click();
    h.run_steps(4);
    h.input_mut().events.push(egui::Event::Text("feature".into()));
    h.run_steps(2);
    h.get_by_label("Browse…").click();
    settle(&mut h);
    assert!(h.query_by_label("Create").is_some(), "the dialog stays");
    h.get_by_label("Create").click();
    settle(&mut h);
    let made = picked.join("repo-feature");
    assert!(made.join("f.txt").exists(), "made in the picked folder, bytes and all");
    // A location typed with a space at its end: taken as it is.
    h.state_mut().activate(0);
    settle(&mut h);
    h.get_by_label("New worktree").click();
    h.run_steps(4);
    h.input_mut().events.push(egui::Event::Text("second".into()));
    h.run_steps(2);
    let field = h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Location");
    field.focus();
    h.run_steps(1);
    h.key_press(egui::Key::End);
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Location").type_text(" x ");
    h.run_steps(2);
    h.get_by_label("Open it in a new tab").click();
    h.run_steps(1);
    h.get_by_label("Create").click();
    settle(&mut h);
    let spaced = dir.path().join("repo-second x ");
    assert!(spaced.join("f.txt").exists(), "{} exists", spaced.display());
}

#[test]
fn adding_from_the_sidebar_and_a_branch() {
    let dir = repo();
    let d = dir.path().join("repo");
    let mut h = app(vec![d.clone()]);
    h.get_by_label("New worktree").click();
    h.run_steps(4);
    // The name field has the focus.
    h.input_mut().events.push(egui::Event::Text("feature/a".into()));
    h.run_steps(2);
    h.get_by_label("Create").click();
    settle(&mut h);
    let p = dir.path().join("repo-feature-a");
    assert!(p.join("f.txt").exists(), "made where the dialog said");
    assert_eq!(h.state().tab_count(), 2, "opened in a tab");
    assert_eq!(out(&p, &["branch", "--show-current"]).trim(), "feature/a");
    // A branch from its menu.
    h.state_mut().activate(0);
    settle(&mut h);
    h.get_by_label("branch topic").click_secondary();
    h.run_steps(2);
    h.get_by_label("Check Out in New Worktree…").click();
    h.run_steps(4);
    h.get_by_label("Open it in a new tab").click();
    h.run_steps(1);
    h.get_by_label("Create").click();
    settle(&mut h);
    let q = dir.path().join("repo-topic");
    assert_eq!(out(&q, &["branch", "--show-current"]).trim(), "topic");
    assert_eq!(h.state().tab_count(), 2, "not opened");
    // Checked out there now: not offered again.
    h.get_by_label("branch topic").click_secondary();
    h.run_steps(2);
    assert!(h.get_by_label("Check Out in New Worktree…").accesskit_node().is_disabled());
}
