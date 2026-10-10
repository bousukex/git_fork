//! Unified diff viewer.

use bstr::ByteSlice;
use egui::{Align2, FontId, Pos2, Rect, RichText, ScrollArea, Sense, vec2};
use tine_core::{diff::DiffOptions, model::ChangeKind};

use super::paint_text;
use crate::{
    diff_model::{DiffDoc, RowKind},
    repo::DiffPane,
    theme::Palette,
};

const FONT_SIZE: f32 = 12.5;

/// Toolbar above the diff; returns changed options.
fn header(ui: &mut egui::Ui, pane: &DiffPane, opts: DiffOptions) -> Option<DiffOptions> {
    let p = Palette::of(ui.ctx());
    let mut new = opts;
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if let Some(f) = &pane.file {
                let path = f.path.to_str_lossy();
                let text = match &f.orig_path {
                    Some(o) => format!("{} → {}", o.to_str_lossy(), path),
                    None => path.into_owned(),
                };
                ui.add(egui::Label::new(RichText::new(text).strong()).truncate());
                if let Some(Ok(doc)) = &pane.doc
                    && !doc.binary
                {
                    ui.label(RichText::new(format!("+{}", doc.added)).color(p.status_added));
                    ui.label(RichText::new(format!("−{}", doc.deleted)).color(p.status_deleted));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut new.ignore_whitespace, "Ignore whitespace");
            });
        });
    });
    (new != opts).then_some(new)
}

fn placeholder(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.centered_and_justified(|ui| ui.label(RichText::new(text).color(p.text_weak)));
}

/// Shows `pane`; returns new diff options if the user changed them.
pub fn show(ui: &mut egui::Ui, pane: &DiffPane, opts: DiffOptions) -> Option<DiffOptions> {
    let Some(file) = &pane.file else {
        placeholder(ui, "Select a file to see its changes");
        return None;
    };
    let changed = header(ui, pane, opts);
    match &pane.doc {
        None => {
            ui.centered_and_justified(|ui| ui.spinner());
        }
        Some(Err(e)) => {
            let p = Palette::of(ui.ctx());
            ui.label(RichText::new(e).color(p.status_deleted));
        }
        Some(Ok(doc)) if doc.binary => placeholder(ui, "Binary file"),
        Some(Ok(doc)) if doc.is_empty() => {
            let msg = if file.is_submodule {
                "Submodule changed".to_owned()
            } else if file.kind == ChangeKind::Renamed || doc.header.iter().any(|h| h.starts_with("rename from")) {
                "File renamed without content changes".to_owned()
            } else if let Some(mode) = doc.header.iter().find(|h| h.starts_with("new mode") || h.starts_with("old mode")) {
                format!("File mode changed ({mode})")
            } else {
                "No content changes".to_owned()
            };
            placeholder(ui, &msg);
        }
        Some(Ok(doc)) => rows(ui, doc),
    }
    changed
}

fn rows(ui: &mut egui::Ui, doc: &DiffDoc) {
    let p = Palette::of(ui.ctx());
    let font = FontId::monospace(FONT_SIZE);
    let char_w = ui.painter().layout_no_wrap("0".into(), font.clone(), p.text).size().x;
    let row_h = (FONT_SIZE * 1.45).round();
    let num_w = (doc.number_width as f32 + 1.0) * char_w;
    let gutter = num_w * 2.0 + 18.0;
    let content_w = gutter + (doc.max_cols as f32 + 4.0) * char_w;

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ScrollArea::both().id_salt("diff").auto_shrink(false).show_rows(ui, row_h, doc.rows.len(), |ui, range| {
            let width = content_w.max(ui.available_width());
            for row in &doc.rows[range] {
                let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let painter = ui.painter();
                let (bg, marker) = match row.kind {
                    RowKind::Added => (Some(p.diff_add_bg), "+"),
                    RowKind::Deleted => (Some(p.diff_del_bg), "-"),
                    RowKind::Hunk => (Some(p.diff_hunk_bg), ""),
                    RowKind::Context | RowKind::NoNewline => (None, ""),
                };
                if let Some(bg) = bg {
                    painter.rect_filled(rect, 0.0, bg);
                }
                let gutter_rect = Rect::from_min_size(rect.min, vec2(gutter, row_h));
                let gutter_bg = match row.kind {
                    RowKind::Added => p.diff_add_strong,
                    RowKind::Deleted => p.diff_del_strong,
                    _ => p.sidebar_bg,
                };
                painter.rect_filled(gutter_rect, 0.0, gutter_bg.gamma_multiply(if bg.is_some() { 0.6 } else { 1.0 }));
                let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                let old_r = Rect::from_min_size(rect.min, vec2(num_w, row_h));
                let new_r = Rect::from_min_size(Pos2::new(rect.left() + num_w + 4.0, rect.top()), vec2(num_w, row_h));
                paint_text(painter, old_r, &num(row.old_no), font.clone(), p.diff_gutter, Align2::RIGHT_CENTER);
                paint_text(painter, new_r, &num(row.new_no), font.clone(), p.diff_gutter, Align2::RIGHT_CENTER);
                let mark_x = rect.left() + num_w * 2.0 + 8.0;
                painter.text(Pos2::new(mark_x, rect.center().y), Align2::LEFT_CENTER, marker, font.clone(), p.text_weak);

                let color = match row.kind {
                    RowKind::Hunk | RowKind::NoNewline => p.text_weak,
                    _ => p.text,
                };
                let galley = painter.layout_no_wrap(row.text.clone(), font.clone(), color);
                let pos = Pos2::new(rect.left() + gutter + 4.0, rect.center().y - galley.size().y / 2.0);
                painter.galley(pos, galley, color);
            }
        });
    });
}
