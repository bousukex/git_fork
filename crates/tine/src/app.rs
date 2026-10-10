//! Top-level window: toolbar, repository tabs and the active repository.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, bounded};
use egui::{Key, KeyboardShortcut, Modifiers, RichText};
use serde::{Deserialize, Serialize};

use crate::{
    repo::{RepoTab, View},
    theme::Palette,
    ui,
};

const STORAGE_KEY: &str = "tine-state";
const MAX_RECENT: usize = 15;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    recent: Vec<PathBuf>,
    open: Vec<PathBuf>,
    active: usize,
}

pub struct TineApp {
    tabs: Vec<RepoTab>,
    active: usize,
    state: Persisted,
    picker: Option<Receiver<Option<PathBuf>>>,
    open_error: Option<String>,
    debug: DebugOverlay,
    focused: bool,
    ctx: egui::Context,
}

/// F12 overlay with frame timing and memory, for checking performance.
#[derive(Default)]
struct DebugOverlay {
    open: bool,
    /// Recent CPU times per frame (seconds).
    frames: std::collections::VecDeque<f32>,
}

impl DebugOverlay {
    fn record(&mut self, cpu: Option<f32>) {
        if let Some(c) = cpu {
            if self.frames.len() == 120 {
                self.frames.pop_front();
            }
            self.frames.push_back(c);
        }
    }

    fn show(&mut self, ctx: &egui::Context, tab: Option<&RepoTab>) {
        if !self.open {
            return;
        }
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:").map(|v| v.trim().to_owned())))
            .unwrap_or_default();
        let max = self.frames.iter().copied().fold(0.0, f32::max);
        let avg = self.frames.iter().sum::<f32>() / self.frames.len().max(1) as f32;
        egui::Window::new("Debug").open(&mut self.open).resizable(false).show(ctx, |ui| {
            ui.label(format!("CPU per frame: avg {:.2} ms, max {:.2} ms (last 120)", avg * 1e3, max * 1e3));
            ui.label(format!("Resident memory: {rss}"));
            if let Some(t) = tab {
                ui.label(format!(
                    "Commits: {} · graph {:.1} MiB",
                    t.history.commits.len(),
                    t.history.graph.heap_bytes() as f64 / (1 << 20) as f64
                ));
                if let Some(e) = t.history.elapsed {
                    ui.label(format!("History load: {e:.2?}"));
                }
                for line in t.activity.iter().rev().take(5) {
                    ui.label(egui::RichText::new(line).small());
                }
            }
        });
    }
}

impl TineApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Vec<PathBuf>) -> Self {
        crate::setup_context(&cc.egui_ctx);
        let state: Persisted = cc.storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default();
        let mut app =
            TineApp {
            tabs: Vec::new(),
            active: 0,
            state,
            picker: None,
            open_error: None,
            debug: DebugOverlay::default(),
            focused: true,
            ctx: cc.egui_ctx.clone(),
        };
        let restoring = paths.is_empty();
        let to_open = if restoring { app.state.open.clone() } else { paths };
        for p in to_open {
            app.open(&p, &cc.egui_ctx);
        }
        // Restore the previous session's active tab; with paths given on the
        // command line, the last one opened stays active.
        if restoring && !app.tabs.is_empty() {
            app.active = app.state.active.min(app.tabs.len() - 1);
        }
        app
    }

    /// True when no repository is loading history.
    pub fn is_idle(&self) -> bool {
        self.tabs.iter().all(|t| !t.history.loading)
    }

    /// Switches the active repository to the local changes view.
    pub fn show_local_changes(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.view = View::LocalChanges;
        }
    }

    fn open(&mut self, path: &Path, ctx: &egui::Context) {
        if let Some(i) = self.tabs.iter().position(|t| t.paths.workdir == path) {
            self.active = i;
            return;
        }
        if !path.is_dir() {
            self.open_error = Some(format!("Folder not found: {}", path.display()));
            return;
        }
        match RepoTab::open(path, ctx) {
            Ok(tab) => {
                let workdir = tab.paths.workdir.clone();
                if let Some(i) = self.tabs.iter().position(|t| t.paths.workdir == workdir) {
                    self.active = i;
                    return;
                }
                self.state.recent.retain(|p| p != &workdir);
                self.state.recent.insert(0, workdir);
                self.state.recent.truncate(MAX_RECENT);
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.open_error = None;
            }
            Err(e) => self.open_error = Some(format!("{}: {e:#}", path.display())),
        }
    }

    fn pick_folder(&mut self) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = bounded(1);
        self.picker = Some(rx);
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let dir = rfd::FileDialog::new().set_title("Open Repository").pick_folder();
            let _ = tx.send(dir);
            ctx.request_repaint();
        });
    }

    /// Closes tab `i`, keeping the same repository active when possible.
    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i);
        if i < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Title of the active repository tab.
    pub fn active_title(&self) -> Option<&str> {
        self.tabs.get(self.active).map(|t| t.title.as_str())
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let open = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
        let refresh = KeyboardShortcut::new(Modifiers::NONE, Key::F5);
        let refresh2 = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
        let close = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
        if ctx.input_mut(|i| i.consume_shortcut(&open)) {
            self.pick_folder();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&refresh) || i.consume_shortcut(&refresh2))
            && let Some(t) = self.tabs.get_mut(self.active)
        {
            t.refresh();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&close)) && !self.tabs.is_empty() {
            self.close_tab(self.active);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F12)) {
            self.debug.open = !self.debug.open;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui.ctx());
        ui.horizontal_centered(|ui| {
            if ui.button("Open…").on_hover_text("Open a repository (Ctrl+O)").clicked() {
                self.pick_folder();
            }
            ui.separator();
            for (label, tip) in [("Fetch", "M3"), ("Pull", "M3"), ("Push", "M3"), ("Stash", "M3"), ("Branch", "M3")] {
                ui.add_enabled(false, egui::Button::new(label))
                    .on_disabled_hover_text(format!("Not implemented yet ({tip})"));
            }
            ui.separator();
            let has_tab = !self.tabs.is_empty();
            if ui.add_enabled(has_tab, egui::Button::new("Refresh")).on_hover_text("Reload (F5)").clicked()
                && let Some(t) = self.tabs.get_mut(self.active)
            {
                t.refresh();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("☰", |ui| {
                    ui.label(RichText::new("Theme").small().color(p.text_weak));
                    let mut pref = ui.ctx().options(|o| o.theme_preference);
                    let before = pref;
                    ui.selectable_value(&mut pref, egui::ThemePreference::System, "System");
                    ui.selectable_value(&mut pref, egui::ThemePreference::Light, "Light");
                    ui.selectable_value(&mut pref, egui::ThemePreference::Dark, "Dark");
                    if pref != before {
                        ui.ctx().set_theme(pref);
                    }
                });
                if let Some(t) = self.tabs.get(self.active) {
                    let h = &t.history;
                    let text = if h.loading {
                        format!("Loading history… {}", h.commits.len())
                    } else if let Some(e) = h.elapsed {
                        format!("{} commits · {:.1}s", h.commits.len(), e.as_secs_f32())
                    } else {
                        String::new()
                    };
                    ui.label(RichText::new(text).small().color(p.text_weak));
                    if h.loading {
                        ui.spinner();
                    }
                }
            });
        });
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let mut select = None;
        let mut close = None;
        ui.horizontal(|ui| {
            for (i, t) in self.tabs.iter().enumerate() {
                let resp = ui
                    .add(egui::Button::selectable(i == self.active, &t.title))
                    .on_hover_text(t.paths.workdir.display().to_string());
                if resp.clicked() {
                    select = Some(i);
                }
                if resp.middle_clicked() {
                    close = Some(i);
                }
                resp.context_menu(|ui| {
                    if ui.button("Close Tab").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            if ui.small_button("+").on_hover_text("Open repository").clicked() {
                self.pick_folder();
            }
        });
        if let Some(i) = select {
            self.active = i;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui.ctx());
        let mut open = None;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.15);
            ui.label(RichText::new("Tine").size(28.0).strong());
            ui.label(RichText::new("A Git client for Linux").color(p.text_weak));
            ui.add_space(16.0);
            if ui.button(RichText::new("Open Repository…").size(15.0)).clicked() {
                self.pick_folder();
            }
            if let Some(e) = &self.open_error {
                ui.add_space(8.0);
                ui.label(RichText::new(e).color(p.status_deleted));
            }
            if !self.state.recent.is_empty() {
                ui.add_space(20.0);
                ui.label(RichText::new("Recent").small().strong().color(p.text_weak));
                for r in &self.state.recent {
                    if ui.link(r.display().to_string()).clicked() {
                        open = Some(r.clone());
                    }
                }
            }
        });
        if let Some(r) = open {
            self.open(&r, ui.ctx());
        }
    }
}

fn error_banner(ui: &mut egui::Ui, tab: &mut RepoTab) {
    if tab.errors.is_empty() {
        return;
    }
    let p = Palette::of(ui.ctx());
    let mut dismiss = false;
    egui::Frame::NONE
        .fill(p.diff_del_bg)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(tab.errors.last().expect("non-empty")).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dismiss = ui.small_button("✕").clicked();
                });
            });
        });
    if dismiss {
        tab.errors.clear();
    }
}

impl eframe::App for TineApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = Palette::of(&ctx);
        self.debug.record(frame.info().cpu_usage);
        self.debug.show(&ctx, self.tabs.get(self.active));
        if let Some(rx) = &self.picker
            && let Ok(choice) = rx.try_recv()
        {
            self.picker = None;
            if let Some(dir) = choice {
                self.open(&dir, &ctx);
            }
        }
        let mut requests = Vec::new();
        for t in &mut self.tabs {
            t.poll();
            requests.extend(t.open_request.take());
        }
        for r in requests {
            self.open(&r, &ctx);
        }
        // Coming back to the window: pick up changes made elsewhere, even
        // if file watching is unavailable (history reloads only if refs moved).
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.focused
            && let Some(t) = self.tabs.get_mut(self.active)
        {
            t.refresh_if_changed();
        }
        self.focused = focused;
        self.shortcuts(&ctx);

        let title = match self.tabs.get(self.active) {
            Some(t) => format!("{} — Tine", t.title),
            None => "Tine".into(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::Panel::top("toolbar")
            .exact_size(38.0)
            .frame(egui::Frame::NONE.fill(p.toolbar_bg).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ui, |ui| self.toolbar(ui));
        if !self.tabs.is_empty() {
            egui::Panel::top("tabs")
                .frame(egui::Frame::NONE.fill(p.toolbar_bg).inner_margin(egui::Margin::symmetric(8, 3)))
                .show(ui, |ui| self.tab_strip(ui));
        }

        let Some(tab) = self.tabs.get_mut(self.active) else {
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui));
            return;
        };

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(240.0)
            .size_range(160.0..=480.0)
            .frame(egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(6, 0)))
            .show(ui, |ui| ui::sidebar::show(ui, tab));

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
            error_banner(ui, tab);
            match tab.view {
                View::History => {
                    let h = ui.available_height();
                    egui::Panel::bottom("detail")
                        .resizable(true)
                        .default_size(h * 0.45)
                        .size_range(120.0..=h - 80.0)
                        .frame(egui::Frame::NONE.fill(p.bg))
                        .show(ui, |ui| ui::detail::show(ui, tab));
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(p.bg))
                        .show(ui, |ui| ui::history::show(ui, tab));
                }
                View::LocalChanges => ui::changes::show(ui, tab),
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.state.open = self.tabs.iter().map(|t| t.paths.workdir.clone()).collect();
        self.state.active = self.active;
        eframe::set_value(storage, STORAGE_KEY, &self.state);
    }
}
