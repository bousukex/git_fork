//! The Git and Tools pages of the settings, and starting external programs
//! from the app (M4d).

mod common;

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use common::{sample_repo, settle};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use pengitto::{app::PengittoApp, shortcuts::Command, tools::ToolSettings};
use pengitto_core::GitRunner;

/// Every app takes all tool results from one process-wide queue (and
/// hands those of other work trees out as notices): one test at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

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

fn app(paths: Vec<PathBuf>, storage: Option<MemStorage>) -> Harness<'static, PengittoApp> {
    Harness::builder()
        .with_size([1200.0, 760.0])
        .build_eframe(move |cc| PengittoApp::with_storage(cc, paths, storage.as_ref().map(|s| s as &dyn eframe::Storage)))
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Steps until `done` (10 s at most).
fn step_until(h: &mut Harness<'static, PengittoApp>, mut done: impl FnMut(&mut Harness<'static, PengittoApp>) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        h.step();
        if done(h) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// A global config of its own (`dir/gitconfig`), through `program`.
fn global_git(dir: &Path, program: Option<&Path>) -> GitRunner {
    let git = GitRunner::new(dir).with_env("GIT_CONFIG_GLOBAL", dir.join("gitconfig")).with_env("GIT_CONFIG_NOSYSTEM", "1");
    match program {
        Some(p) => git.with_program(p),
        None => git,
    }
}

/// Opens the settings at the Git page and waits for the identity.
fn git_page(h: &mut Harness<'static, PengittoApp>) {
    h.state_mut().run_command(Command::Settings);
    h.run_steps(4);
    h.get_by_label("Git").click();
    h.run_steps(4);
    assert!(step_until(h, |h| h.state_mut().settings_ui().identity.stored().is_some()), "read");
    h.run_steps(2);
}

fn disabled(h: &Harness<'static, PengittoApp>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().is_disabled()
}

#[test]
fn the_git_page_reads_and_writes_the_global_identity() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("gitconfig"), "[user]\n\tname = Old Name\n").unwrap();
    let mut h = app(Vec::new(), None);
    h.state_mut().settings_ui().identity.set_runner(global_git(tmp.path(), None));
    git_page(&mut h);
    let id = &mut h.state_mut().settings_ui().identity;
    assert_eq!((id.name.as_str(), id.email.as_str()), ("Old Name", ""), "what the config holds");
    assert!(disabled(&h, "Save"), "nothing to save yet");

    let id = &mut h.state_mut().settings_ui().identity;
    id.name = " 山田 太郎 ".into();
    id.email = "taro@example.com".into();
    h.run_steps(2);
    assert!(!disabled(&h, "Save"));
    h.get_by_label("Save").click();
    assert!(step_until(&mut h, |h| !h.state_mut().settings_ui().identity.saving()));
    h.run_steps(2);
    assert!(h.query_by_label("Saved to the global git config").is_some());
    let config = std::fs::read_to_string(tmp.path().join("gitconfig")).unwrap();
    assert!(config.contains("name = 山田 太郎") && config.contains("email = taro@example.com"), "{config}");
    // Read back after writing: the field shows what is stored (trimmed).
    assert_eq!(h.state_mut().settings_ui().identity.name, "山田 太郎");
    assert!(disabled(&h, "Save"));
}

/// Waits until `path` exists (10 s at most).
fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A git that is told by files what to do: with `email-fails`, writing
/// user.email waits for `release` and fails (and then, with
/// `then-reads-fail`, so does every reading); `hold-once` makes the next
/// reading of user.email wait for `release` (renaming it to `held` when it
/// starts waiting).
fn controlled_git(dir: &Path) -> PathBuf {
    let d = dir.display();
    script(
        dir,
        "git",
        &format!(
            "w=; r=; e=\n\
             for a in \"$@\"; do case \"$a\" in --replace-all) w=1;; --get-all) r=1;; user.email) e=1;; esac; done\n\
             if [ -n \"$w\" ] && [ -n \"$e\" ] && [ -e '{d}/email-fails' ]; then\n\
               while [ ! -e '{d}/release' ]; do sleep 0.02; done\n\
               [ -e '{d}/then-reads-fail' ] && touch '{d}/reads-fail'\n\
               echo 'error: could not lock config file' >&2; exit 4\n\
             fi\n\
             if [ -n \"$r\" ] && [ -e '{d}/reads-fail' ]; then echo 'fatal: bad config line 3' >&2; exit 128; fi\n\
             if [ -n \"$r\" ] && [ -n \"$e\" ] && mv '{d}/hold-once' '{d}/held' 2>/dev/null; then\n\
               while [ ! -e '{d}/release' ]; do sleep 0.02; done; git \"$@\"; s=$?; touch '{d}/held-done'; exit $s\n\
             fi\n\
             exec git \"$@\""
        ),
    )
}

#[test]
fn a_save_in_progress_blocks_another_and_a_half_save_says_so() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("email-fails"), "").unwrap();
    let fake = controlled_git(tmp.path());
    let mut h = app(Vec::new(), None);
    h.state_mut().settings_ui().identity.set_runner(global_git(tmp.path(), Some(&fake)));
    git_page(&mut h);
    let id = &mut h.state_mut().settings_ui().identity;
    id.name = "Taro".into();
    id.email = "taro@example.com".into();
    h.run_steps(2);
    h.get_by_label("Save").click();
    h.run_steps(2);
    // The email write is held: the save is on its way, whatever the timing.
    assert!(h.state_mut().settings_ui().identity.saving());
    assert!(h.query_by_label("Saving…").is_some());
    assert!(disabled(&h, "Save"), "no second save meanwhile");
    std::fs::write(tmp.path().join("release"), "").unwrap();
    assert!(step_until(&mut h, |h| !h.state_mut().settings_ui().identity.saving()));
    h.run_steps(2);
    let id = &mut h.state_mut().settings_ui().identity;
    let status = id.status().cloned().unwrap().unwrap_err();
    assert!(status.contains("The name was saved, the email wasn't") && status.contains("could not lock"), "{status}");
    assert_eq!(id.stored().unwrap().name.as_deref(), Some("Taro"));
    assert_eq!(id.stored().unwrap().email, None);
    assert_eq!(id.email, "taro@example.com", "the unsaved input stays for another try");
    h.run_steps(1);
    assert!(!disabled(&h, "Save"));
}

#[test]
fn an_older_reading_doesnt_undo_a_save() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("gitconfig"), "[user]\n\tname = Old\n\temail = old@x\n").unwrap();
    let fake = controlled_git(tmp.path());
    let mut h = app(Vec::new(), None);
    h.state_mut().settings_ui().identity.set_runner(global_git(tmp.path(), Some(&fake)));
    git_page(&mut h);
    // A reading (as when the dialog opens again) that has read the name,
    // and waits before the email.
    std::fs::write(tmp.path().join("hold-once"), "").unwrap();
    let ctx = h.ctx.clone();
    h.state_mut().settings_ui().identity.load(&ctx);
    wait_file(&tmp.path().join("held"));
    // Meanwhile a save goes through and is read back.
    let id = &mut h.state_mut().settings_ui().identity;
    id.name = "New".into();
    id.email = "new@x".into();
    h.run_steps(1);
    h.get_by_label("Save").click();
    assert!(step_until(&mut h, |h| !h.state_mut().settings_ui().identity.saving()));
    assert_eq!(h.state_mut().settings_ui().identity.stored().unwrap().name.as_deref(), Some("New"));
    // The older reading ends (with the old name): it doesn't count.
    std::fs::write(tmp.path().join("release"), "").unwrap();
    wait_file(&tmp.path().join("held-done"));
    h.run_steps(4);
    let id = &mut h.state_mut().settings_ui().identity;
    assert_eq!((id.name.as_str(), id.email.as_str()), ("New", "new@x"));
    assert_eq!(id.stored().unwrap().name.as_deref(), Some("New"));
}

#[test]
fn editing_one_field_leaves_the_other_to_the_config() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("gitconfig"), "[user]\n\tname = A\n\temail = a@x\n").unwrap();
    let mut h = app(Vec::new(), None);
    h.state_mut().settings_ui().identity.set_runner(global_git(tmp.path(), None));
    git_page(&mut h);
    // The email changes elsewhere while the name is being edited here.
    std::fs::write(tmp.path().join("gitconfig"), "[user]\n\tname = A\n\temail = b@x\n").unwrap();
    h.state_mut().settings_ui().identity.name = "Mine".into();
    let ctx = h.ctx.clone();
    h.state_mut().settings_ui().identity.load(&ctx);
    assert!(step_until(&mut h, |h| h.state_mut().settings_ui().identity.email == "b@x"), "the untouched field follows the config");
    assert_eq!(h.state_mut().settings_ui().identity.name, "Mine", "the edited one stays");
    h.get_by_label("Save").click();
    assert!(step_until(&mut h, |h| !h.state_mut().settings_ui().identity.saving()));
    let config = std::fs::read_to_string(tmp.path().join("gitconfig")).unwrap();
    assert!(config.contains("name = Mine") && config.contains("email = b@x"), "only the name was written: {config}");
}

#[test]
fn a_failed_reading_after_saving_keeps_the_result_in_view() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    for f in ["email-fails", "release", "then-reads-fail"] {
        std::fs::write(tmp.path().join(f), "").unwrap();
    }
    let fake = controlled_git(tmp.path());
    let mut h = app(Vec::new(), None);
    h.state_mut().settings_ui().identity.set_runner(global_git(tmp.path(), Some(&fake)));
    git_page(&mut h);
    let id = &mut h.state_mut().settings_ui().identity;
    id.name = "Taro".into();
    id.email = "taro@example.com".into();
    h.run_steps(1);
    // The name is written and checked, the email write fails, and then
    // so does reading everything back.
    h.get_by_label("Save").click();
    assert!(step_until(&mut h, |h| !h.state_mut().settings_ui().identity.saving()));
    h.run_steps(2);
    let id = &mut h.state_mut().settings_ui().identity;
    assert!(id.read_error().is_some_and(|e| e.contains("bad config")), "{:?}", id.read_error());
    let status = id.status().cloned().unwrap().unwrap_err();
    assert!(status.contains("The name was saved, the email wasn't"), "{status}");
    assert_eq!((id.name.as_str(), id.email.as_str()), ("Taro", "taro@example.com"));
    assert!(!id.name_edited() && id.email_edited(), "the name is known saved, the email still to do");
    // Still shown: the result, the problem, the fields.
    assert!(h.query_by_label_contains("wasn't").is_some());
    assert!(h.query_by_label_contains("Can't read the global git config again").is_some());
    assert!(h.query_by_label("Save").is_some());
}

#[test]
fn the_terminal_setting_starts_the_terminal_and_reports_failures() {
    let _serial = serial();
    let repo = sample_repo();
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let term = script(tmp.path(), "term", &format!("for a in \"$@\"; do echo \"$a\" >> '{}'; done", out.display()));
    let mut h = app(vec![repo.path().into()], None);
    settle(&mut h);
    let workdir = h.state_mut().active_tab_mut().unwrap().paths.workdir.clone();

    // Typed on the Tools page: kept as words; half-typed quotes aren't.
    h.state_mut().run_command(Command::Settings);
    h.run_steps(4);
    h.get_by_label("Tools").click();
    h.run_steps(4);
    let field = h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Terminal");
    field.focus();
    field.type_text(&format!("{} --at {{dir}} 'two words", term.display()));
    h.run_steps(2);
    assert!(h.query_by_label_contains("quote isn't closed").is_some());
    assert_eq!(h.state().tool_settings(), &ToolSettings::default(), "unchanged until it parses");
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, "Terminal").type_text("'");
    h.run_steps(2);
    let words = vec![term.to_string_lossy().into_owned(), "--at".into(), "{dir}".into(), "two words".into()];
    assert_eq!(h.state().tool_settings().terminal, words);
    h.get_by_label("Close").click();
    h.run_steps(4);

    h.state_mut().run_command(Command::OpenTerminal);
    assert!(step_until(&mut h, |_| std::fs::read_to_string(&out).is_ok_and(|s| s.lines().count() == 3)));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), format!("--at\n{}\ntwo words\n", workdir.display()));
    settle(&mut h);
    assert!(h.state_mut().active_tab_mut().unwrap().errors.is_empty());

    // A terminal that fails: the tab says so.
    let failing = script(tmp.path(), "failing", "echo 'cannot open display' >&2\nexit 2");
    h.state_mut().set_tool_settings(ToolSettings { terminal: vec![failing.to_string_lossy().into()] });
    h.state_mut().run_command(Command::OpenTerminal);
    assert!(step_until(&mut h, |h| !h.state_mut().active_tab_mut().unwrap().errors.is_empty()));
    let e = h.state_mut().active_tab_mut().unwrap().errors.join("\n");
    assert!(e.contains("Open in Terminal failed") && e.contains("cannot open display"), "{e}");

    // The settings are kept with the app's state.
    let mut storage = MemStorage::default();
    eframe::App::save(h.state_mut(), &mut storage);
    let h2 = app(Vec::new(), Some(storage));
    assert_eq!(h2.state().tool_settings().terminal, [failing.to_string_lossy().into_owned()]);
}

#[test]
fn a_tool_of_a_closed_tab_reports_to_the_app() {
    let _serial = serial();
    let repo = sample_repo();
    let tmp = tempfile::tempdir().unwrap();
    let slow = script(tmp.path(), "slow", "sleep 0.5\necho 'gave up' >&2\nexit 2");
    let mut h = app(vec![repo.path().into()], None);
    settle(&mut h);
    h.state_mut().set_tool_settings(ToolSettings { terminal: vec![slow.to_string_lossy().into()] });
    h.run_steps(1);
    h.state_mut().run_command(Command::OpenTerminal);
    h.state_mut().run_command(Command::CloseTab);
    assert_eq!(h.state().tab_count(), 0);
    assert!(step_until(&mut h, |h| !h.state().notices().is_empty()));
    let n = h.state().notices().join("\n");
    assert!(n.contains("Open in Terminal failed") && n.contains("gave up"), "{n}");
}

#[test]
fn a_late_note_for_a_closed_tab_reaches_the_app() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let closed = script(tmp.path(), "term", "sleep 0.4\nexit 1");
    let mut h = app(Vec::new(), None);
    h.run_steps(1);
    let launch = pengitto::tools::Launch {
        label: "Open in Terminal".into(),
        program: closed.into(),
        args: Vec::new(),
        dir: tmp.path().into(),
        startup: Some(Duration::from_millis(100)),
        fallback: None,
    };
    pengitto::tools::start(launch, tmp.path().join("closed-repo"), h.ctx.clone());
    assert!(step_until(&mut h, |h| !h.state().notices().is_empty()));
    let n = h.state().notices().join("\n");
    assert!(n.contains("exited with code 1") && n.contains("closed-repo"), "{n}");
}

#[test]
fn open_file_needs_the_file_in_the_working_tree() {
    let _serial = serial();
    let repo = sample_repo();
    std::fs::remove_file(repo.path().join("lib.rs")).unwrap();
    let mut h = app(vec![repo.path().into()], None);
    h.state_mut().show_local_changes();
    settle(&mut h);
    h.get_by_label("unstaged file lib.rs").click_secondary();
    h.run_steps(2);
    assert!(disabled(&h, "Open File") && disabled(&h, "Show in File Manager"), "deleted: nothing to open");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    h.get_by_label("unstaged file README.md").click_secondary();
    h.run_steps(2);
    assert!(!disabled(&h, "Open File") && !disabled(&h, "Show in File Manager"));
}
