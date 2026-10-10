//! Local changes view: unstaged / staged lists, commit box and diff.

use bstr::ByteSlice;
use egui::{Align2, FontId, Key, Modifiers, Pos2, Rect, RichText};
use tine_core::model::{ChangeKind, ChangedFile, RepoState};

use super::{paint_text, split_path, status_badge};
use crate::{
    repo::{RepoTab, Side},
    theme::Palette,
};

const ROW_H: f32 = 22.0;
/// Subject length git tooling commonly recommends staying under.
const SUBJECT_SOFT_LIMIT: usize = 72;

/// Something the user did in a file list, applied after drawing.
enum ListEvent {
    Click { file: ChangedFile, ctrl: bool, shift: bool },
    Toggle(ChangedFile),
    Stage(Vec<ChangedFile>),
    Unstage(Vec<ChangedFile>),
    Discard(Vec<ChangedFile>),
}

fn file_list(ui: &mut egui::Ui, id: &str, files: &[ChangedFile], side: Side, tab: &RepoTab, events: &mut Vec<ListEvent>) {
    let p = Palette::of(ui.ctx());
    let busy = tab.busy();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().id_salt(id).auto_shrink(false).show_rows(ui, ROW_H, files.len(), |ui, range| {
            for f in &files[range] {
                let on_side = tab.changes_selected.as_ref().is_some_and(|(s, _)| *s == side);
                let selected = on_side && tab.changes_multi.contains(&f.path);
                let (rect, resp) = super::list_row(ui, ROW_H, selected, true);
                if resp.double_clicked() {
                    events.push(ListEvent::Toggle(f.clone()));
                } else if resp.clicked() {
                    let m = ui.input(|i| i.modifiers);
                    events.push(ListEvent::Click { file: f.clone(), ctrl: m.command, shift: m.shift });
                }
                resp.context_menu(|ui| {
                    let targets = tab.action_targets(side, f);
                    let n = targets.len();
                    let suffix = if n > 1 { format!(" {n} Files") } else { String::new() };
                    ui.add_enabled_ui(!busy, |ui| match side {
                        Side::Unstaged => {
                            let stageable = targets.iter().any(|f| f.kind != ChangeKind::Conflicted);
                            if ui
                                .add_enabled(stageable, egui::Button::new(format!("Stage{suffix}")))
                                .on_disabled_hover_text("Resolve conflicts before staging")
                                .clicked()
                            {
                                events.push(ListEvent::Stage(targets.clone()));
                                ui.close();
                            }
                            let discardable = targets.iter().any(|f| f.kind != ChangeKind::Conflicted);
                            if ui.add_enabled(discardable, egui::Button::new(format!("Discard Changes{suffix}…"))).clicked() {
                                events.push(ListEvent::Discard(targets.clone()));
                                ui.close();
                            }
                        }
                        Side::Staged => {
                            if ui.button(format!("Unstage{suffix}")).clicked() {
                                events.push(ListEvent::Unstage(targets.clone()));
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Copy Path").clicked() {
                        ui.ctx().copy_text(f.path.to_str_lossy().into_owned());
                        ui.close();
                    }
                });

                status_badge(ui.painter(), Pos2::new(rect.left() + 14.0, rect.center().y), f.kind, p);
                let (name, dir) = split_path(&f.path);
                let text_rect = Rect::from_min_max(Pos2::new(rect.left() + 28.0, rect.top()), Pos2::new(rect.right() - 6.0, rect.bottom()));
                let galley = super::truncated(ui.painter(), &name, FontId::proportional(13.0), p.text, text_rect.width());
                let name_w = galley.size().x;
                ui.painter().galley(Pos2::new(text_rect.left(), rect.center().y - galley.size().y / 2.0), galley, p.text);
                if !dir.is_empty() {
                    let dir_rect = Rect::from_min_max(Pos2::new(text_rect.left() + name_w + 8.0, rect.top()), text_rect.right_bottom());
                    paint_text(ui.painter(), dir_rect, &dir, FontId::proportional(11.5), p.text_weak, Align2::LEFT_CENTER);
                }
            }
        });
    });
}

/// List caption with Stage/Unstage buttons for the selection and for all.
fn list_header(ui: &mut egui::Ui, side: Side, files: &[ChangedFile], tab: &RepoTab, events: &mut Vec<ListEvent>) {
    let p = Palette::of(ui.ctx());
    let (title, verb) = match side {
        Side::Unstaged => ("Unstaged", "Stage"),
        Side::Staged => ("Staged", "Unstage"),
    };
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 3)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{title}  {}", files.len())).small().strong().color(p.text_weak));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Conflicted files are never staged in bulk (that would mark
                // them resolved).
                let all: Vec<ChangedFile> = files.iter().filter(|f| f.kind != ChangeKind::Conflicted).cloned().collect();
                let enabled = !tab.busy() && !all.is_empty();
                if ui.add_enabled(enabled, egui::Button::new(format!("{verb} All")).small()).clicked() {
                    events.push(match side {
                        Side::Unstaged => ListEvent::Stage(all),
                        Side::Staged => ListEvent::Unstage(all),
                    });
                }
                let selection = tab.selected_on(side);
                if ui.add_enabled(enabled && !selection.is_empty(), egui::Button::new(verb).small()).clicked() {
                    events.push(match side {
                        Side::Unstaged => ListEvent::Stage(selection),
                        Side::Staged => ListEvent::Unstage(selection),
                    });
                }
            });
        });
    });
}

fn commit_box(ui: &mut egui::Ui, tab: &mut RepoTab, history: &[String]) {
    let p = Palette::of(ui.ctx());
    ui.add_space(6.0);

    // Ctrl+Enter commits; consume it before the text fields see it.
    let ctrl_enter = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));

    ui.horizontal(|ui| {
        let counter = tab.commit_subject.chars().count();
        let counter_w = 30.0;
        let history_w = 24.0;
        let gaps = ui.spacing().item_spacing.x * 2.0;
        ui.add(
            egui::TextEdit::singleline(&mut tab.commit_subject)
                .hint_text("Commit subject")
                .desired_width(ui.available_width() - counter_w - history_w - gaps),
        );
        let color = if counter > SUBJECT_SOFT_LIMIT { p.status_modified } else { p.text_weak };
        ui.add_sized([counter_w, 20.0], egui::Label::new(RichText::new(counter.to_string()).small().color(color)));
        ui.add_enabled_ui(!history.is_empty(), |ui| {
            // U+25BC is covered by the CJK fonts; the smaller ▾ is not.
            ui.menu_button("▼", |ui| {
                ui.label(RichText::new("Recent messages").small().color(p.text_weak));
                for m in history.iter().rev().take(20) {
                    let first = m.lines().next().unwrap_or_default();
                    if ui.button(first).clicked() {
                        let (s, b) = tine_core::ops::split_message(m);
                        tab.commit_subject = s;
                        tab.commit_body = b;
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Recent commit messages");
        });
    });
    ui.add(
        egui::TextEdit::multiline(&mut tab.commit_body)
            .hint_text("Description")
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        let mut amend = tab.amend;
        if ui
            .add_enabled(!tab.busy(), egui::Checkbox::new(&mut amend, "Amend"))
            .on_hover_text("Replace the last commit (message and staged changes)")
            .changed()
        {
            tab.set_amend(amend);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = tab.status.staged.len();
            let label = if tab.amend {
                "Amend Last Commit".to_owned()
            } else {
                format!("Commit {n} File{}", if n == 1 { "" } else { "s" })
            };
            let can = tab.can_commit();
            let resp = ui.add_enabled(can, egui::Button::new(RichText::new(label).strong())).on_hover_text("Ctrl+Enter");
            let resp = if can {
                resp
            } else {
                let why = if tab.busy() {
                    "Waiting for the current operation"
                } else if !tab.status.conflicted.is_empty() {
                    "Resolve conflicts first"
                } else if tab.commit_subject.trim().is_empty() {
                    "Enter a commit subject"
                } else {
                    "Stage changes to commit"
                };
                resp.on_disabled_hover_text(why)
            };
            if resp.clicked() || (ctrl_enter && can) {
                tab.commit();
            }
            if tab.busy() {
                ui.spinner();
            }
        });
    });
    if tab.status.state != RepoState::Clean {
        ui.label(RichText::new(format!("{:?} in progress", tab.status.state)).color(p.status_conflict));
    }
}

fn discard_dialog(ctx: &egui::Context, tab: &mut RepoTab) {
    let Some(files) = &tab.pending_discard else { return };
    let p = Palette::of(ctx);
    let untracked = files.iter().filter(|f| f.kind == ChangeKind::Untracked).count();
    let mut confirm = false;
    let mut cancel = false;
    let modal = egui::Modal::new(egui::Id::new("discard-dialog")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading(format!("Discard changes in {} file{}?", files.len(), if files.len() == 1 { "" } else { "s" }));
        ui.add_space(6.0);
        ui.label("Changes to tracked files are lost and cannot be undone.");
        if untracked > 0 {
            ui.label(format!("{untracked} untracked file(s) will be moved to the Trash."));
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
            for f in files {
                ui.label(RichText::new(f.path.to_str_lossy()).monospace().small());
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let discard = egui::Button::new(RichText::new("Discard").color(egui::Color32::WHITE)).fill(p.status_deleted);
                if ui.add(discard).clicked() {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    });
    if confirm {
        tab.confirm_discard();
    } else if cancel || modal.should_close() {
        tab.pending_discard = None;
    }
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab, history: &[String]) {
    let p = Palette::of(ui.ctx());
    let mut events = Vec::new();
    let unstaged = tab.list(Side::Unstaged);
    let staged = tab.list(Side::Staged);

    egui::Panel::left("changes-left")
        .resizable(true)
        .default_size(380.0)
        .size_range(240.0..=900.0)
        .frame(egui::Frame::NONE.fill(p.bg))
        .show(ui, |ui| {
            egui::Panel::bottom("commit-box")
                .resizable(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(8)).fill(p.sidebar_bg))
                .show(ui, |ui| commit_box(ui, tab, history));

            let half = (ui.available_height() / 2.0).max(60.0);
            egui::Panel::top("unstaged")
                .resizable(true)
                .default_size(half)
                .frame(egui::Frame::NONE.fill(p.bg))
                .show(ui, |ui| {
                    list_header(ui, Side::Unstaged, &unstaged, tab, &mut events);
                    file_list(ui, "unstaged-list", &unstaged, Side::Unstaged, tab, &mut events);
                });
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
                list_header(ui, Side::Staged, &staged, tab, &mut events);
                file_list(ui, "staged-list", &staged, Side::Staged, tab, &mut events);
            });
        });

    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
        if !tab.status_loaded {
            ui.centered_and_justified(|ui| ui.spinner());
        } else if tab.status.change_count() == 0 {
            ui.centered_and_justified(|ui| ui.label(RichText::new("No local changes").color(p.text_weak)));
        } else if let Some(opts) = super::diff_view::show(ui, &tab.changes_diff, tab.diff_opts) {
            tab.set_diff_options(opts);
        }
    });

    discard_dialog(ui.ctx(), tab);

    for e in events {
        match e {
            ListEvent::Click { file, ctrl, shift } => {
                let side = if staged.iter().any(|f| f == &file) { Side::Staged } else { Side::Unstaged };
                tab.click_change(side, &file, ctrl, shift);
            }
            ListEvent::Toggle(file) => {
                if staged.iter().any(|f| f == &file) {
                    tab.unstage(tab.action_targets(Side::Staged, &file));
                } else {
                    tab.stage(tab.action_targets(Side::Unstaged, &file));
                }
            }
            ListEvent::Stage(files) => tab.stage(files),
            ListEvent::Unstage(files) => tab.unstage(files),
            ListEvent::Discard(files) => tab.request_discard(files),
        }
    }
}
