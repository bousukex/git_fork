//! File History and Blame views (the top of the tab's browse stack).

use bstr::{BString, ByteSlice};
use egui::{Align2, FontId, Pos2, Rect, RichText, ScrollArea, Sense, vec2};
use pengitto_core::model::Oid;

use super::{code_galley, format_time, paint_text};
use crate::{
    blame_model::{AGE_BUCKETS, BlameDoc},
    repo::{
        RepoTab,
        browse::{BlameView, Browse, FileHistoryView},
    },
    theme::Palette,
};

const ROW_H: f32 = 22.0;
const ANNOTATION_W: f32 = 300.0;

/// Something chosen in a browse view, applied after drawing.
enum Action {
    Back,
    CloseAll,
    SelectEntry(usize),
    SelectLine(usize),
    ShowCommit(Oid),
    /// Blame the file at a File History entry.
    BlameEntry(usize),
    BlamePrevious(usize),
    FileHistory(Oid, BString),
    IgnoreWhitespace(bool),
    /// The blame view scrolled to its target line.
    Scrolled,
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let mut actions = Vec::new();
    let depth = tab.browse.len();
    let (is_history, title, note, whitespace) = match tab.browse.last() {
        Some(Browse::FileHistory(v)) => (true, title_history(v), v.note.clone(), None),
        Some(Browse::Blame(v)) => {
            let at = match v.rev {
                Some(r) => format!("at {}", r.short()),
                None => "in the work tree".into(),
            };
            (false, format!("Blame {} {at}", v.path.to_str_lossy()), None, Some(v.ignore_whitespace))
        }
        None => return,
    };
    header(ui, depth, &title, note.as_deref(), whitespace, &mut actions);
    if is_history {
        file_history(ui, tab, &mut actions);
    } else if let Some(Browse::Blame(v)) = tab.browse.last() {
        blame(ui, &tab.paths.workdir, v, &mut actions);
    }
    for a in actions {
        match a {
            Action::Back => tab.close_browse(),
            Action::CloseAll => tab.clear_browse(),
            Action::SelectEntry(i) => tab.select_history_entry(i),
            Action::SelectLine(i) => {
                if let Some(Browse::Blame(v)) = tab.browse.last_mut() {
                    v.selected = Some(i);
                }
            }
            Action::ShowCommit(id) => tab.show_commit(id),
            Action::BlameEntry(i) => tab.blame_of_history_entry(i),
            Action::BlamePrevious(line) => tab.blame_previous(line),
            Action::FileHistory(start, path) => tab.open_file_history(start, path, None),
            Action::IgnoreWhitespace(on) => tab.set_blame_whitespace(on),
            Action::Scrolled => {
                if let Some(Browse::Blame(v)) = tab.browse.last_mut() {
                    v.scroll_to = None;
                }
            }

        }
    }
}

fn title_history(v: &FileHistoryView) -> String {
    format!("History of {}", v.path.to_str_lossy())
}

fn header(ui: &mut egui::Ui, depth: usize, title: &str, note: Option<&str>, whitespace: Option<bool>, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if ui.button("← Back").on_hover_text("Back (Esc)").clicked() {
                actions.push(Action::Back);
            }
            ui.label(RichText::new(title).strong());
            if let Some(n) = note {
                ui.label(RichText::new(n).small().color(p.text_weak));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if depth > 1 && ui.small_button("×").on_hover_text("Close all").clicked() {
                    actions.push(Action::CloseAll);
                }
                if let Some(mut ws) = whitespace
                    && ui.checkbox(&mut ws, "Ignore whitespace").changed()
                {
                    actions.push(Action::IgnoreWhitespace(ws));
                }
            });
        });
    });
}

fn file_history(ui: &mut egui::Ui, tab: &mut RepoTab, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let Some(Browse::FileHistory(v)) = tab.browse.last() else { return };
    egui::Panel::left("file-history-list")
        .resizable(true)
        .default_size(440.0)
        .size_range(260.0..=900.0)
        .frame(egui::Frame::NONE.fill(p.bg))
        .show(ui, |ui| {
            if let Some(e) = &v.error {
                ui.label(RichText::new(e).color(p.status_deleted));
            }
            if v.entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    if v.loading {
                        ui.spinner();
                    } else if v.error.is_none() {
                        ui.label(RichText::new("No commits changed this file").color(p.text_weak));
                    }
                });
                return;
            }
            ui.label(
                RichText::new(format!("{} commits{}", v.entries.len(), if v.loading { "…" } else { "" })).small().color(p.text_weak),
            )
            .on_hover_text(
                "Renames are followed along this commit's history (git log --follow). Where branches were \
                 merged, changes made only on a side branch may be missing.",
            );
            let row_h = ROW_H * 2.0;
            let viewport = ui.available_height();
            let offset_id = egui::Id::new(("file-history-offset", &tab.paths.workdir, v.view_id));
            let offset: f32 = ui.ctx().data(|d| d.get_temp(offset_id)).unwrap_or(0.0);
            let mut set_offset = None;
            let page = (viewport / row_h).max(1.0) as usize;
            if let Some(i) = super::history::keyboard_nav(ui, v.selected, v.entries.len(), page.saturating_sub(1).max(1)) {
                actions.push(Action::SelectEntry(i));
                set_offset = keep_visible(i, row_h, offset, viewport);
            }
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut area = ScrollArea::vertical().id_salt(("file-history", &tab.paths.workdir, v.view_id)).auto_shrink(false);
                if let Some(o) = set_offset {
                    area = area.vertical_scroll_offset(o);
                }
                let out = area.show_rows(
                    ui,
                    ROW_H * 2.0,
                    v.entries.len(),
                    |ui, range| {
                        for i in range {
                            let e = &v.entries[i];
                            let (rect, resp) = super::list_row(ui, ROW_H * 2.0, v.selected == Some(i), true);
                            let label = format!("file history {} {}", e.commit.id.short(), e.commit.subject);
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                            if resp.clicked() {
                                actions.push(Action::SelectEntry(i));
                            }
                            resp.context_menu(|ui| entry_menu(ui, i, e.commit.id, e.status == Some('D'), actions));
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            let painter = ui.painter_at(rect);
                            let top = Rect::from_min_size(rect.min + vec2(8.0, 2.0), vec2(rect.width() - 16.0, ROW_H - 2.0));
                            let bottom = top.translate(vec2(0.0, ROW_H - 4.0));
                            paint_text(&painter, top, &e.commit.subject, FontId::proportional(13.0), p.text, Align2::LEFT_CENTER);
                            // The file's name in this commit, and what happened to it.
                            let mut meta = format!(
                                "{}  {}  {}  ·  {}",
                                e.commit.id.short(),
                                e.commit.author_name,
                                format_time(e.commit.author_time),
                                e.path.to_str_lossy()
                            );
                            match (&e.old_path, e.status) {
                                (Some(old), Some('R')) => meta.push_str(&format!(" (renamed from {})", old.to_str_lossy())),
                                (Some(old), Some('C')) => meta.push_str(&format!(" (copied from {})", old.to_str_lossy())),
                                (_, Some('D')) => meta.push_str(" (deleted)"),
                                (_, Some('A')) => meta.push_str(" (added)"),
                                _ => {}
                            }
                            paint_text(&painter, bottom, &meta, FontId::proportional(11.5), p.text_weak, Align2::LEFT_CENTER);
                        }
                    },
                );
                ui.ctx().data_mut(|d| d.insert_temp(offset_id, out.state.offset.y));
            });
        });
    let diff_id = egui::Id::new(("browse-diff", &tab.paths.workdir, v.view_id));
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
        for e in super::diff_view::show(ui, diff_id, &tab.browse_diff, tab.diff_opts, None) {
            if let super::diff_view::DiffEvent::Options(opts) = e {
                tab.set_diff_options(opts);
            }
        }
    });
}

/// Scroll offset that brings `row` into view, if it isn't.
fn keep_visible(row: usize, row_h: f32, offset: f32, viewport: f32) -> Option<f32> {
    let (top, bottom) = (row as f32 * row_h, (row + 1) as f32 * row_h);
    if top < offset {
        Some(top)
    } else if bottom > offset + viewport {
        Some(bottom - viewport)
    } else {
        None
    }
}

fn entry_menu(ui: &mut egui::Ui, i: usize, id: Oid, deleted: bool, actions: &mut Vec<Action>) {
    if ui.button("Show in History").clicked() {
        actions.push(Action::ShowCommit(id));
        ui.close();
    }
    let blame = if deleted { "Blame Before Deletion" } else { "Blame at This Commit" };
    if ui.button(blame).clicked() {
        actions.push(Action::BlameEntry(i));
        ui.close();
    }
    if ui.button("Copy Commit Hash").clicked() {
        ui.ctx().copy_text(id.to_hex());
        ui.close();
    }
}

fn blame(ui: &mut egui::Ui, workdir: &std::path::Path, v: &BlameView, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let doc = match &v.doc {
        None => {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(p.status_deleted));
            return;
        }
        Some(Ok(doc)) => doc.clone(),
    };
    if doc.blame.lines.is_empty() {
        ui.centered_and_justified(|ui| ui.label(RichText::new("The file is empty").color(p.text_weak)));
        return;
    }
    let size = crate::settings::current(ui.ctx()).diff_font_size;
    let font = FontId::monospace(size);
    // 22 points at the default size, so the annotation fits.
    let row_h = (size + 9.5).round();
    let char_w = ui.painter().layout_no_wrap("0".into(), font.clone(), p.text).size().x;
    let num_w = (doc.number_width as f32 + 1.0) * char_w;
    let code_x = ANNOTATION_W + num_w + 12.0;
    let content_w = code_x + (doc.max_cols as f32 + 4.0) * char_w;
    let selected_origin = v.selected.and_then(|i| doc.blame.lines.get(i)).map(|l| l.origin);
    let viewport = ui.available_height();
    let offset_id = egui::Id::new(("blame-offset", workdir, v.view_id));
    let offset: f32 = ui.ctx().data(|d| d.get_temp(offset_id)).unwrap_or(0.0);
    // A target line (e.g. after "Blame Previous Revision") is centered; a
    // keyboard move only scrolls as far as needed to keep the line visible.
    let mut set_offset = v.scroll_to.map(|line| (line as f32 * row_h - viewport / 2.0).max(0.0));
    let page = (viewport / row_h).max(1.0) as usize;
    if let Some(i) = super::history::keyboard_nav(ui, v.selected, doc.blame.lines.len(), page.saturating_sub(1).max(1)) {
        actions.push(Action::SelectLine(i));
        set_offset = keep_visible(i, row_h, offset, viewport).or(set_offset);
    }
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let mut area = ScrollArea::both().id_salt(("blame", workdir, v.view_id)).auto_shrink(false);
        if let Some(o) = set_offset {
            area = area.vertical_scroll_offset(o);
        }
        let out = area.show_rows(ui, row_h, doc.blame.lines.len(), |ui, range| {
            let width = content_w.max(ui.available_width());
            for i in range {
                let size = vec2(width, row_h);
                blame_row(ui, &doc, i, size, code_x, num_w, &font, v.selected == Some(i), selected_origin, actions);
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(offset_id, out.state.offset.y));
    });
    // Scrolled there once; afterwards the user scrolls.
    if v.scroll_to.is_some() {
        actions.push(Action::Scrolled);
    }
}

#[allow(clippy::too_many_arguments)]
fn blame_row(
    ui: &mut egui::Ui,
    doc: &BlameDoc,
    i: usize,
    size: egui::Vec2,
    code_x: f32,
    num_w: f32,
    font: &FontId,
    selected: bool,
    selected_origin: Option<usize>,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui.ctx());
    let line = &doc.blame.lines[i];
    let origin = &doc.blame.origins[line.origin];
    let commit = &doc.blame.commits[origin.commit];
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let row_h = size.y;
    let label = format!("blame line {} {}", line.final_line, commit.id.short());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if resp.clicked() || resp.secondary_clicked() {
        actions.push(Action::SelectLine(i));
    }
    let uncommitted = commit.is_uncommitted();
    resp.context_menu(|ui| {
        let block = uncommitted.then_some("Not committed yet");
        let item = |ui: &mut egui::Ui, text: &str, block: Option<&str>| {
            let r = ui.add_enabled(block.is_none(), egui::Button::new(text)).on_disabled_hover_text(block.unwrap_or_default());
            let c = r.clicked();
            if c {
                ui.close();
            }
            c
        };
        if item(ui, "Show Commit", block) {
            actions.push(Action::ShowCommit(commit.id));
        }
        // Uncommitted lines can have a "previous" too (the committed version).
        let no_previous = origin.previous.is_none().then_some(if uncommitted { "Not committed yet" } else { "The file starts here" });
        if item(ui, "Blame Previous Revision", no_previous) {
            actions.push(Action::BlamePrevious(i));
        }
        if item(ui, "File History", block) {
            actions.push(Action::FileHistory(commit.id, origin.filename.clone()));
        }
        if item(ui, "Copy Commit Hash", block) {
            ui.ctx().copy_text(commit.id.to_hex());
        }
    });
    let resp = if uncommitted {
        resp
    } else {
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(RichText::new(&commit.summary).strong());
            ui.label(format!("{} <{}>", commit.author, commit.author_mail));
            ui.label(format!("{}  {}", format_time(commit.author_time), commit.id.to_hex()));
            if origin.filename.as_bytes() != doc.blame.origins[doc.blame.lines[0].origin].filename.as_bytes() {
                ui.label(RichText::new(format!("from {}", origin.filename.to_str_lossy())).color(p.text_weak));
            }
        })
    };
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    let in_chunk = selected_origin == Some(line.origin);
    if selected {
        painter.rect_filled(rect, 0.0, p.selection_bg);
    } else if in_chunk {
        painter.rect_filled(rect, 0.0, p.selection_bg_unfocused);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, p.row_hover);
    }
    // Annotation column: age bar always, details on a chunk's first line.
    let ann = Rect::from_min_size(rect.min, vec2(ANNOTATION_W, row_h));
    let age = doc.age[origin.commit] as f32;
    let strength = 1.0 - age / AGE_BUCKETS as f32 * 0.85;
    painter.rect_filled(Rect::from_min_size(ann.min, vec2(4.0, row_h)), 0.0, p.accent.gamma_multiply(strength));
    if doc.chunk_start[i] {
        painter.line_segment([rect.left_top(), rect.right_top()], (1.0, p.separator));
        let text = if uncommitted {
            "Not committed yet".to_owned()
        } else {
            format!("{}  {}  {}", commit.id.short(), commit.author, format_time(commit.author_time))
        };
        let r = Rect::from_min_max(ann.min + vec2(10.0, 0.0), ann.max - vec2(8.0, 0.0));
        paint_text(painter, r, &text, FontId::proportional(11.5), if uncommitted { p.status_modified } else { p.text_weak }, Align2::LEFT_CENTER);
    }
    let num_r = Rect::from_min_size(Pos2::new(rect.left() + ANNOTATION_W, rect.top()), vec2(num_w, row_h));
    painter.rect_filled(num_r.expand2(vec2(4.0, 0.0)), 0.0, p.sidebar_bg);
    paint_text(painter, num_r, &line.final_line.to_string(), font.clone(), p.diff_gutter, Align2::RIGHT_CENTER);
    let galley = code_galley(painter, &doc.text[i], doc.spans(i), font.clone(), p.text, p);
    painter.galley(Pos2::new(rect.left() + code_x, rect.center().y - galley.size().y / 2.0), galley, p.text);
}
