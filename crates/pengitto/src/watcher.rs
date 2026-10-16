//! Turns file system activity in a repository into refresh requests.
//!
//! Events are batched until things have been quiet for [`QUIET`] (or at most
//! [`MAX_DELAY`] during continuous activity). Inside the git directory only
//! files that change what we show are considered (HEAD, refs, index,
//! in-progress operation markers). Work tree paths that git ignores (build
//! output and the like) never trigger a refresh.

use std::{
    collections::BTreeSet,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bstr::BString;
use crossbeam_channel::{RecvTimeoutError, Sender, unbounded};
use notify::{EventKind, RecursiveMode, Watcher as _};
use pengitto_core::{GitRunner, status};

use crate::worker::Msg;

const QUIET: Duration = Duration::from_millis(300);
const MAX_DELAY: Duration = Duration::from_secs(2);
/// Above this many changed paths, skip the ignore check and just refresh.
const MAX_CHECKED: usize = 2000;

/// What needs reloading after a batch of file events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsChange {
    /// Branches, tags or HEAD moved (implies `status`).
    pub refs: bool,
    pub status: bool,
    /// Worktrees were added, removed, locked, or switched branches.
    pub worktrees: bool,
}

impl FsChange {
    pub fn merge(&mut self, other: FsChange) {
        self.refs |= other.refs;
        self.status |= other.status;
        self.worktrees |= other.worktrees;
    }

    pub fn any(self) -> bool {
        self.refs || self.status || self.worktrees
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Refs,
    Status,
    /// The repository's worktrees (another one's HEAD, lock…).
    Worktrees,
    Worktree(BString),
}

/// A change under `worktrees/` (relative path `rel`) that changes the list
/// of worktrees: one's records added or removed, its HEAD moved, its lock
/// or location changed — not its index, logs and the like.
fn changes_worktree_list(rel: &Path) -> bool {
    let parts: Vec<&[u8]> = rel.components().map(|c| c.as_os_str().as_bytes()).collect();
    match parts.as_slice() {
        [b"worktrees"] | [b"worktrees", _] => true,
        [b"worktrees", _, file] => matches!(*file, b"HEAD" | b"locked" | b"gitdir"),
        _ => false,
    }
}

/// What a change at `path` means for the worktree at `workdir` (its git
/// dir `git_dir`, the repository's shared one `common_dir`).
fn classify(path: &Path, workdir: &Path, git_dir: &Path, common_dir: &Path) -> Option<Kind> {
    if let Ok(rel) = path.strip_prefix(git_dir) {
        let name = rel.file_name()?.as_bytes();
        if name.ends_with(b".lock") {
            return None;
        }
        let first = rel.components().next()?.as_os_str().as_bytes();
        return match first {
            b"HEAD" | b"packed-refs" | b"refs" => Some(Kind::Refs),
            b"index" | b"rebase-merge" | b"rebase-apply" => Some(Kind::Status),
            b"worktrees" => changes_worktree_list(rel).then_some(Kind::Worktrees),
            _ if name.ends_with(b"_HEAD") && rel.components().count() == 1 => Some(Kind::Status),
            _ => None,
        };
    }
    // A linked worktree: the refs it shares, the other worktrees.
    if let Ok(rel) = path.strip_prefix(common_dir) {
        let name = rel.file_name()?.as_bytes();
        if name.ends_with(b".lock") {
            return None;
        }
        return match rel.components().next()?.as_os_str().as_bytes() {
            b"packed-refs" | b"refs" => Some(Kind::Refs),
            b"HEAD" => Some(Kind::Worktrees),
            b"worktrees" => changes_worktree_list(rel).then_some(Kind::Worktrees),
            _ => None,
        };
    }
    let rel = path.strip_prefix(workdir).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(Kind::Worktree(BString::from(rel.as_os_str().as_bytes())))
}

/// Keeps a background watch on one repository while alive.
pub struct RepoWatcher {
    stop: Arc<AtomicBool>,
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl RepoWatcher {
    /// Starts watching on a background thread. Setup errors (such as the
    /// inotify watch limit) are reported through `tx` as a failed job.
    pub fn start(
        workdir: PathBuf,
        git_dir: PathBuf,
        common_dir: PathBuf,
        git: GitRunner,
        tx: Sender<Msg>,
        ctx: egui::Context,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let spawned = std::thread::Builder::new().name("pengitto-watch".into()).spawn(move || {
            if let Err(e) = run(&workdir, &git_dir, &common_dir, &git, &tx, &ctx, &stop2) {
                let _ = tx.send(Msg::Job(Err(anyhow::anyhow!(
                    "File watching is unavailable ({e:#}); changes made outside pengitto appear after pressing F5 or refocusing the window."
                ))));
                ctx.request_repaint();
            }
        });
        if let Err(e) = spawned {
            log::warn!("failed to start watcher thread: {e}");
        }
        RepoWatcher { stop }
    }
}

fn run(
    workdir: &Path,
    git_dir: &Path,
    common_dir: &Path,
    git: &GitRunner,
    tx: &Sender<Msg>,
    ctx: &egui::Context,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let (etx, erx) = unbounded();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = etx.send(res);
    })?;
    watcher.watch(workdir, RecursiveMode::Recursive)?;
    // A linked worktree's git dir is inside the shared one: watching that
    // covers both.
    for dir in [common_dir, git_dir] {
        if !dir.starts_with(workdir) {
            watcher.watch(dir, RecursiveMode::Recursive)?;
            if git_dir.starts_with(dir) {
                break;
            }
        }
    }
    log::info!("watching {}", workdir.display());

    let mut change = FsChange::default();
    let mut worktree: BTreeSet<BString> = BTreeSet::new();
    // First and latest relevant event of the batch. Other events (git
    // merely reading files, objects being written) never delay a flush.
    let mut first_event: Option<Instant> = None;
    let mut last_event: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let timeout = match (first_event, last_event) {
            (Some(first), Some(last)) => (last + QUIET).min(first + MAX_DELAY).saturating_duration_since(Instant::now()),
            _ => Duration::from_millis(500),
        };
        match erx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                let event: notify::Event = event;
                if !matches!(event.kind, EventKind::Access(_)) {
                    for path in &event.paths {
                        match classify(path, workdir, git_dir, common_dir) {
                            Some(Kind::Refs) => change.refs = true,
                            Some(Kind::Status) => change.status = true,
                            Some(Kind::Worktrees) => change.worktrees = true,
                            Some(Kind::Worktree(p)) => {
                                worktree.insert(p);
                            }
                            None => continue,
                        }
                        let now = Instant::now();
                        first_event.get_or_insert(now);
                        last_event = Some(now);
                    }
                }
            }
            Ok(Err(e)) => log::warn!("watch error: {e}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let due = first_event.is_some_and(|t| t.elapsed() >= MAX_DELAY) || last_event.is_some_and(|t| t.elapsed() >= QUIET);
        if !due {
            continue;
        }

        // Flush the batch.
        if !change.status && !worktree.is_empty() {
            let paths: Vec<BString> = std::mem::take(&mut worktree).into_iter().collect();
            change.status = paths.len() > MAX_CHECKED
                || match status::ignored(git, &paths) {
                    Ok(ignored) => paths.iter().any(|p| !ignored.contains(p)),
                    Err(e) => {
                        log::warn!("check-ignore failed: {e:#}");
                        true
                    }
                };
        }
        worktree.clear();
        first_event = None;
        last_event = None;
        if change.any() {
            log::debug!("fs change: {change:?}");
            if tx.send(Msg::Fs(change)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
        change = FsChange::default();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths() {
        let wd = Path::new("/r");
        let gd = Path::new("/r/.git");
        let c = |p: &str| classify(Path::new(p), wd, gd, gd);
        assert_eq!(c("/r/.git/HEAD"), Some(Kind::Refs));
        assert_eq!(c("/r/.git/refs/heads/main"), Some(Kind::Refs));
        assert_eq!(c("/r/.git/refs/heads/main.lock"), None);
        assert_eq!(c("/r/.git/packed-refs"), Some(Kind::Refs));
        assert_eq!(c("/r/.git/index"), Some(Kind::Status));
        assert_eq!(c("/r/.git/index.lock"), None);
        assert_eq!(c("/r/.git/MERGE_HEAD"), Some(Kind::Status));
        assert_eq!(c("/r/.git/rebase-merge/done"), Some(Kind::Status));
        assert_eq!(c("/r/.git/objects/ab/cdef"), None);
        assert_eq!(c("/r/.git/logs/HEAD"), None);
        assert_eq!(c("/r/src/日本語.rs"), Some(Kind::Worktree("src/日本語.rs".into())));
        assert_eq!(c("/r"), None);
        assert_eq!(c("/elsewhere/x"), None);
        assert_eq!(c("/r/.git/worktrees/wt/HEAD"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/wt"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/wt/locked"), Some(Kind::Worktrees));
        // Another worktree's work (its index, logs…) doesn't change the list.
        assert_eq!(c("/r/.git/worktrees/wt/index"), None);
        assert_eq!(c("/r/.git/worktrees/wt/logs/HEAD"), None);
        assert_eq!(c("/r/.git/worktrees/wt/ORIG_HEAD"), None);
        // A linked worktree: its own git dir, then what it shares.
        let (wd, gd, cd) = (Path::new("/wt"), Path::new("/r/.git/worktrees/wt"), Path::new("/r/.git"));
        let c = |p: &str| classify(Path::new(p), wd, gd, cd);
        assert_eq!(c("/r/.git/worktrees/wt/HEAD"), Some(Kind::Refs));
        assert_eq!(c("/r/.git/worktrees/wt/index"), Some(Kind::Status));
        assert_eq!(c("/r/.git/refs/heads/main"), Some(Kind::Refs));
        assert_eq!(c("/r/.git/HEAD"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/other/HEAD"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/other/locked"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/other/gitdir"), Some(Kind::Worktrees));
        assert_eq!(c("/r/.git/worktrees/other/index"), None);
        assert_eq!(c("/r/.git/worktrees/other/logs/HEAD"), None);
        assert_eq!(c("/r/.git/objects/ab/cd"), None);
        assert_eq!(c("/wt/src/a.rs"), Some(Kind::Worktree("src/a.rs".into())));
    }
}
