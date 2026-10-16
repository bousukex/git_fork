//! The commit list with graph, ref badges and columns.

use egui::{Align2, FontFamily, FontId, Key, Painter, Pos2, Rect, ScrollArea, Sense, vec2};
use pengitto_core::model::{Head, Oid, RefKind};

use super::{format_time, graph_paint, paint_text};
use crate::{
    input,
    repo::{RepoTab, search::SearchMode},
    theme::Palette,
};

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
pub(super) fn keyboard_nav(ui: &egui::Ui, current: Option<usize>, total: usize, page: usize) -> Option<usize> {
    if total == 0 || ui.ctx().memory(|m| m.focused().is_some()) || !input::views_have_keys(ui.ctx()) {
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

/// The search bar above the list (Ctrl+F).
fn search_bar(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let field_id = egui::Id::new(("history-search", &tab.paths.workdir));
    let ctx = ui.ctx().clone();
    let field_focused = ctx.memory(|m| m.has_focus(field_id));
    let keys = input::views_have_keys(&ctx);
    // Enter in a single-line field would end editing; take it first.
    let mut step = None;
    if keys && field_focused {
        if input::take_key(&ctx, egui::Modifiers::SHIFT, Key::Enter) {
            step = Some(false);
        } else if input::take_key(&ctx, egui::Modifiers::NONE, Key::Enter) {
            step = Some(true);
        }
    }
    if keys && (!ctx.text_edit_focused() || field_focused) {
        if input::take_key(&ctx, egui::Modifiers::SHIFT, Key::F3) {
            step = Some(false);
        } else if input::take_key(&ctx, egui::Modifiers::NONE, Key::F3) {
            step = Some(true);
        }
    }
    let mut close = keys && field_focused && input::take_key(&ctx, egui::Modifiers::NONE, Key::Escape);
    let mut changed = false;
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let s = &mut tab.search;
            egui::ComboBox::from_id_salt("search-mode").selected_text(s.mode.label()).width(90.0).show_ui(ui, |ui| {
                for m in SearchMode::ALL {
                    if ui.selectable_value(&mut s.mode, m, m.label()).changed() {
                        changed = true;
                    }
                }
            });
            let resp = ui.add(
                egui::TextEdit::singleline(&mut s.query).id(field_id).hint_text(s.mode.hint()).desired_width(300.0),
            );
            if std::mem::take(&mut s.focus_field) {
                resp.request_focus();
            }
            changed |= resp.changed();
            let any = !s.rows.is_empty();
            if ui.add_enabled(any, egui::Button::new("↑").small()).on_hover_text("Previous match (Shift+Enter)").clicked() {
                step = Some(false);
            }
            if ui.add_enabled(any, egui::Button::new("↓").small()).on_hover_text("Next match (Enter)").clicked() {
                step = Some(true);
            }
            ui.checkbox(&mut s.only_matches, "Only matches");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui.small_button("×").on_hover_text("Close (Esc)").clicked();
                if s.running {
                    ui.spinner();
                }
                let n = s.matches.len();
                let (text, color) = if let Some(e) = &s.error {
                    (e.lines().next().unwrap_or_default().to_owned(), p.status_deleted)
                } else if s.query.trim().is_empty() {
                    (String::new(), p.text_weak)
                } else if n == 0 && s.complete {
                    ("No matches".to_owned(), p.text_weak)
                } else {
                    let mut t = match tab_position(s) {
                        Some(pos) if s.complete => format!("{pos} / {n}"),
                        _ if s.running || !s.complete => format!("{n} found, searching…"),
                        _ => format!("{n} found"),
                    };
                    if s.unloaded() > 0 {
                        t.push_str(&format!(" · {} not loaded yet", s.unloaded()));
                    }
                    (t, p.text_weak)
                };
                ui.label(egui::RichText::new(text).small().color(color));
            });
        });
    });
    if changed {
        tab.search_changed();
    }
    if close {
        tab.close_search();
    } else if let Some(forward) = step {
        tab.step_match(forward);
    }
}

/// The selection's place among matches, set by `show` each frame.
fn tab_position(s: &crate::repo::search::Search) -> Option<usize> {
    s.selected_position
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    tab.sync_search_rows();
    tab.search.selected_position = tab.match_position();
    if tab.search.open {
        search_bar(ui, tab);
    }
    // With "only matches", the list shows loaded matches without the graph.
    let filtered = tab.search.active() && tab.search.only_matches;
    let dim_others = tab.search.active() && !filtered && (!tab.search.matches.is_empty() || tab.search.complete);
    let total = if filtered { tab.search.rows.len() } else { tab.history.commits.len() };
    let history_row = |tab: &RepoTab, display: usize| if filtered { tab.search.rows[display] } else { display };
    let display_row = |tab: &RepoTab, row: usize| if filtered { tab.search.rows.binary_search(&row).ok() } else { Some(row) };

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
            if tab.history.loading || (filtered && tab.search.running) {
                ui.spinner();
            } else if filtered {
                ui.label(egui::RichText::new("No matching commits loaded").color(p.text_weak));
            } else {
                ui.label(egui::RichText::new("No commits yet").color(p.text_weak));
            }
        });
        return;
    }

    let viewport_h = ui.available_height();
    let page = (viewport_h / ROW_HEIGHT).max(1.0) as usize;
    let selected_row = tab
        .selected_commit
        .and_then(|id| tab.history.index.get(&id).copied())
        .and_then(|r| display_row(tab, r));
    let id = ui.id().with("history-scroll");
    let mut offset = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
    let mut set_offset = None;

    if let Some(row) = keyboard_nav(ui, selected_row, total, page.saturating_sub(1).max(1)) {
        let commit = tab.history.commits[history_row(tab, row)].id;
        tab.select_commit(commit);
        let (top, bottom) = (row as f32 * ROW_HEIGHT, (row + 1) as f32 * ROW_HEIGHT);
        if top < offset {
            set_offset = Some(top);
        } else if bottom > offset + viewport_h {
            set_offset = Some(bottom - viewport_h);
        }
    }
    if let Some(row) = tab.scroll_to_row.take().and_then(|r| display_row(tab, r)) {
        set_offset = Some((row as f32 * ROW_HEIGHT - viewport_h / 2.0 + ROW_HEIGHT / 2.0).max(0.0));
    }

    let head = tab.head_commit();
    let detached = matches!(tab.refs.head, Some(Head::Detached(_)));
    let mut clicked: Option<Oid> = None;
    let mut action: Option<super::menus::RefAction> = None;

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
            let desc_w = columns(Rect::from_min_size(Pos2::ZERO, vec2(row_w, 1.0))).desc.width();
            let layout = graph_paint::Layout::fit(lanes, desc_w * 0.5);
            // Lanes only make sense for the full, contiguous history.
            let graph_w = if filtered { 0.0 } else { layout.width };
            let rows = range.clone();
            let mut first_graph_rect: Option<Rect> = None;
            for di in range {
                let i = history_row(tab, di);
                let c = &tab.history.commits[i];
                let selected = tab.selected_commit == Some(c.id);
                let (rect, resp) = super::list_row(ui, ROW_HEIGHT, selected, true);
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("commit {} {}", c.id.short(), c.subject))
                });
                if di == rows.start {
                    first_graph_rect = Some(Rect::from_min_size(columns(rect).desc.min, vec2(graph_w, ROW_HEIGHT)));
                }
                if resp.clicked() {
                    clicked = Some(c.id);
                }
                resp.context_menu(|ui| {
                    clicked = Some(c.id);
                    super::menus::commit_menu(ui, tab, c, &mut action);
                });
                if !ui.is_rect_visible(rect) {
                    continue;
                }

                let cols = columns(rect);
                let graph_rect = Rect::from_min_size(cols.desc.min, vec2(graph_w, ROW_HEIGHT));
                let is_head = head == Some(c.id);
                if !filtered {
                    graph_paint::paint_row(&ui.painter_at(graph_rect), graph_rect, &layout, graph, i, is_head, p);
                }
                let is_match = tab.search.active() && tab.search.matches.contains(&c.id);
                if is_match {
                    ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(3.0, ROW_HEIGHT)), 0.0, p.accent);
                }
                let dim = dim_others && !is_match;
                let text = if dim { p.text_weak.gamma_multiply(0.7) } else { p.text };
                let weak = if dim { p.text_weak.gamma_multiply(0.7) } else { p.text_weak };

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
                            (format!("✓ {}", r.short_name), p.badge_head)
                        } else {
                            (r.short_name.clone(), badge_color(r.kind, p))
                        };
                        x += badge(&painter, x, cy, &text, fill, r.is_head, p) + 4.0;
                    }
                }
                let is_merge = c.parents.len() > 1;
                let subject_color = if is_merge { weak } else { text };
                let subject_rect = Rect::from_min_max(Pos2::new(x + 2.0, rect.top()), Pos2::new(cols.desc.right() - 8.0, rect.bottom()));
                let body = FontId::proportional(13.0);
                paint_text(&painter, subject_rect, &c.subject, body.clone(), subject_color, Align2::LEFT_CENTER);

                let painter = ui.painter();
                paint_text(painter, cols.author.shrink2(vec2(8.0, 0.0)), &c.author_name, body.clone(), text, Align2::LEFT_CENTER);
                let date = format_time(c.author_time);
                paint_text(painter, cols.date.shrink2(vec2(8.0, 0.0)), &date, body, weak, Align2::LEFT_CENTER);
                paint_text(
                    painter,
                    cols.sha.shrink2(vec2(8.0, 0.0)),
                    &c.id.to_hex()[..7],
                    FontId::monospace(12.0),
                    weak,
                    Align2::LEFT_CENTER,
                );
            }
            // Straight lanes as one segment each across all visible rows.
            if let Some(first) = first_graph_rect.filter(|_| !filtered) {
                let column = Rect::from_min_size(first.min, vec2(graph_w, ROW_HEIGHT * rows.len() as f32));
                graph_paint::paint_through(&ui.painter_at(column), first, ROW_HEIGHT, &layout, graph, rows, p);
            }
        });
        offset = out.state.offset.y;
    });
    ui.ctx().data_mut(|d| d.insert_temp(id, offset));

    if let Some(id) = clicked {
        tab.select_commit(id);
    }
    if let Some(a) = action {
        super::menus::apply(tab, a);
    }
}
