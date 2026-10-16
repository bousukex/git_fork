//! Commit search through RepoTab (M4a).

mod common;

use common::{git, idle, poll_until, sample_repo};
use pengitto::{
    repo::{RepoTab, search::SearchMode},
    worker::Msg,
};
use pengitto_core::search::SearchEnd;

fn open(dir: &std::path::Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading));
    tab
}

fn search(tab: &mut RepoTab, mode: SearchMode, query: &str) {
    tab.open_search();
    tab.search.mode = mode;
    tab.search.query = query.into();
    tab.search_changed();
    assert!(poll_until(tab, |t| t.search.complete || t.search.error.is_some()), "{:?}", tab.search.error);
}

fn subjects(tab: &RepoTab) -> Vec<String> {
    tab.search.rows.iter().map(|&r| tab.history.commits[r].subject.clone()).collect()
}

#[test]
fn message_search_highlights_and_steps_through_matches() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    tab.open_search();
    tab.search.query = "readme".into();
    tab.search_changed();
    assert!(!tab.search.running, "waits for typing to pause");
    search(&mut tab, SearchMode::Message, "readme");
    assert_eq!(subjects(&tab), ["Document usage in README"]);
    search(&mut tab, SearchMode::Message, "a");
    let rows = tab.search.rows.clone();
    assert!(rows.len() >= 3 && rows.windows(2).all(|w| w[0] < w[1]), "{rows:?}");
    assert_eq!(tab.search.unloaded(), 0);

    // Stepping goes through matches in history order and wraps.
    tab.selected_commit = None;
    tab.step_match(true);
    assert_eq!(tab.match_position(), Some(1));
    tab.step_match(true);
    assert_eq!(tab.match_position(), Some(2));
    tab.step_match(false);
    tab.step_match(false);
    assert_eq!(tab.match_position(), Some(rows.len()), "wrapped to the last");
    assert_eq!(tab.scroll_to_row, Some(*rows.last().unwrap()));
}

#[test]
fn author_hash_and_path_searches() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    search(&mut tab, SearchMode::Author, "TARO@example");
    assert_eq!(tab.search.rows.len(), tab.history.commits.len(), "every commit is by Taro");
    let id = tab.history.commits[2].id;
    search(&mut tab, SearchMode::Commit, &id.to_hex()[..6].to_uppercase());
    assert_eq!(tab.search.rows, [2]);
    search(&mut tab, SearchMode::Path, "lib.rs");
    assert_eq!(subjects(&tab), ["Add lib.rs with add()"]);
    // Occurrence counts: the WIP commit keeps one こんにちは (no match); the
    // merge brings it in relative to its first parent (a match).
    search(&mut tab, SearchMode::Changes, "こんにちは");
    assert_eq!(subjects(&tab), ["Merge branch 'feature/日本語'", "挨拶を日本語にする"]);
}

#[test]
fn stale_and_closed_results_are_ignored() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    let some = tab.history.commits[0].id;
    tab.open_search();
    tab.search.query = "first".into();
    tab.search_changed();
    let old = tab.search_seq();
    tab.search.query = "second".into();
    tab.search_changed();
    // The first search's results arrive late: dropped.
    tab.deliver(Msg::SearchBatch(old, vec![some]));
    tab.deliver(Msg::SearchDone(old, Ok(SearchEnd::Complete)));
    assert!(tab.search.matches.is_empty());
    assert!(!tab.search.complete);

    // After closing, the current request's results are dropped too.
    let current = tab.search_seq();
    tab.close_search();
    tab.deliver(Msg::SearchBatch(current, vec![some]));
    assert!(tab.search.matches.is_empty());
    assert!(!tab.search.open);
}

#[test]
fn moving_refs_reruns_the_search() {
    let repo = sample_repo();
    let d = repo.path();
    let mut tab = open(d);
    search(&mut tab, SearchMode::Message, "brand new");
    assert!(tab.search.rows.is_empty());
    let mut t = 1_900_000_000;
    std::fs::write(d.join("new.txt"), "x\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "A brand new commit"], &mut t);
    assert!(poll_until(&mut tab, |t| t.search.complete && t.search.rows.len() == 1));
    assert_eq!(subjects(&tab), ["A brand new commit"]);
    assert_eq!(tab.search.rows, [0], "rows follow the reloaded history");
}

#[test]
fn errors_are_reported_not_fatal() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    search(&mut tab, SearchMode::Message, "x");
    tab.deliver(Msg::SearchDone(tab.search_seq(), Err(anyhow::anyhow!("boom"))));
    assert_eq!(tab.search.error.as_deref(), Some("boom"));
}

use common::gated::{Gate, Gates, gated_tab};

/// Searches wait for the test; with `gate_log`, the history never arrives
/// by itself (the test delivers it).
fn tab_with_gates(dir: &std::path::Path, gate_log: bool) -> (RepoTab, std::sync::Arc<Gates>) {
    gated_tab(dir, Gates { search: Gate::closed(), log: if gate_log { Gate::closed() } else { Gate::default() }, ..Default::default() })
}

/// Waits (polling) until the pending search has started.
fn started(tab: &mut RepoTab) -> pengitto_core::CancelToken {
    assert!(poll_until(tab, |t| t.search.cancel_token().is_some()));
    tab.search.cancel_token().unwrap()
}

#[test]
fn a_replaced_search_really_stops_and_its_results_are_dropped() {
    let repo = sample_repo();
    let (mut tab, gates) = tab_with_gates(repo.path(), false);
    tab.open_search();
    tab.search.query = "a".into();
    tab.search_changed();
    let first = started(&mut tab);
    // The first search is still waiting to run when the query changes.
    tab.search.query = "readme".into();
    tab.search_changed();
    assert!(first.is_cancelled(), "the old search is stopped at once");
    let second = started(&mut tab);
    assert!(!second.is_cancelled());
    assert!(poll_until(&mut tab, |_| gates.search.arrived() == 2));
    gates.search.release_next();
    gates.search.release_next();
    assert!(poll_until(&mut tab, |t| t.search.complete));
    assert_eq!(subjects(&tab), ["Document usage in README"], "nothing from the old search");
}

#[test]
fn closing_or_dropping_stops_a_running_search() {
    let repo = sample_repo();
    let (mut tab, _gates) = tab_with_gates(repo.path(), false);
    tab.open_search();
    tab.search.query = "a".into();
    tab.search_changed();
    let token = started(&mut tab);
    tab.close_search();
    assert!(token.is_cancelled());
    assert!(!tab.search.running);

    tab.open_search();
    tab.search.query = "b".into();
    tab.search_changed();
    let token = started(&mut tab);
    drop(tab);
    assert!(token.is_cancelled(), "a closed tab stops its search");
}

#[test]
fn hash_search_follows_history_as_it_loads_and_is_replaced() {
    let repo = sample_repo();
    let all = pengitto_core::log::collect(
        pengitto_core::CliReader::open(repo.path()).unwrap().runner(),
        &pengitto_core::log::LogOptions::default(),
    )
    .unwrap();
    let (mut tab, _gates) = tab_with_gates(repo.path(), true);
    let target = all[5].id;
    // History arrives in parts; the search starts after the first one.
    tab.deliver(Msg::LogBatch(tab.log_seq(), all[..3].to_vec()));
    search(&mut tab, SearchMode::Commit, &target.to_hex()[..8]);
    assert!(tab.search.rows.is_empty(), "not loaded yet");
    tab.deliver(Msg::LogBatch(tab.log_seq(), all[3..].to_vec()));
    assert_eq!(tab.search.rows, [5], "found once loaded");
    assert_eq!(tab.search.matches.len(), 1);

    // The history is replaced (refresh): rows follow the new order.
    tab.refresh();
    assert!(poll_until(&mut tab, |t| t.search.complete));
    let mut reordered = all.clone();
    reordered.reverse();
    tab.deliver(Msg::LogBatch(tab.log_seq(), reordered));
    tab.deliver(Msg::LogDone(tab.log_seq(), Ok(())));
    assert_eq!(tab.search.rows, [all.len() - 1 - 5]);

    // Replaced by nothing at all: no rows point past the end.
    tab.search.only_matches = true;
    tab.refresh();
    assert!(poll_until(&mut tab, |t| t.search.complete));
    tab.deliver(Msg::LogDone(tab.log_seq(), Ok(())));
    assert!(tab.history.commits.is_empty());
    assert!(tab.search.rows.is_empty());
    assert!(tab.search.matches.is_empty());
}

#[test]
fn a_search_that_cannot_start_ends_with_an_error() {
    let repo = sample_repo();
    let mut tab = open(repo.path());
    tab.worker_mut().fail_spawns_for_test();
    tab.open_search();
    tab.search.query = "a".into();
    tab.search_changed();
    assert!(poll_until(&mut tab, |t| t.search.error.is_some()));
    assert!(!tab.search.running);
    assert!(tab.search.error.as_deref().unwrap().contains("failed to start"));
}
