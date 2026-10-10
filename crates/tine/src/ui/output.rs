//! Output panel: every write operation with the git commands it ran.

use egui::{Color32, RichText};

use crate::{
    ops_registry::{self, OpSnapshot, OpState, Outcome},
    repo::RepoTab,
    theme::Palette,
};

fn status(s: &OpSnapshot, p: &Palette) -> (&'static str, Color32, String) {
    match &s.state {
        OpState::Finished(Outcome::Succeeded) if s.attention => ("!", p.status_modified, s.summary.clone().unwrap_or_default()),
        OpState::Finished(Outcome::Succeeded) => ("✓", p.status_added, s.summary.clone().unwrap_or_default()),
        OpState::Finished(Outcome::Failed(e)) => ("×", p.status_deleted, e.lines().next().unwrap_or_default().to_owned()),
        OpState::Finished(Outcome::Cancelled) => ("⊘", p.text_weak, "cancelled".into()),
        OpState::Finished(Outcome::Unknown(m)) => ("?", p.status_modified, m.clone()),
        OpState::Queued => ("…", p.text_weak, "queued".into()),
        OpState::Running => ("▶", p.accent, s.progress.clone().unwrap_or_else(|| "running".into())),
        OpState::AwaitingAuth => ("…", p.accent, "waiting for credentials".into()),
        OpState::Cancelling => ("⊘", p.text_weak, "cancelling…".into()),
    }
}

/// The most recent progress of a running operation (for the toolbar).
pub fn running_summary(tab: &RepoTab) -> Option<(ops_registry::OpId, String)> {
    ops_registry::snapshots_for(tab.repo_key()).into_iter().find(|s| s.state.is_active()).map(|s| {
        let detail = s.progress.clone().unwrap_or_else(|| "running…".into());
        (s.id, format!("{}: {detail}", s.kind.label()))
    })
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let p = Palette::of(ui.ctx());
    let snaps = ops_registry::snapshots_for(tab.repo_key());
    let focus = tab.output_focus.take();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Output").strong());
        ui.label(RichText::new(format!("{} operations", snaps.len())).small().color(p.text_weak));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Hide").clicked() {
                tab.output_open = false;
            }
        });
    });
    ui.separator();
    egui::ScrollArea::vertical().id_salt("output").auto_shrink(false).show(ui, |ui| {
        if snaps.is_empty() {
            ui.label(RichText::new("No operations yet").color(p.text_weak));
        }
        for s in &snaps {
            let (icon, color, detail) = status(s, p);
            let duration = s.duration.map(|d| format!("  {:.1}s", d.as_secs_f32())).unwrap_or_default();
            let title = RichText::new(format!("{icon} {}  {detail}{duration}", s.kind.label())).color(color);
            let mut header = egui::CollapsingHeader::new(title).id_salt(("op", s.id));
            if focus == Some(s.id) {
                header = header.open(Some(true));
            }
            let resp = header.show(ui, |ui| {
                if s.state.is_active() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        if let Some(line) = &s.progress {
                            ui.label(RichText::new(line).monospace().small());
                        }
                        if !matches!(s.state, OpState::Cancelling) && ui.button("Cancel").clicked() {
                            ops_registry::cancel(s.id);
                        }
                    });
                }
                if let OpState::Finished(Outcome::Failed(e) | Outcome::Unknown(e)) = &s.state {
                    ui.add(egui::Label::new(RichText::new(e).color(p.status_deleted)).wrap().selectable(true));
                }
                for c in &s.commands {
                    let code = match (c.cancelled, c.code) {
                        (true, _) => "cancelled".to_owned(),
                        (_, Some(code)) => format!("exit {code}"),
                        (_, None) => "killed".to_owned(),
                    };
                    ui.label(
                        RichText::new(format!("$ git {}   ({code}, {:.2}s)", c.args.join(" "), c.duration.as_secs_f32()))
                            .monospace()
                            .small()
                            .color(p.text_weak),
                    );
                    for text in [&c.stdout, &c.stderr] {
                        let text = text.trim_end();
                        if !text.is_empty() {
                            ui.add(egui::Label::new(RichText::new(text).monospace().small()).wrap().selectable(true));
                        }
                    }
                }
            });
            if focus == Some(s.id) {
                resp.header_response.scroll_to_me(Some(egui::Align::TOP));
            }
        }
    });
}
