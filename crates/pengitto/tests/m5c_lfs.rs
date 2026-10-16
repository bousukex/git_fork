//! Git LFS in the app (M5c): files marked in the lists, their diffs shown
//! as objects, the banner when git-lfs isn't installed, and the hint when
//! a checkout fails for want of it.

mod common;

use std::path::Path;

use common::{git, idle, poll_until, settle};
use egui_kittest::{Harness, kittest::Queryable as _};
use pengitto::{
    app::PengittoApp,
    repo::{RepoTab, Side},
};

fn pointer(c: char, size: u64) -> String {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {size}\n", c.to_string().repeat(64))
}

/// `*.bin` through Git LFS; big.bin a pointer (and a plain file); a branch
/// `other` with another pointer.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join(".gitattributes"), "*.bin filter=lfs diff=lfs merge=lfs -text\n").unwrap();
    std::fs::write(d.join("big.bin"), pointer('a', 12345)).unwrap();
    std::fs::write(d.join("plain.txt"), "a\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    git(d, &["checkout", "-q", "-b", "other"], &mut t);
    std::fs::write(d.join("big.bin"), pointer('b', 99999)).unwrap();
    git(d, &["commit", "-qam", "bigger"], &mut t);
    git(d, &["checkout", "-q", "main"], &mut t);
    dir
}

fn open_tab(d: &Path) -> RepoTab {
    let mut tab = RepoTab::open(d, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.history.loading && t.lfs.installed.is_some()));
    tab
}

#[test]
fn lfs_files_are_marked_and_shown_as_objects() {
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    assert!(tab.lfs.used);
    let installed = pengitto_core::lfs::installed(&pengitto_core::GitRunner::new(d));
    assert_eq!(tab.lfs_missing(), !installed);
    // Changed, then staged: marked in either list.
    std::fs::write(d.join("big.bin"), pointer('c', 7)).unwrap();
    std::fs::write(d.join("plain.txt"), "b\n").unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.is_lfs(Side::Unstaged, b"big.bin")));
    assert!(!tab.is_lfs(Side::Unstaged, b"plain.txt"));
    let f = tab.list(Side::Unstaged).into_iter().find(|f| f.path == "big.bin").unwrap();
    tab.click_change(Side::Unstaged, &f, false, false);
    assert!(poll_until(&mut tab, |t| matches!(&t.changes_diff.doc, Some(Ok(doc)) if doc.source.lfs.is_some())));
    tab.stage(vec![f]);
    assert!(poll_until(&mut tab, |t| idle(t) && t.is_lfs(Side::Staged, b"big.bin")));
    // A commit's files.
    let other = pengitto_core::model::Oid::from_hex(
        std::process::Command::new("git").current_dir(d).args(["rev-parse", "other"]).output().unwrap().stdout.trim_ascii(),
    )
    .unwrap();
    tab.select_commit(other);
    assert!(poll_until(&mut tab, |t| t.is_lfs_in_commit(b"big.bin")));
    assert!(!tab.is_lfs_in_commit(b"plain.txt"));
}

#[test]
fn a_checkout_failing_for_git_lfs_says_how_to_fix_it() {
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    // As if git-lfs had been set up once, but isn't there (where the
    // filter says it is).
    let bin = tempfile::tempdir().unwrap();
    let missing = bin.path().join("git-lfs");
    let mut t = 1_760_000_000;
    git(d, &["config", "filter.lfs.required", "true"], &mut t);
    git(d, &["config", "filter.lfs.process", &format!("{} filter-process", missing.display())], &mut t);
    tab.checkout_branch("other");
    assert!(poll_until(&mut tab, |t| t.errors.iter().any(|e| e.starts_with("Checkout failed"))));
    let checkout = tab.errors.iter().find(|e| e.starts_with("Checkout failed")).unwrap();
    assert!(checkout.contains("install git-lfs"), "{checkout}");
    // Reading the status fails too (the file needs the filter): same hint.
    assert!(poll_until(&mut tab, |t| t.errors.iter().any(|e| e.starts_with("Failed to read status") && e.contains("install git-lfs"))));
}

#[test]
fn a_git_lfs_that_runs_and_fails_gets_no_install_hint() {
    let repo = repo();
    let d = repo.path();
    let mut tab = open_tab(d);
    // Installed, but the download fails.
    let bin = tempfile::tempdir().unwrap();
    let lfs = bin.path().join("git-lfs");
    std::fs::write(&lfs, "#!/bin/sh\ncat >/dev/null\necho 'Smudge error: batch request: missing credentials' >&2\nexit 2\n").unwrap();
    std::fs::set_permissions(&lfs, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut t = 1_760_000_000;
    git(d, &["config", "filter.lfs.required", "true"], &mut t);
    git(d, &["config", "filter.lfs.smudge", &format!("{} smudge -- %f", lfs.display())], &mut t);
    // (The files here are pointers: cleaning keeps them as they are.)
    git(d, &["config", "filter.lfs.clean", "cat"], &mut t);
    tab.checkout_branch("other");
    assert!(poll_until(&mut tab, |t| t.errors.iter().any(|e| e.starts_with("Checkout failed"))));
    let checkout = tab.errors.iter().find(|e| e.starts_with("Checkout failed")).unwrap();
    assert!(checkout.contains("missing credentials") && checkout.contains("git-lfs") && checkout.contains("filter"), "{checkout}");
    assert!(!checkout.contains("install git-lfs"), "{checkout}");
}

#[test]
fn marks_go_by_each_lists_own_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    std::fs::write(d.join("a.txt"), "a\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "no lfs yet"], &mut t);
    let mut tab = open_tab(d);
    assert!(!tab.lfs.used);
    // Git LFS brought in by the working tree (HEAD knows nothing of it).
    std::fs::write(d.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
    std::fs::write(d.join("big.bin"), pointer('a', 10)).unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.is_lfs(Side::Unstaged, b"big.bin")));
    assert!(!tab.lfs.used);
    git(d, &["add", "-A"], &mut t);
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && t.is_lfs(Side::Staged, b"big.bin")));
    git(d, &["commit", "-qm", "lfs"], &mut t);
    let with_lfs = rev(d, "HEAD");
    // Renamed out of it: the row is marked by what it was.
    git(d, &["mv", "big.bin", "big.dat"], &mut t);
    git(d, &["commit", "-qm", "out of lfs"], &mut t);
    let renamed = rev(d, "HEAD");
    // No longer used: a change to it isn't marked (nothing stale).
    git(d, &["rm", "-q", ".gitattributes"], &mut t);
    git(d, &["commit", "-qm", "no lfs"], &mut t);
    std::fs::write(d.join("big.dat"), pointer('b', 11)).unwrap();
    tab.refresh();
    assert!(poll_until(&mut tab, |t| idle(t) && !t.lfs.used && t.status.unstaged.iter().any(|f| f.path == "big.dat")));
    assert!(poll_until(&mut tab, |t| t.lfs.unstaged.is_empty()));
    // Commits that used it, seen with HEAD not using it.
    tab.select_commit(with_lfs);
    assert!(poll_until(&mut tab, |t| t.is_lfs_in_commit(b"big.bin")));
    tab.select_commit(renamed);
    assert!(poll_until(&mut tab, |t| t.is_lfs_in_commit(b"big.dat")));
}

fn rev(d: &Path, r: &str) -> pengitto_core::model::Oid {
    let out = std::process::Command::new("git").current_dir(d).args(["rev-parse", r]).output().unwrap();
    pengitto_core::model::Oid::from_hex(out.stdout.trim_ascii()).unwrap()
}

#[test]
fn the_banner_and_the_diff_on_screen() {
    let repo = repo();
    let d = repo.path();
    std::fs::write(d.join("big.bin"), pointer('c', 2048)).unwrap();
    let paths = vec![d.to_path_buf()];
    let mut h = Harness::builder().with_size([1300.0, 820.0]).build_eframe(move |cc| {
        let mut app = PengittoApp::new(cc, paths);
        app.show_local_changes();
        app
    });
    settle(&mut h);
    let missing = h.state_mut().active_tab_mut().unwrap().lfs_missing();
    assert_eq!(h.query_by_label_contains("uses Git LFS, but git-lfs isn't installed").is_some(), missing);
    h.get_by_label("unstaged file big.bin").click();
    settle(&mut h);
    assert!(h.query_by_label("Git LFS object").is_some());
    assert!(h.query_by_label_contains("object cccccccccccc · 2.0 KiB").is_some());
    if missing {
        h.get_by_label("Dismiss").click();
        h.run_steps(2);
        assert!(h.query_by_label_contains("uses Git LFS, but git-lfs isn't installed").is_none());
    }
}
