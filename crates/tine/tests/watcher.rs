use std::{path::Path, process::Command, time::Duration};

use crossbeam_channel::{Receiver, unbounded};
use tine::{
    watcher::{FsChange, RepoWatcher},
    worker::Msg,
};
use tine_core::GitRunner;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Next filesystem change within `wait`, if any.
fn next_change(rx: &Receiver<Msg>, wait: Duration) -> Option<FsChange> {
    let deadline = std::time::Instant::now() + wait;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Msg::Fs(c)) => return Some(c),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn reports_relevant_changes_only() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().canonicalize().unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join(".gitignore"), "/build/\n").unwrap();
    std::fs::write(d.join("a.txt"), "1\n").unwrap();
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "init"]);
    std::fs::create_dir(d.join("build")).unwrap();

    let (tx, rx) = unbounded();
    let runner = GitRunner::new(&d).with_env("GIT_CONFIG_GLOBAL", "/dev/null");
    let _w = RepoWatcher::start(d.clone(), d.join(".git"), runner, tx, egui::Context::default());
    std::thread::sleep(Duration::from_millis(500));

    // Ignored build output: nothing.
    std::fs::write(d.join("build/out.o"), "x").unwrap();
    assert_eq!(next_change(&rx, Duration::from_millis(1200)), None);

    // A tracked file edit: status only.
    std::fs::write(d.join("a.txt"), "2\n").unwrap();
    let c = next_change(&rx, Duration::from_secs(3)).expect("status change");
    assert!(c.status && !c.refs, "{c:?}");

    // A commit moves a branch: refs.
    git(&d, &["commit", "-qam", "second"]);
    let mut c = next_change(&rx, Duration::from_secs(3)).expect("refs change");
    while let Some(more) = next_change(&rx, Duration::from_millis(600)) {
        c.merge(more);
    }
    assert!(c.refs, "{c:?}");
}
