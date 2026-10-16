//! Linked worktrees: listing them (paths kept as bytes: any name works),
//! adding one, removing (never the main one), locking and pruning, and
//! what removing one would lose.

use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use bstr::{BString, ByteSlice as _};

use crate::{
    GitRunner,
    model::{Oid, RepoState},
    status::repo_state,
};

/// A worktree as `git worktree list` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<Oid>,
    /// The branch checked out (full ref; `None`: detached, or bare).
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// Locked, with the reason given ("" when none was).
    pub locked: Option<String>,
    /// Git would prune it, and why (its folder is gone…).
    pub prunable: Option<String>,
    /// The main worktree (listed first; never removed, locked or pruned).
    pub main: bool,
}

impl Worktree {
    /// The folder's name, for showing.
    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.branch.as_deref().map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
    }
}

/// Parses `git worktree list --porcelain -z`: NUL-ended lines, an empty one
/// after each worktree. Unknown lines are skipped (newer git may add some).
pub fn parse(out: &[u8]) -> Result<Vec<Worktree>> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    for rec in out.split(|&b| b == 0) {
        if rec.is_empty() {
            if let Some(w) = current.take() {
                list.push(w);
            }
            continue;
        }
        let (key, value) = match rec.find_byte(b' ') {
            Some(i) => (&rec[..i], Some(&rec[i + 1..])),
            None => (rec, None),
        };
        if key == b"worktree" {
            if let Some(w) = current.take() {
                list.push(w);
            }
            let path = value.filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("a worktree without a path"))?;
            current = Some(Worktree {
                path: PathBuf::from(OsString::from_vec(path.to_vec())),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                locked: None,
                prunable: None,
                main: list.is_empty(),
            });
            continue;
        }
        let w = current.as_mut().ok_or_else(|| anyhow!("unexpected `git worktree list` output"))?;
        let text = || value.map(|v| v.to_str_lossy().into_owned()).unwrap_or_default();
        match key {
            b"HEAD" => w.head = value.and_then(Oid::from_hex),
            b"branch" => w.branch = Some(text()),
            b"bare" => w.bare = true,
            b"detached" => w.detached = true,
            b"locked" => w.locked = Some(text()),
            b"prunable" => w.prunable = Some(text()),
            _ => {}
        }
    }
    if let Some(w) = current {
        list.push(w);
    }
    Ok(list)
}

pub fn list(git: &GitRunner) -> Result<Vec<Worktree>> {
    parse(&git.output(["worktree", "list", "--porcelain", "-z"])?)
}

/// What a new worktree checks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewWorktree {
    /// A new branch `name`, starting at `start`.
    NewBranch { name: String, start: String },
    /// A local branch (checked out nowhere else).
    Existing { branch: String },
    /// A commit, detached.
    Detached { commit: String },
}

/// The new branch was made, the worktree wasn't (the branch is still
/// there).
#[derive(Debug)]
pub struct BranchLeft {
    pub branch: String,
    pub error: anyhow::Error,
}

impl std::fmt::Display for BranchLeft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}; the branch '{}' was created anyway (it is still there)", self.error, self.branch)
    }
}

impl std::error::Error for BranchLeft {}

fn branch_exists(git: &GitRunner, name: &str) -> Result<bool> {
    Ok(git.command(["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")]).status()?.success())
}

/// A worktree that was added.
#[derive(Debug, Default)]
pub struct Added {
    /// git reported a failure after making it (a post-checkout hook
    /// failing, say): what it said.
    pub warning: Option<String>,
}

/// Adds a worktree at `path` (absolute; it must not exist, or be an empty
/// folder). git may fail after the worktree is there (a post-checkout hook
/// failing): that is told apart from a worktree that wasn't made, or only
/// partly.
pub fn add(git: &GitRunner, path: &Path, how: &NewWorktree) -> Result<Added> {
    if !path.is_absolute() {
        bail!("the worktree's location must be an absolute path");
    }
    let mut args: Vec<OsString> = vec!["worktree".into(), "add".into()];
    let mut new_branch = None;
    match how {
        NewWorktree::NewBranch { name, start } => {
            if !branch_exists(git, name)? {
                new_branch = Some(name);
            }
            args.extend(["-b".into(), name.into(), path.into(), start.into()]);
        }
        NewWorktree::Existing { branch } => args.extend([path.into(), branch.into()]),
        NewWorktree::Detached { commit } => args.extend(["--detach".into(), path.into(), commit.into()]),
    }
    let Err(error) = git.output(&args) else { return Ok(Added::default()) };
    let listed = list(git).map(|l| l.iter().any(|w| same_place(&w.path, path)));
    // A worktree's folder has a `.git` file pointing at its records.
    let checked_out = path.join(".git").is_file();
    let branch_left = new_branch.filter(|name| branch_exists(git, name).unwrap_or(false));
    match listed {
        Ok(true) if checked_out => Ok(Added { warning: Some(format!("{error:#}")) }),
        Ok(false) if !checked_out => match branch_left {
            Some(name) => Err(BranchLeft { branch: name.clone(), error }.into()),
            None => Err(error),
        },
        Ok(listed) => {
            let what = if listed {
                "git lists the new worktree, but its folder isn't checked out"
            } else {
                "its folder was checked out, but git doesn't list it"
            };
            let branch = branch_left.map(|b| format!("; the branch '{b}' was created")).unwrap_or_default();
            Err(error.context(format!("{what}{branch}")))
        }
        Err(e) => Err(error.context(format!("what is left of it can't be told ({e:#})"))),
    }
}

/// What may go when a worktree is removed anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Force {
    /// Changed, untracked or ignored files, submodules.
    pub discard: bool,
    /// It is locked.
    pub locked: bool,
}

fn not_main(w: &Worktree, what: &str) -> Result<()> {
    if w.main {
        bail!("the main worktree can't be {what}");
    }
    Ok(())
}

/// Removes worktree `w` (`git` must run somewhere else: the main worktree,
/// say). A locked one is unlocked first unless changes are to go too — and
/// locked again (with its reason) if git then refuses.
pub fn remove(git: &GitRunner, w: &Worktree, force: Force) -> Result<()> {
    not_main(w, "removed")?;
    let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
    let mut unlocked = None;
    match &w.locked {
        Some(reason) if force.locked && !force.discard => {
            unlock(git, w)?;
            unlocked = Some(reason);
        }
        Some(_) if force.locked => args.extend(["--force".into(), "--force".into()]),
        _ if force.discard => args.push("--force".into()),
        _ => {}
    }
    args.push(w.path.clone().into());
    let Err(error) = git.output(&args) else { return Ok(()) };
    if let Some(reason) = unlocked
        && list(git).is_ok_and(|l| l.iter().any(|x| same_place(&x.path, &w.path)))
    {
        return Err(match lock_with(git, &w.path, reason) {
            Ok(()) => error.context("it is locked again"),
            Err(e) => error.context(format!("it was unlocked to remove it, and locking it again failed ({e:#})")),
        });
    }
    Err(error)
}

/// Locks `w`, with `reason` (trimmed; none if empty).
pub fn lock(git: &GitRunner, w: &Worktree, reason: &str) -> Result<()> {
    not_main(w, "locked")?;
    lock_with(git, &w.path, reason.trim())
}

fn lock_with(git: &GitRunner, path: &Path, reason: &str) -> Result<()> {
    let mut args: Vec<OsString> = vec!["worktree".into(), "lock".into()];
    if !reason.is_empty() {
        args.extend(["--reason".into(), reason.into()]);
    }
    args.push(path.into());
    git.output(&args)?;
    Ok(())
}

pub fn unlock(git: &GitRunner, w: &Worktree) -> Result<()> {
    not_main(w, "unlocked")?;
    git.output([OsString::from("worktree"), "unlock".into(), w.path.clone().into()])?;
    Ok(())
}

/// What `git worktree prune` would remove (one line each, as git says it).
pub fn prune_preview(git: &GitRunner) -> Result<Vec<String>> {
    // git tells it on stderr.
    let out = git.command(["worktree", "prune", "--dry-run", "-v"]).output()?;
    if !out.status.success() {
        bail!("git worktree prune failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stderr).lines().filter(|l| !l.trim().is_empty()).map(str::to_owned).collect())
}

pub fn prune(git: &GitRunner) -> Result<()> {
    git.output(["worktree", "prune"])?;
    Ok(())
}

/// What removing a worktree would take with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Losses {
    /// Its folder is gone already (only git's record of it is left).
    pub missing: bool,
    /// Changed (staged or not) and untracked files.
    pub changed: Vec<BString>,
    /// Ignored files (removing deletes them too, without asking).
    pub ignored: Vec<BString>,
    /// Submodules checked out in it (git removes such a worktree only by
    /// force).
    pub submodules: Vec<BString>,
    /// Commits only its detached HEAD reaches (no branch, tag, remote
    /// branch or other worktree does): nothing refers to them afterwards.
    pub unreferenced: usize,
    /// An operation in progress there (a rebase…).
    pub state: RepoState,
}

impl Losses {
    /// Removing needs force (changes go, submodules).
    pub fn needs_force(&self) -> bool {
        !self.changed.is_empty() || !self.submodules.is_empty()
    }

    /// Something is lost by removing it (to be confirmed).
    pub fn any(&self) -> bool {
        self.needs_force() || !self.ignored.is_empty() || self.unreferenced > 0
    }
}

/// Looks at worktree `w`: changes, ignored files, submodules, commits only
/// its detached HEAD has, an operation in progress.
pub fn losses(git: &GitRunner, w: &Worktree) -> Result<Losses> {
    let mut l = Losses::default();
    if w.detached
        && let Some(head) = w.head
    {
        let mut args = vec!["rev-list".to_owned(), "--count".into(), head.to_hex(), "--not".into(), "--branches".into(), "--tags".into(), "--remotes".into()];
        args.extend(list(git)?.iter().filter(|x| !same_place(&x.path, &w.path)).filter_map(|x| x.head).map(|h| h.to_hex()));
        l.unreferenced = git.output(&args)?.trim().to_str_lossy().parse()?;
    }
    if !w.path.is_dir() {
        return Ok(Losses { missing: true, ..l });
    }
    let here = git.with_workdir(&w.path);
    let out = here.output(["status", "--porcelain=v1", "-z", "--ignored=matching", "--untracked-files=all"])?;
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        if f.len() < 4 {
            bail!("unexpected `git status` output");
        }
        let (xy, path) = (&f[..2], BString::from(&f[3..]));
        // A rename or copy has the old path next.
        if xy.contains(&b'R') || xy.contains(&b'C') {
            fields.next();
        }
        if xy == b"!!" {
            l.ignored.push(path);
        } else {
            l.changed.push(path);
        }
    }
    let staged = here.output(["ls-files", "-s", "-z"])?;
    for rec in staged.split(|&b| b == 0).filter(|r| r.starts_with(b"160000 ")) {
        if let Some((_, p)) = rec.split_once_str("\t")
            && w.path.join(OsStr::from_bytes(p)).join(".git").exists()
        {
            l.submodules.push(p.into());
        }
    }
    let git_dir = here.output(["rev-parse", "--absolute-git-dir"])?;
    let git_dir = git_dir.strip_suffix(b"\n").unwrap_or(&git_dir);
    l.state = repo_state(Path::new(OsStr::from_bytes(git_dir)));
    Ok(l)
}

/// The worktree (or what it would lose) is no longer as it was when the
/// removal was confirmed: nothing was removed.
#[derive(Debug)]
pub struct Changed(pub String);

impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} since the removal was confirmed; nothing was removed (look again)", self.0)
    }
}

impl std::error::Error for Changed {}

/// Removes `w` as confirmed: only while git lists it as it was then and it
/// would lose just `confirmed` (otherwise [`Changed`]). A failure says what
/// is left of it.
pub fn remove_as_confirmed(git: &GitRunner, w: &Worktree, confirmed: &Losses, force: Force) -> Result<()> {
    let Some(now) = list(git)?.into_iter().find(|x| same_place(&x.path, &w.path)) else {
        return Err(Changed("git no longer lists it".into()).into());
    };
    if (now.head, &now.branch, now.detached, &now.locked) != (w.head, &w.branch, w.detached, &w.locked) {
        return Err(Changed("its HEAD, branch or lock changed".into()).into());
    }
    if &losses(git, &now)? != confirmed {
        return Err(Changed("what removing it would lose changed".into()).into());
    }
    remove(git, &now, force).map_err(|e| {
        let left = what_is_left(git, &now);
        e.context(left)
    })
}

/// After a failed removal of `w`: what is left of it.
pub fn what_is_left(git: &GitRunner, w: &Worktree) -> String {
    let listed = list(git).map(|l| l.into_iter().find(|x| same_place(&x.path, &w.path)));
    match (w.path.exists(), listed) {
        (true, Ok(Some(now))) if w.locked.is_some() && now.locked.is_none() => "nothing was removed, but it is no longer locked".into(),
        (true, Ok(Some(_))) => "nothing was removed".into(),
        (false, Ok(Some(_))) => "its folder is gone, but git still lists it (Prune tidies that up)".into(),
        (true, Ok(None)) => "git no longer lists it, but its folder is still there".into(),
        (false, Ok(None)) => "it is gone after all".into(),
        (_, Err(e)) => format!("what is left of it can't be told ({e:#})"),
    }
}

/// The default place for a new worktree of the repository whose main
/// worktree is `main`: next to it, `<name>-<branch>` (`/` as `-`), with
/// `-2`, `-3`… if taken.
pub fn default_path(main: &Path, branch: &str) -> PathBuf {
    let parent = main.parent().unwrap_or(main);
    let base = main.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| OsString::from("worktree"));
    let suffix: String = branch.chars().map(|c| if c == '/' || c == '\\' { '-' } else { c }).collect();
    let mut name = base.clone();
    name.push(format!("-{suffix}"));
    let first = parent.join(&name);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| {
            let mut name = name.clone();
            name.push(format!("-{n}"));
            parent.join(name)
        })
        .find(|p| !p.exists())
        .expect("some number is free")
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// `a` and `b` are the same folder (symlinks resolved where they exist:
/// worktree paths compare that way).
pub fn same_place(a: &Path, b: &Path) -> bool {
    canon(a) == canon(b)
}

/// `path` is `dir` or inside it (compared like [`same_place`]).
pub fn is_within(path: &Path, dir: &Path) -> bool {
    canon(path).starts_with(canon(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_kind_of_entry() {
        let out = b"worktree /r/main\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0\
worktree /r/new\nline\0HEAD 2222222222222222222222222222222222222222\0detached\0locked\0\0\
worktree /r/caf\xe9\0HEAD 3333333333333333333333333333333333333333\0branch refs/heads/x\0locked on usb\ndisk\0prunable gitdir file points to non-existent location\0future thing\0\0\
worktree /r/bare\0bare\0\0";
        let l = parse(out).unwrap();
        assert_eq!(l.len(), 4);
        assert!(l[0].main && !l[1].main);
        assert_eq!(l[0].branch_name(), Some("main"));
        assert_eq!(l[1].path, Path::new("/r/new\nline"));
        assert!(l[1].detached && l[1].branch.is_none());
        assert_eq!(l[1].locked.as_deref(), Some(""));
        assert_eq!(l[2].path.as_os_str().as_bytes(), b"/r/caf\xe9");
        assert_eq!(l[2].locked.as_deref(), Some("on usb\ndisk"));
        assert!(l[2].prunable.is_some());
        assert!(l[3].bare && l[3].head.is_none());
        // Cut off: what is there is kept; garbage is an error.
        assert_eq!(parse(b"worktree /a\0HEAD 1111111111111111111111111111111111111111\0").unwrap().len(), 1);
        assert!(parse(b"HEAD 1111\0").is_err());
        assert!(parse(b"").unwrap().is_empty());
    }

    #[test]
    fn default_places_avoid_taken_ones() {
        let dir = std::env::temp_dir().join(format!("pengitto-wt-test-{}", crate::interactive::random_id()));
        let main = dir.join("repo");
        std::fs::create_dir_all(&main).unwrap();
        assert_eq!(default_path(&main, "feature/x"), dir.join("repo-feature-x"));
        std::fs::create_dir(dir.join("repo-feature-x")).unwrap();
        std::fs::create_dir(dir.join("repo-feature-x-2")).unwrap();
        assert_eq!(default_path(&main, "feature/x"), dir.join("repo-feature-x-3"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
