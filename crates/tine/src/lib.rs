pub mod app;
pub mod askpass;
pub mod blame_model;
pub mod diff_model;
pub mod fonts;
/// Fuzzy matching lives in the core (the bench measures it too).
pub use tine_core::fuzzy;
pub mod input;
pub mod ops_registry;
pub mod probe;
pub mod repo;
pub mod settings;
pub mod shortcuts;
pub mod theme;
pub mod tools;
pub mod ui;
pub mod watcher;
pub mod worker;

/// Applies fonts and themes to a fresh egui context.
pub fn setup_context(ctx: &egui::Context) {
    ctx.set_fonts(fonts::definitions());
    theme::install(ctx);
}
