//! A diff flattened into display rows, prepared once off the paint path.

use std::sync::Arc;

use tine_core::{
    CancelToken,
    highlight::{Span, highlight_streams, syntax_for},
    model::{FileDiff, LineKind},
};

use crate::settings::DocOptions;

/// Lines longer than this are cut (minified files would otherwise produce
/// megapixel-wide galleys).
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Hunk,
    Context,
    Added,
    Deleted,
    NoNewline,
}

#[derive(Debug, Clone)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    /// Index of the hunk this row belongs to.
    pub hunk: usize,
    /// Index into the hunk's lines (`None` for the hunk header row).
    pub line: Option<usize>,
    /// Syntax highlighting of `text` (empty: plain).
    pub spans: Vec<Span>,
}

impl DiffRow {
    /// Added or deleted (the rows that can be staged individually).
    pub fn is_change(&self) -> bool {
        matches!(self.kind, RowKind::Added | RowKind::Deleted)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffDoc {
    pub rows: Vec<DiffRow>,
    pub binary: bool,
    pub header: Vec<String>,
    /// Widest row in monospace columns (wide CJK characters count twice).
    pub max_cols: usize,
    pub added: usize,
    pub deleted: usize,
    /// Digits needed for line numbers.
    pub number_width: usize,
    /// The parsed diff, kept with its raw bytes for building partial patches
    /// (shared by the plain and highlighted docs: raw lines can be huge).
    pub source: Arc<FileDiff>,
}

/// Terminal-style column width: East Asian wide/fullwidth characters take two.
pub fn char_cols(c: char) -> usize {
    let c = c as u32;
    let wide = matches!(c,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 |
        0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// A line as shown: lossy UTF-8, tabs expanded, CR dropped, cut when very
/// long; with its width in monospace columns.
pub fn display_text(bytes: &[u8], tab_width: u8) -> (String, usize) {
    let tab_width = usize::from(tab_width.max(1));
    // Only the start of a huge line (minified code…) is shown: decode no
    // more than can make it. A character takes at most four bytes (an
    // invalid byte becomes one), so a longer line has more characters than
    // shown and still ends in the ellipsis.
    let bytes = &bytes[..bytes.len().min(MAX_LINE_CHARS * 4 + 8)];
    let s = String::from_utf8_lossy(bytes);
    // The limit counts characters (not UTF-8 bytes: Japanese text would be
    // cut at a third of it).
    let mut out = String::with_capacity(s.len().min(MAX_LINE_CHARS * 4 + 4));
    let mut cols = 0;
    for (n, c) in s.chars().enumerate() {
        if n >= MAX_LINE_CHARS {
            out.push('…');
            cols += 1;
            break;
        }
        match c {
            '\t' => {
                let n = tab_width - cols % tab_width;
                out.extend(std::iter::repeat_n(' ', n));
                cols += n;
            }
            '\r' => {}
            c => {
                out.push(c);
                cols += char_cols(c);
            }
        }
    }
    (out, cols)
}

/// Highlighting limits (beyond them, diffs and blame stay plain).
pub const HIGHLIGHT_MAX_BYTES: usize = 4 << 20;
pub const HIGHLIGHT_MAX_DIFF_ROWS: usize = 20_000;
pub const HIGHLIGHT_MAX_SPANS: usize = 500_000;

impl DiffDoc {
    pub fn new(diff: FileDiff, opts: DocOptions) -> Self {
        Self::build(diff, opts, &CancelToken::default()).expect("not cancelled")
    }

    /// Prepares `diff` for display; `None` once `cancel` fires.
    pub fn build(diff: FileDiff, opts: DocOptions, cancel: &CancelToken) -> Option<Self> {
        let (added, deleted) = diff.stats();
        let source = Arc::new(diff);
        let diff = &*source;
        let mut doc = DiffDoc {
            binary: diff.binary,
            header: diff.header.iter().map(|h| String::from_utf8_lossy(h).into_owned()).collect(),
            added,
            deleted,
            source: source.clone(),
            ..Default::default()
        };
        let mut max_no = 0;
        for (hi, hunk) in diff.hunks.iter().enumerate() {
            let section = String::from_utf8_lossy(&hunk.section);
            let text = format!(
                "@@ -{},{} +{},{} @@ {}",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, section
            );
            doc.max_cols = doc.max_cols.max(text.chars().map(char_cols).sum());
            doc.rows.push(DiffRow { kind: RowKind::Hunk, old_no: None, new_no: None, text, hunk: hi, line: None, spans: Vec::new() });
            for (li, line) in hunk.lines.iter().enumerate() {
                if li % 1024 == 1023 && cancel.is_cancelled() {
                    return None;
                }
                let kind = match line.kind {
                    LineKind::Context => RowKind::Context,
                    LineKind::Added => RowKind::Added,
                    LineKind::Deleted => RowKind::Deleted,
                    LineKind::NoNewline => RowKind::NoNewline,
                };
                let (text, cols) = display_text(&line.content, opts.tab_width);
                doc.max_cols = doc.max_cols.max(cols);
                max_no = max_no.max(line.old_no.unwrap_or(0)).max(line.new_no.unwrap_or(0));
                doc.rows.push(DiffRow {
                    kind,
                    old_no: line.old_no,
                    new_no: line.new_no,
                    text,
                    hunk: hi,
                    line: Some(li),
                    spans: Vec::new(),
                });
            }
        }
        doc.number_width = max_no.max(1).ilog10() as usize + 1;
        (!cancel.is_cancelled()).then_some(doc)
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && !self.binary
    }

    /// A copy with syntax highlighting for the file at `path`, or `None`
    /// (unknown language, too large, cancelled). Each hunk is highlighted
    /// as two streams — the old side (context and deleted lines) and the
    /// new side (context and added lines) — starting fresh at the hunk;
    /// context lines take the new side's colors.
    pub fn highlighted(&self, path: &[u8], cancel: &CancelToken) -> Option<DiffDoc> {
        let lines = self.rows.iter().filter(|r| r.line.is_some());
        if self.rows.len() > HIGHLIGHT_MAX_DIFF_ROWS || lines.map(|r| r.text.len()).sum::<usize>() > HIGHLIGHT_MAX_BYTES {
            return None;
        }
        let first = self.rows.iter().find(|r| matches!(r.kind, RowKind::Context | RowKind::Added)).map(|r| r.text.as_str());
        let syntax = syntax_for(path, first)?;
        // Streams 2h (old side) and 2h + 1 (new side) of hunk h; where
        // each row's line is in them.
        let hunks = self.rows.iter().map(|r| r.hunk + 1).max().unwrap_or(0);
        let mut streams: Vec<Vec<&str>> = vec![Vec::new(); hunks * 2];
        // Context takes the new side's colors: the old side is parsed only
        // as far as its last deleted line (often not at all).
        let mut last_deleted = vec![None; hunks];
        for (i, r) in self.rows.iter().enumerate() {
            if r.kind == RowKind::Deleted {
                last_deleted[r.hunk] = Some(i);
            }
        }
        let mut at: Vec<Option<(usize, usize)>> = Vec::with_capacity(self.rows.len());
        for (i, r) in self.rows.iter().enumerate() {
            let (old, new) = (2 * r.hunk, 2 * r.hunk + 1);
            at.push(match r.kind {
                RowKind::Context => {
                    if last_deleted[r.hunk].is_some_and(|d| i < d) {
                        streams[old].push(&r.text);
                    }
                    streams[new].push(&r.text);
                    Some((new, streams[new].len() - 1))
                }
                RowKind::Added => {
                    streams[new].push(&r.text);
                    Some((new, streams[new].len() - 1))
                }
                RowKind::Deleted => {
                    streams[old].push(&r.text);
                    Some((old, streams[old].len() - 1))
                }
                RowKind::Hunk | RowKind::NoNewline => None,
            });
        }
        let mut spans = highlight_streams(syntax, &streams, cancel, HIGHLIGHT_MAX_SPANS)?;
        // The raw diff is shared, not copied.
        let mut doc = self.clone();
        for (row, at) in doc.rows.iter_mut().zip(at) {
            if let Some((stream, line)) = at {
                row.spans = std::mem::take(&mut spans[stream][line]);
            }
        }
        Some(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tabs_and_measures_wide_chars() {
        assert_eq!(display_text(b"a\tb", 4), ("a   b".into(), 5));
        assert_eq!(display_text(b"a\tb", 8), ("a       b".into(), 9));
        assert_eq!(display_text(b"\tb", 2), ("  b".into(), 3));
        assert_eq!(display_text("日本".as_bytes(), 4).1, 4);
        assert_eq!(display_text(b"x\r", 4).0, "x");
        let long = vec![b'x'; 5000];
        assert!(display_text(&long, 4).0.ends_with('…'));
        // 2000 Japanese characters fit; the limit isn't in bytes.
        let japanese = "日".repeat(MAX_LINE_CHARS);
        assert!(!display_text(japanese.as_bytes(), 4).0.ends_with('…'));
        let over = "日".repeat(MAX_LINE_CHARS + 1);
        let (cut, _) = display_text(over.as_bytes(), 4);
        assert_eq!(cut.chars().count(), MAX_LINE_CHARS + 1, "2000 characters and the ellipsis");
        // Invalid UTF-8 and tabs count as what is shown.
        let mut mixed = vec![0xff; 10];
        mixed.extend_from_slice(b"\tx");
        assert_eq!(display_text(&mixed, 4).0, format!("{}  x", "\u{fffd}".repeat(10)));
        // A huge line is cut the same way, whatever its characters.
        for unit in ["x", "日", "\u{1f600}"] {
            let (cut, _) = display_text(unit.repeat(5_000_000).as_bytes(), 4);
            assert_eq!(cut, format!("{}…", unit.repeat(MAX_LINE_CHARS)));
        }
        let mut invalid = vec![0xffu8; 3_000_000];
        invalid.extend_from_slice("日本".as_bytes());
        assert_eq!(display_text(&invalid, 4).0, format!("{}…", "\u{fffd}".repeat(MAX_LINE_CHARS)));
        let exact = "日".repeat(MAX_LINE_CHARS);
        assert_eq!(display_text(exact.as_bytes(), 4).0, exact, "exactly the limit: no ellipsis");
    }

    use tine_core::{diff::parse_patch, highlight::Token};

    fn doc(patch: &str, opts: DocOptions) -> DiffDoc {
        DiffDoc::new(parse_patch(patch.as_bytes()), opts)
    }

    /// Text of the row's spans of `token`.
    fn tokens(row: &DiffRow, token: Token) -> Vec<&str> {
        row.spans.iter().filter(|s| s.token == token).map(|s| &row.text[s.start as usize..s.end as usize]).collect()
    }

    const PATCH: &str = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n\
        @@ -1,4 +1,4 @@\n \tfn f() {\n-\t/* old\n+\tlet s = \"日本\";\n \tlet n = 1;\n }\n\
        @@ -10,2 +10,2 @@\n-x();\n+// new\n y();\n";

    #[test]
    fn highlighted_rows_keep_their_text_and_sides_are_separate() {
        let plain = doc(PATCH, DocOptions::default());
        let colored = plain.highlighted(b"src/x.rs", &CancelToken::default()).expect("Rust");
        // Same rows (line staging depends on them), only colors added.
        assert_eq!(plain.rows.len(), colored.rows.len());
        for (a, b) in plain.rows.iter().zip(&colored.rows) {
            assert_eq!((a.kind, a.old_no, a.new_no, &a.text, a.hunk, a.line), (b.kind, b.old_no, b.new_no, &b.text, b.hunk, b.line));
            assert!(a.spans.is_empty());
            for s in &b.spans {
                assert!(b.text.get(s.start as usize..s.end as usize).is_some(), "{s:?} in {:?}", b.text);
            }
        }
        let rows = &colored.rows;
        assert!(rows[0].spans.is_empty(), "hunk headers stay plain");
        assert_eq!(tokens(&rows[1], Token::Keyword), ["fn"]);
        // The deleted line opens a comment on the old side only…
        assert_eq!(tokens(&rows[2], Token::Comment), ["/* old"]);
        // …so the added line and the context after it (new side) are code.
        assert_eq!(tokens(&rows[3], Token::String), ["\"日本\""]);
        assert_eq!(tokens(&rows[4], Token::Keyword)[0], "let");
        assert!(tokens(&rows[3], Token::Comment).is_empty() && tokens(&rows[4], Token::Comment).is_empty());
        // Each hunk starts afresh.
        assert_eq!(tokens(&rows[8], Token::Comment), ["// new"]);
        assert!(tokens(&rows[9], Token::Comment).is_empty());
    }

    #[test]
    fn tab_width_shapes_the_text_the_colors_belong_to() {
        let wide = doc(PATCH, DocOptions { tab_width: 8, highlight: true }).highlighted(b"x.rs", &CancelToken::default()).unwrap();
        assert_eq!(wide.rows[1].text, "        fn f() {");
        assert_eq!(tokens(&wide.rows[1], Token::Keyword), ["fn"]);
        let narrow = doc(PATCH, DocOptions { tab_width: 2, highlight: true });
        assert_eq!(narrow.rows[1].text, "  fn f() {");
    }

    #[test]
    fn no_colors_for_unknown_or_huge_files() {
        let d = doc(PATCH, DocOptions::default());
        let colored = d.highlighted(b"x.rs", &CancelToken::default()).unwrap();
        assert!(Arc::ptr_eq(&d.source, &colored.source), "the raw diff is shared");
        let stopped = CancelToken::default();
        stopped.cancel();
        assert!(DiffDoc::build(parse_patch(PATCH.as_bytes()), DocOptions::default(), &stopped).is_none());
        assert!(d.highlighted(b"notes.unknown-ext", &CancelToken::default()).is_none());
        let cancelled = CancelToken::default();
        cancelled.cancel();
        assert!(d.highlighted(b"x.rs", &cancelled).is_none());
        // Too many rows.
        let mut big = String::from("--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1,20001 @@\n");
        for _ in 0..HIGHLIGHT_MAX_DIFF_ROWS {
            big.push_str("+let a = 1;\n");
        }
        let d = doc(&big, DocOptions::default());
        assert!(d.rows.len() > HIGHLIGHT_MAX_DIFF_ROWS);
        assert!(d.highlighted(b"x.rs", &CancelToken::default()).is_none());
        // Too many bytes (few rows).
        let mut wide = String::from("--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1,3000 @@\n");
        let line = format!("+{}\n", "a".repeat(MAX_LINE_CHARS - 1));
        for _ in 0..3000 {
            wide.push_str(&line);
        }
        let d = doc(&wide, DocOptions::default());
        assert!(d.rows.len() < HIGHLIGHT_MAX_DIFF_ROWS);
        assert!(d.highlighted(b"x.rs", &CancelToken::default()).is_none());
    }
}
