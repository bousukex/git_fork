//! Changed-file lists and single-file unified diffs.

use std::{ffi::OsString, os::unix::ffi::OsStringExt};

use anyhow::Result;
use bstr::{BString, ByteSlice};

use crate::{
    model::{ChangeKind, ChangedFile, DiffLine, FileDiff, Hunk, LineKind, Oid},
    runner::GitRunner,
};

/// The raw diff, always: no external diff, no textconv (a program that
/// could do anything, and whose output lines couldn't be staged).
const DIFF_FLAGS: [&str; 4] = ["--no-color", "--no-ext-diff", "--no-textconv", "--find-renames"];

/// What a file diff is computed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    /// A commit compared with `parent` (its first parent), or with nothing
    /// for a root commit.
    Commit { id: Oid, parent: Option<Oid> },
    /// Work tree vs index.
    Unstaged,
    /// Index vs HEAD.
    Staged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    pub context_lines: u32,
    pub ignore_whitespace: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self { context_lines: 3, ignore_whitespace: false }
    }
}

fn os(path: &BString) -> OsString {
    OsString::from_vec(path.to_vec())
}

/// Files changed by `id` relative to `parent` (or all files for a root commit).
pub fn commit_files(git: &GitRunner, id: Oid, parent: Option<Oid>) -> Result<Vec<ChangedFile>> {
    let mut args: Vec<String> =
        ["diff-tree", "-r", "-z", "--raw", "--no-commit-id", "--find-renames"].map(String::from).to_vec();
    match parent {
        Some(p) => args.extend([p.to_hex(), id.to_hex()]),
        None => args.extend(["--root".into(), id.to_hex()]),
    }
    Ok(parse_raw(&git.output(&args)?))
}

/// Parses `--raw -z` output: `:<modes> <oids> <status>\0<path>\0[<path>\0]`.
pub fn parse_raw(out: &[u8]) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut it = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(meta) = it.next() {
        let Some(meta) = meta.strip_prefix(b":") else { continue };
        let fields: Vec<&[u8]> = meta.split(|&b| b == b' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let is_submodule = fields[0] == b"160000" || fields[1] == b"160000";
        let status = fields[4].first().copied().unwrap_or(b'M');
        let kind = match status {
            b'A' => ChangeKind::Added,
            b'D' => ChangeKind::Deleted,
            b'R' => ChangeKind::Renamed,
            b'C' => ChangeKind::Copied,
            b'T' => ChangeKind::TypeChanged,
            b'U' => ChangeKind::Conflicted,
            _ => ChangeKind::Modified,
        };
        let (orig_path, path) = if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            let (Some(src), Some(dst)) = (it.next(), it.next()) else { break };
            (Some(BString::from(src)), BString::from(dst))
        } else {
            let Some(p) = it.next() else { break };
            (None, BString::from(p))
        };
        files.push(ChangedFile { path, orig_path, kind, is_submodule });
    }
    files
}

/// Header marking a [`FileDiff`] built by [`conflict_view`].
pub const CONFLICT_HEADER: &str = "conflict";
/// Header for a conflicted file that no longer exists in the work tree.
pub const CONFLICT_DELETED_HEADER: &str = "conflict: deleted";

/// Shows a conflicted work tree file as a diff-like document: the HEAD side
/// of each conflict as deleted lines, the incoming side as added lines, and
/// conflict markers (and a diff3 base section) as plain notes.
pub fn conflict_view(content: Option<&[u8]>) -> FileDiff {
    let Some(content) = content else {
        return FileDiff { header: vec![CONFLICT_DELETED_HEADER.into()], ..Default::default() };
    };
    if content[..content.len().min(8000)].contains(&0) {
        return FileDiff { header: vec![CONFLICT_HEADER.into()], binary: true, ..Default::default() };
    }
    #[derive(PartialEq)]
    enum Part {
        Outside,
        Ours,
        Base,
        Theirs,
    }
    let mut part = Part::Outside;
    let mut lines = Vec::new();
    let body = content.strip_suffix(b"\n").unwrap_or(content);
    for (i, line) in body.split(|&b| b == b'\n').enumerate() {
        let n = Some(i as u32 + 1);
        let marker = |p: &[u8]| line.starts_with(p) && (line.len() == 7 || line[7] == b' ');
        let kind = if marker(b"<<<<<<<") {
            part = Part::Ours;
            LineKind::NoNewline
        } else if marker(b"|||||||") && part == Part::Ours {
            part = Part::Base;
            LineKind::NoNewline
        } else if line == b"=======" && matches!(part, Part::Ours | Part::Base) {
            part = Part::Theirs;
            LineKind::NoNewline
        } else if marker(b">>>>>>>") && part == Part::Theirs {
            part = Part::Outside;
            LineKind::NoNewline
        } else {
            match part {
                Part::Outside => LineKind::Context,
                Part::Ours => LineKind::Deleted,
                Part::Base => LineKind::NoNewline,
                Part::Theirs => LineKind::Added,
            }
        };
        lines.push(DiffLine { kind, old_no: None, new_no: n, content: BString::from(line) });
    }
    let count = lines.len() as u32;
    FileDiff {
        header: vec![CONFLICT_HEADER.into()],
        binary: false,
        lfs: None,
        hunks: vec![Hunk {
            old_start: 1,
            old_lines: count,
            new_start: 1,
            new_lines: count,
            section: "conflicted file".into(),
            lines,
        }],
    }
}

/// Diff of a single file.
pub fn file_diff(git: &GitRunner, source: DiffSource, file: &ChangedFile, opts: DiffOptions) -> Result<FileDiff> {
    if source == DiffSource::Unstaged && file.kind == ChangeKind::Conflicted {
        let path = git.workdir().join(os(&file.path));
        return Ok(conflict_view(std::fs::read(path).ok().as_deref()));
    }
    let mut args: Vec<OsString> = Vec::new();
    let mut ok_codes: &[i32] = &[];
    let context = format!("-U{}", opts.context_lines);
    match (source, file.kind) {
        (DiffSource::Unstaged, ChangeKind::Untracked) => {
            args.extend(["diff", "--no-index"].map(OsString::from));
            ok_codes = &[1];
        }
        (DiffSource::Commit { id, parent }, _) => {
            args.extend(["diff-tree", "-p", "--no-commit-id"].map(OsString::from));
            args.extend(DIFF_FLAGS.map(OsString::from));
            match parent {
                Some(p) => args.extend([p.to_hex().into(), id.to_hex().into()]),
                None => args.extend([OsString::from("--root"), id.to_hex().into()]),
            }
        }
        (DiffSource::Unstaged, _) => args.extend(["diff"].map(OsString::from)),
        (DiffSource::Staged, _) => args.extend(["diff", "--cached"].map(OsString::from)),
    }
    if !matches!(source, DiffSource::Commit { .. }) {
        args.extend(DIFF_FLAGS.map(OsString::from));
    }
    args.push(context.into());
    if opts.ignore_whitespace {
        args.push("--ignore-all-space".into());
    }
    args.push("--".into());
    if source == DiffSource::Unstaged && file.kind == ChangeKind::Untracked {
        args.push("/dev/null".into());
    } else if let Some(orig) = &file.orig_path {
        args.push(os(orig));
    }
    args.push(os(&file.path));
    let mut diff = parse_patch(&git.output_with(&args, ok_codes)?);
    // Pointer files of Git LFS: what they stand for (when they may be).
    if crate::lfs::maybe_pointer_diff(&diff) {
        match crate::lfs::view(git, source, file) {
            Ok(view) => diff.lfs = view,
            // Not taken for "no LFS": the diff shows as it is.
            Err(e) => log::warn!("couldn't tell whether {} is a Git LFS file: {e:#}", file.path),
        }
    }
    Ok(diff)
}

fn parse_range(s: &[u8]) -> Option<(u32, u32)> {
    let s = s.to_str().ok()?;
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a,b +c,d @@ section`
fn parse_hunk_header(line: &[u8]) -> Option<Hunk> {
    let rest = line.strip_prefix(b"@@ -")?;
    let end = rest.find(b" @@")?;
    let (ranges, section) = (&rest[..end], &rest[end + 3..]);
    let (old, new) = ranges.split_once_str(b" +")?;
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        section: BString::from(section.strip_prefix(b" ").unwrap_or(section)),
        lines: Vec::new(),
    })
}

/// Parses the patch of one file. Unknown lines are kept in `header` and
/// malformed hunks end the hunk instead of failing the whole diff.
pub fn parse_patch(out: &[u8]) -> FileDiff {
    let mut diff = FileDiff::default();
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut in_hunk = false;
    let mut lines = out.split(|&b| b == b'\n').peekable();
    while let Some(line) = lines.next() {
        // The final split element after a trailing newline is empty.
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        if line.starts_with(b"@@ ")
            && let Some(h) = parse_hunk_header(line)
        {
            old_no = h.old_start;
            new_no = h.new_start;
            diff.hunks.push(h);
            in_hunk = true;
            continue;
        }
        if line.starts_with(b"diff --git ") || line.starts_with(b"diff --cc ") {
            in_hunk = false;
        }
        if in_hunk {
            let hunk = diff.hunks.last_mut().expect("in hunk");
            let (kind, content) = match line.first() {
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'-') => (LineKind::Deleted, &line[1..]),
                Some(b' ') => (LineKind::Context, &line[1..]),
                Some(b'\\') => (LineKind::NoNewline, line),
                None => (LineKind::Context, line),
                Some(_) => {
                    in_hunk = false;
                    diff.header.push(BString::from(line));
                    continue;
                }
            };
            let (o, n) = match kind {
                LineKind::Context => {
                    old_no += 1;
                    new_no += 1;
                    (Some(old_no - 1), Some(new_no - 1))
                }
                LineKind::Added => {
                    new_no += 1;
                    (None, Some(new_no - 1))
                }
                LineKind::Deleted => {
                    old_no += 1;
                    (Some(old_no - 1), None)
                }
                LineKind::NoNewline => (None, None),
            };
            hunk.lines.push(DiffLine { kind, old_no: o, new_no: n, content: BString::from(content) });
        } else {
            if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
                diff.binary = true;
            }
            if !(line.starts_with(b"--- ") || line.starts_with(b"+++ ") || line.starts_with(b"diff --git ")) {
                diff.header.push(BString::from(line));
            }
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_patch() {
        let patch = b"diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@ fn main()\n a\n-b\n+B\n c\n@@ -10 +10,2 @@\n x\n+y\n\\ No newline at end of file\n";
        let d = parse_patch(patch);
        assert!(!d.binary);
        assert_eq!(d.header, vec![BString::from("index 1..2 100644")]);
        assert_eq!(d.hunks.len(), 2);
        let h = &d.hunks[0];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (1, 3, 1, 3));
        assert_eq!(h.section, "fn main()");
        let kinds: Vec<_> = h.lines.iter().map(|l| (l.kind, l.old_no, l.new_no)).collect();
        assert_eq!(
            kinds,
            [
                (LineKind::Context, Some(1), Some(1)),
                (LineKind::Deleted, Some(2), None),
                (LineKind::Added, None, Some(2)),
                (LineKind::Context, Some(3), Some(3)),
            ]
        );
        let h2 = &d.hunks[1];
        assert_eq!((h2.old_lines, h2.new_lines), (1, 2));
        assert_eq!(h2.lines.last().unwrap().kind, LineKind::NoNewline);
        assert_eq!(d.stats(), (2, 1));
    }

    #[test]
    fn detects_binary() {
        let d = parse_patch(b"diff --git a/x.png b/x.png\nnew file mode 100644\nBinary files /dev/null and b/x.png differ\n");
        assert!(d.binary);
        assert!(d.hunks.is_empty());
        assert!(!d.is_empty());
    }

    #[test]
    fn keeps_invalid_utf8_and_survives_garbage() {
        let d = parse_patch(b"@@ -1 +1 @@\n-\xff\n+ok\n@@ broken\nzzz\n");
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(d.hunks[0].lines[0].content.as_bytes(), b"\xff");
        let _ = parse_patch(b"@@ -x,y +1 @@\n+a\n");
        let _ = parse_patch(b"");
    }

    #[test]
    fn parses_raw_records() {
        let raw = b":100644 100644 aaaa bbbb M\0src/a.rs\0:000000 100644 0000 cccc A\0new.txt\0:100644 100644 dddd eeee R087\0old.txt\0renamed.txt\0:160000 160000 1111 2222 M\0sub\0";
        let files = parse_raw(raw);
        assert_eq!(files.len(), 4);
        assert_eq!(files[1].kind, ChangeKind::Added);
        assert_eq!(files[2].kind, ChangeKind::Renamed);
        assert_eq!(files[2].orig_path.as_ref().unwrap(), "old.txt");
        assert_eq!(files[2].path, "renamed.txt");
        assert!(files[3].is_submodule);
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn conflict_view_marks_sides() {
        let file = b"top\n<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> topic\nbottom\n";
        let d = conflict_view(Some(file));
        let kinds: Vec<_> = d.hunks[0].lines.iter().map(|l| l.kind).collect();
        use LineKind::*;
        assert_eq!(kinds, [Context, NoNewline, Deleted, NoNewline, NoNewline, NoNewline, Added, NoNewline, Context]);
        assert_eq!(d.header, vec![BString::from(CONFLICT_HEADER)]);
        assert_eq!(d.hunks[0].lines[8].new_no, Some(9));
        assert!(conflict_view(Some(b"\0bin")).binary);
        assert_eq!(conflict_view(None).header, vec![BString::from(CONFLICT_DELETED_HEADER)]);
        // A line that merely starts with the marker text is content.
        let d = conflict_view(Some(b"<<<<<<<<< not a marker\n"));
        assert_eq!(d.hunks[0].lines[0].kind, Context);
    }
}
