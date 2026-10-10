//! Searching history for commits: by message, author, code changes or
//! path. Results stream back as commit ids, over the same commits the
//! history shows ([`crate::log::default_revisions`]).

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, unbounded};

use crate::{
    CancelToken, Cancelled,
    log::{default_revisions, nothing_to_log},
    model::Oid,
    runner::GitRunner,
};

/// The first match goes out at once; later ones at most this long after
/// they arrived, even while git keeps scanning without output.
const FLUSH_EVERY: Duration = Duration::from_millis(100);
const MAX_BATCH: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchKind {
    /// Subject and body, case-insensitive, literal.
    Message,
    /// Author name or email, case-insensitive, literal.
    Author,
    /// Commits that change how often the text occurs (`git log -S`), with
    /// merges compared to their first parent like the diff Tine shows.
    /// Case-sensitive.
    Changes,
    /// Commits touching a path containing the text (case-insensitive):
    /// a file name, a directory name, or several components (`src/re`).
    Path,
}

/// How a search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEnd {
    /// Every match was delivered.
    Complete,
    /// Stopped early (cancelled, or the receiver stopped listening).
    Stopped,
}

/// Escapes glob characters so `text` matches literally inside a glob.
fn glob_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Builds the arguments (and the runner: path search needs pathspec magic,
/// which Tine otherwise turns off).
fn command(git: &GitRunner, kind: SearchKind, query: &str) -> (GitRunner, Vec<String>) {
    let mut args: Vec<String> = ["log", "--format=%H", "-z"].map(String::from).into();
    let mut runner = git.clone();
    let mut pathspec = None;
    match kind {
        SearchKind::Message => args.extend(["-i".into(), "-F".into(), format!("--grep={query}")]),
        SearchKind::Author => args.extend(["-i".into(), "-F".into(), format!("--author={query}")]),
        // --diff-merges turns on patch output; only the ids are wanted.
        SearchKind::Changes => {
            args.extend(["--diff-merges=first-parent".into(), "--no-patch".into(), format!("-S{query}")])
        }
        SearchKind::Path => {
            // `*` stops at `/`: one pattern for matching files, one for
            // everything under matching directories.
            runner = runner.with_env("GIT_LITERAL_PATHSPECS", "0");
            let q = glob_escape(query);
            pathspec = Some([format!(":(icase,glob)**/*{q}*"), format!(":(icase,glob)**/*{q}*/**")]);
        }
    }
    args.extend(default_revisions(git));
    args.push("--".into());
    args.extend(pathspec.into_iter().flatten());
    (runner, args)
}

/// Streams matching commits, newest first in `git log` order. The first
/// match is delivered at once, later ones in batches (every 100ms or 2000
/// commits, also while git is busy without output). Return `false` from
/// `on_batch` to stop. Cancelling stops every git command of the search,
/// including the preparation.
pub fn search(
    git: &GitRunner,
    kind: SearchKind,
    query: &str,
    cancel: &CancelToken,
    on_batch: impl FnMut(Vec<Oid>) -> bool,
) -> Result<SearchEnd> {
    if query.is_empty() {
        bail!("nothing to search for");
    }
    if cancel.is_cancelled() {
        return Ok(SearchEnd::Stopped);
    }
    // Stops the git reading below when cancelled or no longer listened to.
    let inner = CancelToken::default();
    let prep = git.clone().with_cancel(cancel.clone());
    let stopped = |e: anyhow::Error| if e.is::<Cancelled>() { Ok(SearchEnd::Stopped) } else { Err(e) };
    match nothing_to_log(&prep) {
        Ok(true) => {
            return Ok(if cancel.is_cancelled() { SearchEnd::Stopped } else { SearchEnd::Complete });
        }
        Ok(false) => {}
        Err(e) => return stopped(e),
    }
    let (runner, args) = command(&prep, kind, query);
    if cancel.is_cancelled() {
        return Ok(SearchEnd::Stopped);
    }
    let (tx, rx) = unbounded();
    let end = std::thread::scope(|s| {
        let (runner, args, inner) = (&runner, &args, &inner);
        let reader = s.spawn(move || {
            runner.stream_records(args, 0, inner, |rec| {
                let rec = rec.strip_prefix(b"\n").unwrap_or(rec);
                match Oid::from_hex(rec) {
                    Some(id) => tx.send(id).is_ok(),
                    None => true,
                }
            })
        });
        let end = batch_stream(&rx, cancel, on_batch);
        inner.cancel();
        let read = reader.join().unwrap_or_else(|_| Err(anyhow::anyhow!("search reader panicked")));
        (end, read)
    });
    match end {
        (SearchEnd::Stopped, _) => Ok(SearchEnd::Stopped),
        (SearchEnd::Complete, Err(e)) => stopped(e),
        (SearchEnd::Complete, Ok(())) => Ok(if cancel.is_cancelled() { SearchEnd::Stopped } else { SearchEnd::Complete }),
    }
}

/// Groups ids from `rx` into batches: the first at once, then whatever
/// arrived after at most [`FLUSH_EVERY`] (or [`MAX_BATCH`] ids). Ends when
/// the sender is gone (complete), `cancel` fires or `on_batch` declines.
pub fn batch_stream<T>(rx: &Receiver<T>, cancel: &CancelToken, mut on_batch: impl FnMut(Vec<T>) -> bool) -> SearchEnd {
    let mut batch: Vec<T> = Vec::new();
    let mut sent_any = false;
    // When the oldest unsent id must go out.
    let mut due: Option<Instant> = None;
    loop {
        if cancel.is_cancelled() {
            return SearchEnd::Stopped;
        }
        let wait = due.map_or(FLUSH_EVERY, |d| d.saturating_duration_since(Instant::now()));
        let received = rx.recv_timeout(wait);
        match received {
            Ok(id) => {
                batch.push(id);
                due.get_or_insert_with(|| Instant::now() + FLUSH_EVERY);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if cancel.is_cancelled() {
                    return SearchEnd::Stopped;
                }
                if !batch.is_empty() && !on_batch(batch) {
                    return SearchEnd::Stopped;
                }
                return SearchEnd::Complete;
            }
        }
        let flush = !batch.is_empty()
            && (!sent_any || batch.len() >= MAX_BATCH || due.is_some_and(|d| Instant::now() >= d));
        if flush {
            sent_any = true;
            due = None;
            if !on_batch(std::mem::take(&mut batch)) {
                return SearchEnd::Stopped;
            }
        }
    }
}

/// A commit hash prefix (at least 4 hex digits, any case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashPrefix(Vec<u8>);

impl HashPrefix {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_ascii_lowercase();
        if text.len() < 4 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(HashPrefix(text.bytes().map(|b| if b.is_ascii_digit() { b - b'0' } else { b - b'a' + 10 }).collect()))
    }

    pub fn matches(&self, id: &Oid) -> bool {
        let bytes = id.as_bytes();
        self.0.iter().enumerate().all(|(i, &n)| {
            bytes.get(i / 2).is_some_and(|b| if i % 2 == 0 { b >> 4 == n } else { b & 15 == n })
        })
    }
}

/// Commits among `ids` whose hex id starts with `prefix` (at least 4 hex
/// digits; case-insensitive). For searching loaded history by hash.
pub fn hash_prefix_matches<'a>(prefix: &str, ids: impl Iterator<Item = &'a Oid>) -> Vec<Oid> {
    match HashPrefix::parse(prefix) {
        Some(p) => ids.filter(|id| p.matches(id)).copied().collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap()
    }

    #[test]
    fn a_lone_match_is_sent_even_while_git_is_quiet() {
        let (tx, rx) = unbounded();
        let cancel = CancelToken::default();
        let got = std::sync::Mutex::new(Vec::<(Instant, Vec<Oid>)>::new());
        let start = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                tx.send(oid(1)).unwrap();
                tx.send(oid(2)).unwrap();
                // git scans for a while without printing anything.
                std::thread::sleep(Duration::from_millis(600));
                drop(tx);
            });
            let end = batch_stream(&rx, &cancel, |b| {
                got.lock().unwrap().push((Instant::now(), b));
                true
            });
            assert_eq!(end, SearchEnd::Complete);
        });
        let got = got.into_inner().unwrap();
        assert_eq!(got[0].1, [oid(1)], "the first match at once");
        assert_eq!(got[1].1, [oid(2)]);
        let second_after = got[1].0 - start;
        assert!(second_after < Duration::from_millis(400), "sent while git was quiet: {second_after:?}");
    }

    #[test]
    fn batching_stops_when_cancelled_or_declined() {
        let (tx, rx) = unbounded();
        let cancel = CancelToken::default();
        tx.send(oid(1)).unwrap();
        assert_eq!(batch_stream(&rx, &cancel, |_| false), SearchEnd::Stopped);
        cancel.cancel();
        tx.send(oid(2)).unwrap();
        assert_eq!(batch_stream(&rx, &cancel, |_| true), SearchEnd::Stopped);
    }

    #[test]
    fn glob_characters_are_escaped() {
        assert_eq!(glob_escape("a*b?[c]\\d"), "a\\*b\\?\\[c\\]\\\\d");
        assert_eq!(glob_escape("日本語.rs"), "日本語.rs");
    }
}
