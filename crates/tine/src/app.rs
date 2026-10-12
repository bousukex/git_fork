//! Top-level window: toolbar, repository tabs and the active repository.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, bounded};
use egui::{Key, Modifiers, RichText};
use serde::{Deserialize, Serialize};

use crate::{
    input,
    repo::{RepoTab, View},
    shortcuts::{self, Command},
    theme::Palette,
    ui::{
        self,
        quick_launch::{self, QuickLaunch},
    },
};

const STORAGE_KEY: &str = "tine-state";
const MAX_RECENT: usize = 15;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    recent: Vec<PathBuf>,
    open: Vec<PathBuf>,
    active: usize,
    /// Recent commit messages, oldest first.
    messages: Vec<String>,
    settings: crate::settings::Settings,
    tools: crate::tools::ToolSettings,
}

const MAX_MESSAGES: usize = 30;

pub struct TineApp {
    tabs: Vec<RepoTab>,
    active: usize,
    state: Persisted,
    picker: Option<Receiver<Option<PathBuf>>>,
    open_error: Option<String>,
    debug: DebugOverlay,
    focused: bool,
    ctx: egui::Context,
    /// App-level messages (e.g. results of operations from closed tabs).
    notices: Vec<String>,
    /// Results from closed tabs whose work tree is not open (yet); handed
    /// to a tab when that work tree is opened again.
    held_orphans: Vec<crate::ops_registry::OrphanedResult>,
    quick: Option<QuickLaunch>,
    /// Closing the window waits for this answer: unsaved merge editor
    /// changes (one line per tab).
    quit_asked: Option<Vec<String>>,
    /// The answer was "quit anyway".
    quit_confirmed: bool,
    /// The settings dialog is open.
    settings_open: bool,
    settings_ui: ui::settings::SettingsUi,
    /// Repositories being opened in the background, in request order.
    opening: Vec<Opening>,
    /// Worktree removals under way (see `worktree_removals`).
    removals: Vec<Removal>,
    /// Tabs asleep after a failed removal, being checked before they wake.
    wake_checks: Vec<WakeCheck>,
}

/// A worktree being removed: the operation (whichever tab started it), and
/// the tabs asleep for it, by work tree — on the worktree, or in a folder
/// inside it.
struct Removal {
    op: crate::ops_registry::OpId,
    worktree: PathBuf,
    tabs: Vec<PathBuf>,
}

/// A tab whose worktree removal failed: whether its repository is still
/// there as it was (checked off the UI thread).
struct WakeCheck {
    workdir: PathBuf,
    rx: Receiver<Result<(), String>>,
}

/// The repository of `paths` is still there as it was: from its folder, git
/// finds the same work tree, git dir and shared git dir (not, say, a
/// repository around it once its own is gone).
fn still_there(paths: &tine_core::RepoPaths) -> Result<(), String> {
    use tine_core::worktree::same_place;
    let found = tine_core::RepoPaths::discover(&paths.workdir).map_err(|e| format!("{e:#}"))?;
    let same = same_place(&found.workdir, &paths.workdir)
        && same_place(&found.git_dir, &paths.git_dir)
        && same_place(&found.common_dir, &paths.common_dir);
    match same && paths.git_dir.is_dir() {
        true => Ok(()),
        false => Err(format!("git finds the repository of {} there now", found.workdir.display())),
    }
}

/// A repository being discovered off the UI thread.
struct Opening {
    path: PathBuf,
    rx: Receiver<anyhow::Result<tine_core::CliReader>>,
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
        Self::with_storage(cc, paths, cc.storage)
    }

    /// Starts from the state saved in `storage` (tests pass their own).
    pub fn with_storage(cc: &eframe::CreationContext<'_>, paths: Vec<PathBuf>, storage: Option<&dyn eframe::Storage>) -> Self {
        crate::setup_context(&cc.egui_ctx);
        let mut state: Persisted = storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default();
        state.settings = state.settings.validated();
        crate::settings::set_current(&cc.egui_ctx, state.settings);
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
            notices: Vec::new(),
            held_orphans: Vec::new(),
            quick: None,
            quit_asked: None,
            quit_confirmed: false,
            settings_open: false,
            settings_ui: Default::default(),
            opening: Vec::new(),
            removals: Vec::new(),
            wake_checks: Vec::new(),
        };
        let restoring = paths.is_empty();
        let to_open = if restoring { app.state.open.clone() } else { paths };
        // At startup (before the window shows) opening synchronously keeps
        // the tab order and the restored active tab simple.
        for p in to_open {
            app.open_now(&p, &cc.egui_ctx);
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

    fn deliver_orphan(&mut self, o: crate::ops_registry::OrphanedResult) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.paths.workdir == o.origin) {
            t.on_external_op_done(o.kind, o.result);
            return;
        }
        let text = match &o.result {
            Ok(out) if out.summary.is_empty() => None,
            Ok(out) => Some(format!("{} ({})", out.summary, o.origin.display())),
            Err(e) => Some(format!("{} failed in {}: {e:#}", o.kind.label(), o.origin.display())),
        };
        self.notices.extend(text);
        let needs_tab = o.result.as_ref().is_ok_and(|out| !out.follow_ups.is_empty());
        if needs_tab {
            self.held_orphans.push(o);
        }
    }

    /// Opens (or switches to) a repository; git's discovery runs in the
    /// background so a slow file system can't freeze the window.
    pub fn open(&mut self, path: &Path, ctx: &egui::Context) {
        if let Some(i) = self.tabs.iter().position(|t| t.paths.workdir == path) {
            self.active = i;
            return;
        }
        if let Some(why) = self.being_removed(path) {
            self.open_error = Some(why);
            return;
        }
        if !path.is_dir() {
            self.open_error = Some(format!("Folder not found: {}", path.display()));
            return;
        }
        if self.opening.iter().any(|o| o.path == path) {
            return;
        }
        let (tx, rx) = bounded(1);
        let (p, c) = (path.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(tine_core::CliReader::open(&p));
            c.request_repaint();
        });
        self.opening.push(Opening { path: path.to_path_buf(), rx });
    }

    /// Adds repositories whose discovery finished, in the order requested.
    fn poll_opening(&mut self, ctx: &egui::Context) {
        while let Some(first) = self.opening.first() {
            let result = match first.rx.try_recv() {
                Ok(r) => r,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(anyhow::anyhow!("opening failed")),
            };
            let o = self.opening.remove(0);
            self.add_tab(&o.path, result.map(|reader| RepoTab::from_reader(reader, ctx)), ctx);
        }
    }

    /// Opens a repository right away (startup).
    fn open_now(&mut self, path: &Path, ctx: &egui::Context) {
        if let Some(i) = self.tabs.iter().position(|t| t.paths.workdir == path) {
            self.active = i;
            return;
        }
        if !path.is_dir() {
            self.open_error = Some(format!("Folder not found: {}", path.display()));
            return;
        }
        self.add_tab(path, RepoTab::open(path, ctx), ctx);
    }

    fn add_tab(&mut self, path: &Path, tab: anyhow::Result<RepoTab>, _ctx: &egui::Context) {
        match tab {
            Ok(tab) => {
                let workdir = tab.paths.workdir.clone();
                if let Some(i) = self.tabs.iter().position(|t| t.paths.workdir == workdir) {
                    self.active = i;
                    return;
                }
                // Found while its worktree began to be removed.
                if let Some(why) = self.being_removed(&workdir) {
                    self.open_error = Some(why);
                    return;
                }
                self.state.recent.retain(|p| p != &workdir);
                self.state.recent.insert(0, workdir.clone());
                self.state.recent.truncate(MAX_RECENT);
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.open_error = None;
                // Recovery state left by a closed tab for this work tree.
                let (mine, others): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.held_orphans).into_iter().partition(|o| o.origin == workdir);
                self.held_orphans = others;
                let tab = self.tabs.last_mut().expect("just pushed");
                for o in mine {
                    tab.on_external_op_done(o.kind, o.result);
                }
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

    /// Why `path` can't be opened now: a worktree it is in is being
    /// removed.
    fn being_removed(&self, path: &Path) -> Option<String> {
        use tine_core::worktree::is_within;
        let r = self.removals.iter().find(|r| is_within(path, &r.worktree))?;
        Some(format!("{}: the worktree {} is being removed", path.display(), r.worktree.display()))
    }

    /// Worktree removals the tabs asked for, and their ends. Before one
    /// starts, every tab on the worktree or in a folder inside it (a
    /// submodule, say) is checked — nothing of its own may be lost — and put
    /// to sleep. Once the operation ends (whichever tab started it, open or
    /// not), they close if it went; if it failed, each wakes only after its
    /// repository is found to be there as it was (or closes, saying so).
    fn worktree_removals(&mut self) {
        use tine_core::worktree::{is_within, same_place};

        use crate::ops_registry::{OpState, Outcome};
        for i in 0..self.tabs.len() {
            let Some(req) = self.tabs[i].remove_requested.take() else { continue };
            let name = req.worktree.name();
            let affected: Vec<usize> = (0..self.tabs.len()).filter(|&t| is_within(&self.tabs[t].paths.workdir, &req.worktree.path)).collect();
            let blocked = affected.iter().find_map(|&t| self.tabs[t].removal_block().map(|why| (t, why)));
            if let Some((t, why)) = blocked {
                let whose = match same_place(&self.tabs[t].paths.workdir, &req.worktree.path) {
                    true => String::new(),
                    false => format!(" (in {}, open in a tab)", self.tabs[t].paths.workdir.display()),
                };
                self.tabs[i].error(format!("Can't remove the worktree '{name}': {why}{whose}"));
                continue;
            }
            let worktree = req.worktree.path.clone();
            let Some(op) = self.tabs[i].remove_worktree(req) else { continue };
            for &t in &affected {
                self.tabs[t].sleep_for_removal(worktree.clone());
            }
            let tabs = affected.iter().map(|&t| self.tabs[t].paths.workdir.clone()).collect();
            self.removals.push(Removal { op, worktree, tabs });
        }
        let mut k = 0;
        while k < self.removals.len() {
            let state = crate::ops_registry::state(self.removals[k].op);
            if state.as_ref().is_some_and(OpState::is_active) {
                k += 1;
                continue;
            }
            let r = self.removals.remove(k);
            let removed = state == Some(OpState::Finished(Outcome::Succeeded));
            for workdir in r.tabs {
                let Some(t) = self.tabs.iter().position(|t| t.paths.workdir == workdir && t.asleep()) else { continue };
                if removed {
                    self.close_tab_now(t);
                } else {
                    let paths = self.tabs[t].paths.clone();
                    let (tx, rx) = bounded(1);
                    let ctx = self.ctx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(still_there(&paths));
                        ctx.request_repaint();
                    });
                    self.wake_checks.push(WakeCheck { workdir, rx });
                }
            }
        }
        let mut k = 0;
        while k < self.wake_checks.len() {
            let result = match self.wake_checks[k].rx.try_recv() {
                Ok(r) => r,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    k += 1;
                    continue;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err("the check stopped".into()),
            };
            let c = self.wake_checks.remove(k);
            let Some(t) = self.tabs.iter().position(|t| t.paths.workdir == c.workdir && t.asleep()) else { continue };
            match result {
                Ok(()) => self.tabs[t].wake(),
                Err(why) => {
                    self.notices.push(format!(
                        "Closed the tab of {}: removing the worktree failed part-way, and its repository isn't there as it was ({why})",
                        c.workdir.display()
                    ));
                    self.close_tab_now(t);
                }
            }
        }
    }

    /// The tab is asleep and its repository is being checked before it
    /// wakes (after a failed removal).
    fn waking(&self, workdir: &Path) -> bool {
        self.wake_checks.iter().any(|c| c.workdir == workdir)
    }

    /// Unsaved merge editor changes, by tab.
    fn unsaved_merges(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter(|t| t.merge_dirty())
            .map(|t| format!("{}: {}", t.title, t.merge.as_ref().map(|m| m.path.to_string()).unwrap_or_default()))
            .collect()
    }

    /// The window is to close: not before unsaved merge editor changes
    /// (in any tab) were given up.
    fn on_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quit_confirmed {
            return;
        }
        let unsaved = self.unsaved_merges();
        if !unsaved.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_asked = Some(unsaved);
        }
    }

    /// Asking whether to quit with unsaved changes.
    pub fn quit_asked(&self) -> bool {
        self.quit_asked.is_some()
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        let Some(unsaved) = &self.quit_asked else { return };
        let p = Palette::of(ctx);
        let mut answer = None;
        egui::Modal::new(egui::Id::new("quit-unsaved")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Quit and lose unsaved merge changes?");
            ui.label("The merge editor has changes that aren't saved:");
            for line in unsaved {
                ui.label(RichText::new(line).color(p.text_weak));
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let quit = egui::Button::new(RichText::new("Quit Anyway").color(egui::Color32::WHITE)).fill(p.status_deleted);
                if ui.add(quit).clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        match answer {
            Some(true) => {
                self.quit_asked = None;
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(false) => self.quit_asked = None,
            None => {}
        }
    }

    /// Closes tab `i`, keeping the same repository active when possible.
    pub fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        // Unsaved merge editor changes are asked about first (in that tab).
        if !self.tabs[i].request_close() {
            self.active = i;
            return;
        }
        self.close_tab_now(i);
    }

    fn close_tab_now(&mut self, i: usize) {
        self.tabs.remove(i);
        if i < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Title of the active repository tab.
    pub fn settings(&self) -> crate::settings::Settings {
        self.state.settings
    }

    pub fn set_settings(&mut self, settings: crate::settings::Settings) {
        self.state.settings = settings.validated();
    }

    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    pub fn settings_ui(&mut self) -> &mut ui::settings::SettingsUi {
        &mut self.settings_ui
    }

    pub fn tool_settings(&self) -> &crate::tools::ToolSettings {
        &self.state.tools
    }

    pub fn set_tool_settings(&mut self, tools: crate::tools::ToolSettings) {
        crate::tools::set_current(&self.ctx, tools.clone());
        self.state.tools = tools;
    }

    /// App-level messages (e.g. results for tabs that were closed).
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Why the last repository asked for couldn't be opened.
    pub fn open_error(&self) -> Option<&str> {
        self.open_error.as_deref()
    }

    /// Recent commit messages (oldest first).
    pub fn recent_messages(&self) -> &[String] {
        &self.state.messages
    }

    pub fn active_title(&self) -> Option<&str> {
        self.tabs.get(self.active).map(|t| t.title.as_str())
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut RepoTab> {
        self.tabs.get_mut(self.active)
    }

    /// Every tab (tests reach the inactive ones).
    #[doc(hidden)]
    pub fn tabs_mut(&mut self) -> &mut [RepoTab] {
        &mut self.tabs
    }

    /// Makes tab `i` the active one.
    pub fn activate(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
        }
    }

    /// Decides who owns the keyboard (see [`input::Layer`]); called again
    /// whenever a modal may have come up, so the rest of the frame follows.
    /// A tab dialog that appears closes Quick Launch and the settings: only
    /// one modal shows at a time (a prompt hides the others meanwhile).
    fn decide_layer(&mut self, ctx: &egui::Context) {
        let tab_modal = self.tabs.get(self.active).filter(|t| t.modal_open()).map(RepoTab::modal_token);
        if tab_modal.is_some() || self.quit_asked.is_some() {
            self.settings_open = false;
        }
        if tab_modal.is_some() || self.settings_open || self.quit_asked.is_some() {
            self.quick = None;
        }
        let (layer, token) = if let Some(p) = crate::askpass::pending() {
            (input::Layer::Prompt, p.id)
        } else if self.quit_asked.is_some() {
            (input::Layer::Dialog, QUIT_TOKEN)
        } else if let Some(token) = tab_modal {
            (input::Layer::Dialog, token)
        } else if self.settings_open {
            (input::Layer::Settings, 0)
        } else if self.quick.is_some() {
            (input::Layer::QuickLaunch, 0)
        } else {
            (input::Layer::Views, 0)
        };
        input::set_owner(ctx, layer, token);
    }

    /// Global keys, unless a modal layer has them.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if input::layer(ctx) != input::Layer::Views {
            return;
        }
        if let Some(cmd) = shortcuts::take(ctx, !self.tabs.is_empty()) {
            self.run_command(cmd);
        }
        // Esc closes the topmost overlay: a browse view, else the search
        // bar, else the merge editor.
        if !ctx.text_edit_focused()
            && let Some(t) = self.tabs.get_mut(self.active)
            && (!t.browse.is_empty() || t.search.open || (t.view == View::LocalChanges && t.merge_shown()))
            && input::take_key(ctx, Modifiers::NONE, Key::Escape)
        {
            if !t.browse.is_empty() {
                t.close_browse();
            } else if t.search.open {
                t.close_search();
            } else {
                t.request_close_merge();
            }
        }
    }

    /// Runs a command from a shortcut or Quick Launch.
    pub fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::QuickLaunch => self.open_quick_launch(),
            Command::OpenRepository => self.pick_folder(),
            Command::Settings => self.open_settings(),
            Command::DebugOverlay => self.debug.open = !self.debug.open,
            Command::NextTab | Command::PreviousTab if !self.tabs.is_empty() => {
                let n = self.tabs.len();
                self.active = if cmd == Command::NextTab { (self.active + 1) % n } else { (self.active + n - 1) % n };
            }
            Command::CloseTab if !self.tabs.is_empty() => self.close_tab(self.active),
            _ => {
                let Some(t) = self.tabs.get_mut(self.active) else { return };
                // Switching views leaves File History / Blame.
                if matches!(cmd, Command::SearchCommits | Command::LocalChanges | Command::AllCommits) {
                    t.clear_browse();
                }
                match cmd {
                    Command::SearchCommits => t.open_search(),
                    Command::LocalChanges => t.view = View::LocalChanges,
                    Command::AllCommits => t.view = View::History,
                    Command::Fetch => t.open_fetch_dialog(None),
                    Command::Pull => t.open_pull_dialog(None),
                    Command::Push => t.open_push_dialog(None),
                    Command::Stash => t.open_stash_dialog(),
                    Command::NewBranch => t.open_create_branch("HEAD", "HEAD"),
                    Command::Refresh => t.refresh(),
                    Command::ToggleOutput => t.output_open = !t.output_open,
                    Command::OpenTerminal => crate::tools::run(&self.ctx, &crate::tools::Tool::Terminal, &t.paths.workdir),
                    Command::OpenFileManager => crate::tools::run(&self.ctx, &crate::tools::Tool::FileManager, &t.paths.workdir),
                    _ => {}
                }
            }
        }
    }

    pub fn open_settings(&mut self) {
        self.quick = None;
        if !self.settings_open {
            self.settings_open = true;
            self.settings_ui.opened(&self.ctx, &self.state.tools);
        }
    }

    pub fn open_quick_launch(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.request_files();
        }
        let tabs: Vec<(String, PathBuf)> = self.tabs.iter().map(|t| (t.title.clone(), t.paths.workdir.clone())).collect();
        self.quick = Some(QuickLaunch::new(self.tabs.get(self.active), &tabs, &self.state.recent));
    }

    /// Adds a repository to the recent list (as opening one does).
    pub fn remember_recent(&mut self, path: PathBuf) {
        self.state.recent.retain(|p| p != &path);
        self.state.recent.insert(0, path);
        self.state.recent.truncate(MAX_RECENT);
    }

    /// The open Quick Launch palette (for tests).
    pub fn quick_launch(&mut self) -> Option<&mut QuickLaunch> {
        self.quick.as_mut()
    }

    fn quick_launch_frame(&mut self, ctx: &egui::Context) {
        if input::layer(ctx) != input::Layer::QuickLaunch {
            return;
        }
        let Some(ql) = self.quick.as_mut() else { return };
        // Items describe the repository they were collected from.
        let origin = ql.origin.clone();
        let tab = origin.as_ref().and_then(|o| self.tabs.iter().find(|t| &t.paths.workdir == o));
        ql.set_files(tab.and_then(|t| t.files.clone()));
        match quick_launch::show(ctx, ql, tab) {
            quick_launch::Outcome::Stay => {}
            quick_launch::Outcome::Close => self.quick = None,
            quick_launch::Outcome::Run(action) => {
                self.quick = None;
                self.run_action(action, origin, ctx);
            }
        }
    }

    /// Runs a Quick Launch action; repository actions go to `origin` (made
    /// active first), whichever tab is active now.
    fn run_action(&mut self, action: quick_launch::Action, origin: Option<PathBuf>, ctx: &egui::Context) {
        use quick_launch::Action;
        let tab_of = |tabs: &[RepoTab], path: &Path| tabs.iter().position(|t| t.paths.workdir == path);
        match action {
            Action::SwitchTab(path) => {
                if let Some(i) = tab_of(&self.tabs, &path) {
                    self.active = i;
                }
                return;
            }
            Action::Open(path) => return self.open(&path, ctx),
            Action::Command(c) if !c.needs_repo() => return self.run_command(c),
            _ => {}
        }
        let Some(i) = origin.as_deref().and_then(|o| tab_of(&self.tabs, o)) else {
            self.notices.push("The repository Quick Launch was opened for is no longer open".into());
            return;
        };
        self.active = i;
        let t = &mut self.tabs[i];
        match action {
            Action::Command(c) => self.run_command(c),
            Action::Checkout(b) => t.checkout_branch(&b),
            Action::CheckoutRemote(r) => t.checkout_remote(&r),
            Action::Reveal(id) => t.reveal_commit(id),
            Action::FileHistory(path) => t.file_history_of_local(path),
            Action::Blame(path) => t.blame_of_local(path),
            Action::SwitchTab(_) | Action::Open(_) => {}
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui.ctx());
        ui.horizontal_centered(|ui| {
            if ui.button("Open…").on_hover_text("Open a repository (Ctrl+O)").clicked() {
                self.pick_folder();
            }
            ui.separator();
            if let Some(t) = self.tabs.get_mut(self.active) {
                let remote_buttons = [
                    ("Fetch", "Fetch from remotes", Command::Fetch, t.remote_block(crate::repo::Needs::Idle, false)),
                    ("Pull", "Pull into the current branch", Command::Pull, t.remote_block(crate::repo::Needs::CleanState, true)),
                    ("Push", "Push the current branch", Command::Push, t.remote_block(crate::repo::Needs::Idle, true)),
                ];
                for (label, tip, cmd, block) in remote_buttons {
                    let resp = ui
                        .add_enabled(block.is_none(), egui::Button::new(label))
                        .on_hover_text(with_hint(tip, cmd))
                        .on_disabled_hover_text(block.unwrap_or_default());
                    if resp.clicked() {
                        match label {
                            "Fetch" => t.open_fetch_dialog(None),
                            "Pull" => t.open_pull_dialog(None),
                            _ => t.open_push_dialog(None),
                        }
                    }
                }
            } else {
                for label in ["Fetch", "Pull", "Push"] {
                    ui.add_enabled(false, egui::Button::new(label));
                }
            }
            if let Some(t) = self.tabs.get_mut(self.active) {
                let block = t.block_reason(crate::repo::Needs::CleanState);
                let resp = ui
                    .add_enabled(block.is_none(), egui::Button::new("Stash"))
                    .on_hover_text(with_hint("Stash local changes", Command::Stash))
                    .on_disabled_hover_text(block.unwrap_or_default());
                if resp.clicked() {
                    t.open_stash_dialog();
                }
                let block = t.block_reason(crate::repo::Needs::Idle);
                let resp = ui
                    .add_enabled(block.is_none(), egui::Button::new("Branch"))
                    .on_hover_text(with_hint("Create a branch at HEAD", Command::NewBranch))
                    .on_disabled_hover_text(block.unwrap_or_default());
                if resp.clicked() {
                    t.open_create_branch("HEAD", "HEAD");
                }
            }
            ui.separator();
            let has_tab = !self.tabs.is_empty();
            if ui.add_enabled(has_tab, egui::Button::new("Refresh")).on_hover_text("Reload (F5)").clicked()
                && let Some(t) = self.tabs.get_mut(self.active)
            {
                t.refresh();
            }

            if let Some(t) = self.tabs.get_mut(self.active) {
                let label = if t.output_open { "Hide Output" } else { "Output" };
                if ui.button(label).on_hover_text("Git commands run by Tine and their output").clicked() {
                    t.output_open = !t.output_open;
                }
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
                    ui.separator();
                    if ui.button(with_hint("Settings…", Command::Settings)).clicked() {
                        self.open_settings();
                        ui.close();
                    }
                });
                if let Some(t) = self.tabs.get(self.active)
                    && let Some((id, text)) = ui::output::running_summary(t)
                {
                    if ui.small_button("Cancel").clicked() {
                        crate::ops_registry::cancel(id);
                    }
                    ui.label(RichText::new(text).small().color(p.text_weak));
                    ui.spinner();
                } else if let Some(t) = self.tabs.get(self.active) {
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
                    for tool in [crate::tools::Tool::Terminal, crate::tools::Tool::FileManager] {
                        if ui.button(tool.label()).clicked() {
                            crate::tools::run(ui.ctx(), &tool, &t.paths.workdir);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Close Tab").clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            // Repositories still being opened (in the background).
            for o in &self.opening {
                let name = o.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui.add_enabled(false, egui::Button::new(format!("Opening {name}…")))
                    .on_disabled_hover_text(o.path.display().to_string());
                ui.spinner();
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

/// A tooltip with the command's shortcut, e.g. "Fetch from remotes (Ctrl+Shift+F)".
fn with_hint(text: &str, cmd: Command) -> String {
    match shortcuts::hint(cmd) {
        Some(keys) => format!("{text} ({keys})"),
        None => text.to_owned(),
    }
}

fn error_banner(ui: &mut egui::Ui, tab: &mut RepoTab) {
    if tab.errors.is_empty() {
        return;
    }
    let p = Palette::of(ui.ctx());
    let mut dismiss = false;
    let mut details = false;
    egui::Frame::NONE
        .fill(p.diff_del_bg)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(tab.errors.last().expect("non-empty")).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dismiss = ui.small_button("×").clicked();
                    if tab.last_failed_op.is_some() {
                        details = ui.small_button("Details").on_hover_text("Show the git output").clicked();
                    }
                });
            });
        });
    if details {
        tab.output_open = true;
        tab.output_focus = tab.last_failed_op;
    }
    if dismiss {
        tab.errors.clear();
        tab.last_failed_op = None;
    }
}

/// The input owner token of the quit question.
const QUIT_TOKEN: u64 = u64::MAX;

impl eframe::App for TineApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = Palette::of(&ctx);
        input::begin_frame(&ctx);
        self.poll_opening(&ctx);
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
        crate::settings::set_current(&ctx, self.state.settings);
        crate::tools::set_current(&ctx, self.state.tools.clone());
        self.settings_ui.identity.poll();
        let mut requests = Vec::new();
        for t in &mut self.tabs {
            t.set_doc_options(self.state.settings.doc_options());
            t.poll();
            requests.extend(t.open_request.take());
            for m in t.committed_messages.drain(..) {
                self.state.messages.retain(|x| x != &m);
                self.state.messages.push(m);
            }
        }
        self.on_close_request(&ctx);
        self.worktree_removals();
        // Operations whose tab was closed: the work tree that started them
        // takes them over (its stash conflict, stashed changes…); other
        // worktrees of the repository just refresh. Without a tab for that
        // work tree, keep them until one is opened.
        for o in crate::worker::take_orphaned_results() {
            self.deliver_orphan(o);
        }
        // How external programs ended, for the tab they were started from
        // (a notice if it was closed).
        for r in crate::tools::take_results() {
            let tab = self.tabs.iter_mut().find(|t| t.paths.workdir == r.origin);
            match (r.outcome, tab) {
                (Err(e), Some(t)) => t.error(e),
                (Err(e), None) => self.notices.push(format!("{e} ({})", r.origin.display())),
                (Ok(Some(note)), Some(t)) => t.activity.push(note),
                (Ok(Some(note)), None) => self.notices.push(format!("{note} ({})", r.origin.display())),
                (Ok(None), _) => {}
            }
        }
        let excess = self.state.messages.len().saturating_sub(MAX_MESSAGES);
        self.state.messages.drain(..excess);
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
        // After the results of this frame (one may have brought up a
        // dialog), and again after the shortcuts (one may have opened one).
        self.decide_layer(&ctx);
        self.shortcuts(&ctx);
        self.decide_layer(&ctx);

        let title = match self.tabs.get(self.active) {
            Some(t) => format!("{} — Tine", t.title),
            None => "Tine".into(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::Panel::top("toolbar")
            .exact_size(38.0)
            .frame(egui::Frame::NONE.fill(p.toolbar_bg).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ui, |ui| self.toolbar(ui));
        if !self.notices.is_empty() {
            let mut dismiss = false;
            egui::Panel::top("notices")
                .frame(egui::Frame::NONE.fill(p.diff_hunk_bg).inner_margin(egui::Margin::symmetric(10, 6)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(self.notices.last().expect("non-empty"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            dismiss = ui.small_button("×").clicked();
                        });
                    });
                });
            if dismiss {
                self.notices.clear();
            }
        }
        if !self.tabs.is_empty() || !self.opening.is_empty() {
            egui::Panel::top("tabs")
                .frame(egui::Frame::NONE.fill(p.toolbar_bg).inner_margin(egui::Margin::symmetric(8, 3)))
                .show(ui, |ui| self.tab_strip(ui));
        }

        if self.tabs.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui));
            if !ui::askpass::show(&ctx) {
                ui::settings::show(&ctx, &mut self.state.settings, &mut self.state.tools, &mut self.settings_ui, &mut self.settings_open);
                self.quick_launch_frame(&ctx);
            }
            return;
        }
        let waking = self.tabs.get(self.active).is_some_and(|t| self.waking(&t.paths.workdir));
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let asleep = tab.asleep();

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(240.0)
            .size_range(160.0..=480.0)
            .frame(egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(6, 0)))
            .show(ui, |ui| {
                ui.add_enabled_ui(!asleep, |ui| ui::sidebar::show(ui, tab));
            });

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg)).show(ui, |ui| {
            error_banner(ui, tab);
            if asleep {
                // Nothing to do here until the removal is over.
                let text = match waking {
                    true => "Removing this worktree failed; checking that it is all still there…",
                    false => "This worktree is being removed…",
                };
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(text).color(p.text_weak));
                    });
                });
                return;
            }
            ui::banner::show(ui, tab);
            if tab.output_open {
                egui::Panel::bottom("output")
                    .resizable(true)
                    .default_size(220.0)
                    .frame(egui::Frame::NONE.fill(p.sidebar_bg).inner_margin(egui::Margin::symmetric(8, 4)))
                    .show(ui, |ui| ui::output::show(ui, tab));
            }
            if !tab.browse.is_empty() {
                ui::browse::show(ui, tab);
                return;
            }
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
                View::LocalChanges => ui::changes::show(ui, tab, &self.state.messages),
            }
        });
        // Credential prompts belong to operations, not tabs, and come first:
        // a tab's dialog waits until no prompt is open (egui stacks modals
        // by when they first appeared, so it could otherwise cover one).
        if !ui::askpass::show(&ctx) {
            ui::dialogs::show(&ctx, tab);
            // Never two modals at once (decide_layer closes the settings
            // when a dialog comes up).
            if !tab.modal_open() {
                ui::settings::show(&ctx, &mut self.state.settings, &mut self.state.tools, &mut self.settings_ui, &mut self.settings_open);
            }
            self.quick_launch_frame(&ctx);
        }
        // A tab whose unsaved changes were given up so it could close.
        if let Some(i) = self.tabs.iter().position(|t| t.close_requested) {
            self.close_tab_now(i);
        }
        self.quit_dialog(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.state.open = self.tabs.iter().map(|t| t.paths.workdir.clone()).collect();
        self.state.active = self.active;
        eframe::set_value(storage, STORAGE_KEY, &self.state);
    }
}
