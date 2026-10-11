//! The interactive rebase dialog: the commits newest first (like the
//! history), each with its action; reorder by dragging or Alt+↑/↓; edit
//! reworded and combined messages on the right.

use egui::{Align2, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, vec2};
use tine_core::interactive::{Action, MessageKey};

use super::{format_time, paint_text};
use crate::{input, repo::rebase::RebaseDialog, theme::Palette};

/// What the dialog wants after this frame.
pub enum Outcome {
    Keep,
    Close,
    Start,
}

const ACTIONS: [(Action, &str, &str); 6] = [
    (Action::Pick, "P", "Pick: keep the commit"),
    (Action::Reword, "R", "Reword: keep it, with a new message"),
    (Action::Edit, "E", "Edit: stop after it, to amend it or add commits"),
    (Action::Squash, "S", "Squash: meld into the commit below, combining the messages"),
    (Action::Fixup, "F", "Fixup: meld into the commit below, dropping this message"),
    (Action::Drop, "D", "Drop: leave the commit out"),
];

const ROW_H: f32 = 26.0;

/// The list takes keys while it has the keyboard focus (given when the
/// dialog opens and when a row is clicked).
fn list_focus() -> egui::Id {
    egui::Id::new("interactive-rebase-list-focus")
}

pub fn show(ctx: &egui::Context, d: &mut RebaseDialog, dirty: bool, busy: bool) -> Outcome {
    let p = Palette::of(ctx);
    let screen = ctx.content_rect();
    let width = (screen.width() - 80.0).clamp(560.0, 1100.0);
    // Room around it in a big window; most of a small one.
    let height = (screen.height() - 40.0).min((screen.height() - 140.0).max(440.0)).min(760.0);
    let mut outcome = Outcome::Keep;
    let unchanged = d.plan == tine_core::interactive::Plan::new(&d.range);
    keys(ctx, d);
    let modal = egui::Modal::new(egui::Id::new("interactive-rebase")).show(ctx, |ui| {
        ui.set_width(width);
        ui.set_max_width(width);
        ui.set_max_height(height);
        let top = ui.min_rect().top();
        ui.heading("Interactive Rebase");
        let on = d.range.branch.as_deref().map(tine_core::interactive::branch_name).unwrap_or("detached HEAD").to_owned();
        let from = match d.range.base {
            Some(b) => format!("after {}", b.short()),
            None => "from the root".into(),
        };
        ui.label(RichText::new(format!("{} commits of '{on}' {from}, newest first", d.range.commits.len())).color(p.text_weak));
        let published = d.published();
        if published > 0 {
            ui.label(
                RichText::new(format!(
                    "{published} of these commits are on a remote-tracking branch (as last fetched): rebasing rewrites \
                     history others may have."
                ))
                .color(p.status_modified),
            );
        }
        ui.add_space(4.0);
        // The list takes what the rest leaves (the part below as laid out
        // last frame; it is measured again at the end). Fixed sizes: the
        // modal sizes itself to its content.
        let below_id = egui::Id::new("interactive-rebase-below");
        let below = ui.data(|m| m.get_temp::<f32>(below_id)).unwrap_or_else(|| {
            let lines = d.set_aside().len() + d.problems().len().min(3) + usize::from(dirty);
            60.0 + 24.0 * lines as f32
        });
        let list_h = (height - (ui.cursor().top() - top) - below).max(ROW_H * 3.0 + 8.0).round();
        let list_w = (width * 0.56).max(width - 300.0).round();
        let (area, _) = ui.allocate_exact_size(vec2(width, list_h), Sense::hover());
        let list_rect = Rect::from_min_size(area.min, vec2(list_w, list_h));
        let right_rect = Rect::from_min_max(Pos2::new(area.left() + list_w + 16.0, area.top()), area.max);
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        list(&mut list_ui, d, list_w, list_h);
        ui.painter().vline(area.left() + list_w + 8.0, area.y_range(), (1.0, p.separator));
        let mut right_ui = ui.new_child(egui::UiBuilder::new().max_rect(right_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        messages(&mut right_ui, d, list_h);
        ui.add_space(6.0);
        // Messages written for groups the reordering broke up: kept (they
        // apply again if the group comes back), to copy or discard.
        let mut discard = None;
        for (anchor, message) in d.set_aside() {
            ui.horizontal(|ui| {
                let text = RichText::new(format!(
                    "The combined message you wrote for {} is set aside: no group melds into it now.",
                    anchor.short()
                ))
                .small()
                .color(p.status_modified);
                // One line (the whole of it on hover), the buttons beside it.
                let w = (ui.available_width() - 130.0).max(60.0);
                ui.allocate_ui(vec2(w, ui.spacing().interact_size.y), |ui| ui.add(egui::Label::new(text).truncate()));
                if ui.small_button("Copy").on_hover_text(message.clone()).clicked() {
                    ui.ctx().copy_text(message.clone());
                }
                if ui.small_button("Discard").clicked() {
                    discard = Some(anchor);
                }
            });
        }
        if let Some(a) = discard {
            d.discard_set_aside(a);
        }
        let problems = d.problems();
        for problem in problems.iter().take(3) {
            ui.add(egui::Label::new(RichText::new(problem).small().color(p.status_deleted)).truncate());
        }
        if dirty {
            ui.checkbox(&mut d.stash_first, "Stash my local changes first and re-apply them afterwards");
        }
        ui.label(
            RichText::new("Lines starting with # in messages are left out, as in git.").small().color(p.text_weak),
        );
        ui.horizontal(|ui| {
            if ui.button("Reset").on_hover_text("Every commit picked, in the original order").clicked() {
                d.reset();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let why = if !problems.is_empty() {
                    Some("Fix the problems listed first")
                } else if dirty && !d.stash_first {
                    Some("Commit or stash your local changes first")
                } else if busy {
                    Some("Waiting for the current operation")
                } else {
                    None
                };
                let start = ui
                    .add_enabled(why.is_none(), egui::Button::new(RichText::new("Start Rebase").strong()))
                    .on_disabled_hover_text(why.unwrap_or_default());
                if start.clicked() {
                    outcome = Outcome::Start;
                }
                if ui.button("Cancel").clicked() {
                    outcome = Outcome::Close;
                }
            });
        });
        let measured = ui.min_rect().bottom() - area.bottom();
        if (measured - below).abs() > 0.5 {
            ui.data_mut(|m| m.insert_temp(below_id, measured));
            ui.ctx().request_repaint();
        }
    });
    // Esc closes an untouched plan; a changed one closes with Cancel only.
    if matches!(outcome, Outcome::Keep) && modal.should_close() && unchanged {
        outcome = Outcome::Close;
    }
    outcome
}

/// ↑↓ select, Alt+↑/↓ move, P/R/E/S/F/D set the action — while the list
/// has the keyboard focus (not a button or a message field) and no IME is
/// composing.
fn keys(ctx: &egui::Context, d: &mut RebaseDialog) {
    let focused = ctx.memory(|m| m.has_focus(list_focus()));
    if input::layer(ctx) != input::Layer::Dialog || !focused || input::keys_blocked(ctx) {
        return;
    }
    let n = d.plan.items.len();
    let Some(sel) = d.selected.filter(|&s| s < n) else {
        if n > 0 && (input::take_key(ctx, Modifiers::NONE, Key::ArrowDown) || input::take_key(ctx, Modifiers::NONE, Key::ArrowUp)) {
            d.selected = Some(n - 1);
        }
        return;
    };
    // The list shows the newest (last item) on top.
    if input::take_key(ctx, Modifiers::ALT, Key::ArrowUp) && sel + 1 < n {
        d.move_item(sel, sel + 1);
    } else if input::take_key(ctx, Modifiers::ALT, Key::ArrowDown) && sel > 0 {
        d.move_item(sel, sel - 1);
    } else if input::take_key(ctx, Modifiers::NONE, Key::ArrowUp) && sel + 1 < n {
        d.selected = Some(sel + 1);
    } else if input::take_key(ctx, Modifiers::NONE, Key::ArrowDown) && sel > 0 {
        d.selected = Some(sel - 1);
    }
    for (action, letter, _) in ACTIONS {
        let key = Key::from_name(letter).expect("a letter key");
        if input::take_key(ctx, Modifiers::NONE, key) {
            d.set_action(sel, action);
        }
    }
}

fn action_color(a: Action, p: &Palette) -> egui::Color32 {
    match a {
        Action::Pick => p.text,
        Action::Reword => p.accent,
        Action::Edit => p.status_modified,
        Action::Squash | Action::Fixup => p.status_renamed,
        Action::Drop => p.status_deleted,
    }
}

fn list(ui: &mut egui::Ui, d: &mut RebaseDialog, width: f32, height: f32) {
    let p = Palette::of(ui.ctx());
    let n = d.plan.items.len();
    let drag_id = egui::Id::new("interactive-rebase-drag");
    // Stored as Option<usize>: read it as that type.
    let mut dragging: Option<usize> = ui.data(|m| m.get_temp::<Option<usize>>(drag_id)).flatten();
    // The latest position (it stays known on the frame the button is released).
    let (pointer, down) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
    let mut rows: Vec<(usize, Rect)> = Vec::new();
    let mut actions: Vec<(usize, Action)> = Vec::new();
    let mut select = None;
    // The list's keyboard focus: taken when the dialog opens.
    let area = ui.max_rect();
    let focus = ui.interact(area, list_focus(), Sense::focusable_noninteractive());
    if ui.memory(|m| m.focused().is_none()) {
        focus.request_focus();
    }
    // The arrow keys are the list's (not egui's moving the focus around).
    let arrows = egui::EventFilter { vertical_arrows: true, horizontal_arrows: true, ..Default::default() };
    ui.memory_mut(|m| m.set_focus_lock_filter(list_focus(), arrows));
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        egui::ScrollArea::vertical().id_salt("interactive-rebase-list").max_height(height).auto_shrink(false).show_rows(
            ui,
            ROW_H,
            n,
            |ui, range| {
                for shown in range {
                    let i = n - 1 - shown;
                    let (id, action) = d.plan.items[i];
                    let Some(c) = d.commit(id).cloned() else { continue };
                    let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
                    // The action letters on the left are buttons of their
                    // own; the rest of the row selects and drags.
                    let buttons_w = 4.0 + 22.0 * ACTIONS.len() as f32;
                    let body = Rect::from_min_max(Pos2::new(rect.left() + buttons_w, rect.top()), rect.max);
                    let resp = ui.interact(body, ui.id().with(("rebase-row", id)), Sense::click_and_drag());
                    let label = format!("rebase row {} {}", id.short(), c.subject);
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                    rows.push((i, rect));
                    if resp.clicked() || resp.drag_started() {
                        select = Some(i);
                    }
                    if resp.dragged() {
                        dragging = Some(i);
                    }
                    let painter = ui.painter();
                    if d.selected == Some(i) {
                        painter.rect_filled(rect, 0.0, p.selection_bg);
                    } else if resp.hovered() {
                        painter.rect_filled(rect, 0.0, p.row_hover);
                    }
                    // Action letters.
                    let mut x = rect.left() + 4.0;
                    for (a, letter, tip) in ACTIONS {
                        let r = Rect::from_min_size(Pos2::new(x, rect.top() + 3.0), vec2(20.0, ROW_H - 6.0));
                        let b = ui.interact(r, ui.id().with(("rebase-action", i, letter)), Sense::click());
                        let name = format!("{} {}", a.word(), id.short());
                        b.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, a == action, &name));
                        let on = a == action;
                        let fill = if on { action_color(a, p).gamma_multiply(0.85) } else if b.hovered() { p.row_hover } else { p.bg };
                        ui.painter().rect(r, 3.0, fill, egui::Stroke::new(1.0, p.separator), egui::StrokeKind::Inside);
                        let color = if on { p.bg } else { p.text_weak };
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, letter, FontId::proportional(11.5), color);
                        if b.on_hover_text(tip).clicked() {
                            actions.push((i, a));
                        }
                        x += 22.0;
                    }
                    let painter = ui.painter();
                    let indent = if matches!(action, Action::Squash | Action::Fixup) { 18.0 } else { 0.0 };
                    let text_x = x + 8.0 + indent;
                    if indent > 0.0 {
                        // Melds into the commit below.
                        let lx = x + 12.0;
                        painter.line_segment([Pos2::new(lx, rect.top() + 4.0), Pos2::new(lx, rect.bottom())], (1.5, p.status_renamed));
                    }
                    let sha = Rect::from_min_max(Pos2::new(text_x, rect.top()), Pos2::new(text_x + 64.0, rect.bottom()));
                    paint_text(painter, sha, &id.short(), FontId::monospace(11.5), p.text_weak, Align2::LEFT_CENTER);
                    // The author only where the subject keeps enough room
                    // (it is also shown on the right for the selected one).
                    let room = rect.right() - 18.0 - (sha.right() + 4.0);
                    let author_w = (room - 180.0).clamp(0.0, 112.0);
                    let author_w = if author_w < 48.0 { 0.0 } else { author_w };
                    let author_r = Rect::from_min_max(Pos2::new(rect.right() - 18.0 - author_w, rect.top()), Pos2::new(rect.right() - 18.0, rect.bottom()));
                    if author_w > 0.0 {
                        paint_text(painter, author_r, &c.author, FontId::proportional(11.5), p.text_weak, Align2::RIGHT_CENTER);
                    }
                    let subject_r = Rect::from_min_max(Pos2::new(sha.right() + 4.0, rect.top()), Pos2::new(author_r.left() - 8.0, rect.bottom()));
                    let color = if action == Action::Drop { p.text_weak } else { p.text };
                    let shown_subject = match d.plan.messages.get(&MessageKey::Reword(id)) {
                        Some(m) if action == Action::Reword => m.lines().next().unwrap_or_default().to_owned(),
                        _ => c.subject.clone(),
                    };
                    paint_text(painter, subject_r, &shown_subject, FontId::proportional(13.0), color, Align2::LEFT_CENTER);
                    if action == Action::Drop {
                        let y = rect.center().y;
                        painter.line_segment([Pos2::new(subject_r.left(), y), Pos2::new(subject_r.right().min(subject_r.left() + 360.0), y)], (1.0, p.text_weak));
                    }
                    if c.published {
                        painter.circle_filled(Pos2::new(rect.right() - 10.0, rect.center().y), 3.0, p.status_modified);
                    }
                }
            },
        );
    });
    if let Some(i) = select {
        d.selected = Some(i);
        focus.request_focus();
    }
    for (i, a) in actions {
        d.selected = Some(i);
        d.set_action(i, a);
        focus.request_focus();
    }
    // Dragging: a line where the commit would go; dropped on release.
    // Only over the list (released elsewhere, the drag is dropped).
    if let (Some(from), Some(pos)) = (dragging, pointer.filter(|p| area.contains(*p))) {
        let target = rows.iter().find(|(_, r)| r.y_range().contains(pos.y)).map(|(i, r)| (*i, *r));
        if let Some((to, r)) = target
            && to != from
        {
            // Moving up the list (newer) puts it above the row, down below.
            let y = if to > from { r.top() } else { r.bottom() };
            ui.painter().line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], (2.0, p.accent));
            if !down {
                d.move_item(from, to);
            }
        }
    }
    if !down {
        dragging = None;
    }
    ui.data_mut(|m| m.insert_temp(drag_id, dragging));
}

fn messages(ui: &mut egui::Ui, d: &mut RebaseDialog, height: f32) {
    let p = Palette::of(ui.ctx());
    let Some(i) = d.selected.filter(|&i| i < d.plan.items.len()) else {
        ui.label(RichText::new("Select a commit").color(p.text_weak));
        return;
    };
    let (id, action) = d.plan.items[i];
    let Some(c) = d.commit(id).cloned() else { return };
    egui::ScrollArea::vertical().id_salt("interactive-rebase-messages").max_height(height).auto_shrink([false, true]).show(ui, |ui| {
        ui.label(RichText::new(format!("{}  {}", id.short(), c.subject)).strong());
        ui.label(RichText::new(format!("{}  {}", c.author, format_time(c.author_time))).small().color(p.text_weak));
        ui.add_space(6.0);
        if action == Action::Reword {
            ui.label("New message");
            let msg = d.reword_message(id);
            let resp = ui.add(egui::TextEdit::multiline(msg).desired_rows(6).desired_width(f32::INFINITY));
            if resp.changed() {
                d.reword_changed();
            }
        } else {
            ui.label(RichText::new("Message").color(p.text_weak));
            ui.add(egui::Label::new(&c.message).wrap().selectable(true));
        }
        if let Some(anchor) = d.group_anchor(i) {
            ui.add_space(10.0);
            let edited = d.group_edited(anchor);
            ui.label(format!("Combined message of the commits melded into {}", anchor.short()));
            let mut text = d.group_text(anchor).unwrap_or_default();
            let resp = ui.add(egui::TextEdit::multiline(&mut text).desired_rows(8).desired_width(f32::INFINITY));
            if resp.changed() {
                d.set_group_message(anchor, text);
            }
            if edited {
                if ui.small_button("Use git's combination").on_hover_text("Combine the messages as git does, when it gets there").clicked() {
                    d.reset_group_message(anchor);
                }
            } else {
                ui.label(
                    RichText::new("Unchanged, git combines the messages when it gets there (squashed ones, not the fixups'), as they are then.")
                        .small()
                        .color(p.text_weak),
                );
            }
        }
    });
}
