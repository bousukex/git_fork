//! Quick Launch, the search bar and keyboard handling through the AccessKit
//! tree (M4a).

mod common;

use common::{diverged_repo, sample_repo, settle};
use egui::{Event, ImeEvent, Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable as _};
use pengitto::{app::PengittoApp, repo::View};

fn app(paths: Vec<std::path::PathBuf>) -> Harness<'static, PengittoApp> {
    Harness::builder().with_size([1200.0, 760.0]).build_eframe(move |cc| PengittoApp::new(cc, paths))
}

const CTRL_SHIFT: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: false, command: true };

fn head_name(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["symbolic-ref", "--short", "HEAD"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn type_text(h: &mut Harness<'static, PengittoApp>, text: &str) {
    h.event(Event::Text(text.into()));
    h.run_steps(2);
}

#[test]
fn quick_launch_checks_out_a_branch() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_some(), "Ctrl+P opens Quick Launch");
    type_text(&mut h, "wp");
    let listed: Vec<String> = h.state_mut().quick_launch().unwrap().listed().iter().map(|i| i.label.clone()).collect();
    assert_eq!(listed.first().map(String::as_str), Some("wip"), "{listed:?}");
    h.key_press(Key::Enter);
    settle(&mut h);
    assert!(h.state_mut().quick_launch().is_none());
    assert_eq!(head_name(repo.path()), "wip");
}

#[test]
fn quick_launch_runs_commands_and_keeps_blocked_items() {
    let repo = diverged_repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    h.state_mut()
        .active_tab_mut()
        .unwrap()
        .integrate(pengitto::repo::dialog::Integration::Merge { target: "refs/heads/topic".into(), mode: Default::default() }, false);
    settle(&mut h);
    // A checkout is blocked during the merge: Enter does nothing.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    type_text(&mut h, "topic");
    h.key_press(Key::Enter);
    settle(&mut h);
    assert!(h.state_mut().quick_launch().is_some(), "still open");
    assert_eq!(head_name(d), "main");
    assert!(h.query_by_label_contains("Finish or abort").is_some(), "the reason is shown");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_none());

    // A command runs.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    type_text(&mut h, "local changes");
    h.key_press(Key::Enter);
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().view, View::LocalChanges);
}

#[test]
fn ctrl_shift_p_is_push_not_quick_launch() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(CTRL_SHIFT, Key::P);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_none());
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.errors.last().is_some_and(|e| e.contains("Can't push")), "{:?}", tab.errors);
}

#[test]
fn search_bar_finds_and_steps() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    h.run_steps(2);
    assert!(h.state_mut().active_tab_mut().unwrap().search.open);
    type_text(&mut h, "a");
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let rows = tab.search.rows.clone();
    assert!(rows.len() >= 2, "{rows:?}");
    tab.selected_commit = None;
    h.key_press(Key::Enter); // in the field: next match
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().match_position(), Some(1));
    h.key_press(Key::F3);
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().match_position(), Some(2));
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().match_position(), Some(1));
    assert!(h.state_mut().active_tab_mut().unwrap().search.open, "Enter didn't end the search");

    h.get_by_label("Only matches").click();
    h.run_steps(2);
    let n = rows.len();
    let listed = h.query_all_by_label_contains("commit ").count();
    assert_eq!(listed, n, "only matching commits are listed");

    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(!h.state_mut().active_tab_mut().unwrap().search.open);
}

#[test]
fn ime_composition_blocks_shortcuts() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    let preedit = |t: &str| Event::Ime(ImeEvent::Preedit { text: t.into(), active_range_chars: None });
    h.event(preedit("に"));
    h.run_steps(1);
    // Mid-composition (no IME event in this frame): Ctrl+P is the IME's.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(1);
    assert!(h.state_mut().quick_launch().is_none());
    // The frame that confirms the conversion is still blocked (kittest
    // runs a frame per queued event, so put both into one frame).
    let ctrl_p = Event::Key { key: Key::P, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
    h.input_mut().events.extend([Event::Ime(ImeEvent::Commit("日本".into())), ctrl_p]);
    h.step();
    assert!(h.state_mut().quick_launch().is_none());
    // Afterwards shortcuts work again.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_some());
}

#[test]
fn space_stages_from_the_list_but_not_while_typing() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    let staged = |h: &mut Harness<'static, PengittoApp>| h.state_mut().active_tab_mut().unwrap().status.staged.len();
    // Typing a space in the commit subject stages nothing.
    h.state_mut().active_tab_mut().unwrap().commit_subject.clear();
    let subject = h.get_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.value().is_some_and(|v| v.is_empty())).unwrap();
    subject.focus();
    h.run_steps(1);
    h.key_press(Key::Space);
    h.event(Event::Text(" ".into()));
    settle(&mut h);
    assert_eq!(staged(&mut h), 0);

    // Clicking a file gives the list focus; Space stages it.
    let first = h.state_mut().active_tab_mut().unwrap().status.unstaged[0].path.to_string();
    h.get_by_label(&format!("unstaged file {first}")).click();
    h.run_steps(2);
    h.key_press(Key::Space);
    settle(&mut h);
    assert_eq!(staged(&mut h), 1);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert_eq!(tab.status.staged[0].path.to_string(), first);
}

#[test]
fn view_and_tab_shortcuts() {
    let (a, b) = (sample_repo(), sample_repo());
    let mut h = app(vec![a.path().into(), b.path().into()]);
    settle(&mut h);
    let title = |d: &tempfile::TempDir| d.path().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(h.state().active_title(), Some(title(&b).as_str()));
    h.key_press_modifiers(Modifiers::COMMAND, Key::Tab);
    h.run_steps(2);
    assert_eq!(h.state().active_title(), Some(title(&a).as_str()), "wraps to the first");
    h.key_press_modifiers(CTRL_SHIFT, Key::Tab);
    h.run_steps(2);
    assert_eq!(h.state().active_title(), Some(title(&b).as_str()));
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().view, View::LocalChanges);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num2);
    h.run_steps(2);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().view, View::History);
}

#[test]
fn quick_launch_opens_a_recent_repository_in_the_background() {
    let (a, b) = (sample_repo(), sample_repo());
    let mut h = app(vec![a.path().into()]);
    settle(&mut h);
    // b is a recent repository (opened once, then closed).
    h.state_mut().run_command(pengitto::shortcuts::Command::QuickLaunch);
    h.state_mut().quick_launch().unwrap();
    // Keys of the frame in which it takes the keyboard aren't its own.
    h.run_steps(1);
    h.key_press(Key::Escape);
    h.run_steps(1);
    let canon = b.path().canonicalize().unwrap();
    h.state_mut().remember_recent(canon.clone());
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    let name = canon.file_name().unwrap().to_string_lossy().into_owned();
    type_text(&mut h, &name);
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().tab_count(), 2);
    assert_eq!(h.state().active_title(), Some(name.as_str()));
}


fn commit_count(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["rev-list", "--count", "HEAD"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn key_in_frame(h: &mut Harness<'static, PengittoApp>, events: Vec<Event>) {
    h.input_mut().events.extend(events);
    h.step();
}

fn press(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

#[test]
fn ctrl_enter_commits_only_from_the_commit_box() {
    let repo = sample_repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    {
        let tab = h.state_mut().active_tab_mut().unwrap();
        let all = tab.list(pengitto::repo::Side::Unstaged);
        tab.stage(all);
    }
    settle(&mut h);
    h.state_mut().active_tab_mut().unwrap().commit_subject = "from the box".into();
    let before = commit_count(d);
    let ctrl_enter = || press(Key::Enter, Modifiers::COMMAND);

    // A dialog is open.
    h.key_press_modifiers(CTRL_SHIFT, Key::B);
    h.run_steps(2);
    assert!(h.state_mut().active_tab_mut().unwrap().dialog.is_some());
    key_in_frame(&mut h, vec![ctrl_enter()]);
    settle(&mut h);
    assert_eq!(commit_count(d), before, "not from a dialog");
    h.state_mut().active_tab_mut().unwrap().dialog = None;
    h.run_steps(2);

    // Quick Launch is open.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    key_in_frame(&mut h, vec![ctrl_enter()]);
    settle(&mut h);
    assert_eq!(commit_count(d), before, "not from Quick Launch");
    h.key_press(Key::Escape);
    h.run_steps(2);

    // Typing in the sidebar filter.
    let filter = pengitto::ui::sidebar::filter_id(h.state_mut().active_tab_mut().unwrap());
    h.ctx.memory_mut(|m| m.request_focus(filter));
    h.run_steps(1);
    key_in_frame(&mut h, vec![ctrl_enter()]);
    settle(&mut h);
    assert_eq!(commit_count(d), before, "not from the filter");

    // In the commit box: commits.
    let (subject, _) = pengitto::ui::changes::commit_field_ids(h.state_mut().active_tab_mut().unwrap());
    h.ctx.memory_mut(|m| m.request_focus(subject));
    h.run_steps(1);
    key_in_frame(&mut h, vec![ctrl_enter()]);
    settle(&mut h);
    assert_ne!(commit_count(d), before);
}

#[test]
fn a_discard_confirmation_owns_the_keyboard() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    {
        let tab = h.state_mut().active_tab_mut().unwrap();
        let files = tab.list(pengitto::repo::Side::Unstaged);
        tab.request_discard(files);
    }
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_none());
}

#[test]
fn list_keys_stage_unstage_and_ask_to_discard() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    h.state_mut().show_local_changes();
    settle(&mut h);
    let first = h.state_mut().active_tab_mut().unwrap().status.unstaged[0].path.to_string();
    h.get_by_label(&format!("unstaged file {first}")).click();
    h.run_steps(2);
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(h.state_mut().active_tab_mut().unwrap().status.staged.len(), 1);
    h.get_by_label(&format!("staged file {first}")).click();
    h.run_steps(2);
    h.key_press(Key::Space);
    settle(&mut h);
    assert!(h.state_mut().active_tab_mut().unwrap().status.staged.is_empty(), "unstaged again");
    let other = h.state_mut().active_tab_mut().unwrap().status.unstaged[0].path.to_string();
    h.get_by_label(&format!("unstaged file {other}")).click();
    h.run_steps(2);
    h.key_press(Key::Delete);
    h.run_steps(2);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.pending_discard.is_some(), "Delete asks first");
    assert_eq!(tab.status.unstaged.len(), 2, "nothing discarded yet");
}

#[test]
fn ime_frames_neither_close_nor_confirm_modals() {
    let repo = sample_repo();
    let d = repo.path();
    let mut h = app(vec![d.into()]);
    settle(&mut h);
    let commit = || Event::Ime(ImeEvent::Commit("日本".into()));
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    key_in_frame(&mut h, vec![commit(), press(Key::Escape, Modifiers::NONE)]);
    h.run_steps(1);
    assert!(h.state_mut().quick_launch().is_some(), "Esc that ended the conversion doesn't close it");
    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_none());

    h.key_press_modifiers(CTRL_SHIFT, Key::B);
    h.run_steps(2);
    type_text(&mut h, "from-dialog");
    key_in_frame(&mut h, vec![commit(), press(Key::Enter, Modifiers::NONE)]);
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.dialog.is_some(), "Enter that ended the conversion doesn't confirm");
    h.key_press(Key::Enter);
    settle(&mut h);
    // The conversion's text went into the name field, as it should.
    assert!(h.state_mut().active_tab_mut().unwrap().refs.refs.iter().any(|r| r.short_name == "from-dialog日本"));
}

#[test]
fn quick_launch_uses_the_text_typed_with_enter() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    key_in_frame(&mut h, vec![Event::Text("wip".into()), press(Key::Enter, Modifiers::NONE)]);
    settle(&mut h);
    assert_eq!(head_name(repo.path()), "wip", "the top result for the new text, not the old first item");
}

#[test]
fn quick_launch_acts_on_the_repository_it_was_opened_for() {
    let (a, b) = (sample_repo(), sample_repo());
    let mut h = app(vec![a.path().into()]);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    type_text(&mut h, "wip");
    // Meanwhile another repository finishes opening and becomes active.
    let ctx = h.ctx.clone();
    h.state_mut().open(&b.path().canonicalize().unwrap(), &ctx);
    settle(&mut h);
    assert_eq!(h.state().tab_count(), 2);
    let b_title = b.path().canonicalize().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(h.state().active_title(), Some(b_title.as_str()));
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(head_name(a.path()), "wip", "checked out where Quick Launch was opened");
    assert_eq!(head_name(b.path()), "main");
    let a_title = a.path().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(h.state().active_title(), Some(a_title.as_str()));
}

#[test]
fn revealing_a_tag_shows_it_even_with_only_matches() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    {
        let tab = h.state_mut().active_tab_mut().unwrap();
        tab.open_search();
        tab.search.query = "readme".into();
        tab.search.only_matches = true;
        tab.search_changed();
    }
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    type_text(&mut h, "v0.1.0");
    h.key_press(Key::Enter);
    settle(&mut h);
    let tab = h.state_mut().active_tab_mut().unwrap();
    let tag = tab.refs.refs.iter().find(|r| r.short_name == "v0.1.0").unwrap().target;
    assert_eq!(tab.selected_commit, Some(tag));
    assert!(!tab.search.only_matches, "the filter makes way for the revealed commit");
    let row = format!("commit {} Add main.rs", tag.short());
    assert!(h.query_by_label(&row).is_some(), "and it is listed");
}
