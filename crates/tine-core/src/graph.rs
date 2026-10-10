//! Commit graph lane layout.
//!
//! Commits arrive in an order where children precede parents
//! (`--date-order`). Each active lane remembers which commit it is heading
//! to; a commit takes over the leftmost lane waiting for it (other waiting
//! lanes merge into it), its first parent continues in the same lane, and
//! further parents branch off into existing or new lanes. Lanes never move
//! sideways, so lines between rows are straight; freed slots are reused by
//! later branches to keep the graph narrow.
//!
//! Straight pass-through lines are stored as row ranges ([`Span`]) rather
//! than per row: in large histories they outnumber everything else by far
//! (tens of millions of segments for rust-lang/rust).

use std::collections::HashMap;

use crate::model::Oid;

/// Index of a column in the graph.
pub type Lane = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Row top at `from` → node center at `to` (a lane ending in this commit).
    Incoming,
    /// Node center at `from` → row bottom at `to` (towards a parent).
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    pub from: Lane,
    pub to: Lane,
    pub color: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    pub lane: Lane,
    pub color: u16,
    /// Lines touching the node. Pass-through lines are in [`Graph::spans`].
    pub edges: Vec<Edge>,
    /// Number of lanes this row needs to draw.
    pub width: Lane,
}

/// A lane drawn straight through every row in `first_row..=last_row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub lane: Lane,
    pub color: u16,
    pub first_row: u32,
    pub last_row: u32,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    color: u16,
    /// Row whose bottom edge opened this lane.
    opened_at: u32,
}

/// Rows per bucket of the span index.
const BLOCK: u32 = 128;

#[derive(Debug, Default)]
pub struct Graph {
    rows: Vec<GraphRow>,
    spans: Vec<Span>,
    /// Span ids overlapping each block of [`BLOCK`] rows.
    blocks: Vec<Vec<u32>>,
    lanes: Vec<Option<Slot>>,
    /// Lanes currently waiting for a commit; usually one, several when
    /// branches converge on a common parent.
    waiting: HashMap<Oid, Vec<Lane>>,
    next_color: u16,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, index: usize) -> Option<&GraphRow> {
        self.rows.get(index)
    }

    pub fn rows(&self) -> &[GraphRow] {
        &self.rows
    }

    /// Lanes passing straight through `row`, as `(lane, color)`.
    pub fn through(&self, row: usize) -> impl Iterator<Item = (Lane, u16)> + '_ {
        let r = row as u32;
        let closed = self
            .blocks
            .get((r / BLOCK) as usize)
            .into_iter()
            .flatten()
            .map(|&i| &self.spans[i as usize])
            .filter(move |s| s.first_row <= r && r <= s.last_row)
            .map(|s| (s.lane, s.color));
        // Lanes still open (loading in progress, or parents not loaded).
        let last = self.rows.len().saturating_sub(1) as u32;
        let open = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| s.filter(|s| s.opened_at < r && r <= last).map(|s| (i as Lane, s.color)));
        closed.chain(open)
    }

    fn alloc(&mut self, avoid: Option<Lane>) -> Lane {
        let free = self.lanes.iter().enumerate().position(|(i, s)| s.is_none() && Some(i as Lane) != avoid);
        match free {
            Some(i) => i as Lane,
            None => {
                self.lanes.push(None);
                (self.lanes.len() - 1) as Lane
            }
        }
    }

    fn new_color(&mut self) -> u16 {
        let c = self.next_color;
        self.next_color = self.next_color.wrapping_add(1);
        c
    }

    fn occupy(&mut self, lane: Lane, waiting_for: Oid, color: u16, row: u32) {
        self.lanes[lane as usize] = Some(Slot { color, opened_at: row });
        self.waiting.entry(waiting_for).or_default().push(lane);
    }

    /// Records the pass-through part of a lane that ends at `end_row`.
    fn close(&mut self, lane: Lane, slot: Slot, end_row: u32) {
        if end_row > slot.opened_at + 1 {
            let span = Span { lane, color: slot.color, first_row: slot.opened_at + 1, last_row: end_row - 1 };
            let id = self.spans.len() as u32;
            let last_block = (span.last_row / BLOCK) as usize;
            if self.blocks.len() <= last_block {
                self.blocks.resize_with(last_block + 1, Vec::new);
            }
            for b in (span.first_row / BLOCK) as usize..=last_block {
                self.blocks[b].push(id);
            }
            self.spans.push(span);
        }
    }

    /// Lays out the next commit and returns its row.
    pub fn push(&mut self, id: Oid, parents: &[Oid]) -> &GraphRow {
        let row = self.rows.len() as u32;
        let mut edges = Vec::new();
        let mut incoming = self.waiting.remove(&id).unwrap_or_default();
        incoming.sort_unstable();

        let (lane, color) = match incoming.first() {
            Some(&l) => (l, self.lanes[l as usize].expect("waiting lane is occupied").color),
            None => {
                let l = self.alloc(None);
                (l, self.new_color())
            }
        };
        for &l in &incoming {
            let slot = self.lanes[l as usize].take().expect("waiting lane is occupied");
            edges.push(Edge { kind: EdgeKind::Incoming, from: l, to: lane, color: slot.color });
            self.close(l, slot, row);
        }

        // Rightmost lane passing by untouched, for the row width.
        let through_max = self.lanes.iter().rposition(Option::is_some).map(|i| i as Lane);

        if let Some((first, rest)) = parents.split_first() {
            self.occupy(lane, *first, color, row);
            edges.push(Edge { kind: EdgeKind::Outgoing, from: lane, to: lane, color });
            for p in rest {
                // Join a lane already heading to this parent, if any.
                let existing = self.waiting.get(p).and_then(|v| v.iter().copied().find(|&l| l != lane));
                let (target, c) = match existing {
                    Some(l) => (l, self.lanes[l as usize].expect("occupied").color),
                    None => {
                        let l = self.alloc(Some(lane));
                        let c = self.new_color();
                        self.occupy(l, *p, c, row);
                        (l, c)
                    }
                };
                edges.push(Edge { kind: EdgeKind::Outgoing, from: lane, to: target, color: c });
            }
        }

        // Trim trailing free slots so the graph narrows again.
        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }

        let width = edges.iter().map(|e| e.from.max(e.to)).chain([lane]).chain(through_max).max().unwrap_or(0) + 1;
        self.rows.push(GraphRow { lane, color, edges, width });
        self.rows.last().expect("just pushed")
    }

    /// Call once all commits are pushed: lanes still waiting (for parents
    /// outside the loaded range) run to the bottom.
    pub fn finish(&mut self) {
        let end = self.rows.len() as u32;
        for i in 0..self.lanes.len() {
            if let Some(slot) = self.lanes[i].take() {
                self.close(i as Lane, slot, end);
            }
        }
        self.lanes.clear();
        self.waiting.clear();
    }

    /// Approximate heap usage, for diagnostics.
    pub fn heap_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<GraphRow>()
            + self.rows.iter().map(|r| r.edges.capacity() * std::mem::size_of::<Edge>()).sum::<usize>()
            + self.spans.capacity() * std::mem::size_of::<Span>()
            + self.blocks.iter().map(|b| b.capacity() * 4 + 24).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap()
    }

    /// (id, parents) in display order.
    fn layout(commits: &[(u8, &[u8])]) -> Graph {
        let mut g = Graph::new();
        for (id, ps) in commits {
            g.push(oid(*id), &ps.iter().map(|p| oid(*p)).collect::<Vec<_>>());
        }
        g
    }

    fn lanes(g: &Graph) -> Vec<Lane> {
        g.rows().iter().map(|r| r.lane).collect()
    }

    fn through(g: &Graph, row: usize) -> Vec<Lane> {
        let mut v: Vec<_> = g.through(row).map(|(l, _)| l).collect();
        v.sort();
        v
    }

    fn count(row: &GraphRow, kind: EdgeKind) -> usize {
        row.edges.iter().filter(|e| e.kind == kind).count()
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let g = layout(&[(3, &[2]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&g), [0, 0, 0]);
        assert!(g.rows().iter().all(|r| r.width == 1));
        assert_eq!(g.rows()[0].color, g.rows()[2].color);
        // Root commit has no outgoing edge.
        assert_eq!(count(&g.rows()[2], EdgeKind::Outgoing), 0);
        assert!((0..3).all(|r| through(&g, r).is_empty()));
    }

    #[test]
    fn branch_and_merge() {
        // 5 = merge(4, 3); 4 and 3 both come from 2.
        let g = layout(&[(5, &[4, 3]), (4, &[2]), (3, &[2]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&g), [0, 0, 1, 0, 0]);
        let out: Vec<_> = g.rows()[0].edges.iter().filter(|e| e.kind == EdgeKind::Outgoing).collect();
        assert_eq!(out.len(), 2);
        assert_eq!((out[1].from, out[1].to), (0, 1));
        // Branch lane passes by commit 4; main lane passes by commit 3.
        assert_eq!(through(&g, 1), [1]);
        assert_eq!(through(&g, 2), [0]);
        assert_eq!(g.rows()[1].width, 2);
        // Commit 2 receives both lanes.
        assert_eq!(count(&g.rows()[3], EdgeKind::Incoming), 2);
        assert_eq!(g.rows()[3].width, 2);
        assert_eq!(g.rows()[4].width, 1);
    }

    #[test]
    fn two_tips_sharing_history() {
        let g = layout(&[(4, &[2]), (3, &[2]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&g), [0, 1, 0, 0]);
        assert_ne!(g.rows()[0].color, g.rows()[1].color);
        assert_eq!(through(&g, 1), [0]);
    }

    #[test]
    fn octopus_merge() {
        let g = layout(&[(9, &[1, 2, 3]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(count(&g.rows()[0], EdgeKind::Outgoing), 3);
        assert_eq!(lanes(&g), [0, 2, 1, 0]);
        assert_eq!(count(&g.rows()[3], EdgeKind::Incoming), 3);
    }

    #[test]
    fn multiple_roots_and_slot_reuse() {
        let mut g = layout(&[(4, &[3]), (2, &[1]), (3, &[]), (1, &[])]);
        assert_eq!(lanes(&g), [0, 1, 0, 1]);
        assert!(g.lanes.is_empty() && g.waiting.is_empty());
        // A new tip reuses lane 0.
        assert_eq!(g.push(oid(7), &[]).lane, 0);
    }

    #[test]
    fn merge_into_existing_lane() {
        // 6 = merge(5, 3) where 3 is already awaited by tip 4's lane.
        let g = layout(&[(4, &[3]), (6, &[5, 3]), (5, &[3]), (3, &[])]);
        assert_eq!(lanes(&g), [0, 1, 1, 0]);
        let out: Vec<_> = g.rows()[1].edges.iter().filter(|e| e.kind == EdgeKind::Outgoing).collect();
        assert_eq!((out[1].from, out[1].to), (1, 0));
    }

    #[test]
    fn long_spans_are_indexed_across_blocks() {
        // Tip 1000 waits for root 0 while a long linear history runs in lane 1.
        let mut g = Graph::new();
        g.push(oid(255), &[oid(0)]);
        let n = 1000u32;
        let id = |i: u32| Oid::from_hex(format!("{:040x}", 0x1000 + i).as_bytes()).unwrap();
        for i in 0..n {
            let parents = if i + 1 < n { vec![id(i + 1)] } else { vec![oid(0)] };
            g.push(id(i), &parents);
        }
        g.push(oid(0), &[]);
        g.finish();
        assert_eq!(g.len(), n as usize + 2);
        assert_eq!(through(&g, 1), [0]);
        assert_eq!(through(&g, 500), [0]);
        assert_eq!(through(&g, n as usize), [0]);
        assert!(through(&g, n as usize + 1).is_empty());
        assert_eq!(g.spans.len(), 1);
    }

    #[test]
    fn open_lanes_are_drawn_while_loading_and_closed_by_finish() {
        let mut g = Graph::new();
        g.push(oid(3), &[oid(1)]);
        g.push(oid(2), &[oid(9)]);
        // Lane 0 is still waiting for commit 1, passing through row 1.
        assert_eq!(through(&g, 1), [0]);
        g.finish();
        assert_eq!(through(&g, 1), [0]);
        assert!(g.lanes.is_empty());
    }

    #[test]
    fn many_parallel_branches_stay_bounded() {
        let mut g = Graph::new();
        for i in 1..=50u8 {
            g.push(oid(i), &[oid(0)]);
        }
        let root = g.push(oid(0), &[]).clone();
        assert_eq!(count(&root, EdgeKind::Incoming), 50);
        assert_eq!(root.lane, 0);
        assert!(g.lanes.is_empty());
    }
}
