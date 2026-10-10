//! File History and Blame through RepoTab (M4b).

mod common;

use bstr::BString;
use common::{git, idle, poll_until, sample_repo};
use tine::repo::{RepoTab, browse::Browse};
use tine_core::model::{ChangeKind, ChangedFile};

fn open(dir: &std::path::Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading));
    tab
}

fn history_view(tab: &RepoTab) -> &tine::repo::browse::FileHistoryView {
    match tab.browse.last() {
        Some(Browse::FileHistory(v)) => v,
        _ => panic!("not a file history"),
    }
}

fn blame_view(tab: &RepoTab) -> &tine::repo::browse::BlameView {
    match tab.browse.last() {
        Some(Browse::Blame(v)) => v,
        _ => panic!("not a blame"),
    }
}

fn loaded_history(tab: &mut RepoTab) -> Vec<String> {
    assert!(poll_until(tab, |t| !history_view(t).loading));
    history_view(tab).entries.iter().map(|e| e.commit.subject.clone()).collect()
}

fn loaded_blame(tab: &mut RepoTab) -> std::sync::Arc<tine::blame_model::BlameDoc> {
    assert!(poll_until(tab, |t| blame_view(t).doc.is_some()));
    blame_view(tab).doc.clone().unwrap().expect("blamed")
}

#[test]
fn file_history_shows_diffs_in_its_own_pane() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    let head = tab.head_commit().unwrap();
    let before_commit_diff = tab.commit_diff.file.clone();
    tab.open_file_history(head, "README.md".into(), None);
    let subjects = loaded_history(&mut tab);
    assert_eq!(subjects, ["Document usage in README", "Initial commit"], "{subjects:?}");
    // The newest change is selected and its diff loads in the browse pane.
    assert!(poll_until(&mut tab, |t| t.browse_diff.doc.is_some()));
    assert_eq!(history_view(&tab).selected, Some(0));
    tab.select_history_entry(1);
    assert!(poll_until(&mut tab, |t| t.browse_diff.doc.as_ref().is_some_and(|d| d.as_ref().is_ok_and(|d| d.added > 0))));
    assert_eq!(tab.browse_diff.file.as_ref().unwrap().kind, ChangeKind::Added, "the initial commit added it");
    assert_eq!(tab.commit_diff.file, before_commit_diff, "the history view's diff is untouched");
}

#[test]
fn history_of_a_deleted_file_starts_before_the_deletion() {
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_900_000_000;
    git(d, &["rm", "-q", "lib.rs"], &mut t);
    git(d, &["commit", "-qm", "remove lib"], &mut t);
    let mut tab = open(d);
    let delete = tab.history.commits.iter().find(|c| c.subject == "remove lib").unwrap().clone();
    let file = ChangedFile { path: "lib.rs".into(), orig_path: None, kind: ChangeKind::Deleted, is_submodule: false };
    tab.file_history_of_commit_file(delete.id, delete.parents.first().copied(), &file);
    let subjects = loaded_history(&mut tab);
    assert!(subjects.contains(&"Add lib.rs with add()".to_owned()), "{subjects:?}");
    assert!(history_view(&tab).note.as_deref().unwrap().contains("before its deletion"));
    // Blame of the deleted file: the version before.
    tab.blame_of_commit_file(delete.id, delete.parents.first().copied(), &file);
    let doc = loaded_blame(&mut tab);
    assert_eq!(doc.text[0], "pub fn add(a: i32, b: i32) -> i32 {");
}

#[test]
fn browse_stack_goes_back_and_show_commit_leaves_it() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    let head = tab.head_commit().unwrap();
    tab.open_file_history(head, "main.rs".into(), None);
    loaded_history(&mut tab);
    let newest = history_view(&tab).entries[0].commit.id;
    tab.open_blame(Some(newest), "main.rs".into(), None);
    let doc = loaded_blame(&mut tab);
    let changed = doc.blame.lines.iter().position(|l| doc.blame.origins[l.origin].previous.is_some()).expect("a line with history");
    tab.blame_previous(changed);
    let older = loaded_blame(&mut tab);
    assert_ne!(older.blame.commits, doc.blame.commits, "an older revision");
    assert_eq!(blame_view(&tab).selected, Some(doc.blame.lines[changed].orig_line as usize - 1), "the line stays in view");
    assert_eq!(tab.browse.len(), 3);

    tab.close_browse();
    assert!(matches!(tab.browse.last(), Some(Browse::Blame(_))));
    tab.close_browse();
    assert!(matches!(tab.browse.last(), Some(Browse::FileHistory(_))));
    assert!(poll_until(&mut tab, |t| t.browse_diff.doc.is_some()), "the history's diff comes back");

    let commit = doc.blame.commits[0].id;
    tab.show_commit(commit);
    assert!(tab.browse.is_empty());
    assert_eq!(tab.selected_commit, Some(commit));
}

#[test]
fn work_tree_blame_follows_edits() {
    let repo = sample_repo();
    let d = repo.path();
    let mut tab = open(d);
    tab.blame_of_local("lib.rs".into());
    assert_eq!(blame_view(&tab).rev, None, "the work tree's version");
    let first = loaded_blame(&mut tab);
    assert!(first.blame.lines.iter().all(|l| !first.blame.commit_of(l).is_uncommitted()));
    std::fs::write(d.join("lib.rs"), "// new first line\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    assert!(poll_until(&mut tab, |t| blame_view(t).doc.as_ref().is_some_and(|d| d.as_ref().is_ok_and(|d| d.blame.lines.len() == 4))));
    let doc = loaded_blame(&mut tab);
    assert!(doc.blame.commit_of(&doc.blame.lines[0]).is_uncommitted());
    assert_eq!(doc.text[0], "// new first line");
}

#[test]
fn local_files_without_history_cannot_be_browsed() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    let untracked = ChangedFile { path: "new.txt".into(), orig_path: None, kind: ChangeKind::Untracked, is_submodule: false };
    assert_eq!(tab.local_browse_block(&untracked), Some("Not committed yet"));
    let modified = ChangedFile { path: "README.md".into(), orig_path: None, kind: ChangeKind::Modified, is_submodule: false };
    assert!(poll_until(&mut tab, |t| t.head_paths.is_some()), "HEAD's paths are listed");
    assert_eq!(tab.local_browse_block(&modified), None);
    // A new file, staged and then edited again, shows as "modified" in the
    // unstaged list but has no history yet.
    let d = repo.path();
    let mut t = 1_950_000_000;
    std::fs::write(d.join("fresh.txt"), "1\n").unwrap();
    git(d, &["add", "fresh.txt"], &mut t);
    std::fs::write(d.join("fresh.txt"), "2\n").unwrap();
    let fresh = ChangedFile { path: "fresh.txt".into(), orig_path: None, kind: ChangeKind::Modified, is_submodule: false };
    assert_eq!(tab.local_browse_block(&fresh), Some("Not in the last commit"));
    // The new name of a staged rename neither.
    git(d, &["mv", "main.rs", "renamed.rs"], &mut t);
    let renamed = ChangedFile { path: "renamed.rs".into(), orig_path: Some("main.rs".into()), kind: ChangeKind::Renamed, is_submodule: false };
    assert_eq!(tab.local_browse_block(&renamed), Some("Not in the last commit"));
    git(d, &["mv", "renamed.rs", "main.rs"], &mut t);
    // A missing work-tree file is blamed at HEAD.
    std::fs::remove_file(repo.path().join("README.md")).unwrap();
    tab.blame_of_local(BString::from("README.md"));
    assert_eq!(blame_view(&tab).rev, tab.head_commit());
    loaded_blame(&mut tab);
}

#[test]
fn tracked_files_for_quick_launch() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    tab.request_files();
    assert!(poll_until(&mut tab, |t| t.files.is_some()));
    let files = tab.files.clone().unwrap();
    let mut names: Vec<String> = files.paths.iter().map(|p| p.to_string()).collect();
    names.sort();
    assert_eq!(names, ["README.md", "lib.rs", "main.rs"]);
    // Asking again without changes reuses the list.
    tab.request_files();
    assert!(std::sync::Arc::ptr_eq(&files, tab.files.as_ref().unwrap()));
}

use common::gated::{Gate, Gates, gated_tab};

fn wait_arrived(tab: &mut RepoTab, gate: &Gate, n: usize) {
    assert!(poll_until(tab, |_| gate.arrived() >= n), "{} of {n} calls arrived", gate.arrived());
}

#[test]
fn an_older_blame_arriving_last_is_dropped() {
    let repo = sample_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates { blame: Gate::closed(), ..Default::default() });
    tab.open_blame(None, "README.md".into(), None);
    wait_arrived(&mut tab, &gates.blame, 1);
    let first = blame_view(&tab).cancel_token();
    tab.set_blame_whitespace(true);
    assert!(first.is_cancelled(), "the old request is stopped");
    wait_arrived(&mut tab, &gates.blame, 2);
    // The new one finishes first, then the old one.
    gates.blame.release(1);
    let doc = loaded_blame(&mut tab);
    gates.blame.release(0);
    std::thread::sleep(std::time::Duration::from_millis(200));
    tab.poll();
    let after = blame_view(&tab).doc.clone().unwrap().expect("still the new result");
    assert!(std::sync::Arc::ptr_eq(&doc, &after));
    assert!(blame_view(&tab).ignore_whitespace);
}

#[test]
fn results_for_closed_views_are_dropped() {
    let repo = sample_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates { blame: Gate::closed(), file_history: Gate::closed(), ..Default::default() });
    let head = tab.head_commit().unwrap();
    tab.open_file_history(head, "README.md".into(), None);
    tab.open_blame(Some(head), "README.md".into(), None);
    wait_arrived(&mut tab, &gates.blame, 1);
    wait_arrived(&mut tab, &gates.file_history, 1);
    let (history, blame) = match (&tab.browse[0], &tab.browse[1]) {
        (Browse::FileHistory(h), Browse::Blame(b)) => (h.cancel_token(), b.cancel_token()),
        _ => unreachable!(),
    };
    tab.clear_browse();
    assert!(history.is_cancelled() && blame.is_cancelled());
    gates.blame.open();
    gates.file_history.open();
    std::thread::sleep(std::time::Duration::from_millis(200));
    tab.poll();
    assert!(tab.browse.is_empty() && tab.browse_diff.file.is_none(), "nothing came back to life");
}

#[test]
fn file_listing_is_redone_after_changes_and_failures() {
    let repo = sample_repo();
    let d = repo.path();
    let (mut tab, gates) = gated_tab(d, Gates { files: Gate::closed(), ..Default::default() });
    tab.request_files();
    wait_arrived(&mut tab, &gates.files, 1);
    let token = tab.files_cancel_token().unwrap();
    // The work tree changes while listing: that listing is dropped.
    std::fs::write(d.join("added.txt"), "x\n").unwrap();
    let mut t = 1_950_000_000;
    git(d, &["add", "added.txt"], &mut t);
    tab.refresh_status();
    assert!(token.is_cancelled());
    gates.files.release(0);
    std::thread::sleep(std::time::Duration::from_millis(200));
    tab.poll();
    assert!(tab.files.is_none(), "the outdated listing is not used");
    // A failure leaves it stale, so the next request tries again.
    tab.worker_mut().fail_spawns_for_test();
    tab.request_files();
    assert!(poll_until(&mut tab, |t| !t.errors.is_empty()));
    assert!(tab.files.is_none());
    tab.worker_mut().allow_spawns_for_test();
    gates.files.open();
    tab.request_files();
    assert!(poll_until(&mut tab, |t| t.files.is_some()));
    assert!(tab.files.as_ref().unwrap().paths.iter().any(|p| p == "added.txt"));
}

#[test]
fn a_diff_for_old_options_is_dropped() {
    let repo = sample_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates { diff: Gate::closed(), ..Default::default() });
    let head = tab.head_commit().unwrap();
    tab.open_file_history(head, "README.md".into(), None);
    assert!(poll_until(&mut tab, |t| !history_view(t).loading));
    // The newest entry's diff is on its way; the options change meanwhile.
    let before = gates.diff.arrived();
    assert!(poll_until(&mut tab, |_| gates.diff.arrived() > before));
    let mut opts = tab.diff_opts;
    opts.ignore_whitespace = !opts.ignore_whitespace;
    tab.set_diff_options(opts);
    wait_arrived(&mut tab, &gates.diff, before + 2);
    gates.diff.release(before + 1);
    assert!(poll_until(&mut tab, |t| t.browse_diff.doc.is_some()));
    let fresh = tab.browse_diff.doc.clone().unwrap().unwrap();
    gates.diff.release(before);
    std::thread::sleep(std::time::Duration::from_millis(200));
    tab.poll();
    assert!(std::sync::Arc::ptr_eq(&fresh, tab.browse_diff.doc.as_ref().unwrap().as_ref().unwrap()));
}

#[test]
fn closing_the_tab_stops_its_browse_jobs() {
    let repo = sample_repo();
    let (mut tab, gates) =
        gated_tab(repo.path(), Gates { blame: Gate::closed(), file_history: Gate::closed(), files: Gate::closed(), ..Default::default() });
    let head = tab.head_commit().unwrap();
    tab.open_file_history(head, "README.md".into(), None);
    tab.open_blame(Some(head), "README.md".into(), None);
    tab.request_files();
    wait_arrived(&mut tab, &gates.files, 1);
    let tokens = [
        match &tab.browse[0] {
            Browse::FileHistory(v) => v.cancel_token(),
            _ => unreachable!(),
        },
        blame_view(&tab).cancel_token(),
        tab.files_cancel_token().unwrap(),
    ];
    drop(tab);
    assert!(tokens.iter().all(|t| t.is_cancelled()));
    gates.blame.open();
    gates.file_history.open();
    gates.files.open();
}

#[test]
fn browse_jobs_that_cannot_start_show_errors() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    tab.worker_mut().fail_spawns_for_test();
    let head = tab.head_commit().unwrap();
    tab.open_file_history(head, "README.md".into(), None);
    assert!(poll_until(&mut tab, |t| !history_view(t).loading));
    assert!(history_view(&tab).error.as_deref().unwrap().contains("failed to start"));
    tab.open_blame(Some(head), "README.md".into(), None);
    assert!(poll_until(&mut tab, |t| blame_view(t).doc.is_some()));
    assert!(blame_view(&tab).doc.as_ref().unwrap().as_ref().unwrap_err().contains("failed to start"));
    // Recovery: trying again works.
    tab.worker_mut().allow_spawns_for_test();
    tab.reload_blame();
    loaded_blame(&mut tab);
}
