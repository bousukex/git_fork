//! The merge editor: a conflicted file as it will be saved — what the sides
//! agree on, and each conflict with its sides and the ways to resolve it.

use egui::{Align2, Color32, FontId, Key, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, vec2};
use pengitto_core::merge_file::Choice;

use super::{code_galley, paint_text};
use crate::{
    input,
    merge_model::{MergeSession, Part, Row},
    repo::{
        RepoTab,
        merge::{MergeEditor, MergeState},
    },
    theme::Palette,
};

/// What the user did, applied after drawing.
enum Act {
    Close,
    Copy,
    Choose(usize, Choice),
    ToggleBase(usize),
    Next(bool),
    Edit,
    Save(bool),
    MergeTool,
}

const CHOICES: [(Choice, &str, &str); 5] = [
    (Choice::Ours, "Use Ours", "Keep the ours side's lines"),
    (Choice::Theirs, "Use Theirs", "Keep the theirs side's lines"),
    (Choice::OursThenTheirs, "Both: Ours First", "Keep both sides' lines, ours first"),
    (Choice::TheirsThenOurs, "Both: Theirs First", "Keep both sides' lines, theirs first"),
    (Choice::Base, "Use Base", "Keep the lines as they were before either side changed them"),
];

fn choice_name(c: Choice) -> &'static str {
    match c {
        Choice::Unresolved => "unresolved",
        Choice::Ours => "ours",
        Choice::Theirs => "theirs",
        Choice::OursThenTheirs => "both, ours first",
        Choice::TheirsThenOurs => "both, theirs first",
        Choice::Base => "base",
    }
}

fn part_color(part: Part, p: &Palette) -> Color32 {
    match part {
        Part::Ours => p.diff_del_strong,
        Part::Base => p.text_weak,
        Part::Theirs => p.diff_add_strong,
    }
}

fn part_bg(part: Part, p: &Palette) -> Color32 {
    match part {
        Part::Ours => p.diff_del_bg,
        Part::Base => p.sidebar_bg,
        Part::Theirs => p.diff_add_bg,
    }
}

/// F7 / Shift+F7 move between unresolved conflicts (Esc is taken with the
/// other overlays').
fn keys(ctx: &egui::Context, acts: &mut Vec<Act>) {
    if !input::views_have_keys(ctx) || ctx.text_edit_focused() {
        return;
    }
    if input::take_key(ctx, Modifiers::NONE, Key::F7) {
        acts.push(Act::Next(false));
    } else if input::take_key(ctx, Modifiers::SHIFT, Key::F7) {
        acts.push(Act::Next(true));
    }
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let mut acts = Vec::new();
    keys(ui.ctx(), &mut acts);
    let save_block = tab.merge_save_block(false);
    let resolve_block = tab.merge_save_block(true);
    let Some(m) = &mut tab.merge else { return };
    header(ui, m, save_block.as_deref(), resolve_block.as_deref(), &mut acts);
    match &m.state {
        MergeState::Loading => {
            ui.centered_and_justified(|ui| ui.spinner());
        }
        MergeState::Failed { message, unsupported } => {
            let p = Palette::of(ui.ctx());
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                let title = if *unsupported { "This file can't be merged here" } else { "The file couldn't be opened" };
                ui.label(RichText::new(title).strong());
                ui.label(RichText::new(message).color(p.text_weak));
                ui.add_space(8.0);
                if ui.button("Open in Merge Tool").clicked() {
                    acts.push(Act::MergeTool);
                }
            });
        }
        MergeState::Ready(_) => {
            let scroll_to = m.scroll_to.take();
            if let MergeState::Ready(s) = &m.state {
                rows(ui, s, scroll_to, &mut acts);
            }
        }
    }
    for act in acts {
        match act {
            Act::Close => tab.request_close_merge(),
            Act::Copy => {
                if let Some(s) = tab.merge.as_ref().and_then(MergeEditor::session) {
                    ui.ctx().copy_text(String::from_utf8_lossy(&s.result()).into_owned());
                }
            }
            Act::Choose(k, c) => {
                if let Some(s) = tab.merge.as_mut().and_then(MergeEditor::session_mut) {
                    s.choose(k, c);
                    s.current = Some(k);
                }
            }
            Act::ToggleBase(k) => {
                if let Some(s) = tab.merge.as_mut().and_then(MergeEditor::session_mut) {
                    s.toggle_base(k);
                }
            }
            Act::Next(back) => tab.merge_next(back),
            Act::Edit => tab.edit_merge_result(),
            Act::Save(resolved) => tab.save_merge(resolved),
            Act::MergeTool => {
                if let Some(path) = tab.merge.as_ref().map(|m| m.path.clone()) {
                    tab.open_mergetool(path);
                }
            }
        }
    }
}

fn header(ui: &mut egui::Ui, m: &MergeEditor, save_block: Option<&str>, resolve_block: Option<&str>, acts: &mut Vec<Act>) {
    let p = Palette::of(ui.ctx());
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("Merge Editor").strong());
            ui.add(egui::Label::new(RichText::new(m.path.to_string()).color(p.text_weak)).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").on_hover_text("Close the merge editor (Esc)").clicked() {
                    acts.push(Act::Close);
                }
                if m.session().is_some()
                    && ui.button("Copy Result").on_hover_text("Copy the result as it would be saved").clicked()
                {
                    acts.push(Act::Copy);
                }
            });
        });
        let Some(s) = m.session() else { return };
        let side = |ui: &mut egui::Ui, part: Part, text: &str| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("■").color(part_color(part, p)));
                let name = if part == Part::Ours { "Ours" } else { "Theirs" };
                ui.label(RichText::new(format!("{name}:")).small().strong());
                ui.add(egui::Label::new(RichText::new(text).small()).truncate());
            });
        };
        side(ui, Part::Ours, &s.opened.sides.ours);
        side(ui, Part::Theirs, &s.opened.sides.theirs);
        ui.horizontal(|ui| {
            let (n, open) = (s.conflict_count(), s.unresolved());
            let status = if s.stray_markers > 0 {
                format!("{} of {n} resolved; conflict markers left in the edited result", n - (open - s.stray_markers))
            } else {
                format!("{} of {n} resolved", n - open)
            };
            ui.label(RichText::new(status).color(if open == 0 { p.text } else { p.status_modified }));
            let none_left = s.choices.iter().all(|c| *c != Choice::Unresolved);
            if ui.add_enabled(!none_left, egui::Button::new("◀ Previous").small()).on_hover_text("Previous conflict (Shift+F7)").clicked() {
                acts.push(Act::Next(true));
            }
            if ui.add_enabled(!none_left, egui::Button::new("Next ▶").small()).on_hover_text("Next conflict (F7)").clicked() {
                acts.push(Act::Next(false));
            }
            if ui
                .add_enabled(!m.saving, egui::Button::new("Edit Result…"))
                .on_hover_text("Edit the whole result as text")
                .on_disabled_hover_text("Saving…")
                .clicked()
            {
                acts.push(Act::Edit);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resolve = ui
                    .add_enabled(resolve_block.is_none(), egui::Button::new(RichText::new("Save & Mark Resolved").strong()))
                    .on_disabled_hover_text(resolve_block.unwrap_or_default());
                if resolve.clicked() {
                    acts.push(Act::Save(true));
                }
                let save = ui
                    .add_enabled(save_block.is_none(), egui::Button::new("Save"))
                    .on_hover_text(
                        "Write the result to the file; it stays conflicted. Checked right before writing: nothing \
                         else changed the file, its conflict or the operation (a git command run elsewhere at the \
                         same moment can still slip in)",
                    )
                    .on_disabled_hover_text(save_block.unwrap_or_default());
                if save.clicked() {
                    acts.push(Act::Save(false));
                }
                if s.dirty() {
                    ui.label(RichText::new("Not saved").small().color(p.status_modified));
                }
            });
        });
        if let Some(note) = &m.note {
            ui.add(egui::Label::new(RichText::new(note).small().color(p.status_modified)).wrap());
        }
    });
}

/// A button drawn into a row (rows are virtualized: no layout inside).
fn row_button(ui: &mut egui::Ui, rect: Rect, id: egui::Id, label: &str, tip: &str, font: &FontId) -> bool {
    let p = Palette::of(ui.ctx());
    let resp = ui.interact(rect, id, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let fill = if resp.is_pointer_button_down_on() {
        p.selection_bg
    } else if resp.hovered() {
        p.row_hover
    } else {
        p.bg
    };
    ui.painter().rect(rect, 3.0, fill, egui::Stroke::new(1.0, p.separator), egui::StrokeKind::Inside);
    let text = label.split(" (conflict").next().unwrap_or(label);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font.clone(), p.text);
    resp.on_hover_text(tip).clicked()
}

fn rows(ui: &mut egui::Ui, s: &MergeSession, scroll_to: Option<usize>, acts: &mut Vec<Act>) {
    let p = Palette::of(ui.ctx());
    let size = crate::settings::current(ui.ctx()).diff_font_size;
    let font = FontId::monospace(size);
    let small = FontId::proportional((size * 0.9).min(13.0));
    let char_w = ui.painter().layout_no_wrap("0".into(), font.clone(), p.text).size().x;
    let row_h = (size * 1.6).round();
    let gutter = 10.0;
    let content_w = gutter + (s.max_chars() as f32 + 4.0) * char_w;
    let n = s.conflict_count();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let mut area = ScrollArea::both().id_salt("merge-editor").auto_shrink(false);
        if let Some(row) = scroll_to {
            // A little of what comes before stays in view.
            area = area.vertical_scroll_offset((row.saturating_sub(2)) as f32 * row_h);
        }
        area.show_rows(ui, row_h, s.rows.len(), |ui, range| {
            let width = content_w.max(ui.available_width());
            for i in range {
                let row = s.rows[i];
                let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                // The visible part (buttons stay there when scrolled sideways).
                let seen = rect.intersect(ui.clip_rect());
                let text_x = rect.left() + gutter;
                match row {
                    Row::Common { .. } => {
                        if let Some((text, spans)) = s.line(row) {
                            let g = code_galley(ui.painter(), text, spans, font.clone(), p.text, p);
                            ui.painter().galley(Pos2::new(text_x, rect.center().y - g.size().y / 2.0), g, p.text);
                        }
                    }
                    Row::Head { k } => {
                        let current = s.current == Some(k);
                        ui.painter().rect_filled(rect, 0.0, p.diff_hunk_bg);
                        if current {
                            ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(3.0, row_h)), 0.0, p.accent);
                        }
                        let choice = s.choices[k];
                        let title = if choice == Choice::Unresolved {
                            format!("Conflict {} of {n}", k + 1)
                        } else {
                            format!("Conflict {} of {n} — resolved: {}", k + 1, choice_name(choice))
                        };
                        paint_text(
                            ui.painter(),
                            Rect::from_min_max(Pos2::new(seen.left() + gutter, rect.top()), rect.max),
                            &title,
                            small.clone(),
                            p.text,
                            Align2::LEFT_CENTER,
                        );
                        // Buttons from the right edge of what is seen.
                        let mut right = seen.right() - 6.0;
                        let mut button = |ui: &mut egui::Ui, label: String, tip: &str| {
                            let w = ui.painter().layout_no_wrap(label.split(" (conflict").next().unwrap_or("").to_owned(), small.clone(), p.text).size().x + 14.0;
                            let r = Rect::from_min_max(Pos2::new(right - w, rect.top() + 2.0), Pos2::new(right, rect.bottom() - 2.0));
                            right -= w + 4.0;
                            row_button(ui, r, ui.id().with(("merge-button", k, &label)), &label, tip, &small)
                        };
                        if choice == Choice::Unresolved {
                            for (c, verb, tip) in CHOICES.iter().rev() {
                                if button(ui, format!("{verb} (conflict {})", k + 1), tip) {
                                    acts.push(Act::Choose(k, *c));
                                }
                            }
                        } else if button(ui, format!("Change (conflict {})", k + 1), "Unresolve it: show both sides again") {
                            acts.push(Act::Choose(k, Choice::Unresolved));
                        }
                    }
                    Row::Label { k, part } => {
                        let (name, detail) = match part {
                            Part::Ours => ("Ours", s.opened.sides.ours.as_str()),
                            Part::Base => ("Base", s.opened.sides.base.as_str()),
                            Part::Theirs => ("Theirs", s.opened.sides.theirs.as_str()),
                        };
                        let r = Rect::from_min_max(Pos2::new(seen.left() + gutter, rect.top()), Pos2::new(seen.right(), rect.bottom()));
                        if part == Part::Base {
                            let shown = s.base_shown[k];
                            let label = format!("{} {name} — {detail} (conflict {})", if shown { "▼" } else { "▶" }, k + 1);
                            let resp = ui.interact(r, ui.id().with(("merge-base", k)), Sense::click());
                            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                            if resp.hovered() {
                                ui.painter().rect_filled(rect, 0.0, p.row_hover);
                            }
                            let text = label.split(" (conflict").next().unwrap_or_default();
                            paint_text(ui.painter(), r, text, small.clone(), p.text_weak, Align2::LEFT_CENTER);
                            if resp.on_hover_text(if shown { "Hide the base" } else { "Show the base" }).clicked() {
                                acts.push(Act::ToggleBase(k));
                            }
                        } else {
                            // The side's color on the bar; the text stays readable.
                            ui.painter().rect_filled(rect, 0.0, part_bg(part, p).gamma_multiply(0.6));
                            ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(4.0, row_h)), 0.0, part_color(part, p));
                            paint_text(ui.painter(), r, &format!("{name} — {detail}"), small.clone(), p.text_weak, Align2::LEFT_CENTER);
                        }
                    }
                    Row::Line { k, part, .. } => {
                        let resolved = s.choices[k] != Choice::Unresolved;
                        let bg = part_bg(part, p);
                        ui.painter().rect_filled(rect, 0.0, if resolved { bg.gamma_multiply(0.5) } else { bg });
                        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(4.0, row_h)), 0.0, part_color(part, p));
                        if let Some((text, spans)) = s.line(row) {
                            let g = code_galley(ui.painter(), text, spans, font.clone(), p.text, p);
                            ui.painter().galley(Pos2::new(text_x, rect.center().y - g.size().y / 2.0), g, p.text);
                        }
                    }
                    Row::Nothing { .. } => {
                        paint_text(
                            ui.painter(),
                            Rect::from_min_max(Pos2::new(text_x, rect.top()), rect.max),
                            "(no lines)",
                            small.clone(),
                            p.text_weak,
                            Align2::LEFT_CENTER,
                        );
                    }
                }
            }
        });
    });
}

/// What the result editor wants after this frame.
pub enum EditOutcome {
    Keep,
    Cancel,
    Apply,
}

/// Editing the whole result as text (conflicts left in it as markers stay
/// conflicts).
pub fn edit_result(ctx: &egui::Context, text: &mut String, original: &str) -> EditOutcome {
    let p = Palette::of(ctx);
    let screen = ctx.content_rect();
    let width = (screen.width() - 80.0).clamp(420.0, 1100.0);
    let height = (screen.height() - 160.0).clamp(220.0, 760.0);
    let mut outcome = EditOutcome::Keep;
    let modal = egui::Modal::new(egui::Id::new("merge-edit-result")).show(ctx, |ui| {
        ui.set_width(width);
        ui.heading("Edit Result");
        ui.label(
            RichText::new("Conflicts left as markers stay conflicts. Lines end as in the file when saved.").small().color(p.text_weak),
        );
        ui.add_space(4.0);
        let (area, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
        ScrollArea::both().id_salt("merge-edit-scroll").auto_shrink(false).show(&mut child, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .id(egui::Id::new("merge-edit-text"))
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows((height / 16.0) as usize),
            );
        });
        ui.add_space(8.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(RichText::new("Apply").strong()).clicked() {
                outcome = EditOutcome::Apply;
            }
            if ui.button("Cancel").clicked() {
                outcome = EditOutcome::Cancel;
            }
        });
    });
    // Esc closes it only while nothing was typed.
    if matches!(outcome, EditOutcome::Keep) && modal.should_close() && text == original {
        outcome = EditOutcome::Cancel;
    }
    outcome
}
