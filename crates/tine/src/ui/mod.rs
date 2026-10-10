pub mod askpass;
pub mod banner;
pub mod changes;
pub mod detail;
pub mod dialogs;
pub mod diff_view;
pub mod graph_paint;
pub mod history;
pub mod menus;
pub mod output;
pub mod sidebar;

use std::sync::Arc;

use bstr::{BString, ByteSlice};
use egui::{Align2, Color32, FontId, Galley, Painter, Pos2, Rect, text::LayoutJob};
use tine_core::model::ChangeKind;

use crate::theme::Palette;

/// Lays out single-line text cut to `max_width` with an ellipsis.
pub fn truncated(painter: &Painter, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_width.max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// Paints truncated text vertically centered in `rect`.
pub fn paint_text(painter: &Painter, rect: Rect, text: &str, font: FontId, color: Color32, align: Align2) {
    let galley = truncated(painter, text, font, color, rect.width());
    let pos = align.align_size_within_rect(galley.size(), rect).min;
    painter.galley(pos, galley, color);
}

/// `2026-10-10 11:03` in local time.
pub fn format_time(unix: i64) -> String {
    use chrono::TimeZone as _;
    match chrono::Local.timestamp_opt(unix, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

pub fn status_color(kind: ChangeKind, p: &Palette) -> Color32 {
    match kind {
        ChangeKind::Added => p.status_added,
        ChangeKind::Modified | ChangeKind::TypeChanged => p.status_modified,
        ChangeKind::Deleted => p.status_deleted,
        ChangeKind::Renamed | ChangeKind::Copied => p.status_renamed,
        ChangeKind::Untracked => p.status_untracked,
        ChangeKind::Conflicted => p.status_conflict,
    }
}

/// Splits a repo path into (file name, parent directory) for display.
pub fn split_path(path: &BString) -> (String, String) {
    let s = path.to_str_lossy();
    match s.rsplit_once('/') {
        Some((dir, name)) => (name.to_owned(), dir.to_owned()),
        None => (s.into_owned(), String::new()),
    }
}

/// Paints a small rounded status letter box (like Fork's file icons).
pub fn status_badge(painter: &Painter, center: Pos2, kind: ChangeKind, p: &Palette) {
    let rect = Rect::from_center_size(center, egui::vec2(15.0, 15.0));
    let color = status_color(kind, p);
    painter.rect_filled(rect, 3.0, color);
    painter.text(rect.center(), Align2::CENTER_CENTER, kind.letter(), FontId::proportional(10.5), Color32::WHITE);
}

/// A full-width clickable list row; returns its rect and response.
pub fn list_row(ui: &mut egui::Ui, height: f32, selected: bool, focused: bool) -> (Rect, egui::Response) {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    if selected {
        let fill = if focused { p.selection_bg } else { p.selection_bg_unfocused };
        ui.painter().rect_filled(rect, 0.0, fill);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, p.row_hover);
    }
    (rect, resp)
}

/// Section caption used in panels ("Unstaged", "Staged", …).
pub fn caption(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.label(egui::RichText::new(text).small().strong().color(p.text_weak));
}
