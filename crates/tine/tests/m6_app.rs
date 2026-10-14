//! The app for distribution (M6): its state kept whole in its own file,
//! quitting only once running operations have stopped, a missing or old git
//! told at startup, and the About dialog.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{git, settle};
use egui_kittest::{Harness, kittest::Queryable as _};
use tine::app::{AppOptions, TineApp};

#[derive(Default)]
struct MemStorage(HashMap<String, String>);

impl eframe::Storage for MemStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn harness(paths: Vec<PathBuf>, storage: Option<MemStorage>, options: AppOptions) -> Harness<'static, TineApp> {
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .build_eframe(move |cc| TineApp::with_options(cc, paths, storage.as_ref().map(|s| s as &dyn eframe::Storage), options));
    settle(&mut h);
    h
}

fn until(h: &mut Harness<'static, TineApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, TineApp>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(15), "timed out: {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn close_window(h: &mut Harness<'static, TineApp>) {
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    h.step();
    h.run_steps(3);
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn the_state_is_kept_whole_in_its_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.ron");
    let options = || AppOptions { state_file: Some(file.clone()), ..Default::default() };
    let mut h = harness(vec![], None, options());
    let mut settings = h.state().settings();
    settings.tab_width = 6;
    h.state_mut().set_settings(settings);
    h.ctx.set_theme(egui::ThemePreference::Dark);
    h.ctx.set_zoom_factor(1.25);
    // (egui takes the zoom at the next frame.)
    h.run_steps(1);
    let mut storage = MemStorage::default();
    eframe::App::save(h.state_mut(), &mut storage);
    eframe::App::on_exit(h.state_mut());
    assert!(std::fs::read_to_string(&file).unwrap().contains("tab_width: 6"));
    // eframe's own file lost (its storage empty): everything is back.
    let mut h = harness(vec![], Some(MemStorage::default()), options());
    assert_eq!(h.state().settings().tab_width, 6);
    assert_eq!(h.ctx.options(|o| o.theme_preference), egui::ThemePreference::Dark);
    assert_eq!(h.ctx.zoom_factor(), 1.25);
    h.run_steps(1);
    // Both there, apart (another Tine saved only the file since): the file.
    let mut settings = h.state().settings();
    settings.tab_width = 7;
    h.state_mut().set_settings(settings);
    eframe::App::save(h.state_mut(), &mut MemStorage::default());
    eframe::App::on_exit(h.state_mut());
    let mut h = harness(vec![], Some(storage), options());
    assert_eq!(h.state().settings().tab_width, 7);
    h.run_steps(1);
    // An older Tine kept it in eframe's storage only: taken from there.
    let mut old = MemStorage::default();
    eframe::App::save(h.state_mut(), &mut old);
    let other = AppOptions { state_file: Some(dir.path().join("none-yet.ron")), ..Default::default() };
    let h = harness(vec![], Some(old), other);
    assert_eq!(h.state().settings().tab_width, 7);
    drop(h);
    // Any zoom that egui's keys reach comes back (0.2 to 5).
    for zoom in [0.4, 3.5] {
        let mut h = harness(vec![], None, options());
        h.ctx.set_zoom_factor(zoom);
        h.run_steps(1);
        eframe::App::save(h.state_mut(), &mut MemStorage::default());
        eframe::App::on_exit(h.state_mut());
        let h = harness(vec![], Some(MemStorage::default()), options());
        assert_eq!(h.ctx.zoom_factor(), zoom);
    }
}

/// A repository whose next commit waits in its pre-commit hook while
/// `gate` exists; the hook writes its pid to `<dir>/hook.pid` once it runs
/// (`stubborn`: it ignores SIGTERM and lets go of its output, so git itself
/// ends at once).
fn repo_with_slow_hook(stubborn: bool) -> (tempfile::TempDir, PathBuf) {
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
    std::fs::write(d.join("f.txt"), "b\n").unwrap();
    git(&d, &["add", "-A"], &mut t);
    let gate = dir.path().join("gate");
    std::fs::write(&gate, "").unwrap();
    let trap = if stubborn { "exec >/dev/null 2>&1 </dev/null\ntrap '' TERM\n" } else { "" };
    let pid = dir.path().join("hook.pid");
    script(
        &d.join(".git/hooks/pre-commit"),
        &format!("{trap}echo $$ > '{}'\nwhile [ -e '{}' ]; do sleep 0.05; done", pid.display(), gate.display()),
    );
    (dir, gate)
}

/// The hook's process is gone (or only waits to be reaped).
fn hook_gone(dir: &Path) -> bool {
    let pid = std::fs::read_to_string(dir.join("hook.pid")).unwrap();
    std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .map_or(true, |s| s.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('Z')))
}

/// Operations are process-wide (one app per process): these tests take
/// turns.
static OPS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Commits in the active tab; returns once its hook runs.
fn start_commit(h: &mut Harness<'static, TineApp>, dir: &Path) {
    let tab = h.state_mut().active_tab_mut().unwrap();
    tab.commit_subject = "held by its hook".into();
    tab.commit();
    until(h, "the hook runs", |_| dir.join("hook.pid").exists());
}

#[test]
fn quitting_stops_running_operations_first() {
    let _turn = OPS.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, gate) = repo_with_slow_hook(false);
    let d = dir.path().join("repo");
    let mut h = harness(vec![d.clone()], None, AppOptions::default());
    start_commit(&mut h, dir.path());
    let id = tine::ops_registry::active_ops()[0].id;
    // Asked, with the operation named; Cancel keeps everything.
    close_window(&mut h);
    assert!(h.state().quit_asked());
    assert!(h.query_by_label("Stop the running operations and quit?").is_some());
    assert!(h.query_by_label_contains("Commit — repo").is_some());
    // (The toolbar has a Cancel for the running operation too.)
    h.get_all_by_label("Cancel").last().unwrap().click();
    h.run_steps(2);
    assert!(!h.state().quit_asked() && !h.state().stopping());
    assert!(tine::ops_registry::state(id).is_some_and(|s| s.is_active()));
    // Stop and Quit: stopped, waited for, then the window closes.
    close_window(&mut h);
    h.get_by_label("Stop and Quit").click();
    h.run_steps(1);
    assert!(h.state().stopping() && !h.state().quitting(), "not before it has stopped");
    until(&mut h, "it stops and the window closes", |h| h.state().quitting());
    assert_eq!(tine::ops_registry::state(id), Some(tine::ops_registry::OpState::Finished(tine::ops_registry::Outcome::Cancelled)));
    let log = std::process::Command::new("git").current_dir(&d).args(["log", "--format=%s"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "base", "nothing committed");
    std::fs::remove_file(gate).unwrap();
}

#[test]
fn an_operation_slow_to_stop_is_asked_about_again() {
    let _turn = OPS.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, gate) = repo_with_slow_hook(true);
    let d = dir.path().join("repo");
    let options = AppOptions { stop_timeout: Duration::from_millis(300), ..Default::default() };
    let mut h = harness(vec![d], None, options);
    start_commit(&mut h, dir.path());
    close_window(&mut h);
    h.get_by_label("Stop and Quit").click();
    // The hook ignores SIGTERM: still running after the time allowed.
    until(&mut h, "asked again", |h| h.query_by_label("Keep Waiting").is_some());
    assert!(h.query_by_label("Quit Now").is_some());
    h.get_by_label("Keep Waiting").click();
    h.run_steps(1);
    assert!(h.state().stopping() && !h.state().quitting());
    // Killed in the end (SIGKILL after two seconds): then it closes — not
    // before the hook is gone (it let go of its output: git ended at once).
    until(&mut h, "it ends and the window closes", |h| h.state().quitting());
    assert!(hook_gone(dir.path()), "the hook outlived the operation");
    std::fs::remove_file(gate).unwrap();
    // Quit Now doesn't wait.
    let (dir, gate) = repo_with_slow_hook(true);
    let options = AppOptions { stop_timeout: Duration::from_millis(300), ..Default::default() };
    let mut h = harness(vec![dir.path().join("repo")], None, options);
    start_commit(&mut h, dir.path());
    close_window(&mut h);
    h.get_by_label("Stop and Quit").click();
    until(&mut h, "asked again", |h| h.query_by_label("Quit Now").is_some());
    // (The dialog grows and moves when its buttons come: let it settle.)
    h.run_steps(3);
    h.get_by_label("Quit Now").click();
    h.run_steps(2);
    assert!(h.state().quitting());
    std::fs::remove_file(gate).unwrap();
    until(&mut h, "the commit ends", |_| tine::ops_registry::active_ops().is_empty());
}

#[test]
fn a_missing_or_old_git_is_told_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    // (Only the check's notices: results of operations of closed tabs, of
    // other tests here, are told too.)
    let check = |git: PathBuf| {
        let mut h = harness(vec![], None, AppOptions { git, ..Default::default() });
        until(&mut h, "git checked", |h| h.state().git_version().is_some());
        let notices: Vec<String> = h.state().notices().iter().filter(|n| n.contains("Tine needs")).cloned().collect();
        (h.state().git_version().unwrap().clone(), notices)
    };
    let old = dir.path().join("old-git");
    script(&old, "echo 'git version 2.34.1'");
    let (v, notices) = check(old);
    assert_eq!(v, Ok(tine_core::version::GitVersion(2, 34, 1)));
    assert!(notices.iter().any(|n| n.contains("git 2.34.1 is older than 2.43")), "{notices:?}");
    let (v, notices) = check(dir.path().join("no-such-git"));
    assert!(v.is_err());
    assert!(notices.iter().any(|n| n.contains("git isn't usable")), "{notices:?}");
    let odd = dir.path().join("odd-git");
    script(&odd, "echo 'something else'");
    let (_, notices) = check(odd);
    assert!(notices.iter().any(|n| n.contains("can't read the version")), "{notices:?}");
    let (v, notices) = check("git".into());
    assert!(v.is_ok() && notices.is_empty(), "{notices:?}");
}

#[test]
fn about_shows_versions_places_and_licenses() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.ron");
    let mut h = harness(vec![], None, AppOptions { state_file: Some(state.clone()), ..Default::default() });
    until(&mut h, "git checked", |h| h.state().git_version().is_some());
    h.get_by_label("☰").click();
    h.run_steps(2);
    h.get_by_label("About Tine").click();
    h.run_steps(4);
    assert!(h.state().about_open());
    assert!(h.query_by_label(&format!("Tine {}", env!("CARGO_PKG_VERSION"))).is_some());
    assert!(h.query_by_label("MIT OR Apache-2.0 (© 2026 K2Y)").is_some());
    assert!(h.query_by_label(&state.display().to_string()).is_some());
    assert!(h.query_by_label_contains("/proc/").is_some(), "the helper, as git runs it");
    let git = h.state().git_version().unwrap().as_ref().unwrap().to_string();
    assert!(h.query_by_label(&git).is_some());
    h.get_by_label("Third-Party Licenses…").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("THIRD-PARTY LICENSES").is_some());
    // It keeps its size (it once grew a row every frame).
    let at = h.get_by_label("Close").rect();
    h.run_steps(5);
    assert_eq!(h.get_by_label("Close").rect(), at);
    h.get_by_label("Close").click();
    h.run_steps(2);
    assert!(!h.state().about_open());
}

#[test]
fn while_quitting_is_asked_nothing_else_shows_or_takes_keys() {
    let _turn = OPS.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, gate) = repo_with_slow_hook(false);
    let mut h = harness(vec![dir.path().join("repo")], None, AppOptions::default());
    start_commit(&mut h, dir.path());
    // A dialog of the tab is open (confirmed by Enter).
    let main = h.state_mut().active_tab_mut().unwrap().worktrees[0].clone();
    let mut other = main.clone();
    other.main = false;
    other.path = dir.path().join("elsewhere");
    h.state_mut().active_tab_mut().unwrap().request_lock_worktree(&other);
    h.run_steps(4);
    assert!(h.query_by_label("Lock 'elsewhere'").is_some());
    close_window(&mut h);
    assert!(h.state().quit_asked());
    assert!(h.query_by_label("Lock 'elsewhere'").is_none(), "hidden behind the question");
    h.key_press(egui::Key::Enter);
    h.run_steps(2);
    assert!(h.state_mut().active_tab_mut().unwrap().dialog.is_some(), "Enter didn't confirm it");
    h.get_all_by_label("Cancel").last().unwrap().click();
    h.run_steps(4);
    assert!(h.query_by_label("Lock 'elsewhere'").is_some(), "back once quitting is off");
    std::fs::remove_file(gate).unwrap();
    until(&mut h, "the commit ends", |h| !h.state_mut().active_tab_mut().unwrap().op_active());
}

/// A conflicted merge (g.txt) with the merge editor open and changed.
fn unsaved_merge() -> (tempfile::TempDir, Harness<'static, TineApp>) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("g.txt"), "x\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    git(d, &["checkout", "-q", "-b", "topic"], &mut t);
    std::fs::write(d.join("g.txt"), "x topic\n").unwrap();
    git(d, &["commit", "-qam", "topic"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("g.txt"), "x main\n").unwrap();
    git(d, &["commit", "-qam", "main"], &mut t);
    let o = std::process::Command::new("git").current_dir(d).args(["merge", "topic"]).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(!o.status.success());
    let mut h = harness(vec![d.to_path_buf()], None, AppOptions::default());
    let tab = h.state_mut().active_tab_mut().unwrap();
    tab.view = tine::repo::View::LocalChanges;
    tab.open_merge_editor("g.txt".into());
    until(&mut h, "the editor opens", |h| h.query_by_label("Use Ours (conflict 1)").is_some());
    h.get_by_label("Use Ours (conflict 1)").click();
    h.run_steps(2);
    assert!(h.state_mut().active_tab_mut().unwrap().merge_dirty());
    (dir, h)
}

#[test]
fn what_runs_when_quitting_is_answered_is_stopped_first() {
    let _turn = OPS.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, mut h) = unsaved_merge();
    // Asked about the merge only: nothing runs yet.
    close_window(&mut h);
    assert!(h.state().quit_asked());
    let (cancel_at, quit_at) = (h.get_by_label("Cancel").rect().x_range(), h.get_by_label("Quit Anyway").rect().x_range());
    // Something starts meanwhile (until stopped).
    let (tx, _rx) = crossbeam_channel::unbounded();
    let paths = tine_core::RepoPaths { workdir: "/m6/quit".into(), git_dir: "/m6/quit".into(), common_dir: "/m6/quit".into() };
    let id = tine::ops_registry::submit(
        Path::new("/m6/quit"),
        &paths,
        tine::worker::OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(|ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(tine_core::Cancelled.into())
        }),
    )
    .unwrap();
    h.run_steps(2);
    // Told, the buttons side by side as they were (the dialog grows: their
    // row moves as one, nothing else comes under the pointer).
    assert_eq!(h.get_by_label("Cancel").rect().x_range(), cancel_at, "Cancel moved");
    assert_eq!(h.get_by_label("Stop and Quit").rect().x_range(), quit_at);
    h.get_by_label("Stop and Quit").click();
    h.run_steps(1);
    assert!(!h.state().quitting(), "not while it runs");
    until(&mut h, "stopped, then closing", |h| h.state().quitting());
    assert_eq!(
        tine::ops_registry::state(id),
        Some(tine::ops_registry::OpState::Finished(tine::ops_registry::Outcome::Cancelled))
    );
}
