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
use tine_core::{GitRunner, status};

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
}

impl FsChange {
    pub fn merge(&mut self, other: FsChange) {
        self.refs |= other.refs;
        self.status |= other.status;
    }

    pub fn any(self) -> bool {
        self.refs || self.status
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Refs,
    Status,
    Worktree(BString),
}

fn classify(path: &Path, workdir: &Path, git_dir: &Path) -> Option<Kind> {
    if let Ok(rel) = path.strip_prefix(git_dir) {
        let name = rel.file_name()?.as_bytes();
        if name.ends_with(b".lock") {
            return None;
        }
        let first = rel.components().next()?.as_os_str().as_bytes();
        return match first {
            b"HEAD" | b"packed-refs" | b"refs" => Some(Kind::Refs),
            b"index" | b"rebase-merge" | b"rebase-apply" => Some(Kind::Status),
            _ if name.ends_with(b"_HEAD") && rel.components().count() == 1 => Some(Kind::Status),
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
    pub fn start(workdir: PathBuf, git_dir: PathBuf, git: GitRunner, tx: Sender<Msg>, ctx: egui::Context) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let spawned = std::thread::Builder::new().name("tine-watch".into()).spawn(move || {
            if let Err(e) = run(&workdir, &git_dir, &git, &tx, &ctx, &stop2) {
                let _ = tx.send(Msg::Job(Err(anyhow::anyhow!(
                    "File watching is unavailable ({e:#}); changes made outside Tine appear after pressing F5 or refocusing the window."
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
    if !git_dir.starts_with(workdir) {
        watcher.watch(git_dir, RecursiveMode::Recursive)?;
    }
    log::info!("watching {}", workdir.display());

    let mut change = FsChange::default();
    let mut worktree: BTreeSet<BString> = BTreeSet::new();
    let mut first_event: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        match erx.recv_timeout(if first_event.is_some() { QUIET } else { Duration::from_millis(500) }) {
            Ok(Ok(event)) => {
                let event: notify::Event = event;
                if matches!(event.kind, EventKind::Access(_)) {
                    continue;
                }
                for path in &event.paths {
                    match classify(path, workdir, git_dir) {
                        Some(Kind::Refs) => change.refs = true,
                        Some(Kind::Status) => change.status = true,
                        Some(Kind::Worktree(p)) => {
                            worktree.insert(p);
                        }
                        None => continue,
                    }
                    first_event.get_or_insert_with(Instant::now);
                }
                if !first_event.is_some_and(|t| t.elapsed() >= MAX_DELAY) {
                    continue;
                }
            }
            Ok(Err(e)) => {
                log::warn!("watch error: {e}");
                continue;
            }
            Err(RecvTimeoutError::Timeout) if first_event.is_none() => continue,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
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
        let c = |p: &str| classify(Path::new(p), wd, gd);
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
    }
}
