//! The Setup check in the app ([`pengitto_core::setup`]): checked in the
//! background (a newer check replaces an older one's result), shown in its
//! dialog (☰ → Check Setup…), its problems on the welcome screen; libsecret
//! made the credential helper from there.

use crossbeam_channel::{Receiver, bounded};
use egui::RichText;
use pengitto_core::setup::{self, Action, Env, HelperWrite, Probed, Report, Row, Status};

use crate::theme::Palette;

pub struct Setup {
    env: Env,
    report: Option<Report>,
    /// The check under way (a newer one drops it).
    checking: Option<Receiver<Report>>,
    writing: Option<Receiver<HelperWrite>>,
    /// What the last write did (and whether it worked).
    note: Option<(bool, String)>,
    /// The dialog shows.
    pub open: bool,
}

impl Setup {
    pub fn new(env: Env) -> Setup {
        Setup { env, report: None, checking: None, writing: None, note: None, open: false }
    }

    /// Checks (again): what an earlier check still finds is dropped.
    pub fn check(&mut self, ctx: &egui::Context) {
        let (tx, rx) = bounded(1);
        let (env, ctx) = (self.env.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("pengitto-setup".into())
            .spawn(move || {
                if tx.send(setup::check(&env)).is_ok() {
                    ctx.request_repaint();
                }
            })
            .ok();
        self.checking = Some(rx);
    }

    pub fn checking(&self) -> bool {
        self.checking.is_some()
    }

    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.checking
            && let Ok(r) = rx.try_recv()
        {
            self.checking = None;
            self.report = Some(r);
        }
        if let Some(rx) = &self.writing
            && let Ok(w) = rx.try_recv()
        {
            self.writing = None;
            let (ok, text, now) = match w {
                HelperWrite::Done(h) => (true, "libsecret is set as git's credential helper (in your global settings).".to_owned(), Some(h)),
                HelperWrite::Changed(h) => (false, "The credential helpers were changed meanwhile: nothing was written (see them now).".to_owned(), Some(h)),
                HelperWrite::Failed(e, h) => (false, format!("Not set: {e}"), h),
            };
            self.note = Some((ok, text));
            if let (Some(r), Some(h)) = (&mut self.report, now) {
                r.helpers = Probed::Known(h);
            }
            // A check that read them before it changed them: checked again
            // (what it would bring is out of date).
            if self.checking.is_some() {
                self.check(ctx);
            }
        }
    }

    /// What the check found, item by item (and the fonts, if fontconfig
    /// didn't answer at startup).
    pub fn rows(&self) -> Vec<Row> {
        let Some(r) = &self.report else { return Vec::new() };
        let mut rows = setup::rows(r);
        if let Some(t) = crate::fonts::trouble() {
            rows.push(Row {
                what: "Fonts at startup",
                status: Status::Unknown,
                text: format!("fontconfig didn't tell them ({t}): egui's own fonts are used (no Japanese)."),
                advice: Vec::new(),
                action: None,
            });
        }
        rows
    }

    pub fn problems(&self) -> Vec<Row> {
        self.rows().into_iter().filter(|r| matches!(r.status, Status::Problem | Status::Unknown)).collect()
    }

    /// Makes libsecret the credential helper, if the helpers are still as
    /// shown.
    pub fn use_libsecret(&mut self, ctx: &egui::Context) {
        let Some(Probed::Known(shown)) = self.report.as_ref().map(|r| r.helpers.clone()) else { return };
        if self.writing.is_some() {
            return;
        }
        let (tx, rx) = bounded(1);
        let (env, ctx) = (self.env.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("pengitto-setup-write".into())
            .spawn(move || {
                let _ = tx.send(setup::use_libsecret(&env, &shown));
                ctx.request_repaint();
            })
            .ok();
        self.note = None;
        self.writing = Some(rx);
    }

    /// The dialog; what it asks of the app (Settings → Git).
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Action> {
        if !self.open {
            return None;
        }
        let p = Palette::of(ctx);
        let rows = self.rows();
        let (mut close, mut again, mut asked) = (false, false, None);
        let modal = egui::Modal::new(egui::Id::new("setup-check")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.heading("Setup");
            ui.label(RichText::new("What pengitto needs from this system, as it is now.").color(p.text_weak));
            ui.label(
                RichText::new("(Git's system and global settings: a repository's own settings can change them there.)")
                    .small()
                    .color(p.text_weak),
            );
            ui.add_space(6.0);
            if rows.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking…");
                });
            }
            let height = (ctx.content_rect().height() - 220.0).clamp(160.0, 520.0);
            egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| {
                for row in &rows {
                    let (mark, color) = match row.status {
                        Status::Ok => ("✓", p.status_added),
                        Status::Problem => ("!", p.status_deleted),
                        Status::Unknown => ("?", p.status_modified),
                        Status::Info => ("−", p.text_weak),
                    };
                    ui.horizontal_top(|ui| {
                        ui.add_sized([18.0, 18.0], egui::Label::new(RichText::new(mark).strong().color(color)));
                        ui.vertical(|ui| {
                            ui.label(RichText::new(row.what).strong());
                            ui.add(egui::Label::new(&row.text).wrap());
                            for a in &row.advice {
                                ui.add(egui::Label::new(RichText::new(a).monospace().small()).wrap().selectable(true));
                            }
                            match row.action {
                                Some(Action::GitSettings) => {
                                    if ui.button("Open Settings → Git").clicked() {
                                        asked = Some(Action::GitSettings);
                                    }
                                }
                                Some(Action::UseLibsecret) => {
                                    let found = self.report.as_ref().and_then(|r| r.libsecret.clone());
                                    let writing = self.writing.is_some();
                                    let button = ui
                                        .add_enabled(found.is_some() && !writing, egui::Button::new(if writing { "Setting…" } else { "Use libsecret" }))
                                        .on_hover_text("git config --global --add credential.helper libsecret (the desktop's keyring keeps them)")
                                        .on_disabled_hover_text(if writing { "Setting it…" } else { "git-credential-libsecret isn't installed (see above)" });
                                    if button.clicked() {
                                        asked = Some(Action::UseLibsecret);
                                    }
                                }
                                None => {}
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            });
            if let Some((ok, note)) = &self.note {
                ui.add(egui::Label::new(RichText::new(note).color(if *ok { p.status_added } else { p.status_deleted })).wrap());
            }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
                again = ui.add_enabled(!self.checking(), egui::Button::new("Check Again")).clicked();
                if self.checking() && !rows.is_empty() {
                    ui.spinner();
                }
            });
        });
        if again {
            self.note = None;
            self.check(ctx);
        }
        if asked == Some(Action::UseLibsecret) {
            self.use_libsecret(ctx);
            asked = None;
        }
        if close || modal.should_close() || asked.is_some() {
            self.open = false;
        }
        asked
    }
}

#[cfg(test)]
mod tests {
    use pengitto_core::{
        setup::{Agent, Helpers, Ssh, System, Who},
        version::GitVersion,
    };

    use super::*;

    #[test]
    fn fonts_that_failed_at_startup_are_told_whatever_fc_list_says() {
        let _one = crate::fonts::TROUBLE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = Setup::new(Env::from_process("git".into()));
        s.report = Some(Report {
            git: Probed::Known(Some(GitVersion(2, 43, 0))),
            who: Probed::Known(Who::default()),
            helpers: Probed::Known(Helpers::default()),
            libsecret: None,
            ssh: Ssh { agent: Probed::Known(Agent::None), keys: Vec::new() },
            japanese_font: Probed::Known(Some("Noto Sans CJK JP".into())),
            system: System::default(),
        });
        crate::fonts::forget_trouble();
        assert!(!s.problems().iter().any(|r| r.what == "Fonts at startup"));
        crate::fonts::note_trouble("fc-match sans-serif failed (exit status: 1)".into());
        let row = s.problems().into_iter().find(|r| r.what == "Fonts at startup").expect("told");
        assert_eq!(row.status, Status::Unknown);
        assert!(row.text.contains("fc-match sans-serif failed"), "{}", row.text);
        assert!(s.rows().iter().any(|r| r.what == "Japanese font" && r.status == Status::Ok));
        crate::fonts::forget_trouble();
    }
}
