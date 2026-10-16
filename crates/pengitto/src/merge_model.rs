//! The merge editor's model: a conflicted file as opened, the choice made
//! for each conflict, and the rows shown for them (no UI here).

use std::sync::Arc;

use bstr::ByteSlice as _;
use pengitto_core::{
    CancelToken,
    highlight::{Span, highlight_streams, syntax_for},
    merge_file::{self, Choice, Merge, Opened, Segment, WorkFile},
};

use crate::diff_model::display_text;

/// Highlighting stays off above these (as for diffs).
const HIGHLIGHT_MAX_LINES: usize = 20_000;
const HIGHLIGHT_MAX_BYTES: usize = 4 << 20;
const HIGHLIGHT_MAX_SPANS: usize = 500_000;
/// The result can be edited as text up to this size.
pub const MAX_EDIT_BYTES: usize = 1 << 20;

/// A side of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    Ours = 0,
    Base = 1,
    Theirs = 2,
}

/// Each segment's lines as shown (lossy UTF-8, tabs expanded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegLines {
    Common(Vec<String>),
    /// Ours, base, theirs.
    Conflict([Vec<String>; 3]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeText {
    pub segments: Vec<SegLines>,
}

/// Display text per line (the line ending isn't shown; a CR is dropped).
fn lines(bytes: &[u8], tab_width: u8) -> Vec<String> {
    bytes.lines_with_terminator().map(|l| display_text(l.strip_suffix(b"\n").unwrap_or(l), tab_width).0).collect()
}

impl MergeText {
    pub fn new(merge: &Merge, tab_width: u8) -> Self {
        let segments = merge
            .segments
            .iter()
            .map(|s| match s {
                Segment::Common(b) => SegLines::Common(lines(b, tab_width)),
                Segment::Conflict(c) => SegLines::Conflict([&c.ours, &c.base, &c.theirs].map(|b| lines(b, tab_width))),
            })
            .collect();
        MergeText { segments }
    }

    /// Syntax spans for the file at `path`: each side read through as a
    /// file of its own (what the sides agree on, then that side's lines);
    /// agreed lines take the ours reading. `None`: unknown language, too
    /// large, cancelled.
    pub fn highlight(&self, path: &[u8], cancel: &CancelToken) -> Option<MergeSpans> {
        let mut streams: [Vec<&str>; 3] = Default::default();
        for s in &self.segments {
            for (p, stream) in streams.iter_mut().enumerate() {
                match s {
                    SegLines::Common(l) => stream.extend(l.iter().map(String::as_str)),
                    SegLines::Conflict(sides) => stream.extend(sides[p].iter().map(String::as_str)),
                }
            }
        }
        let too_big =
            |s: &Vec<&str>| s.len() > HIGHLIGHT_MAX_LINES || s.iter().map(|l| l.len()).sum::<usize>() > HIGHLIGHT_MAX_BYTES;
        if streams.iter().any(too_big) {
            return None;
        }
        let syntax = syntax_for(path, streams[0].first().copied())?;
        let mut out = highlight_streams(syntax, &streams, cancel, HIGHLIGHT_MAX_SPANS)?.into_iter();
        let mut spans: [std::vec::IntoIter<Vec<Span>>; 3] =
            [out.next()?.into_iter(), out.next()?.into_iter(), out.next()?.into_iter()];
        let mut segments = Vec::with_capacity(self.segments.len());
        for s in &self.segments {
            segments.push(match s {
                SegLines::Common(l) => {
                    let ours: Vec<Vec<Span>> = spans[0].by_ref().take(l.len()).collect();
                    spans[1].by_ref().take(l.len()).for_each(drop);
                    spans[2].by_ref().take(l.len()).for_each(drop);
                    SegSpans::Common(ours)
                }
                SegLines::Conflict(sides) => {
                    SegSpans::Conflict([0, 1, 2].map(|p| spans[p].by_ref().take(sides[p].len()).collect()))
                }
            });
        }
        Some(MergeSpans { segments })
    }
}

/// Syntax spans laid out like [`MergeText`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegSpans {
    Common(Vec<Vec<Span>>),
    Conflict([Vec<Vec<Span>>; 3]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSpans {
    pub segments: Vec<SegSpans>,
}

/// A row of the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A line the sides agree on (segment, line).
    Common { seg: usize, line: usize },
    /// The head of conflict `k`: its choices, or what was chosen.
    Head { k: usize },
    /// The title of a side of an unresolved conflict.
    Label { k: usize, part: Part },
    /// A line of a side: of an unresolved conflict, or the chosen content.
    Line { k: usize, part: Part, line: usize },
    /// A side (or a resolution) with no lines.
    Nothing { k: usize },
}

/// A conflicted file in the editor.
#[derive(Debug, Clone)]
pub struct MergeSession {
    pub opened: Opened,
    /// The working-tree file as last opened or saved (saving checks it).
    pub work: WorkFile,
    pub choices: Vec<Choice>,
    /// Conflicts whose base is shown.
    pub base_shown: Vec<bool>,
    pub text: Arc<MergeText>,
    /// For `text` (dropped when it changes).
    pub spans: Option<Arc<MergeSpans>>,
    pub rows: Vec<Row>,
    /// Conflict → its segment, and its head row.
    pub conflict_seg: Vec<usize>,
    pub conflict_row: Vec<usize>,
    /// Bumped by every change; a save records which it wrote.
    pub edit_gen: u64,
    pub saved_gen: u64,
    /// Markers left in an edited result that isn't a valid conflict layout
    /// (it can't be marked resolved while they are there).
    pub stray_markers: usize,
    /// The conflict last navigated to.
    pub current: Option<usize>,
    pub tab_width: u8,
}

impl MergeSession {
    pub fn new(opened: Opened, text: Arc<MergeText>, tab_width: u8) -> Self {
        let n = opened.merge.conflict_count();
        let work = opened.work.clone();
        let mut s = MergeSession {
            opened,
            work,
            choices: vec![Choice::Unresolved; n],
            base_shown: vec![false; n],
            text,
            spans: None,
            rows: Vec::new(),
            conflict_seg: Vec::new(),
            conflict_row: Vec::new(),
            edit_gen: 0,
            saved_gen: 0,
            stray_markers: 0,
            current: None,
            tab_width,
        };
        s.rebuild_rows();
        s
    }

    /// Changes not saved yet.
    pub fn dirty(&self) -> bool {
        self.edit_gen != self.saved_gen
    }

    pub fn conflict_count(&self) -> usize {
        self.choices.len()
    }

    /// Conflicts still to resolve (with markers left in an edited result).
    pub fn unresolved(&self) -> usize {
        self.choices.iter().filter(|c| **c == Choice::Unresolved).count() + self.stray_markers
    }

    pub fn choose(&mut self, k: usize, choice: Choice) {
        if let Some(c) = self.choices.get_mut(k)
            && *c != choice
        {
            *c = choice;
            self.edit_gen += 1;
            self.rebuild_rows();
        }
    }

    pub fn toggle_base(&mut self, k: usize) {
        if let Some(b) = self.base_shown.get_mut(k) {
            *b = !*b;
            self.rebuild_rows();
        }
    }

    /// The file as it would be saved (index form).
    pub fn result(&self) -> Vec<u8> {
        self.opened.merge.assemble(&self.choices)
    }

    /// The result as text to edit (line endings as `\n`), or why it can't
    /// be.
    pub fn editable_text(&self) -> Result<String, &'static str> {
        let bytes = self.result();
        if bytes.len() > MAX_EDIT_BYTES {
            return Err("The result is too large to edit here (over 1 MiB)");
        }
        let text = String::from_utf8(bytes).map_err(|_| "The result isn't UTF-8 text")?;
        Ok(text.replace("\r\n", "\n"))
    }

    /// Takes an edited result: its line endings become the file's usual
    /// ones, its last line ends as the result's did, and conflicts left in
    /// it (markers of the editor's size) are conflicts again.
    pub fn apply_edit(&mut self, edited: &str) {
        // As it was offered for editing: nothing changes (even when its
        // line endings were mixed).
        if self.editable_text().is_ok_and(|t| t == edited) {
            return;
        }
        let before_ends = self.result().ends_with(b"\n");
        let mut text = edited.replace("\r\n", "\n");
        if before_ends && !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        } else if !before_ends {
            while text.ends_with('\n') {
                text.pop();
            }
        }
        let eol = self.opened.merge.eol.clone();
        let bytes: Vec<u8> = if eol == b"\r\n" { text.replace('\n', "\r\n").into_bytes() } else { text.into_bytes() };
        // Unchanged: the choices stay as they are.
        if bytes == self.result() {
            return;
        }
        let marker = self.opened.merge.marker;
        let (segments, stray) = match merge_file::parse(&bytes, marker) {
            Ok(segments) => (segments, 0),
            Err(_) => {
                let stray = merge_file::marker_lines(&bytes, marker);
                (if bytes.is_empty() { Vec::new() } else { vec![Segment::Common(bytes)] }, stray)
            }
        };
        self.opened.merge.segments = segments;
        let n = self.opened.merge.conflict_count();
        self.choices = vec![Choice::Unresolved; n];
        self.base_shown = vec![false; n];
        self.stray_markers = stray;
        self.text = Arc::new(MergeText::new(&self.opened.merge, self.tab_width));
        self.spans = None;
        self.current = None;
        self.edit_gen += 1;
        self.rebuild_rows();
    }

    /// The display text again (the tab width changed).
    pub fn retext(&mut self, tab_width: u8) {
        self.tab_width = tab_width;
        self.text = Arc::new(MergeText::new(&self.opened.merge, tab_width));
        self.spans = None;
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        self.conflict_seg.clear();
        self.conflict_row.clear();
        for (seg, s) in self.text.segments.iter().enumerate() {
            match s {
                SegLines::Common(l) => rows.extend((0..l.len()).map(|line| Row::Common { seg, line })),
                SegLines::Conflict(sides) => {
                    let k = self.conflict_seg.len();
                    self.conflict_seg.push(seg);
                    self.conflict_row.push(rows.len());
                    rows.push(Row::Head { k });
                    let side = |rows: &mut Vec<Row>, part: Part| {
                        rows.extend((0..sides[part as usize].len()).map(|line| Row::Line { k, part, line }));
                    };
                    let parts: &[Part] = match self.choices.get(k).copied().unwrap_or_default() {
                        Choice::Unresolved => {
                            for part in [Part::Ours, Part::Base, Part::Theirs] {
                                rows.push(Row::Label { k, part });
                                if part == Part::Base && !self.base_shown[k] {
                                    continue;
                                }
                                if sides[part as usize].is_empty() {
                                    rows.push(Row::Nothing { k });
                                }
                                side(&mut rows, part);
                            }
                            continue;
                        }
                        Choice::Ours => &[Part::Ours],
                        Choice::Theirs => &[Part::Theirs],
                        Choice::Base => &[Part::Base],
                        Choice::OursThenTheirs => &[Part::Ours, Part::Theirs],
                        Choice::TheirsThenOurs => &[Part::Theirs, Part::Ours],
                    };
                    let before = rows.len();
                    for &part in parts {
                        side(&mut rows, part);
                    }
                    if rows.len() == before {
                        rows.push(Row::Nothing { k });
                    }
                }
            }
        }
        self.rows = rows;
    }

    /// The text of a line row (and its syntax spans).
    pub fn line(&self, row: Row) -> Option<(&str, &[Span])> {
        let (seg, part, line) = match row {
            Row::Common { seg, line } => (seg, None, line),
            Row::Line { k, part, line } => (*self.conflict_seg.get(k)?, Some(part), line),
            _ => return None,
        };
        let text = match (self.text.segments.get(seg)?, part) {
            (SegLines::Common(l), None) => l.get(line)?,
            (SegLines::Conflict(sides), Some(p)) => sides[p as usize].get(line)?,
            _ => return None,
        };
        let spans = self.spans.as_ref().and_then(|s| match (s.segments.get(seg)?, part) {
            (SegSpans::Common(l), None) => l.get(line),
            (SegSpans::Conflict(sides), Some(p)) => sides[p as usize].get(line),
            _ => None,
        });
        Some((text.as_str(), spans.map_or(&[][..], Vec::as_slice)))
    }

    /// The unresolved conflict after (or before) `from` — wrapping around;
    /// `None` when none is left.
    pub fn next_unresolved(&self, from: Option<usize>, back: bool) -> Option<usize> {
        let n = self.choices.len();
        if n == 0 {
            return None;
        }
        let open = |k: usize| self.choices[k] == Choice::Unresolved;
        let order: Vec<usize> = match (from, back) {
            (None, false) => (0..n).collect(),
            (None, true) => (0..n).rev().collect(),
            (Some(f), false) => (f + 1..n).chain(0..=f.min(n - 1)).collect(),
            (Some(f), true) => (0..f.min(n)).rev().chain((f.min(n - 1)..n).rev()).collect(),
        };
        order.into_iter().find(|&k| open(k))
    }

    /// The width of the longest line in columns (for scrolling sideways).
    pub fn max_chars(&self) -> usize {
        self.text
            .segments
            .iter()
            .flat_map(|s| match s {
                SegLines::Common(l) => l.iter().collect::<Vec<_>>(),
                SegLines::Conflict(sides) => sides.iter().flatten().collect(),
            })
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use pengitto_core::{
        merge_file::{Conflict, Sides, Stages},
        model::RepoState,
    };

    use super::*;

    fn session(segments: Vec<Segment>) -> MergeSession {
        let merge = Merge { marker: 7, labels: ["HEAD".into(), "base".into(), "theirs".into()], segments, eol: b"\n".to_vec() };
        let opened = Opened {
            path: "f.txt".into(),
            state: RepoState::Merging,
            heads: Default::default(),
            stages: Stages::default(),
            work: WorkFile::Missing,
            sides: Sides { ours: "o".into(), base: "b".into(), theirs: "t".into(), labels: Default::default() },
            merge,
        };
        let text = Arc::new(MergeText::new(&opened.merge, 4));
        MergeSession::new(opened, text, 4)
    }

    fn conflict(ours: &str, base: &str, theirs: &str) -> Segment {
        Segment::Conflict(Conflict { ours: ours.into(), base: base.into(), theirs: theirs.into(), eol: b"\n".to_vec() })
    }

    #[test]
    fn rows_follow_the_choices() {
        let mut s = session(vec![Segment::Common(b"a\n".to_vec()), conflict("x\n", "b\n", ""), Segment::Common(b"c\n".to_vec())]);
        use Part::*;
        assert_eq!(s.rows, [
            Row::Common { seg: 0, line: 0 },
            Row::Head { k: 0 },
            Row::Label { k: 0, part: Ours },
            Row::Line { k: 0, part: Ours, line: 0 },
            Row::Label { k: 0, part: Base },
            Row::Label { k: 0, part: Theirs },
            Row::Nothing { k: 0 },
            Row::Common { seg: 2, line: 0 },
        ]);
        s.toggle_base(0);
        assert!(s.rows.contains(&Row::Line { k: 0, part: Base, line: 0 }));
        assert!(!s.dirty(), "showing the base changes nothing");
        s.choose(0, Choice::Theirs);
        assert!(s.dirty());
        assert_eq!(s.rows[1..3], [Row::Head { k: 0 }, Row::Nothing { k: 0 }]);
        s.choose(0, Choice::OursThenTheirs);
        assert_eq!(s.line(s.rows[2]), Some(("x", &[][..])));
        assert_eq!(s.result(), b"a\nx\nc\n");
        assert_eq!(s.unresolved(), 0);
    }

    #[test]
    fn navigation_wraps_around_unresolved_ones() {
        let mut s = session(vec![conflict("1\n", "", "2\n"), conflict("3\n", "", "4\n"), conflict("5\n", "", "6\n")]);
        assert_eq!(s.next_unresolved(None, false), Some(0));
        assert_eq!(s.next_unresolved(None, true), Some(2));
        s.choose(1, Choice::Ours);
        assert_eq!(s.next_unresolved(Some(0), false), Some(2));
        assert_eq!(s.next_unresolved(Some(2), false), Some(0));
        assert_eq!(s.next_unresolved(Some(0), true), Some(2));
        s.choose(0, Choice::Ours);
        s.choose(2, Choice::Ours);
        assert_eq!(s.next_unresolved(Some(0), false), None);
    }

    #[test]
    fn an_edited_result_keeps_its_conflicts_and_endings() {
        let mut s = session(vec![Segment::Common(b"a\n".to_vec()), conflict("x\n", "b\n", "y\n")]);
        s.choose(0, Choice::Ours);
        let text = s.editable_text().unwrap();
        assert_eq!(text, "a\nx\n");
        // A conflict typed back in, the last line ending dropped: it ends
        // as the result did.
        s.apply_edit("a\n<<<<<<< mine\nx\n=======\nz\n>>>>>>> theirs");
        assert_eq!(s.conflict_count(), 1);
        assert_eq!(s.unresolved(), 1);
        assert_eq!(s.result(), b"a\n<<<<<<< HEAD\nx\n||||||| base\n=======\nz\n>>>>>>> theirs\n");
        // Markers that don't make a conflict: counted, can't be resolved.
        s.apply_edit("a\n=======\nb\n");
        assert_eq!((s.conflict_count(), s.unresolved()), (0, 1));
        assert_eq!(s.result(), b"a\n=======\nb\n");
        s.apply_edit("a\nb\n");
        assert_eq!(s.unresolved(), 0);
        s.apply_edit("a\nz");
        assert_eq!(s.result(), b"a\nz\n", "ends as the result did");
        s.apply_edit("a\nb\n");
        let at = s.edit_gen;
        s.apply_edit("a\nb\n");
        assert_eq!(s.edit_gen, at, "the same text changes nothing");

        // CRLF files get CRLF back; no last line ending stays so (however
        // many were typed).
        let mut s = session(vec![Segment::Common(b"a\r\nb".to_vec())]);
        s.opened.merge.eol = b"\r\n".to_vec();
        assert_eq!(s.editable_text().unwrap(), "a\nb");
        s.apply_edit("a\nc\n\n");
        assert_eq!(s.result(), b"a\r\nc");

        // Mixed line endings, applied as offered: unchanged, nothing to save.
        let mut s = session(vec![Segment::Common(b"a\r\nb\nc\n".to_vec())]);
        let offered = s.editable_text().unwrap();
        s.apply_edit(&offered);
        assert_eq!((s.result(), s.dirty()), (b"a\r\nb\nc\n".to_vec(), false));
    }

    #[test]
    fn what_cant_be_edited_as_text() {
        let s = session(vec![Segment::Common(b"caf\xe9\n".to_vec())]);
        assert!(s.editable_text().is_err());
        let s = session(vec![Segment::Common(vec![b'x'; MAX_EDIT_BYTES + 1])]);
        assert!(s.editable_text().is_err());
    }
}
