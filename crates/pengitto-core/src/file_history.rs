//! The history of one file, following renames (`git log --follow`).

use anyhow::{Result, bail};
use bstr::BString;

use crate::{
    CancelToken, Cancelled,
    log::{FIELDS, Parser},
    model::{CommitSummary, Oid},
    runner::GitRunner,
    search::{SearchEnd, batch_stream},
};

/// One commit that changed the file.
#[derive(Debug, Clone)]
pub struct FileHistoryEntry {
    pub commit: CommitSummary,
    /// The file's path in this commit.
    pub path: BString,
    /// Its path before, when this commit renamed or copied it.
    pub old_path: Option<BString>,
    /// git's status letter (A, M, D, R, C, T…); `None` when git listed the
    /// commit without a change for it (e.g. a merge).
    pub status: Option<char>,
    /// Similarity of a rename or copy, in percent.
    pub score: Option<u8>,
}

/// Builds entries from `git log --follow --format=\x1e<FIELDS>
/// --name-status -z` output, one NUL-separated token at a time. Paths are
/// taken by count after each status, so newlines, tabs or other control
/// characters inside them can't be mistaken for separators.
pub struct HistoryParser {
    parser: Parser,
    /// The file's path in the commits being read (older ones may know it
    /// by an older name).
    current_path: BString,
    /// The entry being read; complete when the next one starts.
    pending: Option<FileHistoryEntry>,
    state: State,
}

enum State {
    /// Expecting a commit header or a status.
    Ready,
    /// A status was read; its path(s) follow.
    Path { letter: u8, score: Option<u8>, first: Option<BString> },
}

impl HistoryParser {
    /// `path`: the file's path at the newest commit.
    pub fn new(path: &[u8]) -> Self {
        HistoryParser { parser: Parser::new(), current_path: BString::from(path), pending: None, state: State::Ready }
    }

    /// Reads one token; returns the entry this one completed, if any.
    pub fn feed(&mut self, tok: &[u8]) -> Result<Option<FileHistoryEntry>> {
        match std::mem::replace(&mut self.state, State::Ready) {
            State::Ready => {
                if let Some(header) = tok.strip_prefix(b"\x1e") {
                    let Some(commit) = self.parser.parse(header) else {
                        bail!("unreadable commit in file history: {:?}", bstr::BStr::new(header))
                    };
                    let entry = FileHistoryEntry { commit, path: self.current_path.clone(), old_path: None, status: None, score: None };
                    return Ok(self.pending.replace(entry));
                }
                if tok.is_empty() {
                    return Ok(None);
                }
                // A status: "\nM", "R100", …
                let status = tok.strip_prefix(b"\n").unwrap_or(tok);
                if self.pending.is_none() {
                    bail!("file history starts without a commit");
                }
                let Some((&letter, score)) = status.split_first() else { bail!("empty status in file history") };
                if !letter.is_ascii_uppercase() {
                    bail!("unexpected status {:?} in file history", bstr::BStr::new(status));
                }
                let score = std::str::from_utf8(score).ok().and_then(|s| s.parse().ok());
                self.state = State::Path { letter, score, first: None };
            }
            State::Path { letter, score, first } => {
                if tok.is_empty() {
                    bail!("empty path in file history");
                }
                if first.is_none() && matches!(letter, b'R' | b'C') {
                    self.state = State::Path { letter, score, first: Some(BString::from(tok)) };
                    return Ok(None);
                }
                let new = BString::from(tok);
                // Older commits know the file by its old name.
                self.current_path = first.clone().unwrap_or_else(|| new.clone());
                let entry = self.pending.as_mut().expect("a status follows a commit");
                entry.path = new;
                entry.old_path = first;
                entry.status = Some(letter as char);
                entry.score = score;
            }
        }
        Ok(None)
    }

    /// The output ended: the last entry, or an error if it stopped within one.
    pub fn finish(self) -> Result<Option<FileHistoryEntry>> {
        if let State::Path { .. } = self.state {
            bail!("file history was cut off");
        }
        Ok(self.pending)
    }
}

/// Parses complete output (which ends with a NUL unless empty).
pub fn parse(out: &[u8], path: &[u8]) -> Result<Vec<FileHistoryEntry>> {
    if out.last().is_some_and(|&b| b != 0) {
        bail!("file history was cut off");
    }
    let mut p = HistoryParser::new(path);
    let mut entries = Vec::new();
    let tokens = out.strip_suffix(b"\0").unwrap_or(out);
    if !tokens.is_empty() {
        for tok in tokens.split(|&b| b == 0) {
            entries.extend(p.feed(tok)?);
        }
    }
    entries.extend(p.finish()?);
    Ok(entries)
}

/// The commits that changed `path` going back from `start`, following
/// renames. Merges are compared with their first parent (as pengitto shows
/// them). Entries stream out as git prints them: the first at once, then in
/// batches (every 100ms or 2000). Returns how it ended.
pub fn file_history(
    git: &GitRunner,
    start: Oid,
    path: &[u8],
    cancel: &CancelToken,
    on_batch: impl FnMut(Vec<FileHistoryEntry>) -> bool,
) -> Result<SearchEnd> {
    use std::os::unix::ffi::OsStrExt as _;
    if cancel.is_cancelled() {
        return Ok(SearchEnd::Stopped);
    }
    let mut args: Vec<std::ffi::OsString> = [
        "log".to_owned(),
        start.to_hex(),
        "--follow".into(),
        "--diff-merges=first-parent".into(),
        format!("--format=%x1e{FIELDS}"),
        "--name-status".into(),
        "-z".into(),
        "--".into(),
    ]
    .map(Into::into)
    .into();
    args.push(std::ffi::OsStr::from_bytes(path).to_owned());
    // Stops git when cancelled or no longer listened to.
    let inner = CancelToken::default();
    let (tx, rx) = crossbeam_channel::unbounded();
    let (end, read) = std::thread::scope(|s| {
        let (args, inner) = (&args, &inner);
        let reader = s.spawn(move || -> Result<()> {
            let mut parser = HistoryParser::new(path);
            let mut failure = None;
            git.stream_terminated(args, 0, inner, |tok, terminated| {
                if !terminated && !tok.is_empty() {
                    failure = Some(anyhow::anyhow!("file history was cut off"));
                    return false;
                }
                match parser.feed(tok) {
                    Ok(Some(entry)) => tx.send(entry).is_ok(),
                    Ok(None) => true,
                    Err(e) => {
                        failure = Some(e);
                        false
                    }
                }
            })?;
            if let Some(e) = failure {
                return Err(e);
            }
            if inner.is_cancelled() {
                return Ok(());
            }
            if let Some(last) = parser.finish()? {
                let _ = tx.send(last);
            }
            Ok(())
        });
        let end = batch_stream(&rx, cancel, on_batch);
        inner.cancel();
        (end, reader.join().unwrap_or_else(|_| Err(anyhow::anyhow!("file history reader panicked"))))
    });
    match (end, read) {
        (SearchEnd::Stopped, _) => Ok(SearchEnd::Stopped),
        (SearchEnd::Complete, Err(e)) if e.is::<Cancelled>() => Ok(SearchEnd::Stopped),
        (SearchEnd::Complete, Err(e)) => Err(e),
        (SearchEnd::Complete, Ok(())) => Ok(if cancel.is_cancelled() { SearchEnd::Stopped } else { SearchEnd::Complete }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(n: u8, subject: &str) -> Vec<u8> {
        let hex = format!("{n:02x}").repeat(20);
        format!("\x1e{hex}\x1f\x1fA\x1fa@x\x1f1\x1f1\x1f{subject}\0").into_bytes()
    }

    #[test]
    fn statuses_paths_and_renames() {
        let mut out = header(1, "rename");
        out.extend_from_slice(b"\nR087\0old\nname.txt\0new\tname.txt\0");
        out.extend(header(2, "edit"));
        out.extend_from_slice(b"\nM\0old\nname.txt\0");
        out.extend(header(3, "merge, no status"));
        out.extend(header(4, "add"));
        out.extend_from_slice(b"\nA\0old\nname.txt\0");
        let e = parse(&out, b"new\tname.txt").unwrap();
        assert_eq!(e.len(), 4);
        assert_eq!((e[0].status, e[0].score), (Some('R'), Some(87)));
        assert_eq!(e[0].path, "new\tname.txt");
        assert_eq!(e[0].old_path, Some(BString::from("old\nname.txt")));
        assert_eq!(e[1].path, "old\nname.txt");
        assert_eq!((e[2].status, &e[2].path), (None, &BString::from("old\nname.txt")), "keeps the path known then");
        assert_eq!(e[3].status, Some('A'));
    }

    #[test]
    fn a_path_may_contain_the_record_separator() {
        let mut out = header(1, "weird");
        out.extend_from_slice(b"\nM\0\x1eodd\0");
        let e = parse(&out, b"\x1eodd").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].path, "\x1eodd");
    }

    #[test]
    fn output_must_end_with_a_nul() {
        let mut out = header(1, "edit");
        out.extend_from_slice(b"\nM\0path-cut-of");
        assert!(parse(&out, b"x").is_err(), "the last path lost its NUL");
        assert!(parse(b"", b"x").unwrap().is_empty());
    }

    #[test]
    fn cut_off_output_is_an_error() {
        let mut out = header(1, "rename");
        out.extend_from_slice(b"\nR100\0only-old\0");
        assert!(parse(&out, b"x").is_err());
        let mut out = header(1, "edit");
        out.extend_from_slice(b"\nM\0");
        assert!(parse(&out, b"x").is_err());
        assert!(parse(b"\nM\0x\0", b"x").is_err(), "no commit first");
    }
}
