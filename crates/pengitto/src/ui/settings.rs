//! The settings dialog (Ctrl+, or ☰ → Settings…), in pages: appearance,
//! the global git identity, external tools, keyboard shortcuts. Appearance
//! and tools apply at once; the identity is written to git's global config
//! with Save.

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{RichText, ThemePreference};
use pengitto_core::{
    GitRunner,
    config::{self, Identity},
};

use crate::{
    settings::{FONT_SIZES, Settings, TAB_WIDTHS},
    shortcuts::{self, LOCAL_KEYS, SHORTCUTS},
    theme::Palette,
    tools::{self, ToolSettings},
};

/// Zoom levels offered (egui's zoom factor).
pub const ZOOMS: &[f32] = &[0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    General,
    Git,
    Tools,
    Shortcuts,
}

/// The dialog's own state (kept while the app runs).
pub struct SettingsUi {
    pub page: Page,
    pub identity: GitIdentity,
    /// The terminal command line being edited (it may not parse yet).
    terminal_text: String,
    terminal_error: Option<String>,
    /// The terminal found automatically, as a command line.
    detected: Option<String>,
}

impl Default for SettingsUi {
    fn default() -> Self {
        SettingsUi {
            page: Page::default(),
            identity: GitIdentity::new(config::global_runner()),
            terminal_text: String::new(),
            terminal_error: None,
            detected: None,
        }
    }
}

impl SettingsUi {
    /// The dialog opens: read what may have changed outside.
    pub fn opened(&mut self, ctx: &egui::Context, tools_settings: &ToolSettings) {
        self.identity.load(ctx);
        self.terminal_text = tools::join_words(&tools_settings.terminal);
        self.terminal_error = None;
        self.detected = tools::detect_terminal(&tools::System).map(|w| tools::join_words(&w));
    }
}

enum IdentityEvent {
    Loaded(u64, Result<Identity, String>),
    Saved {
        message: Result<String, String>,
        /// Per field: what was written (`None`: not asked to), and whether
        /// that worked.
        name: Option<(String, bool)>,
        email: Option<(String, bool)>,
        now: Result<Identity, String>,
    },
}

/// user.name and user.email of git's global config, as edited in the Git
/// page. Reading and writing run on threads; writes go one at a time.
///
/// Each field remembers what it showed when it last matched the config
/// (`base`): a field that differs was edited by the user. Only edited
/// fields are written, and a new reading replaces only unedited ones.
pub struct GitIdentity {
    git: GitRunner,
    /// The last values read (or known to be written).
    known: Option<Identity>,
    loading: bool,
    /// The last reading failed.
    read_error: Option<String>,
    pub name: String,
    pub email: String,
    base: (String, String),
    saving: bool,
    status: Option<Result<String, String>>,
    /// Readings started; only the latest one counts (a save starts a new
    /// one, so an older reading can't undo what it wrote).
    loads: u64,
    tx: Sender<IdentityEvent>,
    rx: Receiver<IdentityEvent>,
}

impl GitIdentity {
    pub fn new(git: GitRunner) -> Self {
        let (tx, rx) = unbounded();
        GitIdentity {
            git,
            known: None,
            loading: false,
            read_error: None,
            name: String::new(),
            email: String::new(),
            base: (String::new(), String::new()),
            saving: false,
            status: None,
            loads: 0,
            tx,
            rx,
        }
    }

    /// Uses another git configuration (tests: a private global config).
    pub fn set_runner(&mut self, git: GitRunner) {
        self.git = git;
    }

    pub fn saving(&self) -> bool {
        self.saving
    }

    /// The outcome of the last save: `Ok(note)` or `Err(problem)`.
    pub fn status(&self) -> Option<&Result<String, String>> {
        self.status.as_ref()
    }

    /// What the global config held when last read (or written).
    pub fn stored(&self) -> Option<&Identity> {
        self.known.as_ref()
    }

    pub fn read_error(&self) -> Option<&str> {
        self.read_error.as_deref()
    }

    /// Reads the config again (unless a save is on its way: it reads it
    /// after writing).
    pub fn load(&mut self, ctx: &egui::Context) {
        if self.saving {
            return;
        }
        self.loads += 1;
        self.loading = true;
        let (seq, git, tx, ctx) = (self.loads, self.git.clone(), self.tx.clone(), ctx.clone());
        let started = std::thread::Builder::new().name("pengitto-identity".into()).spawn(move || {
            let _ = tx.send(IdentityEvent::Loaded(seq, config::global_identity(&git).map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
        if let Err(e) = started {
            self.loading = false;
            self.read_error = Some(format!("can't read the git config: {e}"));
        }
    }

    pub fn name_edited(&self) -> bool {
        self.name.trim() != self.base.0
    }

    pub fn email_edited(&self) -> bool {
        self.email.trim() != self.base.1
    }

    fn edited(&self) -> bool {
        self.name_edited() || self.email_edited()
    }

    /// Takes what the threads sent.
    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                IdentityEvent::Loaded(seq, r) => {
                    if seq != self.loads {
                        continue;
                    }
                    self.loading = false;
                    match r {
                        Ok(id) => {
                            self.read_error = None;
                            self.take_reading(&id);
                            self.known = Some(id);
                        }
                        Err(e) => self.read_error = Some(e),
                    }
                }
                IdentityEvent::Saved { message, name, email, now } => {
                    self.saving = false;
                    self.status = Some(message);
                    let now = match now {
                        Ok(id) => {
                            self.read_error = None;
                            Some(id)
                        }
                        Err(e) => {
                            self.read_error = Some(e);
                            None
                        }
                    };
                    let stored = |f: fn(&Identity) -> &Option<String>| now.as_ref().map(|id| f(id).clone().unwrap_or_default());
                    settle_field(&mut self.name, &mut self.base.0, name, stored(|i| &i.name));
                    settle_field(&mut self.email, &mut self.base.1, email, stored(|i| &i.email));
                    if let Some(id) = now {
                        self.known = Some(id);
                    }
                }
            }
        }
    }

    /// Brings the fields the user didn't edit in line with a reading.
    fn take_reading(&mut self, id: &Identity) {
        let (name, email) = (id.name.clone().unwrap_or_default(), id.email.clone().unwrap_or_default());
        if !self.name_edited() {
            self.name.clone_from(&name);
            self.base.0 = name;
        }
        if !self.email_edited() {
            self.email.clone_from(&email);
            self.base.1 = email;
        }
    }

    /// Writes the fields the user edited, then reads the config back.
    pub fn save(&mut self, ctx: &egui::Context) {
        if self.saving || self.known.is_none() || !self.edited() {
            return;
        }
        let pick = |edited: bool, what: &str, value: &str| -> Result<Option<String>, String> {
            if !edited {
                return Ok(None);
            }
            config::check_value(what, value).map(Some).map_err(|e| format!("{e:#}"))
        };
        let (name, email) = match (pick(self.name_edited(), "The name", &self.name), pick(self.email_edited(), "The email", &self.email)) {
            (Ok(n), Ok(e)) => (n, e),
            (Err(e), _) | (_, Err(e)) => {
                self.status = Some(Err(e));
                return;
            }
        };
        // Readings still on their way predate this save: they no longer count.
        self.loads += 1;
        self.loading = false;
        self.saving = true;
        self.status = None;
        let (git, tx, ctx) = (self.git.clone(), self.tx.clone(), ctx.clone());
        let started = std::thread::Builder::new().name("pengitto-identity".into()).spawn(move || {
            let w = config::set_global_identity(&git, name.as_deref(), email.as_deref());
            let err = |r: &Option<anyhow::Result<()>>| r.as_ref().and_then(|r| r.as_ref().err()).map(|e| format!("{e:#}"));
            let (name_err, email_err) = (err(&w.name), err(&w.email));
            let message = match (&name_err, &email_err, w.name.is_some(), w.email.is_some()) {
                (None, None, _, _) => Ok("Saved to the global git config".to_owned()),
                (Some(n), Some(e), _, _) => Err(format!("Nothing was saved: {n}; {e}")),
                (Some(n), None, _, true) => Err(format!("The email was saved, the name wasn't: {n}")),
                (Some(n), None, _, false) => Err(format!("The name wasn't saved: {n}")),
                (None, Some(e), true, _) => Err(format!("The name was saved, the email wasn't: {e}")),
                (None, Some(e), false, _) => Err(format!("The email wasn't saved: {e}")),
            };
            let now = config::global_identity(&git).map_err(|e| format!("{e:#}"));
            let _ = tx.send(IdentityEvent::Saved {
                message,
                name: name.map(|v| (v, name_err.is_none())),
                email: email.map(|v| (v, email_err.is_none())),
                now,
            });
            ctx.request_repaint();
        });
        if let Err(e) = started {
            self.saving = false;
            self.status = Some(Err(format!("can't write the git config: {e}")));
        }
    }
}

/// A field after a save: written fine, it shows what is stored now (or
/// what was written, if it couldn't be read back); failed, it keeps the
/// input for another try; not written, it follows the config unless edited.
fn settle_field(draft: &mut String, base: &mut String, written: Option<(String, bool)>, stored: Option<String>) {
    match written {
        Some((value, true)) => {
            let v = stored.unwrap_or(value);
            draft.clone_from(&v);
            *base = v;
        }
        Some((_, false)) => {
            if let Some(v) = stored {
                *base = v;
            }
        }
        None => {
            if let Some(v) = stored {
                if draft.trim() == base.as_str() {
                    draft.clone_from(&v);
                }
                *base = v;
            }
        }
    }
}

/// Shows the dialog while `open`; edits the settings in place.
pub fn show(ctx: &egui::Context, settings: &mut Settings, tools_settings: &mut ToolSettings, state: &mut SettingsUi, open: &mut bool) {
    if !*open {
        return;
    }
    state.identity.poll();
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new("settings-dialog")).show(ctx, |ui| {
        ui.set_width(500.0);
        ui.heading("Settings");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for (page, label) in [(Page::General, "General"), (Page::Git, "Git"), (Page::Tools, "Tools"), (Page::Shortcuts, "Shortcuts")] {
                ui.selectable_value(&mut state.page, page, label);
            }
        });
        ui.separator();
        ui.add_space(4.0);
        // One height for every page, so the dialog (centered) stays put
        // when switching.
        ui.vertical(|ui| {
            ui.set_min_height(330.0);
            match state.page {
                Page::General => general(ui, settings),
                Page::Git => git(ui, &mut state.identity),
                Page::Tools => tools_page(ui, tools_settings, state),
                Page::Shortcuts => shortcut_list(ui),
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if state.page == Page::General && ui.button("Reset to Defaults").clicked() {
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

fn general(ui: &mut egui::Ui, settings: &mut Settings) {
    let ctx = ui.ctx().clone();
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
}

fn git(ui: &mut egui::Ui, id: &mut GitIdentity) {
    let p = Palette::of(ui.ctx());
    ui.label(RichText::new("Who you are in commits, for every repository (git config --global).").color(p.text_weak));
    ui.add_space(6.0);
    if id.known.is_none() {
        if id.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the global git config…");
            });
        } else if let Some(e) = &id.read_error {
            ui.label(RichText::new(format!("Can't read the global git config: {e}")).color(p.status_deleted));
            if ui.button("Try Again").clicked() {
                id.load(ui.ctx());
            }
        }
        return;
    }
    // Read before, but not this time (e.g. right after saving): what was
    // last read stays, with the problem.
    if let Some(e) = id.read_error.clone() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Can't read the global git config again: {e}")).color(p.status_deleted));
            if ui.add_enabled(!id.saving, egui::Button::new("Try Again")).clicked() {
                id.load(ui.ctx());
            }
        });
    }
    let editable = !id.saving;
    egui::Grid::new("settings-git").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Name");
        ui.add_enabled(editable, egui::TextEdit::singleline(&mut id.name).hint_text("Your Name").desired_width(300.0));
        ui.end_row();
        ui.label("Email");
        ui.add_enabled(editable, egui::TextEdit::singleline(&mut id.email).hint_text("you@example.com").desired_width(300.0));
        ui.end_row();
    });
    if !id.email.trim().is_empty() && !id.email.contains('@') {
        ui.label(RichText::new("That doesn't look like an email address.").small().color(p.status_modified));
    }
    ui.label(
        RichText::new("A repository's own user.name and user.email (git config --local) still take precedence there.")
            .small()
            .color(p.text_weak),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let can_save = !id.saving && id.edited();
        if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
            id.save(ui.ctx());
        }
        if id.saving {
            ui.spinner();
            ui.label("Saving…");
        }
    });
    if let (false, Some(status)) = (id.saving, &id.status) {
        let text = match status {
            Ok(note) => RichText::new(note).color(p.status_added),
            Err(e) => RichText::new(e).color(p.status_deleted),
        };
        ui.add(egui::Label::new(text).wrap());
    }
}

fn tools_page(ui: &mut egui::Ui, tools_settings: &mut ToolSettings, state: &mut SettingsUi) {
    let p = Palette::of(ui.ctx());
    let label = ui.label("Terminal");
    let hint = match &state.detected {
        Some(d) => format!("Automatic: {d}"),
        None => "No terminal found: enter one".to_owned(),
    };
    let resp = ui
        .add(egui::TextEdit::singleline(&mut state.terminal_text).hint_text(hint).desired_width(f32::INFINITY).code_editor())
        .labelled_by(label.id);
    if resp.changed() {
        match tools::split_words(&state.terminal_text) {
            Ok(words) => {
                tools_settings.terminal = words;
                state.terminal_error = None;
            }
            Err(e) => state.terminal_error = Some(e),
        }
    }
    if let Some(e) = &state.terminal_error {
        ui.label(RichText::new(e).small().color(p.status_deleted));
    }
    ui.label(
        RichText::new("The program and its arguments; {dir} stands for the repository's folder. Empty: the terminal found automatically.")
            .small()
            .color(p.text_weak),
    );
    ui.add_space(8.0);
    ui.label(
        RichText::new("Files and folders open with the desktop's default applications (xdg-open).").small().color(p.text_weak),
    );
}

fn shortcut_list(ui: &mut egui::Ui) {
    let p = Palette::of(ui.ctx());
    ui.label(RichText::new("Keyboard Shortcuts").strong());
    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
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
}
