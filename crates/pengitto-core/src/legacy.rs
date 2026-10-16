//! The names of Tine, as pengitto was called before, kept only to take over
//! what it left behind: its settings and a rebase it stopped. (The install
//! scripts and the RPM spec have their own, marked `legacy-names`.)

/// Its name, as shown.
pub const NAME: &str = "Tine";
/// Its app id: the folder of its settings (`$XDG_DATA_HOME/tine`).
pub const APP: &str = "tine";
/// The key of its state in eframe's storage (`app.ron`).
pub const STORAGE_KEY: &str = "tine-state";
/// Its interactive rebase session (in the worktree's git dir).
pub const REBASE_DIR: &str = "tine-rebase";
/// Its mark in `rebase-merge` that the rebase there is its session's.
pub const REBASE_MARK: &str = "tine-session";
