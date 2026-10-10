//! The commit list with graph, ref badges and columns.

use egui::{Align2, FontFamily, FontId, Key, Painter, Pos2, Rect, ScrollArea, Sense, vec2};
use tine_core::model::{Head, Oid, RefKind};

use super::{format_time, graph_paint, paint_text};
use crate::{repo::RepoTab, theme::Palette};

pub const ROW_HEIGHT: f32 = 24.0;
const HEADER_HEIGHT: f32 = 22.0;
const AUTHOR_W: f32 = 150.0;
const DATE_W: f32 = 120.0;
const SHA_W: f32 = 74.0;

struct Columns {
    desc: Rect,
    author: Rect,
    date: Rect,
    sha: Rect,
}

fn columns(row: Rect) -> Columns {
    let sha = Rect::from_min_max(Pos2::new(row.right() - SHA_W, row.top()), row.right_bottom());
    let date = Rect::from_min_max(Pos2::new(sha.left() - DATE_W, row.top()), Pos2::new(sha.left(), row.bottom()));
    let author = Rect::from_min_max(Pos2::new(date.left() - AUTHOR_W, row.top()), Pos2::new(date.left(), row.bottom()));
    let desc = Rect::from_min_max(row.left_top(), Pos2::new(author.left(), row.bottom()));
    Columns { desc, author, date, sha }
}

/// Paints a ref badge starting at `x`; returns its width.
fn badge(painter: &Painter, x: f32, cy: f32, text: &str, fill: egui::Color32, bold: bool, p: &Palette) -> f32 {
    let family = if bold { FontFamily::Name(crate::fonts::BOLD.into()) } else { FontFamily::Proportional };
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::new(11.5, family), p.badge_text);
    let rect = Rect::from_min_size(Pos2::new(x, cy - 8.5), vec2(galley.size().x + 10.0, 17.0));
    painter.rect_filled(rect, 3.0, fill);
    painter.galley(Pos2::new(x + 5.0, cy - galley.size().y / 2.0), galley, p.badge_text);
    rect.width()
}

fn badge_color(kind: RefKind, p: &Palette) -> egui::Color32 {
    match kind {
        RefKind::LocalBranch => p.badge_local,
        RefKind::RemoteBranch => p.badge_remote,
        RefKind::Tag => p.badge_tag,
        RefKind::Other => p.badge_stash,
    }
}

/// Moves the selection with the keyboard; returns the new row.
fn keyboard_nav(ui: &egui::Ui, current: Option<usize>, total: usize, page: usize) -> Option<usize> {
    if total == 0 || ui.ctx().memory(|m| m.focused().is_some()) {
        return None;
    }
    let cur = current.unwrap_or(0) as i64;
    let (up, down, pgup, pgdn, home, end) = ui.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::PageUp),
            i.key_pressed(Key::PageDown),
            i.key_pressed(Key::Home),
            i.key_pressed(Key::End),
        )
    });
    let next = if up {
        cur - 1
    } else if down {
        if current.is_none() { 0 } else { cur + 1 }
    } else if pgup {
        cur - page as i64
    } else if pgdn {
        cur + page as i64
    } else if home {
        0
    } else if end {
        total as i64 - 1
    } else {
        return None;
    };
    Some(next.clamp(0, total as i64 - 1) as usize)
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let total = tab.history.commits.len();

    // Header.
    let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_HEIGHT), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(header, 0.0, p.sidebar_bg);
    painter.line_segment([header.left_bottom(), header.right_bottom()], (1.0, p.separator));
    let cols = columns(header);
    let font = FontId::proportional(11.5);
    for (rect, label) in
        [(cols.desc, "Description"), (cols.author, "Author"), (cols.date, "Date"), (cols.sha, "Commit")]
    {
        paint_text(painter, rect.shrink2(vec2(8.0, 0.0)), label, font.clone(), p.text_weak, Align2::LEFT_CENTER);
    }

    if total == 0 {
        ui.centered_and_justified(|ui| {
            if tab.history.loading {
                ui.spinner();
            } else {
                ui.label(egui::RichText::new("No commits yet").color(p.text_weak));
            }
        });
        return;
    }

    let viewport_h = ui.available_height();
    let page = (viewport_h / ROW_HEIGHT).max(1.0) as usize;
    let selected_row = tab.selected_commit.and_then(|id| tab.history.index.get(&id).copied());
    let id = ui.id().with("history-scroll");
    let mut offset = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
    let mut set_offset = None;

    if let Some(row) = keyboard_nav(ui, selected_row, total, page.saturating_sub(1).max(1)) {
        let commit = tab.history.commits[row].id;
        tab.select_commit(commit);
        let (top, bottom) = (row as f32 * ROW_HEIGHT, (row + 1) as f32 * ROW_HEIGHT);
        if top < offset {
            set_offset = Some(top);
        } else if bottom > offset + viewport_h {
            set_offset = Some(bottom - viewport_h);
        }
    }
    if let Some(row) = tab.scroll_to_row.take() {
        set_offset = Some((row as f32 * ROW_HEIGHT - viewport_h / 2.0 + ROW_HEIGHT / 2.0).max(0.0));
    }

    let head = tab.head_commit();
    let detached = matches!(tab.refs.head, Some(Head::Detached(_)));
    let mut clicked: Option<Oid> = None;

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = ScrollArea::vertical().id_salt("history").auto_shrink(false);
        if let Some(o) = set_offset {
            area = area.vertical_scroll_offset(o);
        }
        let out = area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
            let graph = &tab.history.graph;
            let lanes = range.clone().filter_map(|i| graph.row(i)).map(|r| r.width).max().unwrap_or(1);
            let row_w = ui.available_width();
            let graph_w = graph_paint::width_for(lanes).min(columns(Rect::from_min_size(Pos2::ZERO, vec2(row_w, 1.0))).desc.width() * 0.5);
            for i in range {
                let c = &tab.history.commits[i];
                let selected = tab.selected_commit == Some(c.id);
                let (rect, resp) = super::list_row(ui, ROW_HEIGHT, selected, true);
                if resp.clicked() {
                    clicked = Some(c.id);
                }
                resp.context_menu(|ui| {
                    if ui.button("Copy Commit Hash").clicked() {
                        ui.ctx().copy_text(c.id.to_hex());
                        ui.close();
                    }
                    if ui.button("Copy Commit Message").clicked() {
                        ui.ctx().copy_text(c.subject.clone());
                        ui.close();
                    }
                });
                if !ui.is_rect_visible(rect) {
                    continue;
                }

                let cols = columns(rect);
                let graph_rect = Rect::from_min_size(cols.desc.min, vec2(graph_w, ROW_HEIGHT));
                let is_head = head == Some(c.id);
                graph_paint::paint_row(&ui.painter_at(graph_rect), graph_rect, graph, i, is_head, p);

                let painter = ui.painter_at(cols.desc);
                let cy = rect.center().y;
                let mut x = graph_rect.right() + 2.0;
                let badge_limit = cols.desc.left() + cols.desc.width() * 0.7;
                if is_head && detached {
                    x += badge(&painter, x, cy, "HEAD", p.badge_head, true, p) + 4.0;
                }
                if let Some(refs) = tab.refs_by_commit.get(&c.id) {
                    for (n, &ri) in refs.iter().enumerate() {
                        let r = &tab.refs.refs[ri];
                        if x > badge_limit {
                            x += badge(&painter, x, cy, &format!("+{}", refs.len() - n), p.badge_stash, false, p) + 4.0;
                            break;
                        }
                        let (text, fill) = if r.is_head {
                            (format!("✔ {}", r.short_name), p.badge_head)
                        } else {
                            (r.short_name.clone(), badge_color(r.kind, p))
                        };
                        x += badge(&painter, x, cy, &text, fill, r.is_head, p) + 4.0;
                    }
                }
                let is_merge = c.parents.len() > 1;
                let subject_color = if is_merge { p.text_weak } else { p.text };
                let subject_rect = Rect::from_min_max(Pos2::new(x + 2.0, rect.top()), Pos2::new(cols.desc.right() - 8.0, rect.bottom()));
                let body = FontId::proportional(13.0);
                paint_text(&painter, subject_rect, &c.subject, body.clone(), subject_color, Align2::LEFT_CENTER);

                let painter = ui.painter();
                paint_text(painter, cols.author.shrink2(vec2(8.0, 0.0)), &c.author_name, body.clone(), p.text, Align2::LEFT_CENTER);
                let date = format_time(c.author_time);
                paint_text(painter, cols.date.shrink2(vec2(8.0, 0.0)), &date, body, p.text_weak, Align2::LEFT_CENTER);
                paint_text(
                    painter,
                    cols.sha.shrink2(vec2(8.0, 0.0)),
                    &c.id.to_hex()[..7],
                    FontId::monospace(12.0),
                    p.text_weak,
                    Align2::LEFT_CENTER,
                );
            }
        });
        offset = out.state.offset.y;
    });
    ui.ctx().data_mut(|d| d.insert_temp(id, offset));

    if let Some(id) = clicked {
        tab.select_commit(id);
    }
}
