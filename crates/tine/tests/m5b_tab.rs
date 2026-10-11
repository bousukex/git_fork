//! The merge editor in a tab (M5b): opening conflicts of each operation,
//! saving and marking resolved, leaving with unsaved changes, files
//! changed meanwhile, and results that arrive late.

mod common;

use std::{path::Path, process::Command};

use common::{git, idle, poll_until};
use tine::{
    merge_model::MergeSession,
    repo::{
        RepoTab,
        dialog::Dialog,
        merge::{MergeLeave, MergeState},
    },
};
use tine_core::{merge_file::Choice, model::RepoState};

fn run(d: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(d)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_EDITOR", "true")
        .output()
        .unwrap()
}

fn out(d: &Path, args: &[&str]) -> String {
    let o = run(d, args);
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// f.txt changed in two places and g.txt in one, differently on main and
/// on topic (main checked out).
fn diverged() -> tempfile::TempDir {
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
    dir
}

fn open_tab(d: &Path) -> RepoTab {
    let mut tab = RepoTab::open(d, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading));
    tab
}

/// A tab on `d` with `path` opened in the merge editor.
fn editing(tab: &mut RepoTab, path: &str) {
    tab.refresh();
    assert!(poll_until(tab, |t| idle(t) && t.status.conflicted.iter().any(|f| f.path == path)));
    tab.open_merge_editor(path.into());
    assert!(poll_until(tab, |t| t.merge.as_ref().is_some_and(|m| !matches!(m.state, MergeState::Loading))));
    assert!(tab.merge_shown());
}

fn session(tab: &mut RepoTab) -> &mut MergeSession {
    match tab.merge.as_mut().map(|m| &mut m.state) {
        Some(MergeState::Ready(s)) => s,
        Some(MergeState::Failed { message, .. }) => panic!("{message}"),
        _ => panic!("no merge editor"),
    }
}

#[test]
fn a_merge_resolved_in_the_editor_is_committed() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    let s = session(&mut tab);
    assert_eq!(s.conflict_count(), 2);
    assert!(s.opened.sides.ours.starts_with("HEAD — main"), "{}", s.opened.sides.ours);
    assert!(s.opened.sides.theirs.contains("topic change"), "{}", s.opened.sides.theirs);
    assert_eq!(s.current, Some(0), "the first conflict, in view");
    s.choose(0, Choice::Theirs);
    assert!(tab.merge_save_block(true).is_some(), "one left");
    tab.merge_next(false);
    assert_eq!(session(&mut tab).current, Some(1));
    session(&mut tab).choose(1, Choice::OursThenTheirs);
    assert_eq!(tab.merge_save_block(true), None);
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none() && t.status.conflicted.iter().all(|f| f.path != "f.txt")));
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "1\nA topic\n2\n3\n4\nB main\nB topic\n5\n");
    // g.txt too, then the merge commit.
    editing(&mut tab, "g.txt");
    session(&mut tab).choose(0, Choice::Ours);
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none() && t.status.conflicted.is_empty()));
    tab.commit();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(out(d, &["log", "-1", "--format=%P"]).split_whitespace().count(), 2);
    assert_eq!(out(d, &["show", "HEAD:g.txt"]), "x main\n");
}

#[test]
fn each_operation_names_its_sides() {
    // Rebasing topic onto main: ours is what is rebased so far.
    let repo = diverged();
    let d = repo.path();
    out(d, &["checkout", "-q", "topic"]);
    assert!(!run(d, &["rebase", "main"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "g.txt");
    let s = session(&mut tab);
    assert!(s.opened.sides.ours.starts_with("Rebased so far onto"), "{}", s.opened.sides.ours);
    assert!(s.opened.sides.theirs.starts_with("Being applied — ") && s.opened.sides.theirs.contains("topic change"), "{}", s.opened.sides.theirs);
    s.choose(0, Choice::Theirs);
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none() && t.status.conflicted.iter().all(|f| f.path != "g.txt")));
    editing(&mut tab, "f.txt");
    let s = session(&mut tab);
    s.choose(0, Choice::Theirs);
    s.choose(1, Choice::Theirs);
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none() && t.status.conflicted.is_empty()));
    tab.continue_op();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.state == RepoState::Clean));
    assert_eq!(out(d, &["show", "HEAD:g.txt"]), "x topic\n");

    // Cherry-picking topic's commit.
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["cherry-pick", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "g.txt");
    let s = session(&mut tab);
    assert!(s.opened.sides.theirs.starts_with("Being picked — "), "{}", s.opened.sides.theirs);
    assert_eq!(s.opened.state, RepoState::CherryPicking);

    // Reverting a commit a later one built on.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    for (content, msg) in [("a\n", "base"), ("A1\n", "first"), ("A2\n", "second")] {
        std::fs::write(d.join("f.txt"), content).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", msg], &mut t);
    }
    assert!(!run(d, &["revert", "--no-edit", "HEAD~"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    let s = session(&mut tab);
    assert!(s.opened.sides.base.starts_with("The commit being reverted — ") && s.opened.sides.base.contains("first"), "{}", s.opened.sides.base);
    assert!(s.opened.sides.theirs.starts_with("Before the commit being reverted"), "{}", s.opened.sides.theirs);
    let Some(tine_core::merge_file::Segment::Conflict(c)) = s.opened.merge.segments.first() else { panic!() };
    assert_eq!((c.ours.as_slice(), c.base.as_slice(), c.theirs.as_slice()), (&b"A2\n"[..], &b"A1\n"[..], &b"a\n"[..]));
}

#[test]
fn unsaved_changes_are_asked_about_before_leaving() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    // Nothing changed: closes at once.
    tab.request_close_merge();
    assert!(tab.merge.is_none() && tab.dialog.is_none());
    editing(&mut tab, "f.txt");
    session(&mut tab).choose(0, Choice::Ours);
    assert!(tab.merge_dirty());
    // Close, another file, the tab: each asks first.
    tab.request_close_merge();
    assert!(matches!(&tab.dialog, Some(Dialog::LeaveMerge { then: MergeLeave::Close })));
    tab.dialog = None;
    tab.open_merge_editor("g.txt".into());
    assert!(matches!(&tab.dialog, Some(Dialog::LeaveMerge { then: MergeLeave::Open(p) }) if p == "g.txt"));
    assert_eq!(tab.merge.as_ref().unwrap().path, "f.txt", "still the same file");
    tab.dialog = None;
    assert!(!tab.request_close());
    assert!(matches!(&tab.dialog, Some(Dialog::LeaveMerge { then: MergeLeave::CloseTab })));
    tab.dialog = None;
    // History and back, or another file's diff: kept as it was.
    tab.view = tine::repo::View::History;
    tab.view = tine::repo::View::LocalChanges;
    let g = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "g.txt").unwrap();
    tab.click_change(tine::repo::Side::Unstaged, &g, false, false);
    assert!(!tab.merge_shown() && tab.merge_dirty());
    tab.open_merge_editor("f.txt".into());
    assert!(tab.merge_shown());
    assert_eq!(session(&mut tab).choices[0], Choice::Ours);
    // Given up: then what was asked.
    tab.leave_merge(MergeLeave::Open("g.txt".into()));
    assert!(poll_until(&mut tab, |t| t.merge.as_ref().is_some_and(|m| m.path == "g.txt" && m.session().is_some())));
    session(&mut tab).choose(0, Choice::Theirs);
    tab.leave_merge(MergeLeave::CloseTab);
    assert!(tab.close_requested && tab.merge.is_none());
    assert!(!d.join("f.txt").read_link().is_ok(), "nothing written");
    assert!(std::fs::read_to_string(d.join("f.txt")).unwrap().contains("<<<<<<<"));
}

#[test]
fn a_file_changed_meanwhile_isnt_overwritten() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    session(&mut tab).choose(0, Choice::Ours);
    // Edited in another editor.
    std::fs::write(d.join("f.txt"), "elsewhere\n").unwrap();
    tab.save_merge(false);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.merge.as_ref().unwrap().saving));
    assert!(tab.errors.iter().any(|e| e.contains("changed outside the editor")), "{:?}", tab.errors);
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "elsewhere\n");
    assert!(tab.merge_dirty(), "the edits are kept");

    // Resolved elsewhere while open: closes if nothing is unsaved…
    tab.leave_merge(MergeLeave::Close);
    editing(&mut tab, "g.txt");
    out(d, &["add", "g.txt"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none()));
    // …else says so (and saving is refused).
    out(d, &["checkout", "-m", "--", "g.txt"]);
    editing(&mut tab, "g.txt");
    session(&mut tab).choose(0, Choice::Theirs);
    out(d, &["add", "g.txt"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.as_ref().unwrap().note.as_deref().is_some_and(|n| n.contains("isn't conflicted any more"))));
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.merge.as_ref().unwrap().saving));
    assert!(tab.errors.iter().any(|e| e.contains("conflict changed")), "{:?}", tab.errors);
}

#[test]
fn saving_unresolved_or_failing_to_mark_resolved_keeps_the_editor() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    session(&mut tab).choose(0, Choice::Theirs);
    // Saved with a conflict left: still conflicted, and saved again later.
    tab.save_merge(false);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.merge.as_ref().unwrap().saving));
    assert!(!tab.merge_dirty());
    let text = std::fs::read_to_string(d.join("f.txt")).unwrap();
    assert!(text.starts_with("1\nA topic\n2\n3\n4\n<<<<<<< HEAD\nB main\n"), "{text}");
    assert!(tab.merge.as_ref().unwrap().note.as_deref().unwrap().contains("stays conflicted"));
    session(&mut tab).choose(1, Choice::Ours);
    let lock = d.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.merge.as_ref().unwrap().saving));
    let note = tab.merge.as_ref().unwrap().note.clone().unwrap();
    assert!(note.contains("marking it resolved failed") && note.contains("index.lock"), "{note}");
    assert!(!tab.merge_dirty(), "saved");
    std::fs::remove_file(&lock).unwrap();
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none()), "{:?} {:?}", tab.errors, tab.merge.as_ref().map(|m| &m.note));
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "1\nA topic\n2\n3\n4\nB main\n5\n");
}

#[test]
fn the_result_edited_as_text() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "f.txt");
    session(&mut tab).choose(0, Choice::Ours);
    tab.edit_merge_result();
    let Some(Dialog::EditMergeResult { editor, text, .. }) = tab.dialog.take() else { panic!("the editor opens") };
    assert!(text.starts_with("1\nA main\n2\n"), "{text}");
    // The second conflict left in, the first rewritten by hand.
    tab.apply_merge_edit(editor, &text.replace("A main", "A by hand"));
    let s = session(&mut tab);
    assert_eq!((s.conflict_count(), s.unresolved()), (1, 1));
    assert!(tab.merge_save_block(true).is_some());
    session(&mut tab).choose(0, Choice::TheirsThenOurs);
    tab.save_merge(true);
    assert!(poll_until(&mut tab, |t| idle(t) && t.merge.is_none()));
    assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "1\nA by hand\n2\n3\n4\nB topic\nB main\n5\n");
}

#[test]
fn files_not_for_the_editor_say_why() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("bin"), b"a\0").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    git(d, &["checkout", "-q", "-b", "topic"], &mut t);
    std::fs::write(d.join("bin"), b"b\0").unwrap();
    git(d, &["commit", "-qam", "topic"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("bin"), b"c\0").unwrap();
    git(d, &["commit", "-qam", "main"], &mut t);
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "bin");
    let Some(MergeState::Failed { message, unsupported }) = tab.merge.as_ref().map(|m| &m.state) else { panic!() };
    assert!(*unsupported && message.contains("binary"), "{message}");
}

#[test]
fn late_results_go_nowhere() {
    use common::gated::{Gate, Gates, gated_tab};
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let (mut tab, gates) = gated_tab(d, Gates { merge_open: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.len() == 2));
    let running = tab.worker_mut().running_jobs();
    let wait_done = |running: &std::sync::Arc<tine::worker::RunningJobs>| {
        let start = std::time::Instant::now();
        while running.count("merge-open") > 0 {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    // f.txt asked for, then g.txt: f.txt's result is dropped.
    tab.open_merge_editor("f.txt".into());
    assert!(poll_until(&mut tab, |_| gates.merge_open.arrived() == 1));
    tab.open_merge_editor("g.txt".into());
    assert!(poll_until(&mut tab, |_| gates.merge_open.arrived() == 2));
    gates.merge_open.release(1);
    assert!(poll_until(&mut tab, |t| t.merge.as_ref().unwrap().session().is_some()));
    gates.merge_open.release(0);
    wait_done(&running);
    tab.poll();
    let m = tab.merge.as_ref().unwrap();
    assert_eq!((m.path.as_slice(), m.session().unwrap().opened.path.as_slice()), (&b"g.txt"[..], &b"g.txt"[..]));
    assert_eq!(gates.stopped_merges.load(std::sync::atomic::Ordering::Relaxed), 1, "f.txt's was stopped");
    // Closed while loading: stopped, nothing comes.
    tab.request_close_merge();
    tab.open_merge_editor("f.txt".into());
    assert!(poll_until(&mut tab, |_| gates.merge_open.arrived() == 3));
    tab.request_close_merge();
    assert!(tab.merge.is_none());
    gates.merge_open.release(2);
    wait_done(&running);
    tab.poll();
    assert!(tab.merge.is_none());
    assert_eq!(gates.stopped_merges.load(std::sync::atomic::Ordering::Relaxed), 2, "stopped, git never ran");
}

#[test]
fn a_save_from_an_earlier_opening_changes_nothing_now() {
    use tine::worker::{FollowUp, MergeSaved, Msg, OpKind, OpOutcome};
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "g.txt");
    let earlier = tab.merge.as_ref().unwrap().id();
    tab.request_close_merge();
    editing(&mut tab, "g.txt");
    session(&mut tab).choose(0, Choice::Theirs);
    let (edit_gen, work) = {
        let s = session(&mut tab);
        (s.edit_gen, s.work.clone())
    };
    // The earlier opening's save (same file, same change count) arrives.
    let saved = MergeSaved { editor: earlier, path: "g.txt".into(), edit_gen, work, staged: Some(Ok(())) };
    let out = OpOutcome::new("Resolved g.txt").with(FollowUp::MergeSaved(Box::new(saved)));
    tab.deliver(Msg::OpDone(999, OpKind::SaveMerge { resolved: true, editor: earlier }, Ok(out)));
    assert!(tab.merge.is_some() && tab.merge_dirty(), "still open, still unsaved");
    tab.deliver(Msg::OpDone(1000, OpKind::SaveMerge { resolved: true, editor: earlier }, Err(anyhow::anyhow!("x"))));
    assert!(tab.merge.is_some());
}

#[test]
fn text_being_edited_keeps_the_editor() {
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let mut tab = open_tab(d);
    editing(&mut tab, "g.txt");
    tab.edit_merge_result();
    let Some(Dialog::EditMergeResult { text, .. }) = &mut tab.dialog else { panic!() };
    *text = "typed\n".into();
    assert!(tab.merge_dirty(), "something typed counts");
    // Resolved elsewhere meanwhile: the editor stays for the typed text.
    out(d, &["add", "g.txt"]);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.iter().all(|f| f.path != "g.txt")));
    assert!(tab.merge.is_some());
    // Saving stops editing as text from starting.
    let mut tab2 = open_tab(d);
    editing(&mut tab2, "f.txt");
    session(&mut tab2).choose(0, Choice::Ours);
    tab2.save_merge(false);
    tab2.edit_merge_result();
    assert!(tab2.dialog.is_none(), "not while saving");
}

#[test]
fn settings_changed_while_opening_are_used() {
    use common::gated::{Gate, Gates, gated_tab};
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let (mut tab, gates) = gated_tab(d, Gates { merge_open: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.len() == 2));
    let opts = tab.doc_opts;
    tab.open_merge_editor("f.txt".into());
    assert!(poll_until(&mut tab, |_| gates.merge_open.arrived() == 1));
    tab.set_doc_options(tine::settings::DocOptions { tab_width: 8, ..opts });
    gates.merge_open.release(0);
    assert!(poll_until(&mut tab, |t| t.merge.as_ref().unwrap().session().is_some()));
    assert_eq!(session(&mut tab).tab_width, 8);
}

#[test]
fn a_result_already_sent_for_an_earlier_opening_is_dropped() {
    use common::gated::{Gate, Gates, gated_tab};
    let repo = diverged();
    let d = repo.path();
    assert!(!run(d, &["merge", "topic"]).status.success());
    let (mut tab, gates) = gated_tab(d, Gates { merge_open: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.conflicted.len() == 2));
    let running = tab.worker_mut().running_jobs();
    // f.txt's result is on its way (sent, not taken in) when g.txt opens.
    tab.open_merge_editor("f.txt".into());
    assert!(poll_until(&mut tab, |_| gates.merge_open.arrived() == 1));
    gates.merge_open.release(0);
    let start = std::time::Instant::now();
    while running.count("merge-open") > 0 {
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    tab.open_merge_editor("g.txt".into());
    tab.poll();
    assert!(matches!(tab.merge.as_ref().unwrap().state, MergeState::Loading), "f.txt's isn't taken for g.txt");
    gates.merge_open.release(1);
    assert!(poll_until(&mut tab, |t| t.merge.as_ref().unwrap().session().is_some()));
    assert_eq!(session(&mut tab).opened.path, "g.txt");
}
