//! The settings dialog and how settings reach the views (M4c).

mod common;

use std::collections::HashMap;

use common::{sample_repo, settle};
use egui::{Event, Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable as _};
use tine::{app::TineApp, settings::Settings};

/// eframe's storage, in memory.
#[derive(Default, Clone)]
struct MemStorage(HashMap<String, String>);

impl eframe::Storage for MemStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn app(paths: Vec<std::path::PathBuf>, storage: Option<MemStorage>) -> Harness<'static, TineApp> {
    Harness::builder()
        .with_size([1200.0, 760.0])
        .build_eframe(move |cc| TineApp::with_storage(cc, paths, storage.as_ref().map(|s| s as &dyn eframe::Storage)))
}

fn stored(state: &str) -> MemStorage {
    MemStorage([("tine-state".to_owned(), state.to_owned())].into())
}

#[test]
fn ctrl_comma_opens_settings_which_own_the_keyboard() {
    let mut h = app(Vec::new(), None);
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    h.run_steps(4);
    assert!(h.state().settings_open(), "Ctrl+, opens the settings");
    // The shortcut list comes from the table.
    assert!(h.query_by_label("Quick Launch").is_some() && h.query_by_label("Ctrl+P").is_some());
    assert!(h.query_by_label_contains("Stage / unstage the selection (File list)").is_some());
    // Global shortcuts wait while the dialog is open.
    h.key_press_modifiers(Modifiers::COMMAND, Key::P);
    h.run_steps(2);
    assert!(h.state_mut().quick_launch().is_none());

    // Theme and zoom apply at once (egui keeps them).
    h.get_by_label("Dark").click();
    h.run_steps(2);
    assert_eq!(h.ctx.options(|o| o.theme_preference), egui::ThemePreference::Dark);
    h.get_by_label("125%").click();
    h.run_steps(2);
    assert!((h.ctx.zoom_factor() - 1.25).abs() < 0.01);
    h.get_by_label("100%").click();
    h.run_steps(2);

    h.key_press(Key::Escape);
    h.run_steps(2);
    assert!(!h.state().settings_open(), "Esc closes it");

    // Also from the ☰ menu.
    h.get_by_label("☰").click();
    h.run_steps(2);
    h.get_by_label("Settings… (Ctrl+,)").click();
    h.run_steps(4);
    assert!(h.state().settings_open());
    h.get_by_label("Close").click();
    h.run_steps(2);
    assert!(!h.state().settings_open());
}

#[test]
fn turning_highlighting_off_redraws_the_shown_diff_plain() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()], None);
    settle(&mut h);
    // "Add main.rs" (v0.1.0) changes only main.rs.
    let out = std::process::Command::new("git").current_dir(repo.path()).args(["rev-parse", "v0.1.0^{commit}"]).output().unwrap();
    let id = tine_core::model::Oid::from_hex(String::from_utf8(out.stdout).unwrap().trim().as_bytes()).unwrap();
    h.state_mut().active_tab_mut().unwrap().select_commit(id);
    let colored = |h: &mut Harness<'static, TineApp>| match &h.state_mut().active_tab_mut().unwrap().commit_diff.doc {
        Some(Ok(d)) => Some(d.rows.iter().any(|r| !r.spans.is_empty())),
        _ => None,
    };
    settle(&mut h);
    assert_eq!(colored(&mut h), Some(true), "Rust is colored by default");

    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    // A new window takes a few frames to measure itself before it takes
    // clicks (egui's sizing pass); people are slower than that.
    h.run_steps(4);
    assert!(h.state().settings_open());
    h.get_by_label("Highlight diffs and blame").click();
    settle(&mut h);
    assert!(!h.state().settings().syntax_highlighting);
    assert_eq!(colored(&mut h), Some(false), "the diff was prepared again, plain");

    h.get_by_label("Highlight diffs and blame").click();
    settle(&mut h);
    assert_eq!(colored(&mut h), Some(true));
    // The code font follows the setting.
    h.state_mut().set_settings(Settings { diff_font_size: 16.0, ..Settings::default() });
    h.run_steps(2);
    assert_eq!(tine::settings::current(&h.ctx).diff_font_size, 16.0);
}

#[test]
fn settings_survive_a_restart_and_older_state_loads() {
    // State saved before settings existed: defaults, other state kept.
    let h = app(Vec::new(), Some(stored(r#"(recent:[],open:[],active:0,messages:["older message"])"#)));
    assert_eq!(h.state().settings(), Settings::default());
    assert_eq!(h.state().recent_messages(), ["older message"]);
    drop(h);

    // Out-of-range or partial values are brought back into range.
    let h = app(Vec::new(), Some(stored("(settings:(diff_font_size:99.0,tab_width:0))")));
    assert_eq!(h.state().settings(), Settings { diff_font_size: 20.0, tab_width: 1, syntax_highlighting: true });
    drop(h);

    // Saved and loaded again.
    let mut h = app(Vec::new(), None);
    let chosen = Settings { diff_font_size: 15.0, tab_width: 2, syntax_highlighting: false };
    h.state_mut().set_settings(chosen);
    let mut storage = MemStorage::default();
    eframe::App::save(h.state_mut(), &mut storage);
    drop(h);
    let mut h = app(Vec::new(), Some(storage));
    assert_eq!(h.state().settings(), chosen);
    h.run_steps(1);
    assert_eq!(tine::settings::current(&h.ctx), chosen, "the views see them from the first frame");
}

fn key(k: Key, modifiers: Modifiers) -> Event {
    Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers }
}

fn stash_count(dir: &std::path::Path) -> usize {
    let out = std::process::Command::new("git").current_dir(dir).args(["stash", "list"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().count()
}

#[test]
fn keys_arriving_with_the_shortcut_that_opens_the_settings_go_nowhere() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()], None);
    h.state_mut().show_local_changes();
    settle(&mut h);
    // The file list has the keyboard (Space would stage)…
    let first = h.state_mut().active_tab_mut().unwrap().status.unstaged[0].path.to_string();
    h.get_by_label(&format!("unstaged file {first}")).click();
    h.run_steps(2);
    // …when Ctrl+, and Space arrive in one frame.
    h.input_mut().events.extend([key(Key::Comma, Modifiers::COMMAND), key(Key::Space, Modifiers::NONE)]);
    h.step();
    settle(&mut h);
    assert!(h.state().settings_open());
    assert!(h.state_mut().active_tab_mut().unwrap().status.staged.is_empty(), "Space staged nothing");
}

#[test]
fn a_dialog_coming_up_over_the_settings_takes_over_without_their_keys() {
    let repo = sample_repo();
    let mut h = app(vec![repo.path().into()], None);
    settle(&mut h);
    h.state_mut().run_command(tine::shortcuts::Command::Settings);
    h.run_steps(4);
    assert!(h.query_by_label("Keyboard Shortcuts").is_some());
    // An operation brings up a dialog (as a result arriving would) in the
    // frame an Enter meant for the settings comes.
    h.state_mut().active_tab_mut().unwrap().open_stash_dialog();
    h.input_mut().events.push(key(Key::Enter, Modifiers::NONE));
    h.step();
    assert!(!h.state().settings_open(), "the settings made way");
    assert!(h.state_mut().active_tab_mut().unwrap().dialog.is_some(), "the dialog stays");
    h.run_steps(2);
    assert!(h.query_by_label("Keyboard Shortcuts").is_none(), "one modal at a time");
    assert!(h.query_by_label("Stash Changes").is_some());
    settle(&mut h);
    assert_eq!(stash_count(repo.path()), 0, "that Enter confirmed nothing");
    // The dialog's own Enter works.
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(stash_count(repo.path()), 1);
}

#[test]
fn theme_and_zoom_come_back_with_eguis_memory() {
    // Theme and zoom live in egui's memory, which eframe saves at exit
    // (under its own key) and restores at start; this does the same.
    let mut h = app(Vec::new(), None);
    h.run_steps(2);
    h.state_mut().run_command(tine::shortcuts::Command::Settings);
    h.run_steps(4);
    h.get_by_label("Dark").click();
    h.run_steps(2);
    h.get_by_label("125%").click();
    h.run_steps(2);
    let mut storage = MemStorage::default();
    h.ctx.memory(|m| eframe::set_value(&mut storage, "egui", m));
    eframe::App::save(h.state_mut(), &mut storage);
    drop(h);

    let mut h = app(Vec::new(), Some(storage.clone()));
    let memory: egui::Memory = eframe::get_value(&storage, "egui").expect("egui's memory was saved");
    h.ctx.memory_mut(|m| *m = memory);
    h.run_steps(2);
    assert_eq!(h.ctx.options(|o| o.theme_preference), egui::ThemePreference::Dark);
    assert!((h.ctx.zoom_factor() - 1.25).abs() < 0.01);
}
