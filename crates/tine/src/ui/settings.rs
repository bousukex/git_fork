//! The settings dialog (Ctrl+, or ☰ → Settings…). Changes apply at once.

use egui::{RichText, ThemePreference};

use crate::{
    settings::{FONT_SIZES, Settings, TAB_WIDTHS},
    shortcuts::{self, LOCAL_KEYS, SHORTCUTS},
    theme::Palette,
};

/// Zoom levels offered (egui's zoom factor).
pub const ZOOMS: &[f32] = &[0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// Shows the dialog while `open`; edits `settings` in place.
pub fn show(ctx: &egui::Context, settings: &mut Settings, open: &mut bool) {
    if !*open {
        return;
    }
    let p = Palette::of(ctx);
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new("settings-dialog")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading("Settings");
        ui.add_space(6.0);
        egui::Grid::new("settings-grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Theme");
            ui.horizontal(|ui| {
                let mut pref = ctx.options(|o| o.theme_preference);
                let before = pref;
                ui.selectable_value(&mut pref, ThemePreference::System, "System");
                ui.selectable_value(&mut pref, ThemePreference::Light, "Light");
                ui.selectable_value(&mut pref, ThemePreference::Dark, "Dark");
                if pref != before {
                    ctx.set_theme(pref);
                }
            });
            ui.end_row();

            ui.label("Zoom");
            ui.horizontal(|ui| {
                let zoom = ctx.zoom_factor();
                for &z in ZOOMS {
                    let label = format!("{}%", (z * 100.0).round());
                    if ui.selectable_label((zoom - z).abs() < 0.01, label).clicked() {
                        ctx.set_zoom_factor(z);
                    }
                }
            });
            ui.end_row();

            ui.label("Diff font size");
            ui.add(egui::Slider::new(&mut settings.diff_font_size, FONT_SIZES).step_by(0.5).suffix(" pt"));
            ui.end_row();

            ui.label("Tab width");
            ui.add(egui::Slider::new(&mut settings.tab_width, TAB_WIDTHS).suffix(" columns"));
            ui.end_row();

            ui.label("Syntax");
            ui.checkbox(&mut settings.syntax_highlighting, "Highlight diffs and blame");
            ui.end_row();
        });

        ui.add_space(10.0);
        ui.label(RichText::new("Keyboard Shortcuts").strong());
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            egui::Grid::new("settings-keys").num_columns(2).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                for s in SHORTCUTS {
                    ui.label(s.command.label());
                    ui.label(RichText::new(shortcuts::key_text(s)).monospace().color(p.text_weak));
                    ui.end_row();
                }
                // View-local keys, with the view in which they work.
                let mut shown: Vec<(String, String)> = Vec::new();
                for (scope, m, k, what) in LOCAL_KEYS {
                    let keys = shortcuts::keys_text(*m, *k);
                    let label = format!("{what} ({})", scope.label());
                    // Several keys doing the same thing share a row.
                    match shown.iter_mut().find(|(l, _)| *l == label) {
                        Some((_, k)) => *k = format!("{k}, {keys}"),
                        None => shown.push((label, keys)),
                    }
                }
                for (label, keys) in shown {
                    ui.label(label);
                    ui.label(RichText::new(keys).monospace().color(p.text_weak));
                    ui.end_row();
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Reset to Defaults").clicked() {
                *settings = Settings::default();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
            });
        });
    });
    if close || modal.should_close() {
        *open = false;
    }
    *settings = settings.validated();
}
