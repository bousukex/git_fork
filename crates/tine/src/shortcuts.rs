//! Keyboard shortcuts: one table that drives the key handling, the hints in
//! Quick Launch and the list in the settings.

use egui::{Key, Modifiers};

use crate::input;

/// Something a shortcut (or Quick Launch) can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    QuickLaunch,
    SearchCommits,
    LocalChanges,
    AllCommits,
    NextTab,
    PreviousTab,
    Fetch,
    Pull,
    Push,
    Stash,
    NewBranch,
    OpenRepository,
    CloseTab,
    Refresh,
    ToggleOutput,
    OpenTerminal,
    OpenFileManager,
    Settings,
    DebugOverlay,
}

impl Command {
    pub fn label(self) -> &'static str {
        match self {
            Command::QuickLaunch => "Quick Launch",
            Command::SearchCommits => "Search Commits",
            Command::LocalChanges => "Local Changes",
            Command::AllCommits => "All Commits",
            Command::NextTab => "Next Repository Tab",
            Command::PreviousTab => "Previous Repository Tab",
            Command::Fetch => "Fetch…",
            Command::Pull => "Pull…",
            Command::Push => "Push…",
            Command::Stash => "Stash Changes…",
            Command::NewBranch => "Create Branch…",
            Command::OpenRepository => "Open Repository…",
            Command::CloseTab => "Close Repository Tab",
            Command::Refresh => "Refresh",
            Command::ToggleOutput => "Show/Hide Output",
            Command::OpenTerminal => "Open in Terminal",
            Command::OpenFileManager => "Open in File Manager",
            Command::Settings => "Settings…",
            Command::DebugOverlay => "Debug Overlay",
        }
    }

    /// Needs an open repository.
    pub fn needs_repo(self) -> bool {
        !matches!(
            self,
            Command::QuickLaunch
                | Command::OpenRepository
                | Command::Settings
                | Command::DebugOverlay
                | Command::NextTab
                | Command::PreviousTab
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub command: Command,
    pub modifiers: Modifiers,
    pub key: Key,
    /// Also works while a text field has focus (otherwise typing wins).
    pub while_typing: bool,
}

const NONE: Modifiers = Modifiers::NONE;
const CTRL: Modifiers = Modifiers::COMMAND;
const CTRL_SHIFT: Modifiers = Modifiers { alt: false, ctrl: false, shift: true, mac_cmd: false, command: true };

const fn sc(command: Command, modifiers: Modifiers, key: Key, while_typing: bool) -> Shortcut {
    Shortcut { command, modifiers, key, while_typing }
}

/// Every global shortcut. View-local keys (list Space/Enter/Delete, search
/// Enter/F3, Esc) are handled where they apply.
pub const SHORTCUTS: &[Shortcut] = &[
    sc(Command::QuickLaunch, CTRL, Key::P, true),
    sc(Command::SearchCommits, CTRL, Key::F, false),
    sc(Command::LocalChanges, CTRL, Key::Num1, false),
    sc(Command::AllCommits, CTRL, Key::Num2, false),
    sc(Command::NextTab, CTRL, Key::Tab, true),
    sc(Command::PreviousTab, CTRL_SHIFT, Key::Tab, true),
    sc(Command::Fetch, CTRL_SHIFT, Key::F, false),
    sc(Command::Pull, CTRL_SHIFT, Key::L, false),
    sc(Command::Push, CTRL_SHIFT, Key::P, false),
    sc(Command::Stash, CTRL_SHIFT, Key::S, false),
    sc(Command::NewBranch, CTRL_SHIFT, Key::B, false),
    sc(Command::OpenRepository, CTRL, Key::O, false),
    sc(Command::CloseTab, CTRL, Key::W, false),
    sc(Command::Refresh, NONE, Key::F5, false),
    sc(Command::Refresh, CTRL, Key::R, false),
    sc(Command::Settings, CTRL, Key::Comma, false),
    sc(Command::DebugOverlay, NONE, Key::F12, false),
];

/// Where a view-local key works (handled by that view, only while it owns
/// the keyboard; see [`crate::input::Layer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The focused file list of Local Changes.
    FileList,
    /// The focused search field (F3 also elsewhere in the history).
    SearchBar,
    /// The commit subject or description.
    CommitBox,
    /// The topmost overlay (search bar).
    Overlay,
    /// The merge editor.
    MergeEditor,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::FileList => "File list",
            Scope::SearchBar => "Search bar",
            Scope::CommitBox => "Commit message",
            Scope::Overlay => "Search, File History, Blame, Merge Editor",
            Scope::MergeEditor => "Merge editor",
        }
    }
}

/// View-local keys, listed for the settings and checked against the global
/// table: a global shortcut must never take one of these.
pub const LOCAL_KEYS: &[(Scope, Modifiers, Key, &str)] = &[
    (Scope::FileList, NONE, Key::Space, "Stage / unstage the selection"),
    (Scope::FileList, NONE, Key::Enter, "Stage / unstage the selection"),
    (Scope::FileList, NONE, Key::Delete, "Discard the selection (asks first)"),
    (Scope::SearchBar, NONE, Key::Enter, "Next match"),
    (Scope::SearchBar, Modifiers::SHIFT, Key::Enter, "Previous match"),
    (Scope::SearchBar, NONE, Key::F3, "Next match"),
    (Scope::SearchBar, Modifiers::SHIFT, Key::F3, "Previous match"),
    (Scope::CommitBox, CTRL, Key::Enter, "Commit"),
    (Scope::Overlay, NONE, Key::Escape, "Close"),
    (Scope::MergeEditor, NONE, Key::F7, "Next conflict"),
    (Scope::MergeEditor, Modifiers::SHIFT, Key::F7, "Previous conflict"),
];

/// Human-readable keys, e.g. "Ctrl+Shift+P".
pub fn key_text(s: &Shortcut) -> String {
    keys_text(s.modifiers, s.key)
}

pub fn keys_text(modifiers: Modifiers, key: Key) -> String {
    let mut parts = Vec::new();
    if modifiers.command || modifiers.ctrl {
        parts.push("Ctrl".to_owned());
    }
    if modifiers.alt {
        parts.push("Alt".to_owned());
    }
    if modifiers.shift {
        parts.push("Shift".to_owned());
    }
    parts.push(key.symbol_or_name().to_owned());
    parts.join("+")
}

/// The first key of `command`, for hints.
pub fn hint(command: Command) -> Option<String> {
    SHORTCUTS.iter().find(|s| s.command == command).map(key_text)
}

/// Takes the first global shortcut pressed this frame, if any. `has_repo`:
/// a repository tab is active. Nothing fires while an IME composes, and
/// only `while_typing` shortcuts fire while a text field has focus.
pub fn take(ctx: &egui::Context, has_repo: bool) -> Option<Command> {
    if input::keys_blocked(ctx) {
        return None;
    }
    let typing = ctx.text_edit_focused();
    SHORTCUTS
        .iter()
        .filter(|s| (has_repo || !s.command.needs_repo()) && (!typing || s.while_typing))
        .find(|s| input::take_key(ctx, s.modifiers, s.key))
        .map(|s| s.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_shortcuts_share_keys() {
        for (i, a) in SHORTCUTS.iter().enumerate() {
            for b in &SHORTCUTS[i + 1..] {
                assert!(
                    !(a.key == b.key && input::same_modifiers(a.modifiers, b.modifiers)),
                    "{} and {} both use {}",
                    a.command.label(),
                    b.command.label(),
                    key_text(a)
                );
            }
        }
    }

    #[test]
    fn global_shortcuts_leave_local_keys_alone() {
        // Global shortcuts are taken before the views see keys, so none may
        // equal a local key. (Matching is exact, so Ctrl+Shift+X never
        // counts as Ctrl+X and order doesn't matter.)
        for s in SHORTCUTS {
            for (scope, m, k, _) in LOCAL_KEYS {
                assert!(
                    !(s.key == *k && input::same_modifiers(s.modifiers, *m)),
                    "{} takes {:?}'s {}",
                    s.command.label(),
                    scope,
                    key_text(s)
                );
            }
        }
        // Within a scope, each key does one thing.
        for (i, a) in LOCAL_KEYS.iter().enumerate() {
            for b in &LOCAL_KEYS[i + 1..] {
                let same = a.0 == b.0 && a.2 == b.2 && input::same_modifiers(a.1, b.1);
                assert!(!same || a.3 == b.3, "{:?} {:?} twice", a.0, a.2);
            }
        }
    }

    #[test]
    fn hints_read_naturally() {
        assert_eq!(hint(Command::Push).as_deref(), Some("Ctrl+Shift+P"));
        assert_eq!(hint(Command::Refresh).as_deref(), Some("F5"));
        assert_eq!(hint(Command::LocalChanges).as_deref(), Some("Ctrl+1"));
    }
}
