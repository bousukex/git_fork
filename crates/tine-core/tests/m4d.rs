//! Global git identity (M4d), against a private global config file.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::Path,
    time::{Duration, Instant},
};

use tine_core::{
    GitRunner,
    config::{Identity, global_identity, set_global_identity},
};

/// A runner whose global config is `dir/gitconfig`.
fn runner(dir: &Path) -> GitRunner {
    GitRunner::new(dir).with_env("GIT_CONFIG_GLOBAL", dir.join("gitconfig")).with_env("GIT_CONFIG_NOSYSTEM", "1")
}

fn read(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("gitconfig")).unwrap_or_default()
}

#[test]
fn reads_writes_and_removes_the_global_identity() {
    let dir = tempfile::tempdir().unwrap();
    let git = runner(dir.path());
    assert_eq!(global_identity(&git).unwrap(), Identity::default(), "nothing set (no file yet)");

    let w = set_global_identity(&git, Some("  山田 太郎 "), Some("taro@example.com"));
    assert!(w.all_ok(), "{w:?}");
    let id = global_identity(&git).unwrap();
    assert_eq!(id.name.as_deref(), Some("山田 太郎"), "trimmed");
    assert_eq!(id.email.as_deref(), Some("taro@example.com"));

    // Only what is given changes; values that look like options are fine.
    assert!(set_global_identity(&git, Some("-n --weird"), None).all_ok());
    assert_eq!(global_identity(&git).unwrap().name.as_deref(), Some("-n --weird"));
    assert_eq!(global_identity(&git).unwrap().email.as_deref(), Some("taro@example.com"));

    // A key set twice (by hand) is replaced as a whole.
    std::fs::write(dir.path().join("gitconfig"), "[user]\n\tname = a\n\tname = b\n\temail = e@x\n").unwrap();
    assert!(set_global_identity(&git, Some("c"), None).all_ok());
    assert_eq!(read(dir.path()).matches("name =").count(), 1, "{}", read(dir.path()));
    assert_eq!(global_identity(&git).unwrap().name.as_deref(), Some("c"));

    // Empty removes it; removing what isn't there is fine too.
    assert!(set_global_identity(&git, Some(""), Some(" ")).all_ok());
    assert_eq!(global_identity(&git).unwrap(), Identity::default());
    assert!(set_global_identity(&git, Some(""), Some("")).all_ok());

    // Line breaks can't be stored as one value.
    let w = set_global_identity(&git, Some("a\nb"), None);
    assert!(w.name.unwrap().unwrap_err().to_string().contains("single line"));
}

#[test]
fn one_field_can_fail_alone() {
    let dir = tempfile::tempdir().unwrap();
    // A git that refuses to write user.email (as a locked file would).
    let fake = dir.path().join("git");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = user.email ]; then echo 'error: could not lock config file' >&2; exit 4; fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let git = runner(dir.path()).with_program(&fake);
    let w = set_global_identity(&git, Some("Taro"), Some("taro@example.com"));
    assert!(!w.all_ok());
    assert!(w.name.as_ref().unwrap().is_ok(), "the name was written");
    assert!(format!("{:#}", w.email.unwrap().unwrap_err()).contains("could not lock"));
    let id = global_identity(&runner(dir.path())).unwrap();
    assert_eq!((id.name.as_deref(), id.email), (Some("Taro"), None));
}

#[test]
fn a_value_from_the_other_global_file_is_reported_not_hidden() {
    // git reads ~/.gitconfig and ~/.config/git/config; with both present it
    // writes only the first.
    let dir = tempfile::tempdir().unwrap();
    let (home, xdg) = (dir.path().join("home"), dir.path().join("xdg"));
    std::fs::create_dir_all(xdg.join("git")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(xdg.join("git/config"), "[user]\n\tname = From XDG\n").unwrap();
    std::fs::write(home.join(".gitconfig"), "[user]\n\temail = e@x\n").unwrap();
    let git = GitRunner::new(dir.path()).with_env("HOME", &home).with_env("XDG_CONFIG_HOME", &xdg).with_env("GIT_CONFIG_NOSYSTEM", "1");
    assert_eq!(global_identity(&git).unwrap().name.as_deref(), Some("From XDG"));

    // Removing it from ~/.gitconfig doesn't remove it: say where it is.
    let w = set_global_identity(&git, Some(""), None);
    let e = format!("{:#}", w.name.unwrap().unwrap_err());
    assert!(e.contains("From XDG") && e.contains(&xdg.join("git/config").display().to_string()), "{e}");
    assert_eq!(global_identity(&git).unwrap().name.as_deref(), Some("From XDG"));

    // Setting it works (~/.gitconfig is read last and wins).
    assert!(set_global_identity(&git, Some("Mine"), None).all_ok());
    assert_eq!(global_identity(&git).unwrap().name.as_deref(), Some("Mine"));
}

#[test]
fn writes_from_two_places_go_one_after_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let release = dir.path().join("release");
    // A git that logs each write and holds the first one until released.
    let fake = dir.path().join("git");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nw=\nfor a in \"$@\"; do [ \"$a\" = --replace-all ] && w=1; done\n\
             if [ -n \"$w\" ]; then\n  echo start >> '{log}'\n  if [ ! -e '{log}.held' ]; then touch '{log}.held'; while [ ! -e '{release}' ]; do sleep 0.02; done; fi\n  git \"$@\"; s=$?\n  echo end >> '{log}'\n  exit $s\nfi\nexec git \"$@\"\n",
            log = log.display(),
            release = release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let git = runner(dir.path()).with_program(&fake);
    let first = std::thread::spawn({
        let git = git.clone();
        move || set_global_identity(&git, Some("A"), Some("a@x")).all_ok()
    });
    let started = Instant::now();
    while !std::fs::read_to_string(&log).is_ok_and(|l| l.contains("start")) {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = std::thread::spawn({
        let git = git.clone();
        move || set_global_identity(&git, Some("B"), Some("b@x")).all_ok()
    });
    // While the first is held, the second doesn't start writing.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "start\n");
    std::fs::write(&release, "").unwrap();
    assert!(first.join().unwrap() && second.join().unwrap());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "start\nend\nstart\nend\nstart\nend\nstart\nend\n");
    assert_eq!(global_identity(&runner(dir.path())).unwrap().name.as_deref(), Some("B"));
}
