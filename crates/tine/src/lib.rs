pub mod app;
pub mod diff_model;
pub mod fonts;
pub mod ops_registry;
pub mod probe;
pub mod repo;
pub mod theme;
pub mod ui;
pub mod watcher;
pub mod worker;

/// Applies fonts and themes to a fresh egui context.
pub fn setup_context(ctx: &egui::Context) {
    ctx.set_fonts(fonts::definitions());
    theme::install(ctx);
}
