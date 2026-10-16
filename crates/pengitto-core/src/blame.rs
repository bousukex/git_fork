//! `git blame --porcelain`: who last changed each line of a file.
//!
//! Commit details (author, summary, …) are shared by all lines a commit
//! wrote; the path a line came from and the commit before it ("previous")
//! belong to an *origin* — the same commit can be the origin of lines under
//! different paths (git prints `filename` again in that case), so they are
//! kept per origin, not per commit.

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use bstr::{BString, ByteSlice};

use crate::{CancelToken, model::Oid, runner::GitRunner};

/// Files larger than this are not blamed (the result would not be usable
/// and git would take long).
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_LINES: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    pub id: Oid,
    pub author: String,
    pub author_mail: String,
    /// Unix time.
    pub author_time: i64,
    pub summary: String,
    /// Git stopped here (a root commit, or the edge of a shallow clone).
    pub boundary: bool,
}

impl BlameCommit {
    /// Lines changed in the work tree and not committed yet.
    pub fn is_uncommitted(&self) -> bool {
        self.id.is_zero()
    }
}

/// Where lines came from: a commit and the file's path in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameOrigin {
    /// Index into [`Blame::commits`].
    pub commit: usize,
    pub filename: BString,
    /// The commit before (and the path there), for blaming further back.
    pub previous: Option<(Oid, BString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// Index into [`Blame::origins`].
    pub origin: usize,
    /// Line number in the origin's version of the file (1-based).
    pub orig_line: u32,
    /// Line number in the blamed version (1-based).
    pub final_line: u32,
    pub content: BString,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    pub origins: Vec<BlameOrigin>,
    pub lines: Vec<BlameLine>,
}

impl Blame {
    pub fn commit_of(&self, line: &BlameLine) -> &BlameCommit {
        &self.commits[self.origins[line.origin].commit]
    }
}

/// Undoes git's C-style quoting of a path (`"a\tb\303\251"`); other paths
/// are returned as they are. Works on bytes: the result need not be UTF-8.
pub fn unquote_path(raw: &[u8]) -> BString {
    let Some(inner) = raw.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\"")) else {
        return BString::from(raw);
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let b = inner[i];
        if b != b'\\' || i + 1 >= inner.len() {
            out.push(b);
            i += 1;
            continue;
        }
        let e = inner[i + 1];
        let (byte, used) = match e {
            b'a' => (7, 2),
            b'b' => (8, 2),
            b't' => (b'\t', 2),
            b'n' => (b'\n', 2),
            b'v' => (11, 2),
            b'f' => (12, 2),
            b'r' => (b'\r', 2),
            b'"' | b'\\' => (e, 2),
            b'0'..=b'7' if i + 4 <= inner.len() => {
                let digits = &inner[i + 1..i + 4];
                match std::str::from_utf8(digits).ok().and_then(|d| u8::from_str_radix(d, 8).ok()) {
                    Some(v) => (v, 4),
                    None => (b'\\', 1),
                }
            }
            _ => (b'\\', 1),
        };
        out.push(byte);
        i += used;
    }
    BString::from(out)
}

/// Parses `git blame --porcelain` output.
pub fn parse_porcelain(out: &[u8]) -> Result<Blame> {
    let mut blame = Blame::default();
    let mut commit_index: HashMap<Oid, usize> = HashMap::new();
    let mut origin_index: HashMap<(usize, BString), usize> = HashMap::new();
    // The origin of the latest group of each commit (used when git doesn't
    // repeat the filename).
    let mut last_origin: HashMap<usize, usize> = HashMap::new();
    // The line being read: commit, line numbers (header seen, content not
    // yet), and what its header said.
    let mut group: Option<(usize, u32, u32)> = None;
    // Lines left in the current group (its first header gives the count).
    let mut remaining: u32 = 0;
    let mut group_commit: Option<usize> = None;
    let mut filename: Option<BString> = None;
    let mut previous: Option<(Oid, BString)> = None;

    let mut lines = out.split(|&b| b == b'\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        if let Some(content) = line.strip_prefix(b"\t") {
            let Some((commit, orig, fin)) = group.take() else { bail!("blame line without a header") };
            remaining -= 1;
            let origin = match filename.take() {
                Some(name) => {
                    let key = (commit, name.clone());
                    let prev = previous.take();
                    let next = blame.origins.len();
                    let at = *origin_index.entry(key).or_insert(next);
                    if at == next {
                        blame.origins.push(BlameOrigin { commit, filename: name, previous: prev });
                    }
                    at
                }
                None => *last_origin.get(&commit).context("blame output names no file for a commit")?,
            };
            last_origin.insert(commit, origin);
            blame.lines.push(BlameLine { origin, orig_line: orig, final_line: fin, content: BString::from(content) });
            continue;
        }
        if group.is_none() {
            // A group header: "<oid> <orig line> <final line> [<lines>]".
            let mut f = line.split(|&b| b == b' ');
            let id = f.next().and_then(Oid::from_hex).with_context(|| format!("unexpected blame line {:?}", line.as_bstr()))?;
            let num = |s: Option<&[u8]>| s.and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| s.parse::<u32>().ok());
            let (Some(orig), Some(fin)) = (num(f.next()), num(f.next())) else {
                bail!("unexpected blame header {:?}", line.as_bstr());
            };
            if orig == 0 || fin == 0 {
                bail!("blame line numbers start at 1: {:?}", line.as_bstr());
            }
            let count = f.next();
            if f.next().is_some() {
                bail!("unexpected blame header {:?}", line.as_bstr());
            }
            let next = blame.commits.len();
            let commit = *commit_index.entry(id).or_insert(next);
            if commit == next {
                blame.commits.push(BlameCommit {
                    id,
                    author: String::new(),
                    author_mail: String::new(),
                    author_time: 0,
                    summary: String::new(),
                    boundary: false,
                });
            }
            match count {
                // The first line of a group says how many lines it has.
                Some(n) => {
                    let n = num(Some(n)).filter(|&n| n > 0).with_context(|| format!("bad blame header {:?}", line.as_bstr()))?;
                    if remaining != 0 {
                        bail!("a blame group ended early");
                    }
                    remaining = n;
                    group_commit = Some(commit);
                }
                None if remaining == 0 || group_commit != Some(commit) => bail!("blame line outside its group"),
                None => {}
            }
            group = Some((commit, orig, fin));
            continue;
        }
        let commit = group.map(|g| g.0).expect("checked above");
        let (key, value) = line.split_once_str(" ").unwrap_or((line, b""));
        let c = &mut blame.commits[commit];
        let text = || value.to_str_lossy().into_owned();
        match key {
            b"author" => c.author = text(),
            b"author-mail" => c.author_mail = text().trim_start_matches('<').trim_end_matches('>').to_owned(),
            b"author-time" => c.author_time = value.to_str().ok().and_then(|v| v.parse().ok()).context("bad author-time")?,
            b"summary" => c.summary = text(),
            b"boundary" => c.boundary = true,
            b"filename" => {
                if value.is_empty() {
                    bail!("blame output names an empty file");
                }
                filename = Some(unquote_path(value));
            }
            b"previous" => {
                let (oid, path) = value.split_once_str(" ").context("bad previous line")?;
                let oid = Oid::from_hex(oid).context("bad previous commit")?;
                if path.is_empty() {
                    bail!("bad previous line");
                }
                previous = Some((oid, unquote_path(path)));
            }
            // committer, author-tz, … and anything newer git prints.
            _ => {}
        }
    }
    if group.is_some() || remaining != 0 {
        bail!("blame output was cut off");
    }
    Ok(blame)
}

/// Size limits for [`blame_with_limits`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_lines: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { max_bytes: MAX_BYTES, max_lines: MAX_LINES };
}

/// Lines in `bytes` (a last line without a newline counts too).
fn count_lines(bytes: &[u8]) -> usize {
    memchr::memchr_iter(b'\n', bytes).count() + usize::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// Blames `path` at `rev`, or in the work tree (uncommitted lines
/// included) without one. `ignore_whitespace`: `-w`. Refuses files over
/// [`MAX_BYTES`] / [`MAX_LINES`] before running blame.
pub fn blame(git: &GitRunner, path: &[u8], rev: Option<Oid>, ignore_whitespace: bool, cancel: &CancelToken) -> Result<Blame> {
    blame_with_limits(git, path, rev, ignore_whitespace, cancel, Limits::DEFAULT)
}

/// [`blame`] with other limits (tests use small ones).
pub fn blame_with_limits(
    git: &GitRunner,
    path: &[u8],
    rev: Option<Oid>,
    ignore_whitespace: bool,
    cancel: &CancelToken,
    limits: Limits,
) -> Result<Blame> {
    use std::os::unix::ffi::OsStrExt as _;
    let git = git.clone().with_cancel(cancel.clone());
    let os_path = std::ffi::OsStr::from_bytes(path);
    let too_large = |bytes: u64| anyhow::anyhow!("{} is too large to blame ({} MiB; the limit is {} MiB)", path.as_bstr(), bytes >> 20, limits.max_bytes >> 20);
    let too_long = || anyhow::anyhow!("{} has too many lines to blame (the limit is {})", path.as_bstr(), limits.max_lines);
    // Checked before running blame: a huge file costs a lot to blame and
    // to show.
    match rev {
        None => {
            let file = git.workdir().join(os_path);
            let size = std::fs::metadata(&file).with_context(|| format!("{} is not in the work tree", path.as_bstr()))?.len();
            if size > limits.max_bytes {
                return Err(too_large(size));
            }
            if count_lines(&std::fs::read(&file)?) > limits.max_lines {
                return Err(too_long());
            }
        }
        Some(rev) => {
            let mut spec = format!("{}:", rev.to_hex()).into_bytes();
            spec.extend_from_slice(path);
            let spec = std::ffi::OsStr::from_bytes(&spec);
            let out = git.output([std::ffi::OsStr::new("cat-file"), std::ffi::OsStr::new("-s"), spec])?;
            let size: u64 = out.trim().to_str().ok().and_then(|s| s.parse().ok()).context("unexpected file size")?;
            if size > limits.max_bytes {
                return Err(too_large(size));
            }
            // Count lines while reading the blob; stop as soon as too many.
            let mut lines = 0usize;
            git.stream_records([std::ffi::OsStr::new("cat-file"), std::ffi::OsStr::new("blob"), spec], b'\n', cancel, |_| {
                lines += 1;
                lines <= limits.max_lines
            })?;
            if cancel.is_cancelled() {
                return Err(crate::Cancelled.into());
            }
            if lines > limits.max_lines {
                return Err(too_long());
            }
        }
    }
    let mut args: Vec<std::ffi::OsString> = vec!["blame".into(), "--porcelain".into()];
    if ignore_whitespace {
        args.push("-w".into());
    }
    if let Some(rev) = rev {
        args.push(rev.to_hex().into());
    }
    args.push("--".into());
    args.push(os_path.to_owned());
    let out = git.output(&args)?;
    parse_porcelain(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn unquoting() {
        assert_eq!(unquote_path(b"plain name.txt"), "plain name.txt");
        assert_eq!(unquote_path(b"\"tab\\there\""), "tab\there");
        assert_eq!(unquote_path(b"\"nl\\n \\\"q\\\" \\\\\""), "nl\n \"q\" \\");
        assert_eq!(unquote_path(b"\"\\303\\251t\\351\""), BString::from(&b"\xc3\xa9t\xe9"[..]), "octal bytes, not UTF-8");
    }

    #[test]
    fn origins_are_kept_per_path() {
        let out = format!(
            "{A} 1 1 2\nauthor Alice\nauthor-mail <a@x>\nauthor-time 100\nsummary first\nboundary\nfilename old.txt\n\tline one\n\
             {A} 2 2\n\tline two\n\
             {B} 5 3 1\nauthor Bob\nauthor-mail <b@x>\nauthor-time 200\nsome-new-key value\nsummary second\nprevious {A} old.txt\nfilename \"new\\tname.txt\"\n\tline three\n\
             {A} 7 4 1\nprevious {B} other.txt\nfilename copied.txt\n\tfrom another path\n\
             {A} 3 5 1\n\tline five\n"
        );
        let b = parse_porcelain(out.as_bytes()).unwrap();
        assert_eq!(b.commits.len(), 2);
        assert_eq!((b.commits[0].author.as_str(), b.commits[0].author_mail.as_str()), ("Alice", "a@x"));
        assert!(b.commits[0].boundary);
        assert_eq!(b.commits[1].summary, "second");
        // Commit A is the origin under two paths.
        assert_eq!(b.origins.len(), 3);
        assert_eq!(b.origins[0].filename, "old.txt");
        assert_eq!(b.origins[1].filename, "new\tname.txt");
        assert_eq!(b.origins[1].previous, Some((Oid::from_hex(A.as_bytes()).unwrap(), BString::from("old.txt"))));
        assert_eq!((b.origins[2].commit, b.origins[2].filename.as_bytes()), (0, b"copied.txt".as_slice()));
        let origins: Vec<usize> = b.lines.iter().map(|l| l.origin).collect();
        assert_eq!(origins, [0, 0, 1, 2, 2], "an unrepeated filename means the commit's latest origin");
        assert_eq!(b.lines[3].content, "from another path");
        assert_eq!((b.lines[2].orig_line, b.lines[2].final_line), (5, 3));
    }

    #[test]
    fn sha256_and_odd_content() {
        let id = "ab".repeat(32);
        let out = format!("{id} 1 1 2\nauthor X\nauthor-time 1\nsummary s\nfilename f\n\tcr\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(format!("{id} 2 2\n\t").as_bytes());
        bytes.extend_from_slice(b"\xff\xfe not utf-8\n");
        let b = parse_porcelain(&bytes).unwrap();
        assert_eq!(b.commits[0].id.to_hex(), id);
        assert_eq!(b.lines[0].content, "cr\r");
        assert_eq!(b.lines[1].content, BString::from(&b"\xff\xfe not utf-8"[..]));
    }

    #[test]
    fn broken_output_is_an_error() {
        assert!(parse_porcelain(format!("{A} 1 1 1\nauthor X\nfilename f\n").as_bytes()).is_err(), "cut off");
        assert!(parse_porcelain(b"\tcontent without header\n").is_err());
        assert!(parse_porcelain(format!("{A} 1 1 1\nsummary s\n\tno filename\n").as_bytes()).is_err());
        assert!(parse_porcelain(format!("{A} x 1 1\n").as_bytes()).is_err());
        assert!(parse_porcelain(format!("{A} 1 1 1\nauthor-time soon\nfilename f\n\tx\n").as_bytes()).is_err());
        assert_eq!(parse_porcelain(b"").unwrap(), Blame::default(), "an empty file");
        // A group announcing two lines with only one.
        assert!(parse_porcelain(format!("{A} 1 1 2\nfilename f\n\tone\n").as_bytes()).is_err());
        // A continuation line of another commit, or without a group.
        assert!(parse_porcelain(format!("{A} 1 1 1\nfilename f\n\tone\n{B} 2 2\n\ttwo\n").as_bytes()).is_err());
        assert!(parse_porcelain(format!("{A} 0 1 1\nfilename f\n\tline 0\n").as_bytes()).is_err(), "line numbers start at 1");
        assert!(parse_porcelain(format!("{A} 1 1 1\nfilename \n\tx\n").as_bytes()).is_err(), "empty filename");
        assert!(parse_porcelain(format!("{A} 1 1 1\nprevious {B} \nfilename f\n\tx\n").as_bytes()).is_err(), "empty previous path");
        assert!(parse_porcelain(format!("{A} 1 1 0\nfilename f\n").as_bytes()).is_err(), "a group of no lines");
    }
}
