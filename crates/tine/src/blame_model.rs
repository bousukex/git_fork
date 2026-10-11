//! A blame prepared for display (built on a worker thread).

use std::sync::Arc;

use tine_core::{
    CancelToken,
    blame::Blame,
    highlight::{Span, highlight_streams, syntax_for},
};

use crate::{
    diff_model::{HIGHLIGHT_MAX_BYTES, HIGHLIGHT_MAX_SPANS, display_text},
    settings::DocOptions,
};

/// Shades used for the age bar (0 = newest).
pub const AGE_BUCKETS: u8 = 10;

/// Longest blame that gets syntax highlighting.
pub const HIGHLIGHT_MAX_LINES: usize = 100_000;

#[derive(Debug, Clone, Default)]
pub struct BlameDoc {
    /// Shared by the plain and highlighted docs (and kept to rebuild the
    /// doc when the display options change).
    pub blame: Arc<Blame>,
    /// Each line as shown.
    pub text: Vec<String>,
    /// Syntax highlighting of each line (empty: plain).
    pub spans: Vec<Vec<Span>>,
    /// The line starts a run of lines from the same origin.
    pub chunk_start: Vec<bool>,
    /// Per commit: how recent it is among this file's commits (0 newest).
    pub age: Vec<u8>,
    /// Widest line in monospace columns.
    pub max_cols: usize,
    /// Digits needed for line numbers.
    pub number_width: usize,
}

impl BlameDoc {
    pub fn new(blame: impl Into<Arc<Blame>>, opts: DocOptions) -> Self {
        Self::build(blame, opts, &CancelToken::default()).expect("not cancelled")
    }

    /// Prepares `blame` for display; `None` once `cancel` fires.
    pub fn build(blame: impl Into<Arc<Blame>>, opts: DocOptions, cancel: &CancelToken) -> Option<Self> {
        let blame: Arc<Blame> = blame.into();
        let mut text = Vec::with_capacity(blame.lines.len());
        let mut max_cols = 0;
        for (i, l) in blame.lines.iter().enumerate() {
            if i % 1024 == 1023 && cancel.is_cancelled() {
                return None;
            }
            let (t, cols) = display_text(&l.content, opts.tab_width);
            max_cols = max_cols.max(cols);
            text.push(t);
        }
        let chunk_start =
            (0..blame.lines.len()).map(|i| i == 0 || blame.lines[i].origin != blame.lines[i - 1].origin).collect();
        // Rank commits by time (uncommitted lines count as newest).
        let time = |i: usize| {
            let c = &blame.commits[i];
            if c.is_uncommitted() { i64::MAX } else { c.author_time }
        };
        let mut order: Vec<usize> = (0..blame.commits.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(time(i)));
        let mut age = vec![0u8; blame.commits.len()];
        let n = order.len().max(1);
        for (rank, &i) in order.iter().enumerate() {
            age[i] = ((rank * AGE_BUCKETS as usize) / n) as u8;
        }
        let number_width = blame.lines.len().max(1).ilog10() as usize + 1;
        let doc = BlameDoc { blame, text, spans: Vec::new(), chunk_start, age, max_cols, number_width };
        (!cancel.is_cancelled()).then_some(doc)
    }

    /// A copy with syntax highlighting for the file at `path` (the whole
    /// file as one stream), or `None` (unknown language, too large,
    /// cancelled).
    pub fn highlighted(&self, path: &[u8], cancel: &CancelToken) -> Option<BlameDoc> {
        if self.text.len() > HIGHLIGHT_MAX_LINES || self.text.iter().map(String::len).sum::<usize>() > HIGHLIGHT_MAX_BYTES {
            return None;
        }
        let syntax = syntax_for(path, self.text.first().map(String::as_str))?;
        let stream: Vec<&str> = self.text.iter().map(String::as_str).collect();
        let spans: Vec<Vec<Span>> = highlight_streams(syntax, &[stream], cancel, HIGHLIGHT_MAX_SPANS)?.pop()?;
        Some(BlameDoc { spans, ..self.clone() })
    }

    /// Syntax spans of line `i` (empty when plain).
    pub fn spans(&self, i: usize) -> &[Span] {
        self.spans.get(i).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use bstr::BString;
    use tine_core::{
        blame::{BlameCommit, BlameLine, BlameOrigin},
        model::Oid,
    };

    use super::*;

    fn commit(n: u8, time: i64) -> BlameCommit {
        BlameCommit {
            id: Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap(),
            author: "a".into(),
            author_mail: "a@x".into(),
            author_time: time,
            summary: "s".into(),
            boundary: false,
        }
    }

    #[test]
    fn chunks_and_ages() {
        let blame = Blame {
            commits: vec![commit(1, 100), commit(2, 300), commit(0, 0)],
            origins: (0..3).map(|c| BlameOrigin { commit: c, filename: "f".into(), previous: None }).collect(),
            lines: [0, 0, 1, 2, 0]
                .iter()
                .enumerate()
                .map(|(i, &o)| BlameLine { origin: o, orig_line: 1, final_line: i as u32 + 1, content: BString::from("\tx") })
                .collect(),
        };
        let doc = BlameDoc::new(blame, DocOptions::default());
        assert_eq!(doc.chunk_start, [true, false, true, true, true]);
        assert_eq!(doc.text[0], "    x");
        // Uncommitted (all-zero id) is newest, then commit 2, then 1.
        assert!(doc.age[2] < doc.age[1] && doc.age[1] < doc.age[0], "{:?}", doc.age);
        assert_eq!(BlameDoc::new(doc.blame.clone(), DocOptions { tab_width: 2, highlight: true }).text[0], "  x");
    }

    fn blame_of(lines: &[&str]) -> Blame {
        Blame {
            commits: vec![commit(1, 100)],
            origins: vec![BlameOrigin { commit: 0, filename: "f".into(), previous: None }],
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, l)| BlameLine { origin: 0, orig_line: i as u32 + 1, final_line: i as u32 + 1, content: BString::from(*l) })
                .collect(),
        }
    }

    #[test]
    fn highlighting_follows_the_file_across_lines() {
        use tine_core::highlight::Token;
        let doc = BlameDoc::new(blame_of(&["def f():", "\ts = \"\"\"日本", "\tstill text", "\t\"\"\"", "\treturn s"]), DocOptions::default());
        let colored = doc.highlighted(b"lib/f.py", &CancelToken::default()).expect("Python");
        assert_eq!(colored.text, doc.text);
        let of = |i: usize, t: Token| -> Vec<&str> {
            colored.spans(i).iter().filter(|s| s.token == t).map(|s| &colored.text[i][s.start as usize..s.end as usize]).collect()
        };
        assert_eq!(of(0, Token::Keyword), ["def"]);
        assert_eq!(of(2, Token::String), ["    still text"], "inside the multi-line string");
        assert_eq!(of(4, Token::Keyword), ["return"]);
        assert!(doc.spans(0).is_empty(), "the plain doc stays plain");
        // Unknown languages and very long files stay plain.
        assert!(doc.highlighted(b"f.unknown-ext", &CancelToken::default()).is_none());
        let long = BlameDoc::new(blame_of(&vec!["x = 1"; HIGHLIGHT_MAX_LINES + 1]), DocOptions::default());
        assert!(long.highlighted(b"f.py", &CancelToken::default()).is_none());
        let stopped = CancelToken::default();
        stopped.cancel();
        assert!(BlameDoc::build(blame_of(&["x = 1"]), DocOptions::default(), &stopped).is_none());
    }
}
