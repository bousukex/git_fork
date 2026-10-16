//! Drawing one row of the commit graph.

use egui::{Color32, Painter, Pos2, Rect, Stroke, epaint::CubicBezierShape};
use pengitto_core::graph::{EdgeKind, Graph, Lane};

use crate::theme::Palette;

/// Preferred lane spacing; lanes get narrower (down to [`MIN_LANE_WIDTH`])
/// when many branches run in parallel.
pub const LANE_WIDTH: f32 = 14.0;
pub const MIN_LANE_WIDTH: f32 = 5.0;
pub const PAD_LEFT: f32 = 6.0;
const PAD_RIGHT: f32 = 4.0;

/// Horizontal layout of the graph column for the visible rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub lane_width: f32,
    pub width: f32,
}

impl Layout {
    /// Fits `lanes` lanes into at most `max_width`, shrinking the spacing if
    /// needed. Only beyond [`MIN_LANE_WIDTH`] do lanes get clipped.
    pub fn fit(lanes: Lane, max_width: f32) -> Layout {
        let lanes = lanes.max(1) as f32;
        let room = (max_width - PAD_LEFT - PAD_RIGHT).max(0.0);
        let lane_width = (room / lanes).clamp(MIN_LANE_WIDTH, LANE_WIDTH);
        let width = (PAD_LEFT + lanes * lane_width + PAD_RIGHT).min(max_width.max(PAD_LEFT + MIN_LANE_WIDTH));
        Layout { lane_width, width }
    }

    fn x(&self, rect: Rect, lane: Lane) -> f32 {
        rect.left() + PAD_LEFT + lane as f32 * self.lane_width + self.lane_width / 2.0
    }

    fn scale(&self) -> f32 {
        self.lane_width / LANE_WIDTH
    }
}

fn curve(painter: &Painter, from: Pos2, to: Pos2, stroke: Stroke) {
    if (from.x - to.x).abs() < 0.5 {
        painter.line_segment([from, to], stroke);
        return;
    }
    let mid_y = (from.y + to.y) / 2.0;
    let pts = [from, Pos2::new(from.x, mid_y), Pos2::new(to.x, mid_y), to];
    painter.add(CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, stroke));
}

/// Paints the straight pass-through lines of `rows` (see
/// [`Graph::through_spans`]). `first_rect` is the graph cell of `rows.start`.
pub fn paint_through(
    painter: &Painter,
    first_rect: Rect,
    row_height: f32,
    layout: &Layout,
    graph: &Graph,
    rows: std::ops::Range<usize>,
    p: &Palette,
) {
    let start = rows.start;
    let stroke = |color: u16| Stroke::new(1.6 * layout.scale().max(0.6), p.lane(color as usize));
    for (lane, color, first, last) in graph.through_spans(rows) {
        let x = layout.x(first_rect, lane);
        let top = first_rect.top() + (first - start) as f32 * row_height;
        let bottom = first_rect.top() + (last + 1 - start) as f32 * row_height;
        painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom)], stroke(color));
    }
}

/// Paints the node and the lines touching it for row `index` into `rect`
/// (the graph cell of the row). Pass-through lines come from [`paint_through`].
pub fn paint_row(painter: &Painter, rect: Rect, layout: &Layout, graph: &Graph, index: usize, is_head: bool, p: &Palette) {
    let Some(row) = graph.row(index) else { return };
    let (top, mid, bottom) = (rect.top(), rect.center().y, rect.bottom());
    let k = layout.scale().max(0.6);
    let lane_x = |rect: Rect, lane: Lane| layout.x(rect, lane);
    let stroke = |color: u16| Stroke::new(1.6 * k, p.lane(color as usize));

    for e in &row.edges {
        let (a, b) = match e.kind {
            EdgeKind::Incoming => (Pos2::new(lane_x(rect, e.from), top), Pos2::new(lane_x(rect, e.to), mid)),
            EdgeKind::Outgoing => (Pos2::new(lane_x(rect, e.from), mid), Pos2::new(lane_x(rect, e.to), bottom)),
        };
        curve(painter, a, b, stroke(e.color));
    }

    let center = Pos2::new(lane_x(rect, row.lane), mid);
    let color = p.lane(row.color as usize);
    let is_merge = row.edges.iter().filter(|e| e.kind == EdgeKind::Outgoing).count() > 1;
    if is_head {
        painter.circle_filled(center, 5.5 * k, color);
        painter.circle_filled(center, 2.5 * k, p.bg);
    } else if is_merge {
        painter.circle_filled(center, 3.5 * k, color);
    } else {
        painter.circle_filled(center, 4.5 * k, color);
        painter.circle_stroke(center, 4.5 * k, Stroke::new(1.0, p.bg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_shrinks_lanes_before_clipping() {
        let roomy = Layout::fit(3, 400.0);
        assert_eq!(roomy.lane_width, LANE_WIDTH);
        assert_eq!(roomy.width, PAD_LEFT + 3.0 * LANE_WIDTH + PAD_RIGHT);

        let tight = Layout::fit(40, 300.0);
        assert!(tight.lane_width < LANE_WIDTH && tight.lane_width >= MIN_LANE_WIDTH);
        assert!(tight.width <= 300.0);
        // Everything fits: the last lane's center is inside the column.
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(tight.width, 24.0));
        assert!(tight.x(rect, 39) < rect.right());

        let hopeless = Layout::fit(300, 300.0);
        assert_eq!(hopeless.lane_width, MIN_LANE_WIDTH);
        assert!(hopeless.width <= 300.0);
    }
}
