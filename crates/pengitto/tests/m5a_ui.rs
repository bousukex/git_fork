//! Interactive rebase through the app (M5a): the dialog, the edit stop,
//! undo, restarts and stashed changes.

mod common;

use std::{path::Path, process::Command};

use common::{git, idle, poll_until, settle};
use egui::{Event, Key, Modifiers};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use pengitto::{
    app::PengittoApp,
    repo::{RepoTab, View, dialog::Dialog},
};
use pengitto_core::model::RepoState;

fn app(paths: Vec<std::path::PathBuf>) -> Harness<'static, PengittoApp> {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    Harness::builder().with_size([1300.0, 820.0]).build_eframe(move |cc| PengittoApp::new(cc, paths))
}

fn out(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").current_dir(dir).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// base, then c1..c3 each adding its own file.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("base"), "base\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    for i in 1..=3 {
        std::fs::write(d.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", &format!("c{i}")], &mut t);
    }
    dir
}

fn short(d: &Path, rev: &str) -> String {
    out(d, &["rev-parse", "--short=8", rev]).trim().to_owned()
}

fn subjects(d: &Path) -> Vec<String> {
    out(d, &["log", "--format=%s"]).lines().map(Into::into).collect()
}

/// Opens the dialog for the commits after "base".
fn open_dialog(h: &mut Harness<'static, PengittoApp>, d: &Path) {
    h.get_by_label(&format!("commit {} base", short(d, "HEAD~3"))).click_secondary();
    h.run_steps(2);
    h.get_by_label("Interactive Rebase from Here…").click();
    settle(h);
    assert!(h.query_by_label("Interactive Rebase").is_some(), "the dialog is open");
}

fn disabled(h: &Harness<'static, PengittoApp>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().is_disabled()
}

#[test]
fn squash_from_the_dialog_rewrites_the_history() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    open_dialog(&mut h, d);
    // c2 squashed into c1; c3 reworded.
    h.get_by_label(&format!("squash {}", short(d, "HEAD~1"))).click();
    h.run_steps(2);
    h.get_by_label(&format!("reword {}", short(d, "HEAD"))).click();
    h.run_steps(2);
    let Some(Dialog::InteractiveRebase(dlg)) = &mut h.state_mut().active_tab_mut().unwrap().dialog else { panic!("dialog") };
    let c3 = dlg.plan.items[2].0;
    *dlg.reword_message(c3) = "c3, reworded".into();
    dlg.reword_changed();
    h.run_steps(2);
    assert!(!disabled(&h, "Start Rebase"));
    h.get_by_label("Start Rebase").click();
    settle(&mut h);
    assert_eq!(subjects(d), ["c3, reworded", "c1", "base"]);
    let body = out(d, &["log", "-1", "--format=%B", "HEAD~1"]);
    assert_eq!(body.trim(), "c1\n\nc2", "git's combined message");
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.rebase_done.is_some(), "can be undone");
    assert!(!pengitto_core::interactive::session_dir(&tab.paths.git_dir).exists());
    // Undo opens the hard reset to where it was (losses shown first).
    tab.undo_rebase();
    let Some(Dialog::Reset { mode, .. }) = &tab.dialog else { panic!("reset dialog") };
    assert_eq!(*mode, pengitto_core::integrate::ResetMode::Hard);
}

#[test]
fn plans_that_cant_run_cant_start() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    open_dialog(&mut h, d);
    // The oldest can't meld into anything.
    h.get_by_label(&format!("squash {}", short(d, "HEAD~2"))).click();
    h.run_steps(2);
    assert!(h.query_by_label_contains("no commit below it").is_some());
    assert!(disabled(&h, "Start Rebase"));
    h.get_by_label(&format!("pick {}", short(d, "HEAD~2"))).click();
    h.run_steps(2);
    assert!(!disabled(&h, "Start Rebase"));
    for rev in ["HEAD", "HEAD~1", "HEAD~2"] {
        h.get_by_label(&format!("drop {}", short(d, rev))).click();
        h.run_steps(1);
    }
    h.run_steps(1);
    assert!(h.query_by_label("Every commit would be dropped").is_some());
    assert!(disabled(&h, "Start Rebase"));
    // An untouched plan closes with Esc; a changed one doesn't.
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.query_by_label("Interactive Rebase").is_some(), "changed: Cancel closes it");
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.query_by_label("Interactive Rebase").is_none());
    assert_eq!(subjects(d), ["c3", "c2", "c1", "base"], "nothing ran");
}

fn key(k: Key, modifiers: Modifiers) -> Event {
    Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers }
}

#[test]
fn keys_and_dragging_reorder() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    open_dialog(&mut h, d);
    h.run_steps(2);
    let order = |h: &mut Harness<'static, PengittoApp>| -> Vec<String> {
        let Some(Dialog::InteractiveRebase(dlg)) = &h.state_mut().active_tab_mut().unwrap().dialog else { panic!() };
        dlg.plan.items.iter().map(|(id, a)| format!("{}:{}", dlg.commit(*id).unwrap().subject, a.word())).collect()
    };
    // The newest (c3) is selected: Alt+↓ moves it below c2; D drops it.
    h.input_mut().events.push(key(Key::ArrowDown, Modifiers::ALT));
    h.step();
    h.input_mut().events.push(key(Key::D, Modifiers::NONE));
    h.step();
    assert_eq!(order(&mut h), ["c1:pick", "c3:drop", "c2:pick"]);
    // ↓ selects c1 (below); R rewords it.
    h.input_mut().events.push(key(Key::ArrowDown, Modifiers::NONE));
    h.step();
    h.input_mut().events.push(key(Key::R, Modifiers::NONE));
    h.step();
    assert_eq!(order(&mut h), ["c1:reword", "c3:drop", "c2:pick"]);

    // Drag c1 (bottom row) onto c2 (top row): it goes on top.
    let center = |h: &Harness<'static, PengittoApp>, subject: &str| {
        h.get_by_label(subject).rect().center()
    };
    let c1_row = format!("rebase row {} c1", short(d, "HEAD~2"));
    let c2_row = format!("rebase row {} c2", short(d, "HEAD~1"));
    let (from, to) = (center(&h, &c1_row), center(&h, &c2_row));
    let button = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(button(from, true));
    h.step();
    for k in 1..=6 {
        let p = from + (to - from) * (k as f32 / 6.0);
        h.input_mut().events.push(Event::PointerMoved(p));
        h.step();
    }
    h.input_mut().events.push(button(to, false));
    h.step();
    h.run_steps(2);
    assert_eq!(order(&mut h), ["c3:drop", "c2:pick", "c1:reword"]);
}

#[test]
fn an_edit_stop_lets_the_commit_be_amended() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    open_dialog(&mut h, d);
    h.get_by_label(&format!("edit {}", short(d, "HEAD~1"))).click();
    h.run_steps(2);
    h.get_by_label("Start Rebase").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.state, RepoState::Rebasing);
    let at = tab.edit_stop().expect("stopped at the edit");
    assert_eq!(tab.view, View::LocalChanges, "where amending happens");
    assert!(h.query_by_label_contains(&format!("stopped at {} to edit", at.short())).is_some());
    // Amend the message from the commit box.
    let tab = h.state_mut().active_tab_mut().unwrap();
    tab.set_amend(true);
    assert!(poll_until(tab, |t| t.commit_subject == "c2"), "HEAD's message loaded");
    tab.commit_subject = "c2 amended".into();
    assert!(tab.can_commit());
    tab.commit();
    settle(&mut h);
    h.get_by_label("Continue").click();
    settle(&mut h);
    assert_eq!(subjects(d), ["c3", "c2 amended", "c1", "base"]);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.state, RepoState::Clean);
    assert!(tab.rebase_done.is_some());
}

fn open_tab(d: &Path) -> RepoTab {
    let mut tab = RepoTab::open(d, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading));
    tab
}

#[test]
fn a_stopped_rebase_survives_a_restart_with_its_stash() {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    std::fs::write(d.join("f1"), "local change\n").unwrap();
    let mut tab = open_tab(d);
    let base = pengitto_core::model::Oid::from_hex(out(d, &["rev-parse", "HEAD~3"]).trim().as_bytes()).unwrap();
    tab.open_interactive_rebase(Some(base));
    assert!(poll_until(&mut tab, |t| matches!(t.dialog, Some(Dialog::InteractiveRebase(_)))));
    let Some(Dialog::InteractiveRebase(dlg)) = tab.dialog.take() else { unreachable!() };
    let mut dlg = *dlg;
    let c2 = dlg.plan.items[1].0;
    // Stop at c2; c3 dropped (so the history really changes).
    dlg.set_action(1, pengitto_core::interactive::Action::Edit);
    dlg.set_action(2, pengitto_core::interactive::Action::Drop);
    let _ = c2;
    assert!(dlg.stash_first);
    tab.start_interactive_rebase(dlg);
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    let stash = tab.pending_autostash.expect("the local change is stashed");
    drop(tab);

    // pengitto restarts: the session and the stash are still known (the stash
    // is offered as soon as the tab opens, though only applicable later).
    let mut tab = open_tab(d);
    assert!(tab.rebase_session && tab.edit_stop().is_some());
    assert_eq!(tab.pending_autostash, Some(stash));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(tab.pending_autostash, Some(stash), "offered back once the rebase ended");
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.pending_autostash.is_none()));
    assert_eq!(std::fs::read_to_string(d.join("f1")).unwrap(), "local change\n");
    assert_eq!(subjects(d), ["c2", "c1", "base"]);
}

/// A rebase pengitto stopped at c2 to edit (c3 to be reworded, local
/// changes stashed), what pengitto keeps of it then replaced with what it
/// kept under its former name: the fixture, where c3's planned message is
/// "c3, from the former session". The stash.
fn stopped_under_the_former_name(d: &Path) -> pengitto_core::model::Oid {
    use pengitto_core::interactive::{self, Action};
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    std::fs::write(d.join("f1"), "local change\n").unwrap();
    let (head, base) = (oid(d, "HEAD"), oid(d, "HEAD~3"));
    let mut tab = open_tab(d);
    let mut ids = Vec::new();
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, Action::Edit);
        dlg.set_action(2, Action::Reword);
        let id = dlg.plan.items[2].0;
        *dlg.reword_message(id) = "pengitto's own message".into();
        dlg.reword_changed();
        ids = dlg.plan.items.iter().map(|i| i.0).collect();
        assert!(dlg.stash_first);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    let stash = tab.pending_autostash.expect("the local change is stashed");
    drop(tab);
    let git_dir = d.join(".git");
    std::fs::remove_dir_all(interactive::session_dir(&git_dir)).unwrap();
    std::fs::remove_file(git_dir.join("rebase-merge/pengitto-session")).unwrap();
    let hex = |o: pengitto_core::model::Oid| o.to_hex();
    let values = [
        ("ID", "former-session-1".to_owned()),
        ("HEAD", hex(head)),
        ("BASE", hex(base)),
        ("STASH", hex(stash)),
        ("C1", hex(ids[0])),
        ("C2", hex(ids[1])),
        ("C3", hex(ids[2])),
    ];
    common::former_rebase_session(&git_dir, &values, true);
    stash
}

/// M7a: the tab takes over a rebase stopped under the former name: its
/// planned message and its stash still apply.
#[test]
fn a_rebase_stopped_under_the_former_name_is_taken_over() {
    let repo = repo();
    let d = repo.path();
    let stash = stopped_under_the_former_name(d);
    let (former, _) = common::former_rebase_names();
    let mut tab = open_tab(d);
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_session), "taken over");
    assert!(!d.join(".git").join(&former).exists());
    assert!(tab.activity.iter().any(|a| a.contains("Took over the interactive rebase")), "{:?}", tab.activity);
    assert!(tab.edit_stop().is_some() && tab.rebase_block_reason().is_none());
    assert_eq!(tab.pending_autostash, Some(stash));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(subjects(d), ["c3, from the former session", "c2", "c1", "base"]);
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.pending_autostash.is_none()));
    assert_eq!(std::fs::read_to_string(d.join("f1")).unwrap(), "local change\n");
}

/// The same, aborted: the stash is offered back.
#[test]
fn an_aborted_rebase_of_the_former_name_offers_its_stash_back() {
    let repo = repo();
    let d = repo.path();
    let stash = stopped_under_the_former_name(d);
    let mut tab = open_tab(d);
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_session));
    tab.abort(RepoState::Rebasing);
    let git_dir = d.join(".git");
    assert!(poll_until(&mut tab, |t| idle(t)
        && t.status.state == RepoState::Clean
        && !pengitto_core::interactive::session_dir(&git_dir).exists()));
    assert_eq!(tab.pending_autostash, Some(stash));
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.pending_autostash.is_none()));
    assert_eq!(std::fs::read_to_string(d.join("f1")).unwrap(), "local change\n");
    assert_eq!(subjects(d), ["c3", "c2", "c1", "base"]);
}

/// Not taken over (a link where its folder goes): nothing of the rebase
/// runs — the buttons wait, and so does the queue — and the stash is
/// named; once it can be (F5), it goes on with its message.
#[test]
fn continuing_waits_until_the_former_rebase_is_taken_over() {
    let repo = repo();
    let d = repo.path();
    let stash = stopped_under_the_former_name(d);
    let git_dir = d.join(".git");
    let blocker = pengitto_core::interactive::session_dir(&git_dir);
    std::os::unix::fs::symlink("nowhere", &blocker).unwrap();
    let mut tab = open_tab(d);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.iter().any(|e| e.contains(&stash.to_hex()) && e.contains("git stash apply")), "{:?}", tab.errors);
    assert!(tab.former_rebase && tab.rebase_block_reason().is_some());
    let head = oid(d, "HEAD");
    tab.continue_op();
    tab.skip();
    tab.request_abort();
    assert!(tab.dialog.is_none());
    assert!(poll_until(&mut tab, idle));
    assert_eq!(oid(d, "HEAD"), head);
    assert_eq!(tab.status.state, RepoState::Rebasing);
    std::fs::remove_file(&blocker).unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_session && !t.former_rebase));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(subjects(d), ["c3, from the former session", "c2", "c1", "base"]);
}

/// The queue takes it over too, before telling whose rebase it is, when the
/// tab hasn't seen it yet (here it appears after the tab read the state).
#[test]
fn the_queue_takes_over_what_the_tab_hasnt_seen() {
    let repo = repo();
    let d = repo.path();
    let git_dir = d.join(".git");
    // Read as pengitto's own first…
    let stash = stopped_under_the_former_name(d);
    let (former, mark) = common::former_rebase_names();
    std::fs::rename(git_dir.join(&former), pengitto_core::interactive::session_dir(&git_dir)).unwrap();
    std::fs::rename(git_dir.join("rebase-merge").join(&mark), git_dir.join("rebase-merge/pengitto-session")).unwrap();
    let mut tab = open_tab(d);
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_session && !t.former_rebase));
    // …then under the former name again, the tab not told.
    std::fs::rename(pengitto_core::interactive::session_dir(&git_dir), git_dir.join(&former)).unwrap();
    std::fs::rename(git_dir.join("rebase-merge/pengitto-session"), git_dir.join("rebase-merge").join(&mark)).unwrap();
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert!(tab.rebase_done.is_some(), "ended as pengitto's own rebase");
    assert_eq!(subjects(d), ["c3, from the former session", "c2", "c1", "base"]);
    assert_eq!(tab.pending_autostash, Some(stash));
}

/// M7a: a session the former name left of a rebase that ended since is
/// tidied up too (its stash offered back), whether the state or the refs
/// are read first.
#[test]
fn an_ended_rebase_of_the_former_name_is_tidied_up() {
    let repo = repo();
    let d = repo.path();
    let git_dir = d.join(".git");
    std::fs::write(d.join("f1"), "local change\n").unwrap();
    out(d, &["-c", "user.name=T", "-c", "user.email=t@example.com", "stash", "push", "-q"]);
    let stash = oid(d, "stash@{0}");
    let hex = |o: pengitto_core::model::Oid| o.to_hex();
    let values = [
        ("ID", "former-session-2".to_owned()),
        ("HEAD", hex(oid(d, "HEAD"))),
        ("BASE", hex(oid(d, "HEAD~3"))),
        ("STASH", hex(stash)),
        ("C1", hex(oid(d, "HEAD~2"))),
        ("C2", hex(oid(d, "HEAD~1"))),
        ("C3", hex(oid(d, "HEAD"))),
    ];
    common::former_rebase_session(&git_dir, &values, false);
    let (former, _) = common::former_rebase_names();
    let mut tab = open_tab(d);
    assert!(poll_until(&mut tab, |t| idle(t)
        && !git_dir.join(&former).exists()
        && !pengitto_core::interactive::session_dir(&git_dir).exists()
        && t.pending_autostash == Some(stash)));
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.pending_autostash.is_none()));
    assert_eq!(std::fs::read_to_string(d.join("f1")).unwrap(), "local change\n");
}

/// Found while building the rebase dialog: the diff's drag selection lost
/// its anchor after the first frame (it was read back with the wrong type).
#[test]
fn dragging_over_diff_lines_selects_them_all() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("f"), "a\nb\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "f"], &mut t);
    std::fs::write(d.join("f"), "a\nnew 1\nnew 2\nnew 3\nb\n").unwrap();
    let mut h = app(vec![d.into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    h.get_by_label("unstaged file f").click();
    settle(&mut h);
    let center = |h: &Harness<'static, PengittoApp>, label: &str| h.get_by_label(label).rect().center();
    let (from, to) = (center(&h, "added line 2"), center(&h, "added line 4"));
    let button = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(button(from, true));
    h.step();
    for k in 1..=5 {
        h.input_mut().events.push(Event::PointerMoved(from + (to - from) * (k as f32 / 5.0)));
        h.step();
    }
    h.input_mut().events.push(button(to, false));
    h.step();
    h.run_steps(2);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.changes_diff.selected_rows.len(), 3, "{:?}", tab.changes_diff.selected_rows);
}


fn oid(d: &Path, rev: &str) -> pengitto_core::model::Oid {
    pengitto_core::model::Oid::from_hex(out(d, &["rev-parse", rev]).trim().as_bytes()).unwrap()
}

/// Plans and starts an interactive rebase of the commits after "base"
/// through the tab, `edit` changing the plan.
fn rebase_with(tab: &mut RepoTab, d: &Path, edit: impl FnOnce(&mut pengitto::repo::rebase::RebaseDialog)) {
    tab.open_interactive_rebase(Some(oid(d, "HEAD~3")));
    assert!(poll_until(tab, |t| matches!(t.dialog, Some(Dialog::InteractiveRebase(_)))));
    let Some(Dialog::InteractiveRebase(dlg)) = tab.dialog.take() else { unreachable!() };
    let mut dlg = *dlg;
    edit(&mut dlg);
    tab.start_interactive_rebase(dlg);
}

#[test]
fn undo_only_while_the_branch_is_where_the_rebase_left_it() {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let before = oid(d, "HEAD");
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| dlg.set_action(2, pengitto_core::interactive::Action::Drop));
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_done.is_some()));
    assert!(tab.can_undo_rebase());

    // On another branch: no undo (it would reset that branch).
    out(d, &["checkout", "-q", "-b", "other"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.can_undo_rebase()));
    tab.undo_rebase();
    assert!(tab.dialog.is_none());
    out(d, &["checkout", "-q", "main"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.can_undo_rebase()));

    // Cancelling the reset keeps the undo.
    tab.undo_rebase();
    assert!(matches!(tab.dialog, Some(Dialog::Reset { .. })));
    tab.dialog = None;
    assert!(tab.rebase_done.is_some());

    // The branch moves after the dialog opened: the reset doesn't run.
    tab.undo_rebase();
    let Some(Dialog::Reset { target, mode, lost, guard, .. }) = tab.dialog.take() else { panic!("reset dialog") };
    let mut t = 1_800_500_000;
    std::fs::write(d.join("late"), "x\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "late"], &mut t);
    let moved = oid(d, "HEAD");
    tab.reset(target, mode, lost, guard);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.join("\n").contains("moved"), "{:?}", tab.errors);
    assert_eq!(oid(d, "HEAD"), moved, "nothing reset");
    assert_ne!(target, moved);
    assert_eq!(target, before);
}

#[test]
fn amend_at_an_edit_stop_doesnt_outlive_it() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    tab.commit_subject = "my draft".into();
    rebase_with(tab, d, |dlg| dlg.set_action(1, pengitto_core::interactive::Action::Edit));
    settle(&mut h);
    assert!(h.state_mut().active_tab_mut().unwrap().edit_stop().is_some());
    // Amend from the checkbox; Ctrl+Enter in the subject amends.
    h.get_by_label("Amend").click();
    settle(&mut h);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().commit_subject, "c2", "HEAD's message");
    let subject = h.get_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.value().as_deref() == Some("c2")).unwrap();
    subject.focus();
    subject.type_text(" amended");
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND });
    h.step();
    settle(&mut h);
    assert_eq!(out(d, &["log", "-1", "--format=%s"]).trim(), "c2 amended");
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(!tab.amend, "a commit ends the amend");
    // Armed again, then the rebase goes on: Amend doesn't outlive the stop,
    // and the draft from before comes back.
    tab.set_amend(true);
    settle(&mut h);
    h.get_by_label("Continue").click();
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.state, RepoState::Clean);
    assert!(!tab.amend);
    assert_eq!(tab.commit_subject, "my draft");
    assert_eq!(subjects(d), ["c3", "c2 amended", "c1", "base"]);
}

#[test]
fn an_amend_armed_at_one_edit_stop_isnt_carried_to_the_next() {
    use pengitto_core::interactive::Action;
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, Action::Edit);
        dlg.set_action(2, Action::Edit);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    let first = tab.edit_stop().unwrap();
    tab.commit_subject = "draft".into();
    tab.set_amend(true);
    assert!(poll_until(&mut tab, |t| t.commit_subject == "c2"), "HEAD's message to amend");
    // On to the next stop: the rebase stays in progress, and c3 isn't
    // offered to be amended with c2's message.
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some_and(|s| s != first)));
    assert_eq!(tab.status.state, RepoState::Rebasing);
    assert!(!tab.amend);
    assert_eq!(tab.commit_subject, "draft");
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(subjects(d), ["c3", "c2", "c1", "base"]);
}

#[test]
fn a_failed_abort_keeps_the_session_for_another_try() {
    use pengitto_core::interactive::{Action, read_session};
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let before = oid(d, "HEAD");
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, Action::Edit);
        dlg.set_action(2, Action::Drop);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    // Something else holds the index: git can't go back.
    let lock = d.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    tab.abort(RepoState::Rebasing);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert!(tab.errors.iter().any(|e| e.contains("index.lock")), "{:?}", tab.errors);
    assert_eq!(tab.status.state, RepoState::Rebasing);
    assert!(tab.rebase_session && read_session(&d.join(".git")).unwrap().is_some(), "still pengitto's rebase");
    // Once it's free, the abort goes through and the session ends with it.
    std::fs::remove_file(&lock).unwrap();
    tab.abort(RepoState::Rebasing);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(oid(d, "HEAD"), before);
    assert!(read_session(&d.join(".git")).unwrap().is_none());
    assert!(!tab.rebase_session && tab.rebase_done.is_none(), "nothing to undo");
}

#[test]
fn a_late_range_replaces_no_other_dialog() {
    use common::gated::{Gate, Gates, gated_tab};
    let repo = repo();
    let d = repo.path();
    let (mut tab, gates) = gated_tab(d, Gates { rebase_range: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, idle));
    let running = tab.worker_mut().running_jobs();
    let arrive = |tab: &mut RepoTab, gates: &common::gated::Gates, i: usize| {
        gates.rebase_range.release(i);
        // Over once its result (if it sends one) is on its way to the tab.
        let start = std::time::Instant::now();
        while running.count("rebase-range") > 0 {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tab.poll();
    };
    // Something else came up meanwhile and is showing: a dialog…
    tab.open_interactive_rebase(Some(oid(d, "HEAD~3")));
    assert!(poll_until(&mut tab, |_| gates.rebase_range.arrived() == 1));
    std::fs::write(d.join("f1"), "changed\n").unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, idle));
    tab.open_stash_dialog();
    arrive(&mut tab, &gates, 0);
    assert!(matches!(tab.dialog, Some(Dialog::Stash { .. })), "the stash dialog stays");
    tab.dialog = None;
    // …or the discard confirmation (which isn't one of the dialogs).
    tab.open_interactive_rebase(Some(oid(d, "HEAD~3")));
    assert!(poll_until(&mut tab, |_| gates.rebase_range.arrived() == 2));
    tab.request_discard(tab.status.unstaged.clone());
    assert!(tab.pending_discard.is_some());
    arrive(&mut tab, &gates, 1);
    assert!(tab.dialog.is_none() && tab.pending_discard.is_some(), "the confirmation stays alone");
    tab.pending_discard = None;
    // A dialog came and went meanwhile: the user moved on.
    tab.open_interactive_rebase(Some(oid(d, "HEAD~3")));
    assert!(poll_until(&mut tab, |_| gates.rebase_range.arrived() == 3));
    tab.open_stash_dialog();
    tab.dialog = None;
    arrive(&mut tab, &gates, 2);
    assert!(tab.dialog.is_none(), "no rebase dialog out of the blue");
}

#[test]
fn the_list_takes_keys_only_while_it_has_the_focus() {
    let repo = repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    open_dialog(&mut h, d);
    let actions = |h: &mut Harness<'static, PengittoApp>| -> Vec<&'static str> {
        let Some(Dialog::InteractiveRebase(dlg)) = &h.state_mut().active_tab_mut().unwrap().dialog else { panic!() };
        dlg.plan.items.iter().map(|(_, a)| a.word()).collect()
    };
    // A button has the focus: D does nothing.
    h.get_by_label("Reset").focus();
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: Key::D, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.step();
    assert_eq!(actions(&mut h), ["pick", "pick", "pick"]);
    // Back on the list (a row clicked): D drops the selected commit.
    h.get_by_label(&format!("rebase row {} c3", short(d, "HEAD"))).click();
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: Key::D, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.step();
    assert_eq!(actions(&mut h), ["pick", "pick", "drop"]);

    // A drag released outside the list moves nothing.
    let from = h.get_by_label(&format!("rebase row {} c1", short(d, "HEAD~2"))).rect().center();
    // Beside the list, level with the top row (over that row, it would go
    // there).
    let top = h.get_by_label(&format!("rebase row {} c3", short(d, "HEAD"))).rect();
    let outside = egui::pos2(top.right() + 40.0, top.center().y);
    assert!(h.get_by_label("Message").rect().left() < outside.x, "over the message side");
    let button = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(button(from, true));
    h.step();
    for k in 1..=6 {
        h.input_mut().events.push(Event::PointerMoved(from + (egui::pos2(from.x, 168.0) - from) * (k as f32 / 6.0)));
        h.step();
    }
    h.input_mut().events.push(Event::PointerMoved(outside));
    h.step();
    h.input_mut().events.push(button(outside, false));
    h.step();
    h.run_steps(2);
    let Some(Dialog::InteractiveRebase(dlg)) = &h.state_mut().active_tab_mut().unwrap().dialog else { panic!() };
    let order: Vec<String> = dlg.plan.items.iter().map(|(id, _)| dlg.commit(*id).unwrap().subject.clone()).collect();
    assert_eq!(order, ["c1", "c2", "c3"]);
}

#[test]
fn a_rebase_finished_on_the_command_line_is_noticed_on_refresh() {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, pengitto_core::interactive::Action::Edit);
        dlg.set_action(2, pengitto_core::interactive::Action::Drop);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    let o = Command::new("git").current_dir(d).args(["rebase", "--continue"]).env("GIT_EDITOR", "true").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_done.is_some()));
    assert!(!pengitto_core::interactive::session_dir(&tab.paths.git_dir).exists());
    assert_eq!(subjects(d), ["c2", "c1", "base"]);
}

#[test]
fn stashed_changes_wait_until_really_applied() {
    use pengitto_core::interactive::Action;
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    // Done at once: the changes are back and nothing is left waiting.
    let repo = repo();
    let d = repo.path();
    std::fs::write(d.join("f1"), "local change\n").unwrap();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| dlg.set_action(2, Action::Squash));
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(tab.pending_autostash, None);
    assert_eq!(std::fs::read_to_string(d.join("f1")).unwrap(), "local change\n");
    assert_eq!(out(d, &["stash", "list"]), "");

    // Done at once, but re-applying conflicts (the file is gone): the
    // stash is being applied, not waiting as well.
    let repo = self::repo();
    let d = repo.path();
    std::fs::write(d.join("f2"), "local change\n").unwrap();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| dlg.set_action(1, Action::Drop));
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some() && t.rebase_done.is_some()));
    assert_eq!(tab.pending_autostash, None);
    assert!(tab.stash_conflict.as_ref().unwrap().autostash);

    // Stopped: the changes wait in their stash. Applying them conflicts:
    // no longer waiting; undoing that apply makes them wait again.
    let repo = self::repo();
    let d = repo.path();
    std::fs::write(d.join("f1"), "local change\n").unwrap();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, Action::Edit);
        dlg.set_action(2, Action::Drop);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    let stash = tab.pending_autostash.expect("kept while stopped");
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(tab.pending_autostash, Some(stash));
    std::fs::write(d.join("f1"), "other\n").unwrap();
    out(d, &["commit", "-qam", "other"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.status.has_tracked_changes()));
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));
    assert_eq!(tab.pending_autostash, None, "applied (with conflicts)");
    tab.undo_stash_apply();
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_none()));
    assert_eq!(tab.pending_autostash, Some(stash), "waiting again");
    tab.apply_pending_autostash();
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_some()));
    std::fs::write(d.join("f1"), "resolved\n").unwrap();
    out(d, &["add", "f1"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.is_empty()));
    tab.finish_stash_conflict(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.stash_conflict.is_none()));
    assert_eq!(tab.pending_autostash, None);
    assert_eq!(out(d, &["stash", "list"]), "");
}

#[test]
fn a_rebase_finished_outside_with_commits_since_offers_no_undo() {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, pengitto_core::interactive::Action::Edit);
        dlg.set_action(2, pengitto_core::interactive::Action::Drop);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some()));
    // Finished on the command line, then a commit there before pengitto looks.
    let o = Command::new("git").current_dir(d).args(["rebase", "--continue"]).env("GIT_EDITOR", "true").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    std::fs::write(d.join("later"), "x\n").unwrap();
    out(d, &["add", "-A"]);
    out(d, &["commit", "-qm", "later"]);
    tab.refresh();
    let session = pengitto_core::interactive::session_dir(&tab.paths.git_dir);
    assert!(poll_until(&mut tab, |t| idle(t) && !session.exists()));
    assert!(tab.rebase_done.is_none() && !tab.can_undo_rebase(), "\"later\" isn't undone with it");
    assert!(tab.activity.iter().any(|a| a.contains("ended elsewhere")), "{:?}", tab.activity);
}

#[test]
fn a_session_that_cant_be_read_is_reported_and_kept() {
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    rebase_with(&mut tab, d, |dlg| {
        dlg.set_action(1, pengitto_core::interactive::Action::Edit);
        dlg.set_action(2, pengitto_core::interactive::Action::Drop);
    });
    assert!(poll_until(&mut tab, |t| idle(t) && t.edit_stop().is_some() && t.rebase_session));
    let file = pengitto_core::interactive::session_dir(&tab.paths.git_dir).join("session");
    let good = std::fs::read(&file).unwrap();
    std::fs::write(&file, "head nonsense\n").unwrap();
    tab.refresh();
    let reported = |t: &RepoTab| t.errors.iter().filter(|e| e.contains("Can't read the interactive rebase")).count();
    assert!(poll_until(&mut tab, |t| idle(t) && reported(t) == 1), "{:?}", tab.errors);
    // Reported once (not at every status refresh); nothing tidied away.
    std::fs::write(d.join("noticed"), "x\n").unwrap();
    tab.refresh_status();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "noticed")));
    assert_eq!(reported(&tab), 1);
    assert!(file.exists());
    assert_eq!(tab.status.state, RepoState::Rebasing);
    std::fs::write(&file, good).unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.rebase_session));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean && t.rebase_done.is_some()));
    assert_eq!(subjects(d), ["c2", "c1", "base"]);
}

#[test]
fn the_buttons_stay_on_a_small_screen_whatever_is_listed() {
    use pengitto_core::interactive::Action;
    // Forty commits after "base".
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    for i in 0..=40 {
        std::fs::write(d.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", &if i == 0 { "base".to_owned() } else { format!("c{i}") }], &mut t);
    }
    pengitto::askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let paths = vec![d.to_path_buf()];
    let mut h = Harness::builder().with_size([720.0, 480.0]).build_eframe(move |cc| PengittoApp::new(cc, paths));
    settle(&mut h);
    h.state_mut().active_tab_mut().unwrap().open_interactive_rebase(Some(oid(d, "HEAD~40")));
    settle(&mut h);
    if let Some(Dialog::InteractiveRebase(dlg)) = &mut h.state_mut().active_tab_mut().unwrap().dialog {
        // Twenty combined messages written, then their groups broken up
        // (set aside), and a plan that can't run.
        for anchor in (0..40).step_by(2) {
            dlg.set_action(anchor + 1, Action::Squash);
            let id = dlg.plan.items[anchor].0;
            dlg.set_group_message(id, format!("together {anchor}"));
            dlg.set_action(anchor + 1, Action::Pick);
        }
        dlg.set_action(0, Action::Fixup);
        assert_eq!(dlg.set_aside().len(), 20);
        assert!(!dlg.problems().is_empty());
    } else {
        panic!("the dialog is open");
    }
    settle(&mut h);
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(720.0, 480.0));
    for label in ["Interactive Rebase", "Reset", "Cancel", "Start Rebase"] {
        let r = h.get_by_label(label).rect();
        assert!(screen.contains_rect(r), "{label} at {r:?}");
    }
}
