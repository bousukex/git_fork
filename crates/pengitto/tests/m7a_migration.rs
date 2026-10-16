//! Taking over from pengitto's former name (M7a, `pengitto_core::legacy`):
//! the settings it saved (fixtures written in its format, not by pengitto's
//! own saving), and a broken settings file of pengitto's set aside rather
//! than lost.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use common::settle;
use egui_kittest::Harness;
use pengitto::app::{AppOptions, PengittoApp};
use pengitto_core::legacy;

#[derive(Default)]
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

fn harness(storage: Option<MemStorage>, options: AppOptions) -> Harness<'static, PengittoApp> {
    let mut h = Harness::builder()
        .with_size([1000.0, 700.0])
        .build_eframe(move |cc| PengittoApp::with_options(cc, vec![], storage.as_ref().map(|s| s as &dyn eframe::Storage), options));
    settle(&mut h);
    h
}

/// A copy of a fixture folder of the former name's (so that its files can be
/// seen untouched afterwards).
fn legacy(fixture: &str, to: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy").join(fixture);
    let dir = to.join(legacy::APP);
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), dir.join(e.file_name())).unwrap();
    }
    dir
}

fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut all: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap())
        })
        .collect();
    all.sort();
    all
}

#[test]
fn former_settings_are_taken_over_once_and_left_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("pengitto/state.ron");
    let options = |legacy: &Path| AppOptions { state_file: Some(state.clone()), legacy_dir: Some(legacy.to_path_buf()), ..Default::default() };
    // Its state file first (over its eframe storage).
    let old = legacy("state-file", dir.path());
    let before = files(&old);
    let mut h = harness(None, options(&old));
    let app = h.state();
    assert_eq!(app.settings().tab_width, 6);
    assert!(!app.settings().syntax_highlighting);
    assert_eq!(app.recent(), [PathBuf::from("/home/someone/src/作業"), PathBuf::from("/home/someone/src/other")]);
    assert_eq!(app.recent_messages(), ["Fix the graph lanes"]);
    // (The look: the zoom; kittest sets its own theme after the app is made.)
    assert_eq!(h.ctx.zoom_factor(), 1.25);
    let taken = format!("taken over from {}", legacy::NAME);
    assert!(app.notices().iter().any(|n| n.contains(&taken)), "{:?}", app.notices());
    // Saved as pengitto's own from then on; the former files as they were.
    save_and_close(&mut h);
    assert!(std::fs::read_to_string(&state).unwrap().contains("tab_width: 6"));
    assert_eq!(files(&old), before);
    // Next time pengitto's own is read (the former not again).
    let mut settings = h.state().settings();
    settings.tab_width = 2;
    h.state_mut().set_settings(settings);
    eframe::App::save(h.state_mut(), &mut MemStorage::default());
    eframe::App::on_exit(h.state_mut());
    let h = harness(None, options(&old));
    assert_eq!(h.state().settings().tab_width, 2);
    assert!(!h.state().notices().iter().any(|n| n.contains(legacy::NAME)));
}

#[test]
fn the_former_eframe_storage_alone_is_read_too() {
    // From before the state had a file of its own: only eframe's storage,
    // the state a RON string in a map of them.
    let dir = tempfile::tempdir().unwrap();
    let old = legacy("storage-only", dir.path());
    let options = AppOptions { state_file: Some(dir.path().join("pengitto/state.ron")), legacy_dir: Some(old), ..Default::default() };
    let h = harness(None, options);
    assert_eq!(h.state().settings().tab_width, 3);
    assert_eq!(h.state().recent(), [PathBuf::from("/home/someone/src/from-storage")]);
}

#[test]
fn pengittos_own_state_comes_before_the_former() {
    let dir = tempfile::tempdir().unwrap();
    let old = legacy("state-file", dir.path());
    let state = dir.path().join("pengitto/state.ron");
    let options = || AppOptions { state_file: Some(state.clone()), legacy_dir: Some(old.clone()), ..Default::default() };
    // pengitto's copy in eframe's storage (its state file lost).
    let mut h = harness(None, AppOptions::default());
    let mut settings = h.state().settings();
    settings.tab_width = 8;
    h.state_mut().set_settings(settings);
    let mut storage = MemStorage::default();
    eframe::App::save(h.state_mut(), &mut storage);
    let h = harness(Some(storage), options());
    assert_eq!(h.state().settings().tab_width, 8);
    assert!(h.state().notices().is_empty(), "{:?}", h.state().notices());
}

/// The files kept aside next to `state`.
fn kept_aside(state: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(state.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("state.ron.broken-"))
        .collect()
}

/// Saved as when the app is closed.
fn save_and_close(h: &mut Harness<'static, PengittoApp>) {
    eframe::App::save(h.state_mut(), &mut MemStorage::default());
    eframe::App::on_exit(h.state_mut());
}

#[test]
fn a_broken_settings_file_is_kept_not_written_over() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("pengitto/state.ron");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let old = legacy("state-file", dir.path());
    // Cut off, and not UTF-8 at all.
    for broken in [&b"(recent: [\"/cut/off"[..], &b"(recent: [\"/caf\xe9\"])"[..]] {
        for p in kept_aside(&state) {
            std::fs::remove_file(p).unwrap();
        }
        std::fs::write(&state, broken).unwrap();
        let mut h = harness(None, AppOptions { state_file: Some(state.clone()), legacy_dir: Some(old.clone()), ..Default::default() });
        // Kept, told, and the next candidate read (here the former).
        let aside = kept_aside(&state);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(&aside[0]).unwrap(), broken);
        assert!(h.state().notices().iter().any(|n| n.contains("is broken") && n.contains("state.ron.broken-")), "{:?}", h.state().notices());
        assert_eq!(h.state().settings().tab_width, 6);
        // The next save writes a whole new one; what was kept stays.
        save_and_close(&mut h);
        assert!(std::fs::read_to_string(&state).unwrap().contains("tab_width: 6"));
        assert_eq!(std::fs::read(&aside[0]).unwrap(), broken);
    }
}

#[test]
fn a_settings_file_that_cant_be_read_isnt_saved_over() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("pengitto/state.ron");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, "(recent: [\"/mine\"])").unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&state).is_ok() {
        eprintln!("skipped: files can be read whatever their permissions here (root)");
        return;
    }
    let mut h = harness(None, AppOptions { state_file: Some(state.clone()), ..Default::default() });
    assert!(h.state().notices().iter().any(|n| n.contains("can't be read") && n.contains("aren't saved")), "{:?}", h.state().notices());
    // Readable again before pengitto saves: still not written over.
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
    save_and_close(&mut h);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "(recent: [\"/mine\"])");
    assert!(kept_aside(&state).is_empty());
}

#[test]
fn a_broken_file_that_cant_be_kept_isnt_saved_over() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("pengitto");
    let state = folder.join("state.ron");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(&state, "(recent: [\"/cut").unwrap();
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(folder.join("probe")).is_ok() {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: folders can be written whatever their permissions here (root)");
        return;
    }
    let mut h = harness(None, AppOptions { state_file: Some(state.clone()), ..Default::default() });
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(h.state().notices().iter().any(|n| n.contains("couldn't be kept aside") && n.contains("aren't saved")), "{:?}", h.state().notices());
    // Writable again before pengitto saves: still not written over.
    save_and_close(&mut h);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "(recent: [\"/cut");
}

#[test]
fn the_former_folder_is_found_as_eframe_finds_its_own() {
    // eframe's rule: an absolute XDG_DATA_HOME, else ~/.local/share (a
    // relative one ignored). (Read in a child process: the environment is
    // the process's.)
    let exe = std::env::current_exe().unwrap();
    let home = format!("/home/x/.local/share/{}", legacy::APP);
    for (data_home, want) in [
        (Some("/data/home"), format!("/data/home/{}", legacy::APP)),
        (Some("relative/data"), home.clone()),
        (Some(""), home.clone()),
        (None, home.clone()),
    ] {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["--exact", "print_legacy_dir", "--nocapture", "--ignored"]).env("HOME", "/home/x");
        match data_home {
            Some(v) => cmd.env("XDG_DATA_HOME", v),
            None => cmd.env_remove("XDG_DATA_HOME"),
        };
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find_map(|l| l.strip_prefix("legacy-dir=")).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(line, want, "XDG_DATA_HOME={data_home:?}");
    }
}

#[test]
#[ignore = "run by the_former_folder_is_found_as_eframe_finds_its_own"]
fn print_legacy_dir() {
    println!("legacy-dir={}", pengitto::state_file::legacy_dir().unwrap().display());
}
