//! Partial patches for staging, unstaging or discarding selected lines.
//!
//! A selection of `+`/`-` lines from a displayed diff is turned into a
//! patch and handed to `git apply`, which applies it atomically (all hunks
//! or nothing):
//!
//! | mode    | applied to | unselected `+` | unselected `-` | git apply flags       |
//! |---------|------------|----------------|----------------|-----------------------|
//! | Stage   | index      | dropped        | context        | `--cached`            |
//! | Unstage | index      | context        | dropped        | `--cached --reverse`  |
//! | Discard | work tree  | context        | dropped        | `--reverse`           |
//!
//! Forward patches must match the diff's old side (the index); reverse
//! patches must match its new side (the index for staged diffs, the work
//! tree for unstaged ones).

use anyhow::{Result, bail};
use bstr::BString;

use crate::{
    diff::{self, DiffOptions, DiffSource},
    model::{ChangeKind, ChangedFile, FileDiff, LineKind},
    runner::GitRunner,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchMode {
    /// Selected lines of an unstaged diff go into the index.
    Stage,
    /// Selected lines of a staged diff are taken out of the index.
    Unstage,
    /// Selected lines of an unstaged diff are reverted in the work tree.
    Discard,
}

impl PatchMode {
    fn reverse(self) -> bool {
        !matches!(self, PatchMode::Stage)
    }

    fn apply_args(self, zero_context: bool) -> Vec<&'static str> {
        let mut args = match self {
            PatchMode::Stage => vec!["apply", "--cached"],
            PatchMode::Unstage => vec!["apply", "--cached", "--reverse"],
            PatchMode::Discard => vec!["apply", "--reverse"],
        };
        args.extend(["--recount", "--whitespace=nowarn"]);
        if zero_context {
            // Needed when a hunk has no context lines (e.g. a one-line file);
            // `apply` checks beforehand that the diff is still current.
            args.push("--unidiff-zero");
        }
        args.push("-");
        args
    }

    /// The diff the selection must have been taken from.
    fn source(self) -> DiffSource {
        match self {
            PatchMode::Stage | PatchMode::Discard => DiffSource::Unstaged,
            PatchMode::Unstage => DiffSource::Staged,
        }
    }
}

/// A generated patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub bytes: Vec<u8>,
    /// Some hunk has no context lines, so `git apply` can't anchor it.
    pub zero_context: bool,
}

/// C-style quoting as used in git patch headers; always quoting is valid
/// and handles spaces, tabs, quotes and non-ASCII names uniformly.
fn quote_path(prefix: &str, path: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    for &b in prefix.as_bytes().iter().chain(path) {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x20..=0x7e => out.push(b),
            _ => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
        }
    }
    out.push(b'"');
    out
}

/// Builds a patch containing only the lines for which `selected(hunk,
/// line)` is true. Returns `Ok(None)` if no `+`/`-` line is selected, and an
/// error for selections that can't be expressed as a valid patch (a line
/// added after a last line that has no newline, without the change that
/// adds that newline: git would glue the two lines together).
pub fn build_patch(
    path: &BString,
    diff: &FileDiff,
    mode: PatchMode,
    selected: impl Fn(usize, usize) -> bool,
) -> Result<Option<Patch>> {
    let reverse = mode.reverse();
    let mut body = Vec::new();
    let mut zero_context = false;
    // How far the side being rewritten has shifted due to earlier hunks.
    let mut offset: i64 = 0;
    for (hi, hunk) in diff.hunks.iter().enumerate() {
        let mut lines = Vec::new();
        let (mut old_count, mut new_count) = (0u32, 0u32);
        let mut any = false;
        let mut has_context = false;
        let mut prev_kept = false;
        // Marker kind of the previously emitted line, and whether the old /
        // new side already ended without a newline.
        let mut prev_marker = 0u8;
        let (mut old_ended, mut new_ended) = (false, false);
        for (li, line) in hunk.lines.iter().enumerate() {
            let sel = selected(hi, li);
            let marker = match line.kind {
                LineKind::Context => Some(b' '),
                LineKind::Added if sel => Some(b'+'),
                LineKind::Deleted if sel => Some(b'-'),
                // Unselected lines: present on the side the patch is
                // matched against become context, the others vanish.
                LineKind::Added if reverse => Some(b' '),
                LineKind::Deleted if !reverse => Some(b' '),
                LineKind::Added | LineKind::Deleted => None,
                LineKind::NoNewline => {
                    if prev_kept {
                        lines.extend_from_slice(&line.content);
                        lines.push(b'\n');
                        old_ended |= matches!(prev_marker, b' ' | b'-');
                        new_ended |= matches!(prev_marker, b' ' | b'+');
                    }
                    continue;
                }
            };
            prev_kept = marker.is_some();
            let Some(m) = marker else { continue };
            if (old_ended && matches!(m, b' ' | b'-')) || (new_ended && matches!(m, b' ' | b'+')) {
                bail!(
                    "this selection depends on the missing newline at the end of the file; \
                     also select the neighbouring changed lines"
                );
            }
            prev_marker = m;
            any |= sel && line.kind != LineKind::Context;
            match m {
                b' ' => {
                    old_count += 1;
                    new_count += 1;
                    has_context = true;
                }
                b'+' => new_count += 1,
                _ => old_count += 1,
            }
            lines.push(m);
            lines.extend_from_slice(&line.content);
            lines.push(b'\n');
        }
        if !any {
            continue;
        }
        zero_context |= !has_context;
        // The side matched against keeps its original range. The other
        // side's start follows from it; an empty range names the line
        // before it (unified diff convention), which matters without context.
        let first_line = |start: i64, count: u32| if count == 0 { start + 1 } else { start };
        let empty_adjust = |start: i64, count: u32| if count == 0 { start - 1 } else { start };
        let (old_start, new_start) = if reverse {
            let new_start = hunk.new_start as i64;
            (empty_adjust(first_line(new_start, new_count) - offset, old_count), new_start)
        } else {
            let old_start = hunk.old_start as i64;
            (old_start, empty_adjust(first_line(old_start, old_count) + offset, new_count))
        };
        offset += new_count as i64 - old_count as i64;
        body.extend_from_slice(
            format!("@@ -{},{} +{},{} @@\n", old_start.max(0), old_count, new_start.max(0), new_count).as_bytes(),
        );
        body.extend_from_slice(&lines);
    }
    if body.is_empty() {
        return Ok(None);
    }
    let mut patch = b"diff --git ".to_vec();
    patch.extend_from_slice(&quote_path("a/", path));
    patch.push(b' ');
    patch.extend_from_slice(&quote_path("b/", path));
    patch.extend_from_slice(b"\n--- ");
    patch.extend_from_slice(&quote_path("a/", path));
    patch.extend_from_slice(b"\n+++ ");
    patch.extend_from_slice(&quote_path("b/", path));
    patch.push(b'\n');
    patch.extend_from_slice(&body);
    Ok(Some(Patch { bytes: patch, zero_context }))
}

/// Builds and applies a partial patch.
///
/// First the diff is recomputed and must still equal `diff` (the one the
/// selection was made on), so a selection never lands on changed content;
/// this also makes context-free hunks safe to apply. `git apply` itself is
/// atomic, so on any error neither the index nor the work tree changes.
pub fn apply(
    git: &GitRunner,
    path: &BString,
    diff: &FileDiff,
    opts: DiffOptions,
    mode: PatchMode,
    selected: impl Fn(usize, usize) -> bool,
) -> Result<()> {
    let Some(patch) = build_patch(path, diff, mode, selected)? else {
        bail!("no changed lines selected");
    };
    let file = ChangedFile { path: path.clone(), orig_path: None, kind: ChangeKind::Modified, is_submodule: false };
    let current = diff::file_diff(git, mode.source(), &file, opts)?;
    if current.hunks != diff.hunks {
        bail!("{path} changed since its diff was shown; review the updated diff and try again");
    }
    git.output_with_input(mode.apply_args(patch.zero_context), &patch.bytes, &[])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_patch;

    #[test]
    fn quotes_paths() {
        assert_eq!(quote_path("a/", b"x y.txt"), b"\"a/x y.txt\"");
        assert_eq!(quote_path("b/", "日".as_bytes()), b"\"b/\\346\\227\\245\"");
        assert_eq!(quote_path("a/", b"q\"\\\t"), b"\"a/q\\\"\\\\\\t\"");
    }

    #[test]
    fn stage_one_added_line() {
        let diff = parse_patch(b"@@ -1,3 +1,5 @@\n a\n+x\n b\n+y\n c\n");
        // Select only "+y" (hunk 0, line 3).
        let p = build_patch(&"f".into(), &diff, PatchMode::Stage, |h, l| (h, l) == (0, 3)).unwrap().unwrap();
        assert!(!p.zero_context);
        let text = String::from_utf8(p.bytes).unwrap();
        assert!(text.ends_with("@@ -1,3 +1,4 @@\n a\n b\n+y\n c\n"), "{text}");
    }

    #[test]
    fn unstage_mirrors_unselected_lines() {
        let diff = parse_patch(b"@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");
        // Unstage only the deletion: "+B" stays in the index, so it is context.
        let p = build_patch(&"f".into(), &diff, PatchMode::Unstage, |_, l| l == 1).unwrap().unwrap();
        let text = String::from_utf8(p.bytes).unwrap();
        assert!(text.ends_with("@@ -1,4 +1,3 @@\n a\n-b\n B\n c\n"), "{text}");
    }

    #[test]
    fn context_free_hunks_use_unified_diff_conventions() {
        // Appending to an empty file: old range is empty (line 0).
        let diff = parse_patch(b"@@ -0,0 +1,2 @@\n+x\n+y\n");
        let p = build_patch(&"f".into(), &diff, PatchMode::Stage, |_, l| l == 0).unwrap().unwrap();
        assert!(p.zero_context);
        assert!(String::from_utf8(p.bytes).unwrap().ends_with("@@ -0,0 +1,1 @@\n+x\n"));
        // Unstaging everything from a one-line file leaves the new side empty.
        let diff = parse_patch(b"@@ -1 +0,0 @@\n-only\n");
        let p = build_patch(&"f".into(), &diff, PatchMode::Unstage, |_, _| true).unwrap().unwrap();
        assert!(String::from_utf8(p.bytes).unwrap().ends_with("@@ -1,1 +0,0 @@\n-only\n"));
        // Replacing the single line of a file.
        let diff = parse_patch(b"@@ -1 +1 @@\n-a\n+b\n");
        let p = build_patch(&"f".into(), &diff, PatchMode::Stage, |_, _| true).unwrap().unwrap();
        assert!(p.zero_context);
        assert!(String::from_utf8(p.bytes).unwrap().ends_with("@@ -1,1 +1,1 @@\n-a\n+b\n"));
    }

    #[test]
    fn refuses_lines_after_a_missing_newline() {
        // "b" gains a newline and "c" is appended.
        let diff = parse_patch(b"@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+b\n+c\n");
        let err = build_patch(&"f".into(), &diff, PatchMode::Stage, |_, l| l == 4).unwrap_err();
        assert!(err.to_string().contains("missing newline"), "{err}");
        // With the newline change included it is fine.
        assert!(build_patch(&"f".into(), &diff, PatchMode::Stage, |_, _| true).unwrap().is_some());
    }

    #[test]
    fn skips_hunks_without_selection_and_shifts_later_ones() {
        let diff = parse_patch(b"@@ -1,2 +1,3 @@\n a\n+x\n b\n@@ -10,2 +11,3 @@\n p\n+q\n r\n");
        let all = build_patch(&"f".into(), &diff, PatchMode::Stage, |_, _| true).unwrap().unwrap();
        let text = String::from_utf8(all.bytes).unwrap();
        assert!(text.contains("@@ -1,2 +1,3 @@") && text.contains("@@ -10,2 +11,3 @@"), "{text}");
        let second = build_patch(&"f".into(), &diff, PatchMode::Stage, |h, _| h == 1).unwrap().unwrap();
        let text = String::from_utf8(second.bytes).unwrap();
        assert!(!text.contains("@@ -1,") && text.contains("@@ -10,2 +10,3 @@"), "{text}");
        assert!(build_patch(&"f".into(), &diff, PatchMode::Stage, |_, _| false).unwrap().is_none());
        // Context-only selection is not a change.
        assert!(build_patch(&"f".into(), &diff, PatchMode::Stage, |_, l| l == 0).unwrap().is_none());
    }
}
