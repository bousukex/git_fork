//! File History and Blame through the UI (M4b).

mod common;

use common::{sample_repo, settle};
use egui::{Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable as _};
use pengitto::{app::PengittoApp, repo::browse::Browse};

fn app(paths: Vec<std::path::PathBuf>) -> Harness<'static, PengittoApp> {
    Harness::builder().with_size([1300.0, 800.0]).build_eframe(move |cc| PengittoApp::new(cc, paths))
}

fn top(h: &mut Harness<'static, PengittoApp>) -> Option<&'static str> {
    match h.state_mut().active_tab_mut().unwrap().browse.last() {
        Some(Browse::FileHistory(_)) => Some("history"),
        Some(Browse::Blame(_)) => Some("blame"),
        None => None,
    }
}

#[test]
fn from_a_commit_file_to_history_blame_and_back() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    // Select the commit that touched README.md and open its file's menu.
    h.get_by_label_contains("Document usage in README").click();
    settle(&mut h);
    h.get_by_label("changed file README.md").click_secondary();
    h.run_steps(2);
    h.get_by_label("File History").click();
    settle(&mut h);
    assert_eq!(top(&mut h), Some("history"));
    assert!(h.query_by_label_contains("History of README.md").is_some());
    let entries = h.query_all_by_label_contains("file history ").count();
    assert_eq!(entries, 2);

    // Blame at the initial commit from the entry's menu.
    h.get_by_label_contains("Initial commit").click_secondary();
    h.run_steps(2);
    h.get_by_label("Blame at This Commit").click();
    settle(&mut h);
    assert_eq!(top(&mut h), Some("blame"));
    assert!(h.query_by_label_contains("blame line 1 ").is_some());

    // Esc goes back one step.
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert_eq!(top(&mut h), Some("history"));

    // Show in History leaves browsing with the commit selected.
    h.get_by_label_contains("Initial commit").click_secondary();
    h.run_steps(2);
    h.get_by_label("Show in History").click();
    settle(&mut h);
    assert_eq!(top(&mut h), None);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let selected = tab.selected_commit.unwrap();
    assert_eq!(tab.history.commits.iter().find(|c| c.id == selected).unwrap().subject, "Initial commit");
}

#[test]
fn blame_line_menu_shows_the_commit() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.state_mut().active_tab_mut().unwrap().blame_of_local("README.md".into());
    settle(&mut h);
    let line = h.get_by_label_contains("blame line 1 ");
    line.click_secondary();
    h.run_steps(2);
    h.get_by_label("Show Commit").click();
    settle(&mut h);
    assert_eq!(top(&mut h), None);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let selected = tab.selected_commit.unwrap();
    assert_eq!(tab.history.commits.iter().find(|c| c.id == selected).unwrap().subject, "Initial commit");
}

#[test]
fn quick_launch_opens_files() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    settle(&mut h); // the file list loads in the background
    h.event(egui::Event::Text("lib.rs".into()));
    h.run_steps(2);
    let listed = h.state_mut().quick_launch().unwrap().listed();
    assert_eq!(listed.first().map(|l| (l.kind, l.label.as_str())), Some((pengitto::ui::quick_launch::ItemKind::File, "lib.rs")));
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(top(&mut h), Some("history"));

    h.key_press(Key::Escape);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    settle(&mut h);
    h.event(egui::Event::Text("lib.rs".into()));
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    settle(&mut h);
    assert_eq!(top(&mut h), Some("blame"), "Shift+Enter blames");
}

#[test]
fn switching_views_leaves_browsing() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    let head = h.state_mut().active_tab_mut().unwrap().head_commit().unwrap();
    h.state_mut().active_tab_mut().unwrap().open_file_history(head, "main.rs".into(), None);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    h.run_steps(2);
    assert_eq!(top(&mut h), None);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().view, pengitto::repo::View::LocalChanges);
}

#[test]
fn keyboard_moves_through_blame_and_history() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    let head = h.state_mut().active_tab_mut().unwrap().head_commit().unwrap();
    h.state_mut().active_tab_mut().unwrap().open_file_history(head, "main.rs".into(), None);
    settle(&mut h);
    h.key_press(Key::ArrowDown);
    settle(&mut h);
    let selected = |h: &mut Harness<'static, PengittoApp>| match h.state_mut().active_tab_mut().unwrap().browse.last() {
        Some(Browse::FileHistory(v)) => v.selected,
        Some(Browse::Blame(v)) => v.selected,
        None => None,
    };
    assert_eq!(selected(&mut h), Some(1), "↓ picks the next commit");
    h.state_mut().active_tab_mut().unwrap().open_blame(Some(head), "main.rs".into(), None);
    settle(&mut h);
    h.key_press(Key::ArrowDown);
    h.key_press(Key::ArrowDown);
    h.run_steps(2);
    assert_eq!(selected(&mut h), Some(1));
    h.key_press(Key::End);
    h.run_steps(2);
    assert_eq!(selected(&mut h), Some(3), "End: the last line");
}

/// A repository whose `long.txt` has `lines` lines over `commits` commits.
fn long_repo(lines: usize, commits: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    common::git(d, &["init", "-q", "-b", "main"], &mut t);
    for c in 0..commits {
        // Each commit rewrites one line (round robin), so every one changes the file.
        let body: String = (0..lines).map(|i| format!("line {i} v{}\n", if i == c % lines { c } else { 0 })).collect();
        std::fs::write(d.join("long.txt"), body).unwrap();
        common::git(d, &["add", "-A"], &mut t);
        common::git(d, &["commit", "-qm", &format!("change {c}")], &mut t);
    }
    dir
}

#[test]
fn a_long_history_follows_the_keyboard() {
    let repo = long_repo(5, 60);
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    let head = h.state_mut().active_tab_mut().unwrap().head_commit().unwrap();
    h.state_mut().active_tab_mut().unwrap().open_file_history(head, "long.txt".into(), None);
    settle(&mut h);
    h.key_press(Key::End);
    settle(&mut h);
    // Rows are drawn only when visible: the oldest commit's row is there.
    assert!(h.query_by_label_contains(" change 0").is_some(), "the selected row is in view");
    assert!(h.query_by_label_contains(" change 59").is_none(), "the newest scrolled out");
}

#[test]
fn scroll_positions_are_kept_per_tab_and_view() {
    let (a, b) = (long_repo(300, 1), long_repo(300, 1));
    let mut h = app(vec![a.path().into(), b.path().into()]);
    settle(&mut h);
    let title = |d: &tempfile::TempDir| d.path().file_name().unwrap().to_string_lossy().into_owned();
    for _ in 0..2 {
        let tab = h.state_mut().active_tab_mut().unwrap();
        let head = tab.head_commit().unwrap();
        tab.open_blame(Some(head), "long.txt".into(), None);
        h.key_press_modifiers(Modifiers::COMMAND, Key::Tab);
        settle(&mut h);
    }
    // Tab b (active again after two switches): scroll to the end.
    assert_eq!(h.state().active_title(), Some(title(&b).as_str()));
    h.key_press(Key::End);
    settle(&mut h);
    assert!(h.query_by_label_contains("blame line 300 ").is_some());
    // Tab a still starts at the top; back in b, still at the end.
    h.key_press_modifiers(Modifiers::COMMAND, Key::Tab);
    settle(&mut h);
    assert!(h.query_by_label_contains("blame line 1 ").is_some(), "a has its own scroll position");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Tab);
    settle(&mut h);
    assert!(h.query_by_label_contains("blame line 300 ").is_some(), "b kept its position");
}

#[test]
fn blame_before_deletion_from_the_history() {
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    common::git(d, &["rm", "-q", "lib.rs"], &mut t);
    common::git(d, &["commit", "-qm", "remove lib"], &mut t);
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    let head = h.state_mut().active_tab_mut().unwrap().head_commit().unwrap();
    h.state_mut().active_tab_mut().unwrap().open_file_history(head, "lib.rs".into(), None);
    settle(&mut h);
    h.get_by_label_contains("remove lib").click_secondary();
    h.run_steps(2);
    h.get_by_label("Blame Before Deletion").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let Some(Browse::Blame(v)) = tab.browse.last() else { panic!("no blame") };
    assert_eq!(v.rev, tab.history.commits.iter().find(|c| c.subject == "remove lib").unwrap().parents.first().copied());
    assert!(v.doc.as_ref().unwrap().is_ok(), "{:?}", v.doc.as_ref().unwrap().as_ref().err());
}

#[test]
fn uncommitted_lines_can_blame_the_previous_revision() {
    let repo = sample_repo();
    let d = repo.path();
    std::fs::write(d.join("lib.rs"), "pub fn add(a: i32, b: i32) -> i32 {\n    a + b + 0\n}\n").unwrap();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    h.state_mut().active_tab_mut().unwrap().blame_of_local("lib.rs".into());
    settle(&mut h);
    h.get_by_label_contains("blame line 2 00000000").click_secondary();
    h.run_steps(2);
    h.get_by_label("Blame Previous Revision").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let Some(Browse::Blame(v)) = tab.browse.last() else { panic!("no blame") };
    assert_eq!(v.rev, tab.head_commit(), "the committed version");
    assert_eq!(tab.browse.len(), 2);
}
