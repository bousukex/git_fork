//! Streaming `git log` for the history list.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use bstr::ByteSlice;

use crate::{
    model::{CommitDetails, CommitSummary, Oid, Signature},
    runner::{CancelToken, GitRunner},
};

/// Field order matters: the subject comes last so a stray `\x1f` inside it
/// cannot shift the other fields. Records are NUL-terminated (`-z`).
const FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%ct%x1f%s";

/// Which commits to list.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Start points; empty means all branches, remote branches, tags and HEAD.
    pub revisions: Vec<String>,
    pub max_count: Option<usize>,
}

#[derive(Default)]
struct Interner(HashMap<Box<[u8]>, Arc<str>>);

impl Interner {
    fn get(&mut self, bytes: &[u8]) -> Arc<str> {
        if let Some(s) = self.0.get(bytes) {
            return s.clone();
        }
        let s: Arc<str> = Arc::from(String::from_utf8_lossy(bytes).as_ref());
        self.0.insert(bytes.into(), s.clone());
        s
    }
}

struct Parser {
    names: Interner,
}

impl Parser {
    fn parse(&mut self, record: &[u8]) -> Option<CommitSummary> {
        // `git log -z` separates records with NUL but a leading newline can
        // appear between records for some formats; tolerate it.
        let record = record.strip_prefix(b"\n").unwrap_or(record);
        let mut f = record.splitn(7, |&b| b == 0x1f);
        let id = Oid::from_hex(f.next()?)?;
        let parents = f
            .next()?
            .split(|&b| b == b' ')
            .filter(|p| !p.is_empty())
            .map(Oid::from_hex)
            .collect::<Option<Vec<_>>>()?;
        let author_name = self.names.get(f.next()?);
        let author_email = self.names.get(f.next()?);
        let author_time = parse_i64(f.next()?)?;
        let commit_time = parse_i64(f.next()?)?;
        let subject = String::from_utf8_lossy(f.next().unwrap_or_default()).into_owned();
        Some(CommitSummary { id, parents, author_name, author_email, author_time, commit_time, subject })
    }
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    b.to_str().ok()?.trim().parse().ok()
}

/// True when HEAD points at a commit (false in a fresh repository).
pub fn head_exists(git: &GitRunner) -> bool {
    git.output(["rev-parse", "--verify", "-q", "HEAD^{commit}"]).is_ok()
}

/// Streams commits in `--date-order` (children always before parents),
/// delivering them in batches. Return `false` from `on_batch` to stop.
pub fn stream(
    git: &GitRunner,
    opts: &LogOptions,
    batch_size: usize,
    cancel: &CancelToken,
    mut on_batch: impl FnMut(Vec<CommitSummary>) -> bool,
) -> Result<()> {
    let mut args: Vec<String> = vec!["log".into(), "--date-order".into(), "-z".into(), FORMAT.into()];
    if let Some(n) = opts.max_count {
        args.push(format!("--max-count={n}"));
    }
    if opts.revisions.is_empty() {
        args.extend(["--branches", "--remotes", "--tags"].map(String::from));
        if head_exists(git) {
            args.push("HEAD".into());
        }
    } else {
        args.extend(opts.revisions.iter().cloned());
    }
    args.push("--".into());

    let mut parser = Parser { names: Interner::default() };
    let mut batch = Vec::with_capacity(batch_size);
    let mut keep_going = true;
    let result = git.stream_records(&args, 0, cancel, |rec| {
        if rec.is_empty() {
            return true;
        }
        match parser.parse(rec) {
            Some(c) => batch.push(c),
            None => log::warn!("unparsable log record: {:?}", rec.as_bstr()),
        }
        if batch.len() >= batch_size {
            keep_going = on_batch(std::mem::replace(&mut batch, Vec::with_capacity(batch_size)));
        }
        keep_going
    });
    // An empty repository with no refs at all makes `git log` fail with
    // "does not have any commits yet"; that's just an empty history.
    if let Err(e) = result {
        if !head_exists(git) && git.output(["for-each-ref", "--count=1"])?.is_empty() {
            return Ok(());
        }
        return Err(e);
    }
    if keep_going && !batch.is_empty() {
        on_batch(batch);
    }
    Ok(())
}

/// Convenience wrapper that collects everything.
pub fn collect(git: &GitRunner, opts: &LogOptions) -> Result<Vec<CommitSummary>> {
    let mut all = Vec::new();
    stream(git, opts, 4096, &CancelToken::default(), |b| {
        all.extend(b);
        true
    })?;
    Ok(all)
}

/// Full metadata and message for one commit.
pub fn details(git: &GitRunner, id: &Oid) -> Result<CommitDetails> {
    let out = git.output([
        "show",
        "-s",
        "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%B",
        &id.to_hex(),
        "--",
    ])?;
    parse_details(&out).ok_or_else(|| anyhow::anyhow!("unexpected `git show` output for {id}"))
}

fn parse_details(out: &[u8]) -> Option<CommitDetails> {
    let mut f = out.splitn(9, |&b| b == 0);
    let id = Oid::from_hex(f.next()?)?;
    let parents = f
        .next()?
        .split(|&b| b == b' ')
        .filter(|p| !p.is_empty())
        .map(Oid::from_hex)
        .collect::<Option<Vec<_>>>()?;
    let sig = |f: &mut dyn Iterator<Item = &[u8]>| -> Option<Signature> {
        Some(Signature {
            name: String::from_utf8_lossy(f.next()?).into_owned(),
            email: String::from_utf8_lossy(f.next()?).into_owned(),
            time: parse_i64(f.next()?)?,
        })
    };
    let author = sig(&mut f)?;
    let committer = sig(&mut f)?;
    let message = String::from_utf8_lossy(f.next().unwrap_or_default()).trim_end().to_owned();
    Some(CommitDetails { id, parents, author, committer, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn parses_record_with_separator_in_subject() {
        let mut p = Parser { names: Interner::default() };
        let rec = format!("{A}\x1f{B}\x1f山田 太郎\x1ftaro@example.com\x1f100\x1f200\x1ffix: a\x1fb");
        let c = p.parse(rec.as_bytes()).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert_eq!(&*c.author_name, "山田 太郎");
        assert_eq!(c.author_time, 100);
        assert_eq!(c.commit_time, 200);
        assert_eq!(c.subject, "fix: a\x1fb");
    }

    #[test]
    fn interns_names() {
        let mut p = Parser { names: Interner::default() };
        let rec = format!("{A}\x1f\x1fX\x1fx@e\x1f1\x1f1\x1fs");
        let c1 = p.parse(rec.as_bytes()).unwrap();
        let c2 = p.parse(rec.as_bytes()).unwrap();
        assert!(Arc::ptr_eq(&c1.author_name, &c2.author_name));
        assert!(c1.parents.is_empty());
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        let mut p = Parser { names: Interner::default() };
        for rec in ["", "zz", &format!("{A}\x1f"), &format!("{A}\x1fnot-an-oid\x1fa\x1fb\x1f1\x1f1\x1fs")] {
            assert!(p.parse(rec.as_bytes()).is_none(), "{rec:?}");
        }
        // Invalid UTF-8 in the subject is replaced, not rejected.
        let mut rec = format!("{A}\x1f\x1fa\x1fb\x1f1\x1f1\x1f").into_bytes();
        rec.extend_from_slice(b"\xff\xfe");
        assert!(p.parse(&rec).unwrap().subject.contains('\u{FFFD}'));
    }

    #[test]
    fn parses_details() {
        let out = format!("{A}\0{B}\0a\0a@e\01\0c\0c@e\02\0subject\n\nbody line\n");
        let d = parse_details(out.as_bytes()).unwrap();
        assert_eq!(d.author.name, "a");
        assert_eq!(d.committer.time, 2);
        assert_eq!(d.message, "subject\n\nbody line");
    }
}
