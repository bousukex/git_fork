//! Banners for operations in progress, stash conflicts and stashed changes
//! waiting to be re-applied.

use egui::RichText;
use tine_core::model::RepoState;

use crate::{
    repo::{RepoTab, View},
    theme::Palette,
};

fn frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = Palette::of(ui.ctx());
    egui::Frame::NONE
        .fill(if p.dark { egui::Color32::from_rgb(0x3a, 0x33, 0x1f) } else { egui::Color32::from_rgb(0xfd, 0xf3, 0xd8) })
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add);
        });
}

fn conflicts_text(n: usize) -> String {
    match n {
        0 => "all conflicts resolved".into(),
        1 => "1 conflicted file".into(),
        n => format!("{n} conflicted files"),
    }
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let conflicts = tab.status.conflicted.len();
    let busy = tab.busy();
    let state = tab.status.state;

    if !matches!(state, RepoState::Clean | RepoState::Bisecting) {
        let title = match state {
            RepoState::Merging => "Merge in progress".to_owned(),
            RepoState::Rebasing => match tab.rebase_progress {
                Some((n, total)) => format!("Rebase in progress ({n}/{total})"),
                None => "Rebase in progress".into(),
            },
            RepoState::CherryPicking => "Cherry-pick in progress".into(),
            RepoState::Reverting => "Revert in progress".into(),
            _ => unreachable!(),
        };
        let note = match tab.edit_stop() {
            Some(id) => format!("— stopped at {} to edit: amend it or add commits, then Continue", id.short()),
            None if state == RepoState::Rebasing && tab.rebase_stop.is_some() && conflicts == 0 => {
                "— stopped; continue when ready".to_owned()
            }
            None => format!("— {}", conflicts_text(conflicts)),
        };
        frame(ui, |ui| {
            ui.label(RichText::new(title).strong());
            ui.add(egui::Label::new(RichText::new(note).color(p.text_weak)).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new("Abort…")).clicked() {
                    tab.request_abort();
                }
                if state == RepoState::Merging {
                    let resp = ui
                        .add_enabled(conflicts == 0 && !busy, egui::Button::new(RichText::new("Commit Merge…").strong()))
                        .on_hover_text("Finish the merge from the commit box")
                        .on_disabled_hover_text("Resolve the conflicts first");
                    if resp.clicked() {
                        tab.view = View::LocalChanges;
                    }
                } else {
                    if ui.add_enabled(!busy, egui::Button::new("Skip")).on_hover_text("Leave out the current commit").clicked() {
                        tab.skip();
                    }
                    let resp = ui
                        .add_enabled(conflicts == 0 && !busy, egui::Button::new(RichText::new("Continue").strong()))
                        .on_disabled_hover_text("Resolve the conflicts first");
                    if resp.clicked() {
                        tab.continue_op();
                    }
                }
            });
        });
    }

    if state == RepoState::Rebasing && tab.rebase_session {
        frame(ui, |ui| {
            ui.label(
                RichText::new(
                    "Started in Tine: continue, skip or abort here (not with git on the command line), so the messages you wrote are used.",
                )
                .small()
                .color(p.text_weak),
            );
        });
    }

    if let Some(sc) = tab.stash_conflict.clone() {
        frame(ui, |ui| {
            ui.label(RichText::new(format!("Applying stash {} conflicted", sc.oid.short())).strong());
            ui.label(RichText::new(format!("— {}", conflicts_text(conflicts))).color(p.text_weak));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if sc.was_clean_before && ui.add_enabled(!busy, egui::Button::new("Undo Apply…")).clicked() {
                    tab.request_undo_stash_apply();
                }
                let done = conflicts == 0 && !busy;
                if ui
                    .add_enabled(done, egui::Button::new("Done & Drop Stash"))
                    .on_disabled_hover_text("Resolve the conflicts first")
                    .clicked()
                {
                    tab.finish_stash_conflict(true);
                }
                if ui
                    .add_enabled(done, egui::Button::new(RichText::new("Done").strong()))
                    .on_hover_text("Keep the stash")
                    .on_disabled_hover_text("Resolve the conflicts first")
                    .clicked()
                {
                    tab.finish_stash_conflict(false);
                }
            });
        });
    } else if state == RepoState::Clean && tab.generic_conflict.is_some() {
        // Conflicts of unknown origin (e.g. a stash applied elsewhere).
        frame(ui, |ui| {
            if conflicts > 0 {
                ui.label(RichText::new("Conflicted files").strong());
                ui.label(
                    RichText::new(format!("— {}; resolve them in Local Changes", conflicts_text(conflicts)))
                        .color(p.text_weak),
                );
            } else {
                ui.label(RichText::new("Conflicts resolved").strong());
                ui.label(RichText::new("— resolving staged the files").color(p.text_weak));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new("Done").strong())).clicked() {
                        tab.finish_generic_conflict(false);
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Unstage Resolved Files"))
                        .on_hover_text("Leave the resolved changes unstaged, like before they conflicted")
                        .clicked()
                    {
                        tab.finish_generic_conflict(true);
                    }
                });
            }
        });
    }

    if let Some(done) = tab.rebase_done.clone() {
        frame(ui, |ui| {
            let on = done.branch.as_deref().map(tine_core::interactive::branch_name).unwrap_or("HEAD").to_owned();
            ui.label(format!("Rebased '{on}' interactively; it was at {} before.", done.previous.short()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Dismiss").clicked() {
                    tab.rebase_done = None;
                }
                if ui
                    .add_enabled(!busy && tab.can_undo_rebase(), egui::Button::new("Undo Rebase…"))
                    .on_hover_text("Reset to where it was before (you see what a hard reset would discard first)")
                    .on_disabled_hover_text("Only while the branch is still where the rebase left it")
                    .clicked()
                {
                    tab.undo_rebase();
                }
            });
        });
    }

    if let Some(oid) = tab.pending_autostash {
        frame(ui, |ui| {
            ui.label(format!("Your local changes from before the operation are saved in stash {}.", oid.short()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Dismiss").on_hover_text("Keep the stash; apply it later from the sidebar").clicked() {
                    tab.pending_autostash = None;
                }
                let ready = state == RepoState::Clean && conflicts == 0 && !busy;
                if ui
                    .add_enabled(ready, egui::Button::new("Apply Stash"))
                    .on_disabled_hover_text("Finish the operation in progress first")
                    .clicked()
                {
                    tab.apply_pending_autostash();
                }
            });
        });
    }
}
