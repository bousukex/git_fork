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
    let _w = RepoWatcher::start(d.clone(), d.join(".git"), d.join(".git"), runner, tx, egui::Context::default());
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

#[test]
fn constant_reads_by_other_tools_do_not_delay_updates() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().canonicalize().unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.txt"), "1\n").unwrap();
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "init"]);

    let (tx, rx) = unbounded();
    let runner = GitRunner::new(&d).with_env("GIT_CONFIG_GLOBAL", "/dev/null");
    let _w = RepoWatcher::start(d.clone(), d.join(".git"), d.join(".git"), runner, tx, egui::Context::default());
    std::thread::sleep(Duration::from_millis(500));

    // A shell prompt or editor polling git: reads every 20ms.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let (d, stop) = (d.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                git(&d, &["rev-parse", "HEAD"]);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    git(&d, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let c = next_change(&rx, Duration::from_secs(3));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    poller.join().unwrap();
    assert!(c.is_some_and(|c| c.refs), "{c:?}");
}

#[test]
fn continuous_changes_are_reported_within_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().canonicalize().unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.txt"), "0\n").unwrap();
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "init"]);
    let (tx, rx) = unbounded();
    let runner = GitRunner::new(&d).with_env("GIT_CONFIG_GLOBAL", "/dev/null");
    let _w = RepoWatcher::start(d.clone(), d.join(".git"), d.join(".git"), runner, tx, egui::Context::default());
    std::thread::sleep(Duration::from_millis(500));

    // A tracked file rewritten every 100ms (never quiet for 300ms).
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (d, stop) = (d.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i += 1;
                std::fs::write(d.join("a.txt"), format!("{i}\n")).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let start = std::time::Instant::now();
    let c = next_change(&rx, Duration::from_secs(4));
    let took = start.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    assert!(c.is_some_and(|c| c.status), "{c:?}");
    assert!(took < Duration::from_millis(2600), "{took:?}");
}
