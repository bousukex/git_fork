//! Unified diff viewer.

use bstr::ByteSlice;
use egui::{Align2, FontId, Pos2, Rect, RichText, ScrollArea, Sense, vec2};
use pengitto_core::{diff::DiffOptions, model::ChangeKind};

use super::{code_galley, paint_text};
use std::collections::BTreeSet;

use pengitto_core::patch::PatchMode;

use crate::{
    diff_model::{DiffDoc, RowKind},
    repo::{DiffPane, LineActions, PatchTarget, Side},
    theme::Palette,
};

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

/// Something the user did in the diff view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEvent {
    Options(DiffOptions),
    /// New row selection and Shift+click anchor.
    Select(BTreeSet<usize>, Option<usize>),
    Apply(PatchMode, PatchTarget),
}

/// Shows `pane`. `actions` enables hunk/line staging (local changes only).
/// `id` keeps this diff's scroll state apart from the other diffs (commit
/// details, local changes, file history) and tabs.
pub fn show(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, pane: &DiffPane, opts: DiffOptions, actions: Option<LineActions>) -> Vec<DiffEvent> {
    ui.push_id(egui::Id::new(id), |ui| show_inner(ui, pane, opts, actions)).inner
}

fn show_inner(ui: &mut egui::Ui, pane: &DiffPane, opts: DiffOptions, actions: Option<LineActions>) -> Vec<DiffEvent> {
    let mut events = Vec::new();
    let Some(file) = &pane.file else {
        placeholder(ui, "Select a file to see its changes");
        return events;
    };
    events.extend(header(ui, pane, opts).map(DiffEvent::Options));
    match &pane.doc {
        None => {
            ui.centered_and_justified(|ui| ui.spinner());
        }
        Some(Err(e)) => {
            let p = Palette::of(ui.ctx());
            ui.label(RichText::new(e).color(p.status_deleted));
        }
        Some(Ok(doc)) if doc.header.iter().any(|h| h == pengitto_core::diff::CONFLICT_DELETED_HEADER) => placeholder(
            ui,
            "Deleted on one side of the conflict. Right-click the file to keep a version or resolve by deleting.",
        ),
        Some(Ok(doc)) if doc.source.lfs.is_some() => lfs_view(ui, doc.source.lfs.as_ref().expect("checked")),
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
        Some(Ok(doc)) => {
            if doc.header.iter().any(|h| h == pengitto_core::diff::CONFLICT_HEADER) {
                conflict_legend(ui);
            }
            rows(ui, doc, pane, actions, &mut events)
        }
    }
    events
}

/// A size in bytes, readably.
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} bytes") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// A Git LFS file's diff: the objects its pointers stand for (nothing is
/// downloaded to show it).
fn lfs_view(ui: &mut egui::Ui, view: &pengitto_core::lfs::LfsView) {
    use pengitto_core::lfs::LfsSide;
    let p = Palette::of(ui.ctx());
    let side = |s: &LfsSide| match s {
        LfsSide::Absent => "(no file)".to_owned(),
        LfsSide::Pointer(ptr) => format!("object {} · {}", ptr.short_oid(), human_size(ptr.size)),
        LfsSide::Contents { size } => format!("the file's contents · {}", human_size(*size)),
    };
    egui::Frame::NONE.inner_margin(egui::Margin::same(16)).show(ui, |ui| {
        ui.label(RichText::new("Git LFS object").strong());
        ui.add_space(4.0);
        egui::Grid::new("lfs-sides").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("Before").color(p.text_weak));
            ui.label(RichText::new(side(&view.old)).monospace());
            ui.end_row();
            ui.label(RichText::new("After").color(p.text_weak));
            ui.label(RichText::new(side(&view.new)).monospace());
            ui.end_row();
        });
        ui.add_space(6.0);
        if matches!(view.new, LfsSide::Contents { .. }) {
            ui.label(RichText::new("The working tree has the file's contents (not its pointer).").small().color(p.text_weak));
        }
        ui.label(
            RichText::new("The repository keeps a small pointer to the file; its contents are stored with Git LFS.")
                .small()
                .color(p.text_weak),
        );
    });
}

fn conflict_legend(ui: &mut egui::Ui) {
    let p = Palette::of(ui.ctx());
    egui::Frame::NONE.inner_margin(egui::Margin::symmetric(8, 3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("■").color(p.diff_del_strong));
            ui.label(RichText::new("HEAD (ours)").small());
            ui.label(RichText::new("■").color(p.diff_add_strong));
            ui.label(RichText::new("Incoming (theirs)").small());
            ui.label(
                RichText::new("— edit the file, or right-click it in the list to pick a side")
                    .small()
                    .color(p.text_weak),
            );
        });
    });
}

/// Selection after a press on `row` (only changed rows are selectable).
fn select_on_press(doc: &DiffDoc, pane: &DiffPane, row: usize, ctrl: bool, shift: bool) -> (BTreeSet<usize>, Option<usize>) {
    let mut sel = pane.selected_rows.clone();
    match (ctrl, shift, pane.anchor) {
        (_, true, Some(anchor)) => {
            sel = range_of(doc, anchor, row);
            (sel, Some(anchor))
        }
        (true, _, _) => {
            if !sel.remove(&row) {
                sel.insert(row);
            }
            (sel, Some(row))
        }
        _ => ([row].into(), Some(row)),
    }
}

fn range_of(doc: &DiffDoc, a: usize, b: usize) -> BTreeSet<usize> {
    (a.min(b)..=a.max(b)).filter(|&i| doc.rows.get(i).is_some_and(|r| r.is_change())).collect()
}

/// Buttons in a hunk header: act on the selected lines of this hunk if
/// any, otherwise on the whole hunk.
fn hunk_buttons(ui: &mut egui::Ui, row_rect: Rect, hunk: usize, selected_here: usize, actions: LineActions, events: &mut Vec<DiffEvent>) {
    let (target, noun) = if selected_here > 0 {
        // Exactly the lines counted on the button, never other hunks' selections.
        (PatchTarget::SelectedIn(hunk), format!("Lines ({selected_here})"))
    } else {
        (PatchTarget::Hunk(hunk), "Hunk".to_owned())
    };
    let buttons: &[(PatchMode, &str)] = match actions.side {
        Side::Unstaged => &[(PatchMode::Stage, "Stage"), (PatchMode::Discard, "Discard")],
        Side::Staged => &[(PatchMode::Unstage, "Unstage")],
    };
    // Keep the buttons at the right edge of the visible area, not of the
    // (horizontally scrollable) content. They are painted by hand so they
    // take no layout space inside the virtualized rows.
    let p = Palette::of(ui.ctx());
    let enabled = actions.disabled.is_none();
    // 11.5 at the default row height; follows the code font size.
    let font = FontId::proportional((row_rect.height() * 0.64).min(13.0));
    let mut right = ui.clip_rect().right().min(row_rect.right()) - 4.0;
    for (mode, verb) in buttons {
        let label = format!("{verb} {noun}");
        let text_color = if enabled { p.text } else { p.text_weak };
        let galley = ui.painter().layout_no_wrap(label.clone(), font.clone(), text_color);
        let w = galley.size().x + 14.0;
        let rect = Rect::from_min_max(Pos2::new(right - w, row_rect.top() + 1.5), Pos2::new(right, row_rect.bottom() - 1.5));
        let id = ui.id().with(("hunk-button", hunk, verb));
        let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
        // Hand-drawn, so describe it for accessibility (and UI tests).
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &label));
        let fill = if !enabled {
            p.sidebar_bg
        } else if resp.is_pointer_button_down_on() {
            p.selection_bg
        } else if resp.hovered() {
            p.row_hover
        } else {
            p.bg
        };
        ui.painter().rect(rect, 3.0, fill, egui::Stroke::new(1.0, p.separator), egui::StrokeKind::Inside);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, text_color);
        let resp = match actions.disabled {
            Some(why) => resp.on_hover_text(why),
            None => resp.on_hover_text(match mode {
                PatchMode::Stage => "Add these changes to the index",
                PatchMode::Unstage => "Take these changes out of the index",
                PatchMode::Discard => "Revert these changes in the working tree",
            }),
        };
        if enabled && resp.clicked() {
            events.push(DiffEvent::Apply(*mode, target));
        }
        right -= w + 4.0;
    }
}

fn rows(ui: &mut egui::Ui, doc: &DiffDoc, pane: &DiffPane, actions: Option<LineActions>, events: &mut Vec<DiffEvent>) {
    let p = Palette::of(ui.ctx());
    let size = crate::settings::current(ui.ctx()).diff_font_size;
    let font = FontId::monospace(size);
    let char_w = ui.painter().layout_no_wrap("0".into(), font.clone(), p.text).size().x;
    let row_h = (size * 1.45).round();
    let num_w = (doc.number_width as f32 + 1.0) * char_w;
    let gutter = num_w * 2.0 + 18.0;
    let content_w = gutter + (doc.max_cols as f32 + 4.0) * char_w;
    let selectable = actions.is_some_and(|a| a.disabled.is_none());
    // Row where a drag selection started (kept while the button is held).
    let drag_id = ui.id().with("diff-drag");
    // Stored as Option<usize>: read it as that type.
    let mut drag_anchor: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)).flatten();
    let (pointer, primary_down) = ui.input(|i| (i.pointer.interact_pos(), i.pointer.primary_down()));
    if !primary_down {
        drag_anchor = None;
    }
    let selected_in_hunk = |h: usize| pane.selected_rows.iter().filter(|&&i| doc.rows[i].hunk == h).count();

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ScrollArea::both().id_salt("diff").auto_shrink(false).show_rows(ui, row_h, doc.rows.len(), |ui, range| {
            let width = content_w.max(ui.available_width());
            for i in range {
                let row = &doc.rows[i];
                let sense = if selectable && row.is_change() { Sense::click_and_drag() } else { Sense::hover() };
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), sense);
                if selectable && row.is_change() {
                    // For assistive technology (and UI tests).
                    let (what, no) = match row.kind {
                        RowKind::Added => ("added", row.new_no),
                        _ => ("deleted", row.old_no),
                    };
                    let label = format!("{what} line {}", no.unwrap_or(0));
                    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, pane.selected_rows.contains(&i), &label));
                }

                if selectable && row.is_change() {
                    if resp.drag_started() || resp.clicked() {
                        let m = ui.input(|i| i.modifiers);
                        let (sel, anchor) = select_on_press(doc, pane, i, m.command, m.shift);
                        if !m.command && !m.shift {
                            drag_anchor = Some(i);
                        }
                        events.push(DiffEvent::Select(sel, anchor));
                    } else if let (Some(a), Some(pos)) = (drag_anchor, pointer)
                        && primary_down
                        && rect.y_range().contains(pos.y)
                        && a != i
                    {
                        events.push(DiffEvent::Select(range_of(doc, a, i), Some(a)));
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let selected = pane.selected_rows.contains(&i);
                let painter = ui.painter();
                let (bg, marker) = match row.kind {
                    RowKind::Added => (Some(if selected { p.diff_add_strong } else { p.diff_add_bg }), "+"),
                    RowKind::Deleted => (Some(if selected { p.diff_del_strong } else { p.diff_del_bg }), "-"),
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
                if selected {
                    painter.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, row_h)), 0.0, p.accent);
                } else if resp.hovered() && row.is_change() && selectable {
                    painter.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, row_h)), 0.0, p.accent.gamma_multiply(0.4));
                }
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
                let galley = code_galley(painter, &row.text, &row.spans, font.clone(), color, p);
                let pos = Pos2::new(rect.left() + gutter + 4.0, rect.center().y - galley.size().y / 2.0);
                painter.galley(pos, galley, color);

                if row.kind == RowKind::Hunk
                    && let Some(actions) = actions
                {
                    hunk_buttons(ui, rect, row.hunk, selected_in_hunk(row.hunk), actions, events);
                }
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(drag_id, drag_anchor));
}
