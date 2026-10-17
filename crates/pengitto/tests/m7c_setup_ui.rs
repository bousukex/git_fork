//! The Setup check in the app (M7c): its problems on the welcome screen,
//! its dialog (☰ → Check Setup…), Settings → Git from it, and libsecret made
//! the credential helper — only if the helpers are still as shown.

mod common;

use std::{
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};

use common::{settle, write_executable};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use pengitto::{
    app::{AppOptions, PengittoApp},
    ui::settings::Page,
};
use pengitto_core::setup::{Env, Probed};

/// A scratch home: git's configuration, git's exec path, and stand-ins
/// for fontconfig.
struct Place {
    dir: tempfile::TempDir,
}

impl Place {
    fn new(fonts: &str) -> Place {
        let p = Place { dir: tempfile::tempdir().unwrap() };
        for d in ["home", "bin", "exec"] {
            std::fs::create_dir_all(p.path(d)).unwrap();
        }
        write_executable(&p.path("bin/fc-list"), &format!("#!/bin/sh\nprintf '{fonts}'\n"));
        p
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn global(&self) -> PathBuf {
        self.path("gitconfig")
    }

    fn env(&self) -> Env {
        let path = format!("{}:{}", self.path("bin").display(), std::env::var("PATH").unwrap());
        let var = |k: &str, v: OsString| (OsString::from(k), Some(v));
        Env {
            git: "git".into(),
            vars: vec![
                var("GIT_CONFIG_GLOBAL", self.global().into()),
                var("GIT_CONFIG_SYSTEM", self.path("system").into()),
                var("HOME", self.path("home").into()),
                var("PATH", path.into()),
                var("GIT_EXEC_PATH", self.path("exec").into()),
                (OsString::from("SSH_AUTH_SOCK"), None),
            ],
            home: self.path("home"),
            root: self.path("root"),
            container_var: None,
            timeout: Duration::from_secs(5),
        }
    }
}

fn harness(p: &Place) -> Harness<'static, PengittoApp> {
    let options = AppOptions { setup: Some(p.env()), ..Default::default() };
    let mut h = Harness::builder().with_size([1100.0, 800.0]).build_eframe(move |cc| PengittoApp::with_options(cc, vec![], None, options));
    settle(&mut h);
    until(&mut h, "the check done", |h| h.state().setup_report().is_some());
    h.run_steps(3);
    h
}

fn until(h: &mut Harness<'static, PengittoApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, PengittoApp>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(15), "never: {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Clicks `label`, the pointer over it a frame first (as a real one is):
/// moved and pressed in one frame from over a label (selectable text), the
/// press goes to the label.
fn press(h: &mut Harness<'static, PengittoApp>, label: &str) {
    h.get_by_label(label).hover();
    h.step();
    h.get_by_label(label).click();
}

fn shows(h: &Harness<'static, PengittoApp>, text: &str) -> bool {
    h.query_by_label_contains(text).is_some()
}

#[test]
fn the_welcome_screen_tells_what_is_missing() {
    let p = Place::new("");
    let mut h = harness(&p);
    assert!(shows(&h, "Setup: something to look at"));
    for t in ["Who commits: user.name and user.email aren't set", "HTTPS credentials: No credential helper", "Japanese font: None"] {
        assert!(shows(&h, t), "{t}");
    }
    // What is fine isn't listed there.
    assert!(!shows(&h, "Git: git"));
    // The dialog, from there.
    press(&mut h, "Check Setup…");
    h.run_steps(4);
    assert!(h.state().setup_open());
    assert!(shows(&h, "What pengitto needs from this system"));
    // Settings → Git, from the dialog.
    press(&mut h, "Open Settings → Git");
    h.run_steps(4);
    assert!(!h.state().setup_open() && h.state().settings_open());
    assert_eq!(h.state().settings_page(), Page::Git);
}

#[test]
fn nothing_shows_when_nothing_is_missing() {
    let p = Place::new("Noto Sans CJK JP\\n");
    std::fs::write(p.global(), "[user]\n\tname = Taro\n\temail = taro@example.com\n[credential]\n\thelper = store\n").unwrap();
    let h = harness(&p);
    assert!(!shows(&h, "Setup: something to look at"));
    // And from the menu, all of it with its marks.
    let mut h = h;
    press(&mut h, "☰");
    h.run_steps(2);
    press(&mut h, "Check Setup…");
    until(&mut h, "checked again", |h| h.state().setup_open() && h.query_by_label_contains("Check Again").is_some_and(|b| !b.accesskit_node().is_disabled()));
    for t in ["Taro <taro@example.com>", "Kept by store", "Noto Sans CJK JP", "no ssh-agent"] {
        assert!(shows(&h, t), "{t}");
    }
    assert!(h.query_by_label("Use libsecret").is_none());
    press(&mut h, "Close");
    h.run_steps(3);
    assert!(!h.state().setup_open());
}

fn open_dialog(h: &mut Harness<'static, PengittoApp>) {
    press(h, "Check Setup…");
    until(h, "the dialog checked", |h| h.state().setup_open() && h.query_by_label_contains("Check Again").is_some_and(|b| !b.accesskit_node().is_disabled()));
}

fn helpers(h: &Harness<'static, PengittoApp>) -> Vec<String> {
    match &h.state().setup_report().unwrap().helpers {
        Probed::Known(x) => x.general.iter().map(|s| s.value.clone()).collect(),
        Probed::Unknown(e) => panic!("{e}"),
    }
}

#[test]
fn libsecret_made_the_helper_from_the_dialog() {
    let p = Place::new("");
    // Not installed: offered, not usable.
    let mut h = harness(&p);
    open_dialog(&mut h);
    assert!(h.get_by_label("Use libsecret").accesskit_node().is_disabled());
    assert!(shows(&h, "git-credential-libsecret"), "how to install it");
    press(&mut h, "Close");
    h.run_steps(3);
    // Installed: written (added), read back.
    write_executable(&p.path("exec/git-credential-libsecret"), "#!/bin/sh\n");
    open_dialog(&mut h);
    press(&mut h, "Use libsecret");
    until(&mut h, "written", |h| shows(h, "libsecret is set as git's credential helper"));
    assert_eq!(helpers(&h), ["libsecret"]);
    assert!(std::fs::read_to_string(p.global()).unwrap().contains("helper = libsecret"));
    assert!(h.query_by_label("Use libsecret").is_none(), "nothing more to offer");
    assert!(shows(&h, "Kept by libsecret"));
}

#[test]
fn helpers_changed_meanwhile_are_shown_not_overwritten() {
    let p = Place::new("");
    write_executable(&p.path("exec/git-credential-libsecret"), "#!/bin/sh\n");
    let mut h = harness(&p);
    open_dialog(&mut h);
    // Set elsewhere after the check showed none.
    std::fs::write(p.global(), "[credential]\n\thelper = store\n").unwrap();
    press(&mut h, "Use libsecret");
    until(&mut h, "told", |h| shows(h, "changed meanwhile"));
    assert_eq!(std::fs::read_to_string(p.global()).unwrap(), "[credential]\n\thelper = store\n");
    assert_eq!(helpers(&h), ["store"]);
    assert!(shows(&h, "Kept by store"));
}

#[test]
fn an_older_check_finishing_later_doesnt_replace_a_newer_one() {
    let p = Place::new("Noto Sans CJK JP\\n");
    let slow = late_git(&p);
    std::fs::write(&slow, "").unwrap();
    let mut env = p.env();
    env.git = p.path("bin/git").into();
    let options = AppOptions { setup: Some(env), ..Default::default() };
    let mut h = Harness::builder().with_size([1100.0, 800.0]).build_eframe(move |cc| PengittoApp::with_options(cc, vec![], None, options));
    h.run_steps(3);
    assert!(h.state().setup_checking() && h.state().setup_report().is_none());
    // Meanwhile: who commits is set, and checked again (quickly).
    std::fs::remove_file(&slow).unwrap();
    std::fs::write(p.global(), "[user]\n\tname = Taro\n\temail = taro@example.com\n").unwrap();
    press(&mut h, "☰");
    h.run_steps(2);
    press(&mut h, "Check Setup…");
    until(&mut h, "checked again", |h| !h.state().setup_checking());
    let who = |h: &Harness<'static, PengittoApp>| h.state().setup_report().map(|r| matches!(&r.who, Probed::Known(w) if w.name.is_some()));
    assert_eq!(who(&h), Some(true));
    // The first one ends (it found nobody): dropped.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        h.step();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(who(&h), Some(true));
}

/// A git that answers late (with what it read at once) while the file it
/// returns is there.
fn late_git(p: &Place) -> PathBuf {
    let real = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let slow = p.path("slow");
    write_executable(
        &p.path("bin/git"),
        &format!(
            "#!/bin/sh\nlate=0\n[ -e '{}' ] && late=1 && echo \"$*\" >> '{}'\nout=$(mktemp)\n{} \"$@\" > \"$out\"\ncode=$?\n[ $late = 1 ] && sleep 2\ncat \"$out\"\nrm -f \"$out\"\nexit $code\n",
            slow.display(),
            p.path("late").display(),
            real.trim()
        ),
    );
    slow
}

#[test]
fn a_check_under_way_as_libsecret_is_set_doesnt_undo_it() {
    let p = Place::new("");
    write_executable(&p.path("exec/git-credential-libsecret"), "#!/bin/sh\n");
    let slow = late_git(&p);
    let mut env = p.env();
    env.git = p.path("bin/git").into();
    let options = AppOptions { setup: Some(env), ..Default::default() };
    let mut h = Harness::builder().with_size([1100.0, 800.0]).build_eframe(move |cc| PengittoApp::with_options(cc, vec![], None, options));
    until(&mut h, "the check done", |h| h.state().setup_report().is_some() && !h.state().setup_checking());
    open_dialog(&mut h);
    // A check that reads none and answers late; meanwhile libsecret is set.
    std::fs::write(&slow, "").unwrap();
    press(&mut h, "Check Again");
    // (Once the helpers are being read, late.)
    until(&mut h, "reading the helpers late", |_| std::fs::read_to_string(p.path("late")).is_ok_and(|l| l.contains("credential")));
    assert!(h.state().setup_checking());
    std::fs::remove_file(&slow).unwrap();
    press(&mut h, "Use libsecret");
    until(&mut h, "set", |h| shows(h, "libsecret is set as git's credential helper"));
    // The late check ends (it read none): what shows stays as it is.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        h.step();
        std::thread::sleep(Duration::from_millis(50));
    }
    until(&mut h, "checked again", |h| !h.state().setup_checking());
    assert_eq!(helpers(&h), ["libsecret"]);
    assert!(shows(&h, "Kept by libsecret"));
}
