//! A blame prepared for display (built on a worker thread).

use tine_core::blame::Blame;

use crate::diff_model::display_text;

/// Shades used for the age bar (0 = newest).
pub const AGE_BUCKETS: u8 = 10;

#[derive(Debug, Clone, Default)]
pub struct BlameDoc {
    pub blame: Blame,
    /// Each line as shown.
    pub text: Vec<String>,
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
    pub fn new(blame: Blame) -> Self {
        let mut text = Vec::with_capacity(blame.lines.len());
        let mut max_cols = 0;
        for l in &blame.lines {
            let (t, cols) = display_text(&l.content);
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
        BlameDoc { blame, text, chunk_start, age, max_cols, number_width }
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
        let doc = BlameDoc::new(blame);
        assert_eq!(doc.chunk_start, [true, false, true, true, true]);
        assert_eq!(doc.text[0], "    x");
        // Uncommitted (all-zero id) is newest, then commit 2, then 1.
        assert!(doc.age[2] < doc.age[1] && doc.age[1] < doc.age[0], "{:?}", doc.age);
    }
}
