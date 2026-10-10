//! Commit search of a repository tab (Ctrl+F above the history).
//!
//! Typing invalidates the previous search at once and starts the new one
//! after a short pause. Matches arrive as commit ids; `rows` maps the
//! loaded ones to history rows (in history order) for highlighting,
//! stepping through them and the "only matches" list. Refs moving reruns
//! the search, since other commits may be reachable now.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use tine_core::{
    CancelToken,
    model::Oid,
    search::{HashPrefix, SearchEnd, SearchKind},
};

use super::RepoTab;
use crate::worker::Gen;

/// Wait after the last keystroke before searching.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Message,
    Author,
    /// Commit hash prefix, among loaded commits.
    Commit,
    /// Code changes (`git log -S`).
    Changes,
    Path,
}

impl SearchMode {
    pub const ALL: [SearchMode; 5] =
        [SearchMode::Message, SearchMode::Author, SearchMode::Commit, SearchMode::Changes, SearchMode::Path];

    pub fn label(self) -> &'static str {
        match self {
            SearchMode::Message => "Message",
            SearchMode::Author => "Author",
            SearchMode::Commit => "Commit",
            SearchMode::Changes => "Changes",
            SearchMode::Path => "Path",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            SearchMode::Message => "Search commit messages",
            SearchMode::Author => "Search author names and emails",
            SearchMode::Commit => "Commit hash (at least 4 characters)",
            SearchMode::Changes => "Text added or removed by commits (case-sensitive)",
            SearchMode::Path => "Part of a file path",
        }
    }

    fn kind(self) -> Option<SearchKind> {
        match self {
            SearchMode::Message => Some(SearchKind::Message),
            SearchMode::Author => Some(SearchKind::Author),
            SearchMode::Commit => None,
            SearchMode::Changes => Some(SearchKind::Changes),
            SearchMode::Path => Some(SearchKind::Path),
        }
    }
}

#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub query: String,
    pub mode: SearchMode,
    /// Show only matching commits (without the graph).
    pub only_matches: bool,
    /// Ask the search field for focus on the next frame.
    pub focus_field: bool,
    /// Request whose results are accepted; others are stale.
    seq: Gen,
    cancel: Option<CancelToken>,
    /// When to start the search for the current query.
    start_at: Option<Instant>,
    pub running: bool,
    /// Every match has arrived.
    pub complete: bool,
    pub error: Option<String>,
    pub matches: HashSet<Oid>,
    /// History rows of loaded matches, ascending.
    pub rows: Vec<usize>,
    /// History epoch and length `rows` reflects.
    indexed_epoch: u64,
    indexed_len: usize,
    /// A running hash search: it matches loaded history as it grows (and
    /// again after the history is replaced).
    hash_prefix: Option<HashPrefix>,
    /// The selection's 1-based place among loaded matches (for display).
    pub selected_position: Option<usize>,
}

impl Search {
    /// A query is set and results (some or all) are meaningful.
    pub fn active(&self) -> bool {
        self.open && !self.query.trim().is_empty()
    }

    /// Matches that are not loaded in the history (yet).
    pub fn unloaded(&self) -> usize {
        self.matches.len() - self.rows.len()
    }

    fn stop(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.running = false;
    }

    /// Drops all results (they belong to another query or history).
    fn reset_results(&mut self) {
        self.matches.clear();
        self.rows.clear();
        self.indexed_len = 0;
        self.complete = false;
        self.error = None;
        self.hash_prefix = None;
    }

    /// The token of the running search (tests check it gets cancelled).
    #[doc(hidden)]
    pub fn cancel_token(&self) -> Option<CancelToken> {
        self.cancel.clone()
    }
}

impl RepoTab {
    /// Opens the search bar (Ctrl+F) over the history.
    pub fn open_search(&mut self) {
        self.view = super::View::History;
        self.search.open = true;
        self.search.focus_field = true;
    }

    pub fn close_search(&mut self) {
        self.search.stop();
        self.search.reset_results();
        self.search.open = false;
        self.search.start_at = None;
        self.search.seq = self.gens.bump();
    }

    /// The query or mode changed: forget the old results now, search after
    /// a pause.
    pub fn search_changed(&mut self) {
        self.search.stop();
        self.search.reset_results();
        self.search.seq = self.gens.bump();
        self.search.start_at = (!self.search.query.trim().is_empty()).then(|| Instant::now() + DEBOUNCE);
        self.worker_ctx().request_repaint_after(DEBOUNCE);
    }

    /// Reruns the current search right away (e.g. after refs moved).
    pub fn rerun_search(&mut self) {
        if self.search.active() {
            self.search.stop();
            self.search.reset_results();
            self.search.seq = self.gens.bump();
            self.search.start_at = Some(Instant::now());
        }
    }

    /// Starts a pending search once its time has come.
    pub(super) fn poll_search(&mut self) {
        let Some(at) = self.search.start_at else { return };
        let now = Instant::now();
        if now < at {
            self.worker_ctx().request_repaint_after(at - now);
            return;
        }
        self.search.start_at = None;
        let query = self.search.query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        match self.search.mode.kind() {
            None => {
                match HashPrefix::parse(&query) {
                    Some(prefix) => {
                        self.search.hash_prefix = Some(prefix);
                        // Matched against the history now and as it loads.
                        self.search.rows.clear();
                        self.search.indexed_len = 0;
                        self.sync_search_rows();
                    }
                    None => self.search.error = Some("Enter at least 4 hex digits of a commit hash".into()),
                }
                self.search.complete = true;
            }
            Some(kind) => {
                let cancel = CancelToken::default();
                self.search.cancel = Some(cancel.clone());
                self.search.running = true;
                let seq = self.search.seq;
                self.worker.search(seq, kind, query, cancel);
            }
        }
    }

    pub(super) fn on_search_batch(&mut self, seq: Gen, ids: Vec<Oid>) {
        if seq == self.search.seq {
            self.add_matches(ids);
        }
    }

    /// Stops filtering to matches when `id` is not one (an explicit reveal
    /// must show the commit).
    pub(super) fn show_in_search(&mut self, id: Oid) {
        if self.search.active() && self.search.only_matches && !self.search.matches.contains(&id) {
            self.search.only_matches = false;
        }
    }

    pub(super) fn on_search_done(&mut self, seq: Gen, result: anyhow::Result<SearchEnd>) {
        if seq != self.search.seq {
            return;
        }
        self.search.running = false;
        self.search.cancel = None;
        match result {
            Ok(end) => self.search.complete = end == SearchEnd::Complete,
            Err(e) => self.search.error = Some(format!("{e:#}")),
        }
    }

    fn add_matches(&mut self, ids: Vec<Oid>) {
        self.sync_search_rows();
        let s = &mut self.search;
        for id in ids {
            if !s.matches.insert(id) {
                continue;
            }
            if let Some(&row) = self.history.index.get(&id)
                && row < s.indexed_len
                && let Err(at) = s.rows.binary_search(&row)
            {
                s.rows.insert(at, row);
            }
        }
    }

    /// Brings `rows` up to date with the history: new batches, a replaced
    /// or emptied history. Cheap when nothing changed; called after every
    /// history change and before drawing.
    pub fn sync_search_rows(&mut self) {
        let s = &mut self.search;
        let commits = &self.history.commits;
        if s.indexed_epoch != self.history.epoch || s.indexed_len > commits.len() {
            s.indexed_epoch = self.history.epoch;
            s.rows.clear();
            s.indexed_len = 0;
            if s.hash_prefix.is_some() {
                // Hash matches are about loaded commits: redo them.
                s.matches.clear();
            }
        }
        if s.matches.is_empty() && s.hash_prefix.is_none() {
            s.indexed_len = commits.len();
            return;
        }
        for (i, c) in commits.iter().enumerate().skip(s.indexed_len) {
            if s.hash_prefix.as_ref().is_some_and(|p| p.matches(&c.id)) {
                s.matches.insert(c.id);
            }
            if s.matches.contains(&c.id) {
                s.rows.push(i);
            }
        }
        s.indexed_len = commits.len();
    }

    /// Selects the next (or previous) loaded match after the selection,
    /// wrapping around.
    pub fn step_match(&mut self, forward: bool) {
        let rows = &self.search.rows;
        if rows.is_empty() {
            return;
        }
        let current = self.selected_commit.and_then(|id| self.history.index.get(&id).copied());
        let pick = match (current, forward) {
            (None, true) => rows[0],
            (None, false) => rows[rows.len() - 1],
            (Some(cur), true) => rows.iter().copied().find(|&r| r > cur).unwrap_or(rows[0]),
            (Some(cur), false) => rows.iter().rev().copied().find(|&r| r < cur).unwrap_or(rows[rows.len() - 1]),
        };
        let id = self.history.commits[pick].id;
        self.select_commit(id);
        self.scroll_to_row = Some(pick);
    }

    /// The request whose results are accepted now.
    pub fn search_seq(&self) -> Gen {
        self.search.seq
    }

    /// Position of the selection among loaded matches (1-based).
    pub fn match_position(&self) -> Option<usize> {
        let row = self.selected_commit.and_then(|id| self.history.index.get(&id).copied())?;
        self.search.rows.binary_search(&row).ok().map(|i| i + 1)
    }
}
