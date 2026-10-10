//! Shared helpers for UI tests.

#![allow(dead_code)]

use std::process::Command;

use egui_kittest::Harness;

/// Lets background git jobs finish while the UI keeps polling.
pub fn settle<S>(h: &mut Harness<'_, S>) {
    for _ in 0..40 {
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    h.run_steps(4);
}

pub fn git(dir: &std::path::Path, args: &[&str], t: &mut i64) {
    *t += 3600;
    let date = format!("@{t} +0900");
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "山田 太郎")
        .env("GIT_AUTHOR_EMAIL", "taro@example.com")
        .env("GIT_COMMITTER_NAME", "山田 太郎")
        .env("GIT_COMMITTER_EMAIL", "taro@example.com")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A small repository with branches, a merge, tags and local changes.
pub fn sample_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    let write = |name: &str, body: &str| std::fs::write(d.join(name), body).unwrap();
    git(d, &["init", "-q", "-b", "main"], &mut t);
    // Commits made through the app use the repository's own config.
    git(d, &["config", "user.name", "山田 太郎"], &mut t);
    git(d, &["config", "user.email", "taro@example.com"], &mut t);
    write("README.md", "# Sample\n\nA sample project.\n");
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "Initial commit"], &mut t);
    write("main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "Add main.rs"], &mut t);
    git(d, &["tag", "v0.1.0"], &mut t);
    git(d, &["checkout", "-q", "-b", "feature/日本語"], &mut t);
    write("main.rs", "fn main() {\n    println!(\"こんにちは\");\n    // greet\n}\n");
    git(d, &["commit", "-qam", "挨拶を日本語にする"], &mut t);
    write("lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "Add lib.rs with add()"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    write("README.md", "# Sample\n\nA sample project.\n\n## Usage\n\nRun it.\n");
    git(d, &["commit", "-qam", "Document usage in README"], &mut t);
    git(d, &["merge", "-q", "--no-ff", "-m", "Merge branch 'feature/日本語'", "feature/日本語"], &mut t);
    git(d, &["checkout", "-q", "-b", "wip"], &mut t);
    write("main.rs", "fn main() {\n    println!(\"こんにちは、世界\");\n}\n");
    git(d, &["commit", "-qam", "WIP: 世界に挨拶"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    git(d, &["tag", "-a", "v0.2.0", "-m", "v0.2.0"], &mut t);
    write("README.md", "# Sample\n\nA sample project.\n\n## Usage\n\nRun it with cargo.\n");
    write("新規ファイル.txt", "未追跡\n");
    dir
}


/// Polls a tab until `done` (10s timeout); returns whether it happened.
pub fn poll_until(tab: &mut tine::repo::RepoTab, mut done: impl FnMut(&tine::repo::RepoTab) -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(10) {
        tab.poll();
        if done(tab) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// Loaded and no operation (or its refresh) pending.
pub fn idle(tab: &tine::repo::RepoTab) -> bool {
    !tab.history.loading && tab.history.elapsed.is_some() && tab.status_loaded && tab.refs.head.is_some() && !tab.busy()
}

/// main and topic diverge on f.txt (base → "main" / "topic"); g.txt is a
/// separate tracked file for local edits.
pub fn diverged_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "山田 太郎"], &mut t);
    git(d, &["config", "user.email", "taro@example.com"], &mut t);
    std::fs::write(d.join("f.txt"), "base\n").unwrap();
    std::fs::write(d.join("g.txt"), "g\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    git(d, &["checkout", "-q", "-b", "topic"], &mut t);
    std::fs::write(d.join("f.txt"), "topic\n").unwrap();
    git(d, &["commit", "-qam", "topic change"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    std::fs::write(d.join("f.txt"), "main\n").unwrap();
    git(d, &["commit", "-qam", "main change"], &mut t);
    dir
}
