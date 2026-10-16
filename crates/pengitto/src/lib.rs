pub mod app;
pub mod appimage;
pub mod askpass;
pub mod blame_model;
pub mod cli;
pub mod diff_model;
pub mod fonts;
/// Fuzzy matching lives in the core (the bench measures it too).
pub use pengitto_core::fuzzy;
pub mod input;
pub mod merge_model;
pub mod ops_registry;
pub mod probe;
pub mod repo;
pub mod selfcheck;
pub mod settings;
pub mod shortcuts;
pub mod state_file;
pub mod theme;
pub mod tools;
pub mod ui;
pub mod watcher;
pub mod worker;

/// pengitto's icon (256 px), for the window.
pub const WINDOW_ICON: &[u8] = include_bytes!("../../../packaging/icons/hicolor/256x256/apps/pengitto.png");

/// Applies fonts and themes to a fresh egui context.
pub fn setup_context(ctx: &egui::Context) {
    ctx.set_fonts(fonts::definitions());
    theme::install(ctx);
}
