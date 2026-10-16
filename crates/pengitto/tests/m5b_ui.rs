//! The merge editor through the UI (M5b): opening it from the file list,
//! resolving with the buttons and F7, the result edited as text, and
//! leaving with unsaved changes.

mod common;

use std::{path::Path, process::Command};

use common::{git, settle};
use egui::{Event, Key, Modifiers};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use pengitto::{app::PengittoApp, repo::dialog::Dialog};

/// f.txt conflicting in two places, g.txt in one; merging topic into main.
fn merging() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    let write = |f: &str, s: &str| std::fs::write(d.join(f), s).unwrap();
    write("f.txt", "1\na\n2\n3\n4\nb\n5\n");
    write("g.txt", "x\n");
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    git(d, &["checkout", "-q", "-b", "topic"], &mut t);
    write("f.txt", "1\nA topic\n2\n3\n4\nB topic\n5\n");
    write("g.txt", "x topic\n");
    git(d, &["commit", "-qam", "topic change"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    write("f.txt", "1\nA main\n2\n3\n4\nB main\n5\n");
    write("g.txt", "x main\n");
    git(d, &["commit", "-qam", "main change"], &mut t);
    let o = Command::new("git").current_dir(d).args(["merge", "topic"]).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(!o.status.success());
    dir
}

/// One test at a time: operations are counted process-wide (the quit
/// question lists those of every app in this process).
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn app(d: &Path) -> Harness<'static, PengittoApp> {
    let paths = vec![d.to_path_buf()];
    let mut h = Harness::builder().with_size([1300.0, 820.0]).build_eframe(move |cc| {
        let mut app = PengittoApp::new(cc, paths);
        app.show_local_changes();
        app
    });
    settle(&mut h);
    h
}

fn key(h: &mut Harness<'static, PengittoApp>, key: Key, modifiers: Modifiers) {
    h.input_mut().events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
    h.step();
}

/// Two clicks a frame apart (kittest has no double click).
fn double_click(h: &mut Harness<'static, PengittoApp>, label: &str) {
    let pos = h.get_by_label(label).rect().center();
    let button = |pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.input_mut().events.push(Event::PointerMoved(pos));
    h.step();
    for _ in 0..2 {
        h.input_mut().events.extend([button(true), button(false)]);
        h.step();
    }
}

fn disabled(h: &Harness<'static, PengittoApp>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().is_disabled()
}

fn conflicted(h: &mut Harness<'static, PengittoApp>) -> Vec<String> {
    h.state_mut().active_tab_mut().unwrap().status.conflicted.iter().map(|f| f.path.to_string()).collect()
}

#[test]
fn resolving_from_the_menu_with_buttons_and_f7() {
    let _turn = turn();
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    h.get_by_label("unstaged file f.txt").click_secondary();
    h.run_steps(2);
    h.get_by_label("Resolve in Merge Editor…").click();
    settle(&mut h);
    assert!(h.query_by_label("Use Theirs (conflict 1)").is_some(), "the editor shows");
    assert!(disabled(&h, "Save & Mark Resolved"), "two conflicts left");
    h.get_by_label("Use Theirs (conflict 1)").click();
    h.run_steps(2);
    // F7 to the next one still open (also from the first, resolved now).
    key(&mut h, Key::F7, Modifiers::NONE);
    h.run_steps(2);
    let current = h.state_mut().active_tab_mut().unwrap().merge.as_ref().unwrap().session().unwrap().current;
    assert_eq!(current, Some(1));
    assert!(h.query_by_label("Change (conflict 1)").is_some());
    h.get_by_label("Use Ours (conflict 2)").click();
    h.run_steps(2);
    assert!(!disabled(&h, "Save & Mark Resolved"));
    h.get_by_label("Save & Mark Resolved").click();
    settle(&mut h);
    assert_eq!(conflicted(&mut h), ["g.txt"]);
    assert!(h.query_by_label("Use Ours (conflict 2)").is_none(), "closed once resolved");
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "1\nA topic\n2\n3\n4\nB main\n5\n");
}

#[test]
fn double_click_opens_and_esc_asks_before_discarding() {
    let _turn = turn();
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    h.get_by_label("Use Ours (conflict 1)").click();
    h.run_steps(2);
    key(&mut h, Key::Escape, Modifiers::NONE);
    h.run_steps(4);
    assert!(h.query_by_label("Discard Changes").is_some(), "asked first");
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.query_by_label("Change (conflict 1)").is_some(), "still there");
    key(&mut h, Key::Escape, Modifiers::NONE);
    h.run_steps(4);
    h.get_by_label("Discard Changes").click();
    h.run_steps(2);
    assert!(h.query_by_label("Change (conflict 1)").is_none());
    assert_eq!(conflicted(&mut h), ["f.txt", "g.txt"], "nothing saved");
    // Nothing changed: Esc closes at once.
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    key(&mut h, Key::Escape, Modifiers::NONE);
    h.run_steps(2);
    assert!(h.query_by_label("Use Ours (conflict 1)").is_none());
}

#[test]
fn the_result_edited_in_a_dialog() {
    let _turn = turn();
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    h.get_by_label("Edit Result…").click();
    h.run_steps(4);
    let Some(Dialog::EditMergeResult { text, .. }) = &h.state_mut().active_tab_mut().unwrap().dialog else { panic!("open") };
    assert!(text.contains("<<<<<<< HEAD"), "{text}");
    assert!(h.query_by_label("Edit Result").is_some());
    // Applied unchanged: nothing to save.
    h.get_by_label("Apply").click();
    h.run_steps(2);
    assert!(!h.state_mut().active_tab_mut().unwrap().merge_dirty());
    // Rewritten by hand.
    h.get_by_label("Edit Result…").click();
    h.run_steps(4);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let Some(Dialog::EditMergeResult { text, .. }) = &mut tab.dialog else { panic!("open") };
    *text = "x both\n".into();
    h.run_steps(1);
    h.get_by_label("Apply").click();
    h.run_steps(2);
    assert!(!disabled(&h, "Save & Mark Resolved"));
    h.get_by_label("Save & Mark Resolved").click();
    settle(&mut h);
    assert_eq!(std::fs::read_to_string(d.join("g.txt")).unwrap(), "x both\n");
    assert_eq!(conflicted(&mut h), ["f.txt"]);
}

#[test]
fn closing_the_tab_asks_about_unsaved_changes() {
    let _turn = turn();
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    h.get_by_label("Use Theirs (conflict 1)").click();
    h.run_steps(2);
    key(&mut h, Key::W, Modifiers::COMMAND);
    h.run_steps(4);
    assert_eq!(h.state().tab_count(), 1, "not yet");
    h.get_by_label("Discard Changes").click();
    h.run_steps(3);
    assert_eq!(h.state().tab_count(), 0);
}

#[test]
fn the_editor_stays_reachable_after_the_merge_ends_elsewhere() {
    let _turn = turn();
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    h.get_by_label("Use Theirs (conflict 1)").click();
    h.run_steps(2);
    // Another file looked at: the editor hides, kept as it is…
    h.get_by_label("unstaged file f.txt").click();
    settle(&mut h);
    assert!(h.query_by_label("Change (conflict 1)").is_none());
    h.get_by_label("Show").click();
    settle(&mut h);
    assert!(h.query_by_label("Change (conflict 1)").is_some(), "…and comes back");
    // Aborted on the command line: no changes listed, the editor still there.
    let o = Command::new("git").current_dir(d).args(["merge", "--abort"]).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success());
    h.state_mut().active_tab_mut().unwrap().refresh();
    settle(&mut h);
    assert!(h.state_mut().active_tab_mut().unwrap().status.change_count() == 0);
    assert!(h.query_by_label("Copy Result").is_some(), "what was resolved can be copied");
    let note = h.state_mut().active_tab_mut().unwrap().merge.as_ref().unwrap().note.clone().unwrap_or_default();
    assert!(note.contains("isn't conflicted any more"), "{note}");
    h.get_by_label("Close").click();
    h.run_steps(4);
    h.get_by_label("Discard Changes").click();
    h.run_steps(2);
    assert!(h.query_by_label("Copy Result").is_none());
}

#[test]
fn closing_the_window_asks_about_unsaved_changes() {
    let _turn = turn();
    let close = |h: &mut Harness<'static, PengittoApp>| {
        h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
        h.step();
        h.run_steps(3);
    };
    let repo = merging();
    let d = repo.path();
    let mut h = app(d);
    // Nothing unsaved: nothing asked.
    close(&mut h);
    assert!(!h.state().quit_asked());
    double_click(&mut h, "unstaged file g.txt");
    settle(&mut h);
    h.get_by_label("Use Ours (conflict 1)").click();
    h.run_steps(2);
    close(&mut h);
    assert!(h.state().quit_asked());
    assert!(h.query_by_label("Quit and lose unsaved merge changes?").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(!h.state().quit_asked());
    assert!(h.state_mut().active_tab_mut().unwrap().merge_dirty(), "kept");
    close(&mut h);
    h.get_by_label("Quit Anyway").click();
    h.run_steps(2);
    assert!(!h.state().quit_asked());
}
