//! Worktrees (M5c): listing them (any path), adding, removing, locking,
//! pruning, and what a removal would lose — against real git.

mod common;

use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use common::TestRepo;
use tine_core::{
    RepoPaths,
    model::RepoState,
    worktree::{self, BranchLeft, Changed, Force, Losses, NewWorktree, Worktree},
};

/// A repository with one commit, and a scratch folder next to it for
/// worktrees.
fn repo() -> (TestRepo, tempfile::TempDir) {
    let repo = TestRepo::new();
    repo.commit_file("f.txt", "a\n", "base");
    (repo, tempfile::tempdir().unwrap())
}

fn find<'a>(list: &'a [Worktree], path: &Path) -> &'a Worktree {
    list.iter().find(|w| worktree::same_place(&w.path, path)).unwrap_or_else(|| panic!("{} not listed", path.display()))
}

#[test]
fn lists_worktrees_with_any_path() {
    let (repo, dir) = repo();
    let git = repo.runner();
    let odd = dir.path().join("new\nline");
    let latin = dir.path().join(OsStr::from_bytes(b"caf\xe9"));
    worktree::add(&git, &odd, &NewWorktree::NewBranch { name: "nl".into(), start: "HEAD".into() }).unwrap();
    worktree::add(&git, &latin, &NewWorktree::Detached { commit: "HEAD".into() }).unwrap();
    let gone = dir.path().join("gone");
    worktree::add(&git, &gone, &NewWorktree::NewBranch { name: "gone".into(), start: "HEAD".into() }).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let list = worktree::list(&git).unwrap();
    assert_eq!(list.len(), 4);
    assert!(list[0].main && worktree::same_place(&list[0].path, repo.path()));
    assert_eq!(find(&list, &odd).branch_name(), Some("nl"));
    assert!(find(&list, &latin).detached);
    assert!(find(&list, &gone).prunable.is_some());
    // Locked, with a reason on two lines, and without one.
    worktree::lock(&git, find(&list, &odd), "on usb\ndisk").unwrap();
    worktree::lock(&git, find(&list, &latin), "").unwrap();
    let list = worktree::list(&git).unwrap();
    assert_eq!(find(&list, &odd).locked.as_deref(), Some("on usb\ndisk"));
    assert_eq!(find(&list, &latin).locked.as_deref(), Some(""));
    worktree::unlock(&git, find(&list, &latin)).unwrap();
    assert!(find(&worktree::list(&git).unwrap(), &latin).locked.is_none());
    // Opened from a path with a line break in it.
    let paths = RepoPaths::discover(&odd).unwrap();
    assert!(worktree::same_place(&paths.workdir, &odd), "{:?}", paths.workdir);
    assert!(worktree::same_place(&paths.common_dir, &repo.path().join(".git")));
    // Branches checked out elsewhere.
    let mut other = tine_core::refs::branches_in_other_worktrees(&git, repo.path()).unwrap();
    other.sort();
    assert_eq!(other, ["gone", "nl"]);
}

#[test]
fn adding_checks_out_what_was_asked() {
    let (repo, dir) = repo();
    let git = repo.runner();
    repo.git(["branch", "feature"]);
    let a = dir.path().join("a");
    worktree::add(&git, &a, &NewWorktree::Existing { branch: "feature".into() }).unwrap();
    assert_eq!(find(&worktree::list(&git).unwrap(), &a).branch_name(), Some("feature"));
    // Not again elsewhere (git refuses); a relative path isn't taken.
    assert!(worktree::add(&git, &dir.path().join("b"), &NewWorktree::Existing { branch: "feature".into() }).is_err());
    assert!(worktree::add(&git, Path::new("rel"), &NewWorktree::Detached { commit: "HEAD".into() }).is_err());
    // The folder taken: the new branch is made, the worktree isn't.
    let taken = dir.path().join("taken");
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("x"), "x").unwrap();
    let e = worktree::add(&git, &taken, &NewWorktree::NewBranch { name: "left".into(), start: "HEAD".into() }).unwrap_err();
    let left = e.downcast_ref::<BranchLeft>().unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(left.branch, "left");
    assert!(format!("{e}").contains("already exists"), "{e}");
    assert_eq!(repo.git(["branch", "--list", "left"]), "left");
    // A branch that was there already isn't reported as left behind.
    let e = worktree::add(&git, &taken, &NewWorktree::NewBranch { name: "feature".into(), start: "HEAD".into() }).unwrap_err();
    assert!(e.downcast_ref::<BranchLeft>().is_none());
    // A post-checkout hook failing: git fails, but the worktree is there
    // (with its branch), and is told as added.
    let hook = repo.path().join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\necho checkout hook said no >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let hooked = dir.path().join("hooked");
    let added = worktree::add(&git, &hooked, &NewWorktree::NewBranch { name: "hooked".into(), start: "HEAD".into() }).unwrap();
    assert!(added.warning.as_deref().is_some_and(|w| w.contains("checkout hook said no")), "{added:?}");
    assert_eq!(find(&worktree::list(&git).unwrap(), &hooked).branch_name(), Some("hooked"));
    assert!(hooked.join("f.txt").exists());
    let detached = dir.path().join("hooked-detached");
    assert!(worktree::add(&git, &detached, &NewWorktree::Detached { commit: "HEAD".into() }).unwrap().warning.is_some());
    std::fs::remove_file(&hook).unwrap();
    assert!(worktree::add(&git, &dir.path().join("fine"), &NewWorktree::Detached { commit: "HEAD".into() }).unwrap().warning.is_none());
}

#[test]
fn removing_goes_only_as_far_as_allowed() {
    let (repo, dir) = repo();
    let git = repo.runner();
    let list = worktree::list(&git).unwrap();
    // Never the main one (refused before git is asked; git refuses too).
    let ours = |r: anyhow::Result<()>, what: &str| {
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains(&format!("the main worktree can't be {what}")), "{e}");
    };
    ours(worktree::remove(&git, &list[0], Force { discard: true, locked: true }), "removed");
    ours(worktree::lock(&git, &list[0], ""), "locked");
    ours(worktree::unlock(&git, &list[0]), "unlocked");
    let at = |name: &str, branch: &str| {
        let p = dir.path().join(name);
        worktree::add(&git, &p, &NewWorktree::NewBranch { name: branch.into(), start: "HEAD".into() }).unwrap();
        find(&worktree::list(&git).unwrap(), &p).clone()
    };
    // Changes: only by force.
    let w = at("dirty", "d");
    std::fs::write(w.path.join("new.txt"), "x").unwrap();
    let l = worktree::losses(&git, &w).unwrap();
    assert_eq!(l.changed, ["new.txt"]);
    assert!(l.needs_force());
    assert!(worktree::remove(&git, &w, Force::default()).is_err());
    assert!(w.path.exists());
    worktree::remove(&git, &w, Force { discard: true, locked: false }).unwrap();
    assert!(!w.path.exists());
    // Ignored files go without force (so they are listed).
    let w = at("ignored", "i");
    std::fs::write(w.path.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(w.path.join("x.log"), "l").unwrap();
    let l = worktree::losses(&git, &w).unwrap();
    assert_eq!((l.ignored.clone(), l.changed.clone()), (vec!["x.log".into()], vec![".gitignore".into()]));
    // Locked and clean: unlocked, then removed.
    let w = at("locked", "l");
    worktree::lock(&git, &w, "usb").unwrap();
    let w = find(&worktree::list(&git).unwrap(), &w.path).clone();
    assert!(worktree::remove(&git, &w, Force::default()).is_err());
    worktree::remove(&git, &w, Force { discard: false, locked: true }).unwrap();
    assert!(!w.path.exists());
    // Locked with changes: git refuses after the unlock, which is undone
    // (the same reason, line break and all).
    let locked_dirty = |name: &str, reason: &str| {
        let w = at(name, name);
        worktree::lock(&git, &w, reason).unwrap();
        std::fs::write(w.path.join("f.txt"), "changed\n").unwrap();
        find(&worktree::list(&git).unwrap(), &w.path).clone()
    };
    let w = locked_dirty("ld", "on usb\ndisk");
    let e = format!("{:#}", worktree::remove(&git, &w, Force { discard: false, locked: true }).unwrap_err());
    assert!(e.contains("it is locked again"), "{e}");
    let now = find(&worktree::list(&git).unwrap(), &w.path).clone();
    assert_eq!(now.locked.as_deref(), Some("on usb\ndisk"));
    assert_eq!(worktree::what_is_left(&git, &w), "nothing was removed");
    // Both forces remove a locked one with changes (it is locked: one
    // force wouldn't do).
    let mut once = std::process::Command::new("git");
    once.current_dir(repo.path()).args(["worktree", "remove", "--force"]).arg(&now.path);
    assert!(!once.output().unwrap().status.success(), "one force isn't enough for a locked worktree");
    worktree::remove(&git, &now, Force { discard: true, locked: true }).unwrap();
    assert!(!now.path.exists());
    // Unlocked behind its back: said so.
    let w = locked_dirty("ld2", "");
    worktree::unlock(&git, &w).unwrap();
    assert_eq!(worktree::what_is_left(&git, &w), "nothing was removed, but it is no longer locked");
}

#[test]
fn removing_only_what_was_confirmed() {
    let (repo, dir) = repo();
    let git = repo.runner();
    let p = dir.path().join("wt");
    worktree::add(&git, &p, &NewWorktree::NewBranch { name: "wt".into(), start: "HEAD".into() }).unwrap();
    std::fs::write(p.join("draft.txt"), "x").unwrap();
    let w = find(&worktree::list(&git).unwrap(), &p).clone();
    let confirmed = worktree::losses(&git, &w).unwrap();
    assert_eq!(confirmed.changed, ["draft.txt"]);
    let force = Force { discard: confirmed.needs_force(), locked: false };
    let refused = |r: anyhow::Result<()>| {
        let e = r.unwrap_err();
        assert!(e.downcast_ref::<Changed>().is_some(), "{e:#}");
        assert!(p.join("draft.txt").exists(), "nothing removed");
    };
    // A new file, an ignored one, the lock, HEAD: each changes what goes.
    std::fs::write(p.join("later.txt"), "y").unwrap();
    refused(worktree::remove_as_confirmed(&git, &w, &confirmed, force));
    std::fs::remove_file(p.join("later.txt")).unwrap();
    std::fs::create_dir_all(repo.path().join(".git/info")).unwrap();
    std::fs::write(repo.path().join(".git/info/exclude"), "*.log\n").unwrap();
    std::fs::write(p.join("build.log"), "l").unwrap();
    refused(worktree::remove_as_confirmed(&git, &w, &confirmed, force));
    std::fs::remove_file(p.join("build.log")).unwrap();
    worktree::lock(&git, &w, "").unwrap();
    refused(worktree::remove_as_confirmed(&git, &w, &confirmed, force));
    worktree::unlock(&git, &w).unwrap();
    let (ok, _, err) = TestRepo::try_git_in(&p, ["commit", "-q", "--allow-empty", "-m", "more"]);
    assert!(ok, "{err}");
    refused(worktree::remove_as_confirmed(&git, &w, &confirmed, force));
    // As confirmed: removed.
    let w = find(&worktree::list(&git).unwrap(), &p).clone();
    worktree::remove_as_confirmed(&git, &w, &confirmed, force).unwrap();
    assert!(!p.exists());
    // Gone meanwhile: refused too.
    let e = worktree::remove_as_confirmed(&git, &w, &confirmed, force).unwrap_err();
    assert!(format!("{e}").contains("no longer lists it"), "{e}");
}

#[test]
fn commits_only_a_detached_head_has() {
    let (repo, dir) = repo();
    let git = repo.runner();
    let p = dir.path().join("detached");
    worktree::add(&git, &p, &NewWorktree::Detached { commit: "HEAD".into() }).unwrap();
    let at = || find(&worktree::list(&git).unwrap(), &p).clone();
    assert_eq!(worktree::losses(&git, &at()).unwrap(), Losses::default());
    for m in ["one", "two"] {
        let (ok, _, err) = TestRepo::try_git_in(&p, ["commit", "-q", "--allow-empty", "-m", m]);
        assert!(ok, "{err}");
    }
    let l = worktree::losses(&git, &at()).unwrap();
    assert_eq!(l.unreferenced, 2);
    assert!(l.any() && !l.needs_force());
    // A tag keeps one; another worktree at the tip keeps both.
    repo.git(["tag", "keep", &format!("{}~1", at().head.unwrap().to_hex())]);
    assert_eq!(worktree::losses(&git, &at()).unwrap().unreferenced, 1);
    worktree::add(&git, &dir.path().join("other"), &NewWorktree::Detached { commit: at().head.unwrap().to_hex() }).unwrap();
    assert_eq!(worktree::losses(&git, &at()).unwrap().unreferenced, 0);
}

#[test]
fn what_a_removal_would_lose() {
    let (repo, dir) = repo();
    let git = repo.runner();
    // A submodule checked out in the worktree.
    let sub = TestRepo::new();
    sub.commit_file("s.txt", "s\n", "s");
    let (ok, _, err) = repo.try_git(["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub.path().to_str().unwrap(), "sub"]);
    assert!(ok, "{err}");
    repo.git(["commit", "-q", "-m", "add sub"]);
    let p = dir.path().join("with-sub");
    worktree::add(&git, &p, &NewWorktree::NewBranch { name: "s".into(), start: "HEAD".into() }).unwrap();
    let w = find(&worktree::list(&git).unwrap(), &p).clone();
    assert!(worktree::losses(&git, &w).unwrap().submodules.is_empty(), "not checked out yet");
    let (ok, _, err) = TestRepo::try_git_in(&p, ["-c", "protocol.file.allow=always", "submodule", "update", "-q", "--init"]);
    assert!(ok, "{err}");
    let l = worktree::losses(&git, &w).unwrap();
    assert_eq!(l.submodules, ["sub"]);
    assert!(l.needs_force());
    assert!(worktree::remove(&git, &w, Force::default()).is_err(), "git refuses without force");
    // A rebase stopped in it.
    let p = dir.path().join("rebasing");
    worktree::add(&git, &p, &NewWorktree::NewBranch { name: "r".into(), start: "HEAD".into() }).unwrap();
    let (ok, _, err) = TestRepo::try_git_in(&p, ["-c", "sequence.editor=sed -i 1s/^pick/edit/", "rebase", "-q", "-i", "HEAD~"]);
    assert!(ok, "{err}");
    let w = find(&worktree::list(&git).unwrap(), &p).clone();
    assert_eq!(worktree::losses(&git, &w).unwrap().state, RepoState::Rebasing);
    // Gone already: only git's record is left; prune takes it.
    let p = dir.path().join("gone");
    worktree::add(&git, &p, &NewWorktree::Detached { commit: "HEAD".into() }).unwrap();
    std::fs::remove_dir_all(&p).unwrap();
    let w = find(&worktree::list(&git).unwrap(), &p).clone();
    assert!(worktree::losses(&git, &w).unwrap().missing);
    let preview = worktree::prune_preview(&git).unwrap();
    assert_eq!(preview.len(), 1);
    assert!(preview[0].contains("gone"), "{preview:?}");
    worktree::prune(&git).unwrap();
    assert!(worktree::list(&git).unwrap().iter().all(|x| !worktree::same_place(&x.path, &p)));
    let _ = PathBuf::new();
}

// --- Git LFS (shown only; git-lfs needn't be installed) ---------------------

use tine_core::{
    diff::{self, DiffOptions, DiffSource},
    lfs::{self, AttrSource, LfsSide},
    model::{ChangeKind, ChangedFile},
};

fn pointer(oid_char: char, size: u64) -> String {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {size}\n", oid_char.to_string().repeat(64))
}

fn changed(path: &str, kind: ChangeKind) -> ChangedFile {
    ChangedFile { path: path.into(), orig_path: None, kind, is_submodule: false }
}

fn lfs_diff(repo: &TestRepo, source: DiffSource, f: &ChangedFile) -> tine_core::model::FileDiff {
    diff::file_diff(&repo.runner(), source, f, DiffOptions { context_lines: 3, ignore_whitespace: false }).unwrap()
}

fn head(repo: &TestRepo) -> tine_core::model::Oid {
    tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD"]).as_bytes()).unwrap()
}

#[test]
fn which_files_go_through_lfs_where() {
    let repo = TestRepo::new();
    let git = repo.runner();
    assert!(!lfs::repo_uses_lfs(&git).unwrap(), "no commit yet");
    repo.commit_file("a.txt", "a\n", "base");
    assert!(!lfs::repo_uses_lfs(&git).unwrap());
    // Only a nested .gitattributes says so.
    repo.commit_file("assets/.gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n", "lfs for assets");
    let first = head(&repo);
    assert!(lfs::repo_uses_lfs(&git).unwrap());
    repo.commit_file("assets/.gitattributes", "*.dat filter=lfs\n", "dat instead");
    let second = head(&repo);
    let paths: [&[u8]; 3] = [b"assets/x.bin", b"assets/y.dat", b"a.txt"];
    let at = |s| lfs::tracked(&git, &paths, s).unwrap();
    assert_eq!(at(AttrSource::Commit(first)), [bstr::BString::from("assets/x.bin")].into());
    assert_eq!(at(AttrSource::Commit(second)), [bstr::BString::from("assets/y.dat")].into());
    // Staged and not: the index and the working tree can differ.
    repo.write("assets/.gitattributes", "*.png filter=lfs\n");
    repo.git(["add", "assets/.gitattributes"]);
    repo.write("assets/.gitattributes", "*.bin filter=lfs\n");
    assert_eq!(lfs::tracked(&git, &[b"assets/p.png"], AttrSource::Index).unwrap().len(), 1);
    assert!(lfs::tracked(&git, &[b"assets/p.png"], AttrSource::WorkTree).unwrap().is_empty());
    assert_eq!(lfs::tracked(&git, &[b"assets/x.bin"], AttrSource::WorkTree).unwrap().len(), 1);
    // git-lfs there or not (a stand-in git that has it, or hasn't).
    let fake = tempfile::tempdir().unwrap();
    for (name, code) in [("with", 0), ("without", 1)] {
        let f = fake.path().join(name);
        std::fs::write(&f, format!("#!/bin/sh\ncase \"$*\" in *\"lfs version\") exit {code};; esac\nexec git \"$@\"\n")).unwrap();
        std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(lfs::installed(&git.clone().with_program(&f)), code == 0, "{name}");
    }
}

#[test]
fn pointer_diffs_show_the_objects() {
    let repo = TestRepo::new();
    repo.commit_file(".gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n", "lfs");
    // Added, changed, deleted, each in a commit.
    repo.commit_file("big.bin", &pointer('a', 12345), "add");
    let added = head(&repo);
    repo.commit_file("big.bin", &pointer('b', 99999), "change");
    let changed_at = head(&repo);
    repo.git(["rm", "-q", "big.bin"]);
    repo.git(["commit", "-q", "-m", "delete"]);
    let deleted = head(&repo);
    let parent = |id| tine_core::model::Oid::from_hex(repo.git(["rev-parse", &format!("{id}^")]).as_bytes());
    let commit = |id| DiffSource::Commit { id, parent: parent(id) };
    let v = lfs_diff(&repo, commit(added), &changed("big.bin", ChangeKind::Added)).lfs.unwrap();
    assert_eq!(v.old, LfsSide::Absent);
    let LfsSide::Pointer(p) = &v.new else { panic!("{v:?}") };
    assert_eq!((p.short_oid(), p.size), ("aaaaaaaaaaaa", 12345));
    let v = lfs_diff(&repo, commit(changed_at), &changed("big.bin", ChangeKind::Modified)).lfs.unwrap();
    assert!(matches!((&v.old, &v.new), (LfsSide::Pointer(a), LfsSide::Pointer(b)) if a.size == 12345 && b.size == 99999));
    let v = lfs_diff(&repo, commit(deleted), &changed("big.bin", ChangeKind::Deleted)).lfs.unwrap();
    assert_eq!(v.new, LfsSide::Absent);

    // Staged, and the working tree with the file's contents (smudged).
    repo.commit_file("big.bin", &pointer('c', 5), "back");
    repo.write("big.bin", &pointer('d', 6));
    repo.git(["add", "big.bin"]);
    let v = lfs_diff(&repo, DiffSource::Staged, &changed("big.bin", ChangeKind::Modified)).lfs.unwrap();
    assert!(matches!((&v.old, &v.new), (LfsSide::Pointer(a), LfsSide::Pointer(b)) if a.size == 5 && b.size == 6));
    repo.write("big.bin", "the real contents, not a pointer\n");
    let v = lfs_diff(&repo, DiffSource::Unstaged, &changed("big.bin", ChangeKind::Modified)).lfs.unwrap();
    assert!(matches!(v.new, LfsSide::Contents { size: 33 }), "{v:?}");

    // With an extension: still a pointer.
    let ext = format!(
        "version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{}\noid sha256:{}\nsize 7\n",
        "e".repeat(64),
        "f".repeat(64)
    );
    repo.write("big.bin", &ext);
    let v = lfs_diff(&repo, DiffSource::Unstaged, &changed("big.bin", ChangeKind::Modified)).lfs.unwrap();
    assert!(matches!(&v.new, LfsSide::Pointer(p) if p.extensions.len() == 1));
}

#[test]
fn what_isnt_a_pointer_diffs_as_text() {
    let repo = TestRepo::new();
    repo.commit_file(".gitattributes", "*.bin filter=lfs\n", "lfs");
    // A broken pointer (bad oid): the text, as it is.
    let broken = "version https://git-lfs.github.com/spec/v1\noid sha256:xyz\nsize 1\n";
    repo.commit_file("big.bin", broken, "broken");
    let id = head(&repo);
    let parent = tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD^"]).as_bytes());
    let d = lfs_diff(&repo, DiffSource::Commit { id, parent }, &changed("big.bin", ChangeKind::Added));
    assert!(d.lfs.is_none() && !d.hunks.is_empty());
    // A pointer-looking file that doesn't go through LFS.
    repo.commit_file("notes.txt", &pointer('a', 1), "looks like one");
    let id = head(&repo);
    let parent = tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD^"]).as_bytes());
    let d = lfs_diff(&repo, DiffSource::Commit { id, parent }, &changed("notes.txt", ChangeKind::Added));
    assert!(d.lfs.is_none());
    // A file that has nothing to do with it: no attribute lookups at all.
    repo.commit_file("plain.rs", "fn main() {}\n", "plain");
    let id = head(&repo);
    let parent = tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD^"]).as_bytes());
    let d = lfs_diff(&repo, DiffSource::Commit { id, parent }, &changed("plain.rs", ChangeKind::Added));
    assert!(!lfs::maybe_pointer_diff(&d) && d.lfs.is_none());
}

fn diff_with(repo: &TestRepo, source: DiffSource, f: &ChangedFile, context_lines: u32) -> tine_core::model::FileDiff {
    diff::file_diff(&repo.runner(), source, f, DiffOptions { context_lines, ignore_whitespace: false }).unwrap()
}

fn commit_of(repo: &TestRepo) -> DiffSource {
    let parent = tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD^"]).as_bytes());
    DiffSource::Commit { id: head(repo), parent }
}

#[test]
fn lfs_diffs_are_found_whatever_the_context() {
    let repo = TestRepo::new();
    repo.commit_file(".gitattributes", "*.bin filter=lfs\n", "lfs");
    // Many extensions: with 3 lines of context the version line is out of
    // sight; with none, only the changed lines show.
    let with_exts = |oid: char, size: u64| {
        let exts: String = (0..5).map(|i| format!("ext-{i}-x{i} sha256:{}\n", "e".repeat(64))).collect();
        format!("version https://git-lfs.github.com/spec/v1\n{exts}oid sha256:{}\nsize {size}\n", oid.to_string().repeat(64))
    };
    repo.commit_file("big.bin", &with_exts('a', 1), "add");
    repo.commit_file("big.bin", &with_exts('b', 2), "change");
    let f = changed("big.bin", ChangeKind::Modified);
    for context in [0, 3] {
        let d = diff_with(&repo, commit_of(&repo), &f, context);
        assert!(!d.hunks.iter().flat_map(|h| &h.lines).any(|l| l.content.starts_with(b"version")), "context {context}");
        let v = d.lfs.unwrap_or_else(|| panic!("context {context}"));
        assert!(matches!((&v.old, &v.new), (LfsSide::Pointer(a), LfsSide::Pointer(b)) if a.size == 1 && b.size == 2 && b.extensions.len() == 5));
    }
    // Only renamed: no lines at all.
    repo.git(["mv", "big.bin", "moved.bin"]);
    repo.git(["commit", "-q", "-m", "move"]);
    let renamed = ChangedFile { path: "moved.bin".into(), orig_path: Some("big.bin".into()), kind: ChangeKind::Renamed, is_submodule: false };
    let d = diff_with(&repo, commit_of(&repo), &renamed, 3);
    assert!(d.hunks.is_empty());
    assert!(matches!(d.lfs.map(|v| v.new), Some(LfsSide::Pointer(p)) if p.size == 2));
    // An empty file is an empty object's pointer.
    repo.commit_file("empty.bin", "", "empty");
    let v = diff_with(&repo, commit_of(&repo), &changed("empty.bin", ChangeKind::Added), 3).lfs.unwrap();
    assert!(matches!(v.new, LfsSide::Pointer(p) if p.size == 0));
}

#[test]
fn the_working_tree_side_shows_as_it_is_unless_it_is_plain() {
    let repo = TestRepo::new();
    repo.commit_file(".gitattributes", "*.bin filter=lfs\n", "lfs");
    repo.commit_file("big.bin", &pointer('a', 10), "add");
    let f = changed("big.bin", ChangeKind::Modified);
    // Small real contents (smudged): the object and the contents.
    repo.write("big.bin", "tiny\n");
    let v = lfs_diff(&repo, DiffSource::Unstaged, &f).lfs.unwrap();
    assert_eq!(v.new, LfsSide::Contents { size: 5 });
    // A broken pointer: its text, not "contents".
    repo.write("big.bin", "version https://git-lfs.github.com/spec/v1\noid sha256:nothex\nsize 3\n");
    let d = lfs_diff(&repo, DiffSource::Unstaged, &f);
    assert!(d.lfs.is_none() && !d.hunks.is_empty());
    // A symlink in its place: the diff as git has it.
    std::fs::remove_file(repo.path().join("big.bin")).unwrap();
    std::os::unix::fs::symlink("elsewhere", repo.path().join("big.bin")).unwrap();
    let d = lfs_diff(&repo, DiffSource::Unstaged, &changed("big.bin", ChangeKind::TypeChanged));
    assert!(d.lfs.is_none(), "{:?}", d.lfs);
    // Unreadable: an error, logged; the diff shows as git has it.
    std::fs::remove_file(repo.path().join("big.bin")).unwrap();
    repo.write("big.bin", &pointer('c', 1));
    let path = repo.path().join("big.bin");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_err() {
        assert!(lfs::view(&repo.runner(), DiffSource::Unstaged, &f).is_err());
    }
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
}

#[test]
fn marks_follow_each_row() {
    use lfs::{LfsRow, tracked_rows};
    let repo = TestRepo::new();
    let git = repo.runner();
    repo.commit_file(".gitattributes", "*.bin filter=lfs\n", "lfs");
    repo.commit_file("a.bin", &pointer('a', 1), "a");
    // Renamed out of LFS, and into it: each row is marked by its path.
    repo.git(["mv", "a.bin", "a.dat"]);
    repo.write("b.bin", &pointer('b', 1));
    repo.git(["add", "b.bin"]);
    repo.git(["commit", "-q", "-m", "renames"]);
    let id = head(&repo);
    let parent = tine_core::model::Oid::from_hex(repo.git(["rev-parse", "HEAD^"]).as_bytes());
    let source = DiffSource::Commit { id, parent };
    let rows = LfsRow::rows(
        source,
        None,
        &[
            ChangedFile { path: "a.dat".into(), orig_path: Some("a.bin".into()), kind: ChangeKind::Renamed, is_submodule: false },
            changed("b.bin", ChangeKind::Added),
            changed("c.txt", ChangeKind::Added),
        ],
    );
    let marked = tracked_rows(&git, &rows).unwrap();
    assert_eq!(marked, [bstr::BString::from("a.dat"), "b.bin".into()].into());
    // Taken out of LFS by its .gitattributes (same path): the old side
    // counts too — and is looked at only then.
    repo.commit_file("big.bin", &pointer('c', 1), "big");
    repo.write(".gitattributes", "*.txt diff\n");
    repo.write("big.bin", &pointer('d', 2));
    let modified = changed("big.bin", ChangeKind::Modified);
    let rows = LfsRow::rows(DiffSource::Unstaged, None, &[modified.clone(), changed(".gitattributes", ChangeKind::Modified)]);
    assert_eq!(rows[0].sides.len(), 2);
    assert!(tracked_rows(&git, &rows).unwrap().contains(&bstr::BString::from("big.bin")));
    let rows = LfsRow::rows(DiffSource::Unstaged, None, std::slice::from_ref(&modified));
    assert_eq!(rows[0].sides.len(), 1, "attributes unchanged: one side");
    repo.git(["checkout", "-q", "--", ".gitattributes"]);
    // LFS brought in by the working tree only (not committed yet): the
    // unstaged rows say so, the staged ones don't.
    repo.write(".gitattributes", "*.bin filter=lfs\n*.psd filter=lfs\n");
    repo.write("art.psd", "x");
    repo.git(["add", "art.psd"]);
    repo.write("art.psd", "y");
    let f = changed("art.psd", ChangeKind::Modified);
    let head_id = Some(head(&repo));
    let gitattributes = changed(".gitattributes", ChangeKind::Modified);
    assert_eq!(tracked_rows(&git, &LfsRow::rows(DiffSource::Unstaged, head_id, &[f, gitattributes])).unwrap().len(), 1);
    assert!(tracked_rows(&git, &LfsRow::rows(DiffSource::Staged, head_id, &[changed("art.psd", ChangeKind::Added)])).unwrap().is_empty());
}

#[test]
fn textconv_never_runs_for_a_diff() {
    let repo = TestRepo::new();
    let tools = tempfile::tempdir().unwrap();
    let ran = tools.path().join("textconv-ran");
    let conv = tools.path().join("conv.sh");
    std::fs::write(&conv, format!("#!/bin/sh\ntouch '{}'\nsed s/^/CONV:/ \"$1\"\n", ran.display())).unwrap();
    std::fs::set_permissions(&conv, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    repo.git(["config", "diff.conv.textconv", conv.to_str().unwrap()]);
    repo.commit_file(".gitattributes", "*.txt diff=conv\n", "attrs");
    repo.commit_file("f.txt", "a\n", "one");
    repo.commit_file("f.txt", "b\n", "two");
    repo.write("f.txt", "c\n");
    repo.write("new.txt", "n\n");
    let staged = repo.write("s.txt", "s\n");
    repo.git(["add", staged.to_str().unwrap()]);
    for (source, f) in [
        (commit_of(&repo), changed("f.txt", ChangeKind::Modified)),
        (DiffSource::Unstaged, changed("f.txt", ChangeKind::Modified)),
        (DiffSource::Unstaged, changed("new.txt", ChangeKind::Untracked)),
        (DiffSource::Staged, changed("s.txt", ChangeKind::Added)),
    ] {
        let d = lfs_diff(&repo, source, &f);
        assert!(!d.hunks.iter().flat_map(|h| &h.lines).any(|l| l.content.starts_with(b"CONV:")), "{source:?} {}", f.path);
        assert!(!ran.exists(), "textconv ran for {source:?} {}", f.path);
    }
}
