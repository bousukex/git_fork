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

#[test]
fn stage_hunk_button() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    // README.md is selected first and has one hunk.
    h.get_by_label("Stage Hunk").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.status.staged.iter().any(|f| f.path == "README.md"), "{:?}", tab.status.staged);
    assert!(!tab.status.unstaged.iter().any(|f| f.path == "README.md"));
}

#[test]
fn second_hunk_button_stages_only_its_lines() {
    use common::git;
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_950_000_000;
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("long.txt"), &body).unwrap();
    git(d, &["add", "long.txt"], &mut t);
    git(d, &["commit", "-qm", "long"], &mut t);
    std::fs::write(d.join("long.txt"), body.replace("line 2\n", "line two\n").replace("line 28\n", "line 28\nextra\n")).unwrap();

    let mut h = app(vec![d.into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let f = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "long.txt").unwrap();
    tab.click_change(tine::repo::Side::Unstaged, &f, false, false);
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let Some(Ok(doc)) = &tab.changes_diff.doc else { panic!("no diff") };
    let rows: std::collections::BTreeSet<usize> =
        doc.rows.iter().enumerate().filter(|(_, r)| r.text == "line two" || r.text == "extra").map(|(i, _)| i).collect();
    assert_eq!(rows.len(), 2);
    tab.select_diff_rows(rows, None);
    h.run_steps(2);

    let buttons = h.get_all_by_label("Stage Lines (1)").collect::<Vec<_>>();
    assert_eq!(buttons.len(), 2, "one button per hunk");
    buttons[1].click();
    settle(&mut h);
    let staged = std::process::Command::new("git").current_dir(d).args(["show", ":long.txt"]).output().unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(staged.contains("\nextra\n"), "{staged}");
    assert!(staged.contains("\nline 2\n"), "the first hunk must stay unstaged: {staged}");
}

fn head_branch(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["symbolic-ref", "--short", "HEAD"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn sidebar_menu_checkout_and_branch_dialog() {
    let repo = common::diverged_repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    h.get_by_label("branch topic").click_secondary();
    h.run_steps(2);
    h.get_by_label("Check Out 'topic'").click();
    settle(&mut h);
    assert_eq!(head_branch(d), "topic");

    h.get_by_label("Branch").click();
    h.run_steps(2);
    if let Some(tine::repo::dialog::Dialog::CreateBranch { name, .. }) = &mut h.state_mut().active_tab_mut().unwrap().dialog {
        *name = "from-ui".into();
    } else {
        panic!("create branch dialog expected");
    }
    h.run_steps(2);
    h.get_by_label("Create").click();
    settle(&mut h);
    assert_eq!(head_branch(d), "from-ui");
}

#[test]
fn conflict_banner_abort_needs_confirmation() {
    let repo = common::diverged_repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    h.state_mut()
        .active_tab_mut()
        .unwrap()
        .integrate(tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() }, false);
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.state, tine_core::model::RepoState::Merging);

    // Resolve via the conflict menu, then abort via the banner.
    h.get_by_label("unstaged file f.txt").click_secondary();
    h.run_steps(2);
    h.get_by_label("Use Incoming Version (theirs)").click();
    settle(&mut h);
    assert!(h.state_mut().active_tab_mut().unwrap().status.conflicted.is_empty());

    h.get_by_label("Abort…").click();
    h.run_steps(2);
    assert_eq!(
        h.state_mut().active_tab_mut().unwrap().status.state,
        tine_core::model::RepoState::Merging,
        "nothing happens before confirming"
    );
    h.get_by_label("Abort merge").click();
    settle(&mut h);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().status.state, tine_core::model::RepoState::Clean);
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "main\n");
}
