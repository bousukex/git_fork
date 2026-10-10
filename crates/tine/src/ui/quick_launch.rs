//! Quick Launch (Ctrl+P): a palette that finds commands, branches, tags and
//! repositories by fuzzy matching and runs the chosen one.

use std::path::PathBuf;

use egui::{Align2, FontId, Key, Modifiers, RichText, text::LayoutJob};
use tine_core::model::{GitRef, Oid, RefKind};

use crate::{
    fuzzy::{self, Prepared},
    input,
    repo::{Needs, RepoTab},
    shortcuts::{self, Command},
    theme::Palette,
};

/// What choosing an item does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Command(Command),
    Checkout(String),
    CheckoutRemote(GitRef),
    /// Show a tag's commit in the history.
    Reveal(Oid),
    /// Switch to the open repository with this work tree.
    SwitchTab(PathBuf),
    Open(PathBuf),
    /// File History of a tracked file (from HEAD).
    FileHistory(bstr::BString),
    /// Blame of a tracked file (the work tree's version).
    Blame(bstr::BString),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Command,
    Branch,
    RemoteBranch,
    Tag,
    Repository,
    File,
}

impl ItemKind {
    fn tag(self) -> &'static str {
        match self {
            ItemKind::Command => "Command",
            ItemKind::Branch => "Branch",
            ItemKind::RemoteBranch => "Remote",
            ItemKind::Tag => "Tag",
            ItemKind::Repository => "Repository",
            ItemKind::File => "File",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    pub label: String,
    /// Shown dimmed after the label (shortcut, path, upstream…).
    pub detail: String,
    pub action: Action,
}

/// Commands offered in Quick Launch (in this order when nothing is typed).
const COMMANDS: &[Command] = &[
    Command::SearchCommits,
    Command::Fetch,
    Command::Pull,
    Command::Push,
    Command::Stash,
    Command::NewBranch,
    Command::LocalChanges,
    Command::AllCommits,
    Command::Refresh,
    Command::ToggleOutput,
    Command::OpenRepository,
    Command::CloseTab,
];

const MAX_SHOWN: usize = 50;

/// A result: one of the fixed items, or a tracked file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Item(usize),
    File(usize),
}

/// A listed result, for tests.
pub struct Listed {
    pub kind: ItemKind,
    pub label: String,
}

pub struct QuickLaunch {
    /// Work tree of the repository the items were collected from; actions
    /// on branches, tags and repository commands run there, even if another
    /// tab became active meanwhile.
    pub origin: Option<PathBuf>,
    pub query: String,
    selected: usize,
    items: Vec<Item>,
    prepared: Vec<Prepared>,
    /// Tracked files of the origin repository (arrive after opening).
    files: Option<std::sync::Arc<crate::worker::FileIndex>>,
    results: Vec<(Hit, fuzzy::Match)>,
    /// Bumped whenever `files` is replaced: file hits index into one list.
    files_gen: u64,
    /// Query and file list generation the results are for.
    shown_query: Option<(String, u64)>,
    focus: bool,
}

impl QuickLaunch {
    /// Collects the items: commands, then the active repository's branches
    /// and tags, then open and recent repositories.
    pub fn new(tab: Option<&RepoTab>, tabs: &[(String, PathBuf)], recent: &[PathBuf]) -> Self {
        let mut items: Vec<Item> = COMMANDS
            .iter()
            .filter(|c| tab.is_some() || !c.needs_repo())
            .map(|&c| Item {
                kind: ItemKind::Command,
                label: c.label().trim_end_matches('…').to_owned(),
                detail: shortcuts::hint(c).unwrap_or_default(),
                action: Action::Command(c),
            })
            .collect();
        if let Some(t) = tab {
            for r in &t.refs.refs {
                let item = match r.kind {
                    RefKind::LocalBranch if !r.is_head => Item {
                        kind: ItemKind::Branch,
                        label: r.short_name.clone(),
                        detail: r.upstream.clone().unwrap_or_default(),
                        action: Action::Checkout(r.short_name.clone()),
                    },
                    RefKind::RemoteBranch => Item {
                        kind: ItemKind::RemoteBranch,
                        label: r.short_name.clone(),
                        detail: String::new(),
                        action: Action::CheckoutRemote(r.clone()),
                    },
                    RefKind::Tag => Item {
                        kind: ItemKind::Tag,
                        label: r.short_name.clone(),
                        detail: r.target.short(),
                        action: Action::Reveal(r.target),
                    },
                    _ => continue,
                };
                items.push(item);
            }
        }
        for (title, path) in tabs {
            items.push(Item {
                kind: ItemKind::Repository,
                label: title.clone(),
                detail: path.display().to_string(),
                action: Action::SwitchTab(path.clone()),
            });
        }
        for path in recent.iter().filter(|p| !tabs.iter().any(|(_, t)| t == *p)) {
            let label = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            items.push(Item {
                kind: ItemKind::Repository,
                label,
                detail: path.display().to_string(),
                action: Action::Open(path.clone()),
            });
        }
        let prepared = items.iter().map(|it| Prepared::new(&it.label)).collect();
        QuickLaunch {
            origin: tab.map(|t| t.paths.workdir.clone()),
            query: String::new(),
            selected: 0,
            items,
            prepared,
            files: tab.and_then(|t| t.files.clone()),
            files_gen: 0,
            results: Vec::new(),
            shown_query: None,
            focus: true,
        }
    }

    /// Files arrived (or were listed again).
    pub fn set_files(&mut self, files: Option<std::sync::Arc<crate::worker::FileIndex>>) {
        let same = match (&self.files, &files) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.files = files;
            // Hits of the old list mean other files now (or none at all).
            self.files_gen += 1;
            self.results.clear();
            self.shown_query = None;
        }
    }

    fn refresh_results(&mut self) {
        let key = (self.query.clone(), self.files_gen);
        if self.shown_query.as_ref() == Some(&key) {
            return;
        }
        self.results = if self.query.trim().is_empty() {
            // Commands first, then everything else, in the order collected.
            (0..self.items.len().min(MAX_SHOWN))
                .map(|i| (Hit::Item(i), fuzzy::Match { score: 0, positions: Vec::new() }))
                .collect()
        } else {
            let mut hits: Vec<(Hit, fuzzy::Match)> = fuzzy::top_prepared(&self.query, &self.prepared, MAX_SHOWN)
                .into_iter()
                .map(|(i, m)| (Hit::Item(i), m))
                .collect();
            if let Some(files) = &self.files {
                hits.extend(fuzzy::top_prepared(&self.query, &files.prepared, MAX_SHOWN).into_iter().map(|(i, m)| (Hit::File(i), m)));
            }
            // Best first; on a tie, commands and refs before files.
            hits.sort_by_key(|(h, m)| (std::cmp::Reverse(m.score), matches!(h, Hit::File(_))));
            hits.truncate(MAX_SHOWN);
            hits
        };
        self.selected = 0;
        self.shown_query = Some(key);
    }

    fn item(&self, hit: Hit) -> Option<Item> {
        match hit {
            Hit::Item(i) => self.items.get(i).cloned(),
            Hit::File(i) => {
                let path = self.files.as_ref()?.paths.get(i)?.clone();
                Some(Item {
                    kind: ItemKind::File,
                    label: path.to_string(),
                    detail: "Enter: history · Shift+Enter: blame".into(),
                    action: Action::FileHistory(path),
                })
            }
        }
    }

    /// Results currently listed (for tests).
    pub fn listed(&mut self) -> Vec<Listed> {
        self.refresh_results();
        self.results.iter().filter_map(|(h, _)| self.item(*h)).map(|i| Listed { kind: i.kind, label: i.label }).collect()
    }
}

/// Why an item can't run now, if it can't.
pub fn block_reason(item: &Item, tab: Option<&RepoTab>) -> Option<&'static str> {
    let Some(tab) = tab else {
        return match &item.action {
            Action::Command(c) if c.needs_repo() => Some("Open a repository first"),
            Action::FileHistory(_) | Action::Blame(_) => Some("Open a repository first"),
            _ => None,
        };
    };
    match &item.action {
        Action::FileHistory(path) | Action::Blame(path) => tab.path_browse_block(path),
        Action::Command(Command::Fetch) => tab.remote_block(Needs::Idle, false),
        Action::Command(Command::Pull) => tab.remote_block(Needs::CleanState, true),
        Action::Command(Command::Push) => tab.remote_block(Needs::Idle, true),
        Action::Command(Command::Stash) => tab.block_reason(Needs::CleanState),
        Action::Command(Command::NewBranch) => tab.block_reason(Needs::Idle),
        Action::Checkout(_) | Action::CheckoutRemote(_) => tab.block_reason(Needs::CleanState),
        _ => None,
    }
}

/// Text with the matched characters in bold.
fn highlighted(text: &str, positions: &[usize], color: egui::Color32, strong: egui::Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = egui::TextFormat { font_id: FontId::proportional(13.5), color, ..Default::default() };
    let bold = egui::TextFormat {
        font_id: FontId::new(13.5, egui::FontFamily::Name(crate::fonts::BOLD.into())),
        color: strong,
        ..Default::default()
    };
    for (i, ch) in text.chars().enumerate() {
        let fmt = if positions.binary_search(&i).is_ok() { bold.clone() } else { normal.clone() };
        job.append(&ch.to_string(), 0.0, fmt);
    }
    job
}

/// What happened in the palette this frame.
pub enum Outcome {
    Stay,
    Close,
    Run(Action),
}

/// Shows the palette. Keys: ↑/↓ to choose, Enter to run, Esc to close.
pub fn show(ctx: &egui::Context, ql: &mut QuickLaunch, tab: Option<&RepoTab>) -> Outcome {
    let p = Palette::of(ctx);
    // Keys are taken before the text field sees them (Enter would end
    // editing), but applied after it: text typed in the same frame as Enter
    // must pick from the results for the new text.
    let down = input::take_key(ctx, Modifiers::NONE, Key::ArrowDown);
    let up = input::take_key(ctx, Modifiers::NONE, Key::ArrowUp);
    let enter = input::take_key(ctx, Modifiers::NONE, Key::Enter);
    let shift_enter = input::take_key(ctx, Modifiers::SHIFT, Key::Enter);
    let escape = input::take_key(ctx, Modifiers::NONE, Key::Escape);
    let mut clicked = None;

    let area = egui::Modal::default_area(egui::Id::new("quick-launch-area")).anchor(Align2::CENTER_TOP, [0.0, 60.0]);
    let modal = egui::Modal::new(egui::Id::new("quick-launch")).area(area).show(ctx, |ui| {
        ui.set_width(560.0);
        let resp = ui.add(
            egui::TextEdit::singleline(&mut ql.query)
                .hint_text("Type a command, branch, tag or repository")
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut ql.focus) || !resp.has_focus() {
            resp.request_focus();
        }
        ql.refresh_results();
        let n = ql.results.len();
        if down && n > 0 {
            ql.selected = (ql.selected + 1) % n;
        }
        if up && n > 0 {
            ql.selected = (ql.selected + n - 1) % n;
        }
        ui.add_space(4.0);
        if ql.results.is_empty() {
            ui.label(RichText::new("Nothing matches").color(p.text_weak));
        }
        egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show(ui, |ui| {
            for (row, (hit, m)) in ql.results.iter().enumerate() {
                let Some(item) = &ql.item(*hit) else { continue };
                let block = block_reason(item, tab);
                let selected = row == ql.selected;
                let (text_color, strong) = if block.is_some() { (p.text_weak, p.text_weak) } else { (p.text, p.accent) };
                let frame = egui::Frame::NONE
                    .fill(if selected { p.selection_bg } else { egui::Color32::TRANSPARENT })
                    .inner_margin(egui::Margin::symmetric(6, 3));
                let resp = frame
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.add_sized([74.0, 18.0], egui::Label::new(RichText::new(item.kind.tag()).small().color(p.text_weak)));
                            ui.label(highlighted(&item.label, &m.positions, text_color, strong));
                            let detail = block.map(str::to_owned).unwrap_or_else(|| item.detail.clone());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add(egui::Label::new(RichText::new(detail).small().color(p.text_weak)).truncate());
                            });
                        });
                    })
                    .response
                    .interact(egui::Sense::click());
                let label = format!("{} {}", item.kind.tag(), item.label);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, block.is_none(), &label));
                if selected {
                    resp.scroll_to_me(None);
                }
                if resp.clicked() {
                    clicked = Some(row);
                }
            }
        });
    });

    let chosen = clicked.or((enter || shift_enter).then_some(ql.selected));
    if let Some(row) = chosen
        && let Some((hit, _)) = ql.results.get(row)
    {
        let Some(item) = ql.item(*hit) else { return Outcome::Stay };
        if block_reason(&item, tab).is_none() {
            return Outcome::Run(match item.action {
                // Shift+Enter on a file: blame instead of history.
                Action::FileHistory(path) if shift_enter => Action::Blame(path),
                action => action,
            });
        }
        ql.selected = row;
    }
    if escape || modal.should_close() { Outcome::Close } else { Outcome::Stay }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::worker::FileIndex;

    fn index(paths: &[&str]) -> Arc<FileIndex> {
        Arc::new(FileIndex {
            paths: paths.iter().map(|p| bstr::BString::from(*p)).collect(),
            prepared: paths.iter().map(|p| Prepared::new(p)).collect(),
        })
    }

    #[test]
    fn a_new_file_list_replaces_the_old_hits() {
        let mut ql = QuickLaunch::new(None, &[], &[]);
        ql.set_files(Some(index(&["src/zeta.rs", "src/alpha.rs", "src/beta.rs"])));
        ql.query = "src".into();
        assert_eq!(ql.listed().len(), 3);
        // Listed again while the query is unchanged: shorter and reordered.
        ql.set_files(Some(index(&["src/beta.rs"])));
        let labels: Vec<String> = ql.listed().into_iter().map(|l| l.label).collect();
        assert_eq!(labels, ["src/beta.rs"], "no stale index into the old list");
        ql.set_files(Some(index(&["lib/other.rs", "src/gamma.rs"])));
        let labels: Vec<String> = ql.listed().into_iter().map(|l| l.label).collect();
        assert_eq!(labels, ["src/gamma.rs"]);
    }
}
