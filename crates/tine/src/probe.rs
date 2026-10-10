//! `tine --ime-probe`: a minimal window for checking Japanese input and font
//! rendering on a given desktop session before relying on it in the real UI.

use std::collections::VecDeque;

use egui::{Event, ImeEvent, RichText};

use crate::theme::Palette;

const SAMPLE: &str = "日本語表示：漢字・ひらがな・カタカナ／全角ＡＢＣ１２３ — Fix: 起動時のクラッシュを修正";

#[derive(Default)]
pub struct ImeProbe {
    subject: String,
    description: String,
    events: VecDeque<String>,
}

impl ImeProbe {
    fn record_events(&mut self, ui: &egui::Ui) {
        let events: Vec<String> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    Event::Ime(ImeEvent::Preedit { text, active_range_chars }) => {
                        Some(format!("Preedit {text:?} {active_range_chars:?}"))
                    }
                    Event::Ime(ImeEvent::Commit(t)) => Some(format!("Commit {t:?}")),
                    Event::Ime(other) => Some(format!("{other:?}")),
                    Event::Text(t) => Some(format!("Text {t:?}")),
                    _ => None,
                })
                .collect()
        });
        for e in events {
            log::info!("ime-probe: {e}");
            if self.events.len() == 200 {
                self.events.pop_front();
            }
            self.events.push_back(e);
        }
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| "(unset)".into())
}

impl eframe::App for ImeProbe {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.record_events(ui);
        let p = Palette::of(ui.ctx());

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Tine — IME / font probe");
            ui.label(RichText::new(SAMPLE).size(15.0));
            ui.label(RichText::new(format!("monospace: {SAMPLE}")).monospace());
            ui.add_space(8.0);

            ui.label("Commit subject");
            ui.add(
                egui::TextEdit::singleline(&mut self.subject)
                    .hint_text("ここに日本語を入力して変換してみてください")
                    .desired_width(f32::INFINITY),
            );
            ui.label("Description");
            ui.add(
                egui::TextEdit::multiline(&mut self.description)
                    .hint_text("複数行の入力・変換候補ウィンドウの位置も確認してください")
                    .desired_rows(5)
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "WAYLAND_DISPLAY={}  DISPLAY={}  XMODIFIERS={}  GTK_IM_MODULE={}  QT_IM_MODULE={}",
                    env("WAYLAND_DISPLAY"),
                    env("DISPLAY"),
                    env("XMODIFIERS"),
                    env("GTK_IM_MODULE"),
                    env("QT_IM_MODULE"),
                ))
                .small()
                .color(p.text_weak),
            );
            ui.separator();
            ui.label(RichText::new("Input events (newest last)").strong());
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for e in &self.events {
                    ui.label(RichText::new(e).monospace().small());
                }
            });
        });
    }
}
