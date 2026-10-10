//! Drawing one row of the commit graph.

use egui::{Color32, Painter, Pos2, Rect, Stroke, epaint::CubicBezierShape};
use tine_core::graph::{EdgeKind, Graph, Lane};

use crate::theme::Palette;

pub const LANE_WIDTH: f32 = 14.0;
pub const PAD_LEFT: f32 = 6.0;
const LINE_WIDTH: f32 = 1.6;

pub fn lane_x(rect: Rect, lane: Lane) -> f32 {
    rect.left() + PAD_LEFT + lane as f32 * LANE_WIDTH + LANE_WIDTH / 2.0
}

/// Width needed to draw `lanes` lanes.
pub fn width_for(lanes: Lane) -> f32 {
    PAD_LEFT + lanes as f32 * LANE_WIDTH + 4.0
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

/// Paints row `index` of `graph` into `rect` (the graph column of the row).
pub fn paint_row(painter: &Painter, rect: Rect, graph: &Graph, index: usize, is_head: bool, p: &Palette) {
    let Some(row) = graph.row(index) else { return };
    let (top, mid, bottom) = (rect.top(), rect.center().y, rect.bottom());
    let stroke = |color: u16| Stroke::new(LINE_WIDTH, p.lane(color as usize));

    for (lane, color) in graph.through(index) {
        let x = lane_x(rect, lane);
        painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom)], stroke(color));
    }
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
        painter.circle_filled(center, 5.5, color);
        painter.circle_filled(center, 2.5, p.bg);
    } else if is_merge {
        painter.circle_filled(center, 3.5, color);
    } else {
        painter.circle_filled(center, 4.5, color);
        painter.circle_stroke(center, 4.5, Stroke::new(1.0, p.bg));
    }
}
