//! Local changes view: unstaged / staged lists, commit box and diff.

use egui::{Align2, FontId, Pos2, Rect, RichText};
use tine_core::model::{ChangedFile, RepoState};

use super::{paint_text, split_path, status_badge};
use crate::{
    repo::{RepoTab, Side},
    theme::Palette,
};

const ROW_H: f32 = 22.0;

fn file_list(ui: &mut egui::Ui, id: &str, files: &[&ChangedFile], side: Side, tab: &RepoTab) -> Option<ChangedFile> {
    let p = Palette::of(ui.ctx());
    let mut picked = None;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().id_salt(id).auto_shrink(false).show_rows(ui, ROW_H, files.len(), |ui, range| {
            for f in &files[range] {
                let selected = tab.changes_selected.as_ref().is_some_and(|(s, sel)| *s == side && sel.path == f.path);
                let (rect, resp) = super::list_row(ui, ROW_H, selected, true);
                if resp.clicked() {
                    picked = Some((*f).clone());
                }
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
    picked
}

fn list_header(ui: &mut egui::Ui, title: &str, count: usize) {
    let p = Palette::of(ui.ctx());
    egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(format!("{title}  {count}")).small().strong().color(p.text_weak));
    });
}

fn commit_box(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    ui.add_space(6.0);
    ui.add(
        egui::TextEdit::singleline(&mut tab.commit_subject)
            .hint_text("Commit subject")
            .desired_width(f32::INFINITY),
    );
    ui.add(
        egui::TextEdit::multiline(&mut tab.commit_body)
            .hint_text("Description")
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        ui.add_enabled(false, egui::Checkbox::new(&mut false, "Amend"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = tab.status.staged.len();
            ui.add_enabled(false, egui::Button::new(format!("Commit {n} file{}", if n == 1 { "" } else { "s" })))
                .on_disabled_hover_text("Committing arrives in the next milestone (M2a)");
        });
    });
    if tab.status.state != RepoState::Clean {
        ui.label(RichText::new(format!("{:?} in progress", tab.status.state)).color(p.status_conflict));
    }
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let mut picked: Option<(Side, ChangedFile)> = None;

    egui::Panel::left("changes-left")
        .resizable(true)
        .default_size(380.0)
        .size_range(240.0..=900.0)
        .frame(egui::Frame::NONE.fill(p.bg))
        .show(ui, |ui| {
            egui::Panel::bottom("commit-box")
                .resizable(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(8)).fill(p.sidebar_bg))
                .show(ui, |ui| commit_box(ui, tab));

            let unstaged: Vec<&ChangedFile> = tab.status.conflicted.iter().chain(&tab.status.unstaged).collect();
            let staged: Vec<&ChangedFile> = tab.status.staged.iter().collect();
            let half = (ui.available_height() / 2.0).max(60.0);
            egui::Panel::top("unstaged")
                .resizable(true)
                .default_size(half)
                .frame(egui::Frame::NONE.fill(p.bg))
                .show(ui, |ui| {
                    list_header(ui, "Unstaged", unstaged.len());
                    if let Some(f) = file_list(ui, "unstaged-list", &unstaged, Side::Unstaged, tab) {
                        picked = Some((Side::Unstaged, f));
                    }
                });
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
                list_header(ui, "Staged", staged.len());
                if let Some(f) = file_list(ui, "staged-list", &staged, Side::Staged, tab) {
                    picked = Some((Side::Staged, f));
                }
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

    if let Some((side, f)) = picked {
        tab.select_change(side, f);
    }
}
