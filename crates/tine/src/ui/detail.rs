//! Bottom pane of the history view: commit metadata, changed files, diff.

use egui::{Align2, FontId, Pos2, Rect, RichText};
use tine_core::model::{CommitDetails, Oid};

use super::{format_time, paint_text, split_path, status_badge};
use crate::{repo::RepoTab, theme::Palette};

const FILE_ROW_H: f32 = 22.0;

enum Action {
    Reveal(Oid),
    SelectFile(usize),
    FileHistory(usize),
    Blame(usize),
}

fn info(ui: &mut egui::Ui, d: &CommitDetails, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let (subject, body) = d.message.split_once('\n').unwrap_or((&d.message, ""));
    ui.add(egui::Label::new(RichText::new(subject).heading().size(15.0)).wrap().selectable(true));
    ui.add_space(4.0);
    egui::Grid::new("commit-info").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
        let key = |ui: &mut egui::Ui, k: &str| ui.label(RichText::new(k).color(p.text_weak));
        key(ui, "Author");
        ui.add(
            egui::Label::new(format!("{} <{}>  {}", d.author.name, d.author.email, format_time(d.author.time)))
                .selectable(true),
        );
        ui.end_row();
        if d.committer.name != d.author.name || d.committer.email != d.author.email || d.committer.time != d.author.time {
            key(ui, "Committer");
            ui.add(
                egui::Label::new(format!(
                    "{} <{}>  {}",
                    d.committer.name,
                    d.committer.email,
                    format_time(d.committer.time)
                ))
                .selectable(true),
            );
            ui.end_row();
        }
        key(ui, "SHA");
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(d.id.to_hex()).monospace()).selectable(true));
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(d.id.to_hex());
            }
        });
        ui.end_row();
        if !d.parents.is_empty() {
            key(ui, if d.parents.len() > 1 { "Parents" } else { "Parent" });
            ui.horizontal(|ui| {
                for parent in &d.parents {
                    if ui.link(RichText::new(parent.short()).monospace()).clicked() {
                        actions.push(Action::Reveal(*parent));
                    }
                }
            });
            ui.end_row();
        }
    });
    let body = body.trim();
    if !body.is_empty() {
        ui.add_space(6.0);
        ui.add(egui::Label::new(body).wrap().selectable(true));
    }
}

fn files(ui: &mut egui::Ui, tab: &RepoTab, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    super::caption(ui, &format!("{} changed files", tab.commit.files.len()));
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().id_salt("commit-files").auto_shrink(false).show_rows(
            ui,
            FILE_ROW_H,
            tab.commit.files.len(),
            |ui, range| {
                for i in range {
                    let f = &tab.commit.files[i];
                    let (rect, resp) = super::list_row(ui, FILE_ROW_H, tab.commit.selected_file == Some(i), true);
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("changed file {}", f.path)));
                    if resp.clicked() {
                        actions.push(Action::SelectFile(i));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("File History").clicked() {
                            actions.push(Action::FileHistory(i));
                            ui.close();
                        }
                        let label = if f.kind == tine_core::model::ChangeKind::Deleted { "Blame Before Deletion" } else { "Blame at This Commit" };
                        if ui.button(label).clicked() {
                            actions.push(Action::Blame(i));
                            ui.close();
                        }
                        if ui.button("Copy Path").clicked() {
                            ui.ctx().copy_text(f.path.to_string());
                            ui.close();
                        }
                    });
                    status_badge(ui.painter(), Pos2::new(rect.left() + 14.0, rect.center().y), f.kind, p);
                    let (name, dir) = split_path(&f.path);
                    let text_rect = Rect::from_min_max(Pos2::new(rect.left() + 28.0, rect.top()), rect.right_bottom());
                    let name_galley = super::truncated(ui.painter(), &name, FontId::proportional(13.0), p.text, text_rect.width());
                    let name_w = name_galley.size().x;
                    ui.painter().galley(
                        Pos2::new(text_rect.left(), rect.center().y - name_galley.size().y / 2.0),
                        name_galley,
                        p.text,
                    );
                    if !dir.is_empty() {
                        let dir_rect = Rect::from_min_max(Pos2::new(text_rect.left() + name_w + 8.0, rect.top()), Pos2::new(rect.right() - 6.0, rect.bottom()));
                        paint_text(ui.painter(), dir_rect, &dir, FontId::proportional(11.5), p.text_weak, Align2::LEFT_CENTER);
                    }
                }
            },
        );
    });
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    match &tab.commit.details {
        None if tab.commit.id.is_none() => {
            ui.centered_and_justified(|ui| ui.label(RichText::new("Select a commit").color(p.text_weak)));
            return;
        }
        None => {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(p.status_deleted));
            return;
        }
        Some(Ok(_)) => {}
    }

    egui::Panel::left("commit-detail-left")
        .resizable(true)
        .default_size(380.0)
        .size_range(220.0..=900.0)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(8)).fill(p.bg))
        .show(ui, |ui| {
            if let Some(Ok(d)) = &tab.commit.details {
                egui::ScrollArea::vertical()
                    .id_salt("commit-info")
                    .max_height(ui.available_height() * 0.45)
                    .auto_shrink([false, true])
                    .show(ui, |ui| info(ui, d, &mut actions));
            }
            ui.separator();
            files(ui, tab, &mut actions);
        });
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
        for e in super::diff_view::show(ui, ("commit-diff", &tab.paths.workdir), &tab.commit_diff, tab.diff_opts, None) {
            if let super::diff_view::DiffEvent::Options(opts) = e {
                tab.set_diff_options(opts);
            }
        }
    });

    let parent = match &tab.commit.details {
        Some(Ok(d)) => d.parents.first().copied(),
        _ => None,
    };
    for a in actions {
        match a {
            Action::Reveal(id) => tab.reveal_commit(id),
            Action::SelectFile(i) => tab.select_commit_file(i),
            Action::FileHistory(i) | Action::Blame(i) => {
                let (Some(id), Some(file)) = (tab.commit.id, tab.commit.files.get(i).cloned()) else { continue };
                if matches!(a, Action::FileHistory(_)) {
                    tab.file_history_of_commit_file(id, parent, &file);
                } else {
                    tab.blame_of_commit_file(id, parent, &file);
                }
            }
        }
    }
}
