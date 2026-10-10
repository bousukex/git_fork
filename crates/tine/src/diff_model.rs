//! A diff flattened into display rows, prepared once off the paint path.

use tine_core::model::{FileDiff, LineKind};

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
    /// The parsed diff, kept with its raw bytes for building partial patches.
    pub source: FileDiff,
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
pub fn display_text(bytes: &[u8]) -> (String, usize) {
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
                let n = 4 - cols % 4;
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

impl DiffDoc {
    pub fn new(diff: FileDiff) -> Self {
        let (added, deleted) = diff.stats();
        let mut doc = DiffDoc {
            source: diff.clone(),
            binary: diff.binary,
            header: diff.header.iter().map(|h| String::from_utf8_lossy(h).into_owned()).collect(),
            added,
            deleted,
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
            doc.rows.push(DiffRow { kind: RowKind::Hunk, old_no: None, new_no: None, text, hunk: hi, line: None });
            for (li, line) in hunk.lines.iter().enumerate() {
                let kind = match line.kind {
                    LineKind::Context => RowKind::Context,
                    LineKind::Added => RowKind::Added,
                    LineKind::Deleted => RowKind::Deleted,
                    LineKind::NoNewline => RowKind::NoNewline,
                };
                let (text, cols) = display_text(&line.content);
                doc.max_cols = doc.max_cols.max(cols);
                max_no = max_no.max(line.old_no.unwrap_or(0)).max(line.new_no.unwrap_or(0));
                doc.rows.push(DiffRow { kind, old_no: line.old_no, new_no: line.new_no, text, hunk: hi, line: Some(li) });
            }
        }
        doc.number_width = max_no.max(1).ilog10() as usize + 1;
        doc
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && !self.binary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tabs_and_measures_wide_chars() {
        assert_eq!(display_text(b"a\tb"), ("a   b".into(), 5));
        assert_eq!(display_text("日本".as_bytes()).1, 4);
        assert_eq!(display_text(b"x\r").0, "x");
        let long = vec![b'x'; 5000];
        assert!(display_text(&long).0.ends_with('…'));
        // 2000 Japanese characters fit; the limit isn't in bytes.
        let japanese = "日".repeat(MAX_LINE_CHARS);
        assert!(!display_text(japanese.as_bytes()).0.ends_with('…'));
        let over = "日".repeat(MAX_LINE_CHARS + 1);
        let (cut, _) = display_text(over.as_bytes());
        assert_eq!(cut.chars().count(), MAX_LINE_CHARS + 1, "2000 characters and the ellipsis");
        // Invalid UTF-8 and tabs count as what is shown.
        let mut mixed = vec![0xff; 10];
        mixed.extend_from_slice(b"\tx");
        assert_eq!(display_text(&mixed).0, format!("{}  x", "\u{fffd}".repeat(10)));
    }
}
