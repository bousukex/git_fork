//! UI interaction through the AccessKit tree (no GPU needed).

mod common;

use common::{sample_repo, settle};
use egui_kittest::{Harness, kittest::Queryable as _};
use tine::app::TineApp;

fn app(paths: Vec<std::path::PathBuf>) -> Harness<'static, TineApp> {
    Harness::builder().with_size([1200.0, 760.0]).build_eframe(move |cc| TineApp::new(cc, paths))
}

fn title(dir: &tempfile::TempDir) -> String {
    dir.path().file_name().unwrap().to_string_lossy().into_owned()
}

#[test]
fn refresh_button_reloads_history() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    assert!(h.state().is_idle());
    h.get_by_label("Refresh").click();
    h.step();
    assert!(!h.state().is_idle(), "clicking Refresh must start a reload");
    settle(&mut h);
    assert!(h.state().is_idle());
}

#[test]
fn switching_and_closing_tabs() {
    let (a, b, c) = (sample_repo(), sample_repo(), sample_repo());
    let mut h = app(vec![a.path().into(), b.path().into(), c.path().into()]);
    settle(&mut h);
    assert_eq!(h.state().tab_count(), 3);
    assert_eq!(h.state().active_title(), Some(title(&c).as_str()));

    // Clicking a tab activates it.
    h.get_by_label(&title(&a)).click();
    h.run_steps(2);
    assert_eq!(h.state().active_title(), Some(title(&a).as_str()));

    // Closing a tab left of the active one keeps the same repository active.
    h.get_by_label(&title(&c)).click();
    h.run_steps(2);
    h.state_mut().close_tab(0);
    h.run_steps(2);
    assert_eq!(h.state().tab_count(), 2);
    assert_eq!(h.state().active_title(), Some(title(&c).as_str()));

    // Context menu "Close Tab" on the active tab.
    h.get_by_label(&title(&c)).click_secondary();
    h.run_steps(2);
    h.get_by_label("Close Tab").click();
    h.run_steps(2);
    assert_eq!(h.state().tab_count(), 1);
    assert_eq!(h.state().active_title(), Some(title(&b).as_str()));
}

#[test]
fn stage_all_and_commit_buttons() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    h.get_by_label("Stage All").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.staged.len(), 2);
    assert!(tab.status.unstaged.is_empty());
    tab.commit_subject = "UIからのコミット".into();
    h.run_steps(2);
    h.get_by_label("Commit 2 Files").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.history.commits[0].subject, "UIからのコミット");
    assert_eq!(tab.status.change_count(), 0);
}
