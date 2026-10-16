//! Process-wide registry of write operations.
//!
//! Operations run on one queue (thread) per repository — keyed by the
//! repository's common git dir, so linked worktrees share it — and are
//! tracked here independently of any tab: their state, cancel token and the
//! record of every git command they ran. Tabs ask the registry whether
//! their repository is busy, the Output panel reads the records, and
//! results whose tab has been closed are kept for the app to deliver.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crossbeam_channel::{Sender, unbounded};
use pengitto_core::{CancelToken, Cancelled, CommandRecord, GitRunner, Recorder, RepoPaths};

use crate::worker::{Msg, OpKind, OpOutcome};

pub type OpId = u64;

/// How an operation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed(String),
    Cancelled,
    /// It may or may not have taken effect (e.g. a push whose connection
    /// dropped); the message says what was verified afterwards.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpState {
    Queued,
    Running,
    /// Waiting for the user to answer a credential prompt.
    AwaitingAuth,
    Cancelling,
    Finished(Outcome),
}

impl OpState {
    pub fn is_active(&self) -> bool {
        !matches!(self, OpState::Finished(_))
    }
}

/// The result could not be determined (see [`Outcome::Unknown`]).
#[derive(Debug, Clone)]
pub struct UnknownResult(pub String);

impl std::fmt::Display for UnknownResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnknownResult {}

/// A copy of an operation's state for display.
#[derive(Debug, Clone)]
pub struct OpSnapshot {
    pub id: OpId,
    pub repo: PathBuf,
    pub kind: OpKind,
    pub state: OpState,
    pub summary: Option<String>,
    /// Succeeded but left something to handle (conflicts, kept stash, …).
    pub attention: bool,
    pub commands: Vec<CommandRecord>,
    /// Extra findings, e.g. lock files left behind after a cancel.
    pub notes: Vec<String>,
    /// Latest progress line (fetch/push).
    pub progress: Option<String>,
    pub duration: Option<Duration>,
}

/// Handed to an operation while it runs.
pub struct OpContext {
    pub id: OpId,
    pub cancel: CancelToken,
    recorder: Recorder,
    ctx: egui::Context,
    kind: OpKind,
    workdir: PathBuf,
}

impl OpContext {
    /// `base` configured to record every command into this operation and
    /// to stop when it is cancelled.
    pub fn git(&self, base: &GitRunner) -> GitRunner {
        base.clone().with_recorder(self.recorder.clone()).with_cancel(self.cancel.clone())
    }

    /// `git` for talking to remotes: credential prompts go to the app's
    /// dialog (see [`crate::askpass`]).
    pub fn network(&self, git: GitRunner) -> anyhow::Result<GitRunner> {
        let env = crate::askpass::env_for(self.id, self.kind.label(), &self.workdir, Some(self.ctx.clone()))?;
        Ok(env.into_iter().fold(git, |g, (k, v)| g.with_env(k, v)))
    }

    /// Reports a progress line (shown in the toolbar and Output panel).
    pub fn progress(&self, line: &str) {
        set_progress(self.id, line);
        self.ctx.request_repaint();
    }
}

/// Sets the latest progress line of operation `id`.
pub fn set_progress(id: OpId, line: &str) {
    with(|r| {
        if let Some(e) = r.entry_mut(id) {
            e.progress = Some(line.to_owned());
        }
    });
}

/// Marks operation `id` as waiting for credentials (or running again).
pub fn set_awaiting_auth(id: OpId, waiting: bool) {
    with(|r| {
        if let Some(e) = r.entry_mut(id)
            && matches!(e.state, OpState::Running | OpState::AwaitingAuth)
        {
            e.state = if waiting { OpState::AwaitingAuth } else { OpState::Running };
        }
    });
}

/// Current state of operation `id`.
pub fn state(id: OpId) -> Option<OpState> {
    with(|r| r.entries.iter().find(|e| e.id == id).map(|e| e.state.clone()))
}

pub type OpFn = Box<dyn FnOnce(&OpContext) -> anyhow::Result<OpOutcome> + Send>;

/// Result of an operation whose tab was closed before it finished.
pub struct OrphanedResult {
    pub repo: PathBuf,
    /// Work tree of the tab that started it (its follow-ups, like a stash
    /// conflict, belong to that work tree).
    pub origin: PathBuf,
    pub id: OpId,
    pub kind: OpKind,
    pub result: anyhow::Result<OpOutcome>,
}

struct Job {
    id: OpId,
    kind: OpKind,
    run: OpFn,
    reply: Sender<Msg>,
    ctx: egui::Context,
    paths: RepoPaths,
}

struct Entry {
    id: OpId,
    repo: PathBuf,
    kind: OpKind,
    state: OpState,
    summary: Option<String>,
    attention: bool,
    notes: Vec<String>,
    progress: Option<String>,
    cancel: CancelToken,
    recorder: Recorder,
    started: Option<Instant>,
    finished: Option<Instant>,
}

#[derive(Default)]
struct Registry {
    next_id: OpId,
    entries: Vec<Entry>,
    queues: HashMap<PathBuf, Sender<Job>>,
    orphans: Vec<OrphanedResult>,
    /// Per repository: how many operations have finished. Tabs stay busy
    /// until they have reloaded state after the latest one.
    generations: HashMap<PathBuf, u64>,
}

/// Finished operations kept per repository for the Output panel.
const KEEP_FINISHED: usize = 200;

impl Registry {
    fn entry_mut(&mut self, id: OpId) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    fn prune(&mut self, repo: &Path) {
        let finished: Vec<OpId> =
            self.entries.iter().filter(|e| e.repo == repo && !e.state.is_active()).map(|e| e.id).collect();
        if finished.len() > KEEP_FINISHED {
            let drop: Vec<OpId> = finished[..finished.len() - KEEP_FINISHED].to_vec();
            self.entries.retain(|e| !drop.contains(&e.id));
        }
    }
}

fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    let mut r = REGISTRY.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    f(&mut r)
}

/// Identifies a repository across tabs and worktrees.
pub fn repo_key(common_dir: &Path) -> PathBuf {
    common_dir.canonicalize().unwrap_or_else(|_| common_dir.to_path_buf())
}

fn outcome_of(result: &anyhow::Result<OpOutcome>) -> Outcome {
    match result {
        Ok(_) => Outcome::Succeeded,
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => Outcome::Cancelled,
        Err(e) => match e.downcast_ref::<UnknownResult>() {
            Some(u) => Outcome::Unknown(u.0.clone()),
            None => Outcome::Failed(format!("{e:#}")),
        },
    }
}

fn run_queue(repo: PathBuf, rx: crossbeam_channel::Receiver<Job>) {
    for job in rx {
        let (cancel, recorder) = with(|r| match r.entry_mut(job.id) {
            Some(e) => {
                if !e.cancel.is_cancelled() {
                    e.state = OpState::Running;
                }
                e.started = Some(Instant::now());
                (e.cancel.clone(), e.recorder.clone())
            }
            None => (CancelToken::default(), Recorder::default()),
        });
        let result = if cancel.is_cancelled() {
            // Cancelled while still queued: never started.
            Err(Cancelled.into())
        } else {
            let ctx = OpContext {
                id: job.id,
                cancel,
                recorder,
                ctx: job.ctx.clone(),
                kind: job.kind.clone(),
                workdir: job.paths.workdir.clone(),
            };
            (job.run)(&ctx)
        };
        crate::askpass::forget(job.id);
        let outcome = outcome_of(&result);
        let notes = if outcome == Outcome::Cancelled {
            let locks = pengitto_core::maintenance::leftover_locks(&job.paths);
            locks.iter().map(|p| format!("lock file left behind: {} (remove it once no git is running)", p.display())).collect()
        } else {
            Vec::new()
        };
        with(|r| {
            *r.generations.entry(repo.clone()).or_default() += 1;
            if let Some(e) = r.entry_mut(job.id) {
                e.notes = notes;
                e.state = OpState::Finished(outcome);
                e.finished = Some(Instant::now());
                e.summary = result.as_ref().ok().map(|o| o.summary.clone());
                e.attention = result.as_ref().is_ok_and(|o| !o.follow_ups.is_empty());
            }
            r.prune(&repo);
        });
        if let Err(crossbeam_channel::SendError(Msg::OpDone(id, kind, result))) =
            job.reply.send(Msg::OpDone(job.id, job.kind, result))
        {
            let origin = job.paths.workdir.clone();
            with(|r| r.orphans.push(OrphanedResult { repo: repo.clone(), origin, id, kind, result }));
        }
        job.ctx.request_repaint();
    }
}

/// Queues `run` behind earlier operations on the same repository. The result
/// arrives as [`Msg::OpDone`] on `reply`, or — if that tab is gone — through
/// [`take_orphaned_results`].
pub fn submit(
    repo: &Path,
    paths: &RepoPaths,
    kind: OpKind,
    reply: Sender<Msg>,
    ctx: egui::Context,
    run: OpFn,
) -> anyhow::Result<OpId> {
    with(|r| {
        r.next_id += 1;
        let id = r.next_id;
        let queue = match r.queues.get(repo) {
            Some(q) => q.clone(),
            None => {
                let (tx, rx) = unbounded::<Job>();
                let key = repo.to_path_buf();
                std::thread::Builder::new().name("pengitto-ops".into()).spawn(move || run_queue(key, rx))?;
                r.queues.insert(repo.to_path_buf(), tx.clone());
                tx
            }
        };
        r.entries.push(Entry {
            id,
            repo: repo.to_path_buf(),
            kind: kind.clone(),
            state: OpState::Queued,
            summary: None,
            attention: false,
            notes: Vec::new(),
            progress: None,
            cancel: CancelToken::default(),
            recorder: Recorder::default(),
            started: None,
            finished: None,
        });
        queue
            .send(Job { id, kind, run, reply, ctx, paths: paths.clone() })
            .map_err(|_| anyhow::anyhow!("operation queue closed"))?;
        Ok(id)
    })
}

/// Asks a queued or running operation to stop.
pub fn cancel(id: OpId) {
    with(|r| {
        if let Some(e) = r.entry_mut(id)
            && e.state.is_active()
        {
            e.cancel.cancel();
            e.state = OpState::Cancelling;
        }
    });
}

/// Whether `repo` has operations that have not finished.
pub fn active_for(repo: &Path) -> bool {
    with(|r| r.entries.iter().any(|e| e.repo == repo && e.state.is_active()))
}

/// Operations queued or running, on any repository.
pub fn active_ops() -> Vec<OpSnapshot> {
    with(|r| r.entries.iter().filter(|e| e.state.is_active()).map(to_snapshot).collect())
}

pub fn snapshot(id: OpId) -> Option<OpSnapshot> {
    with(|r| r.entries.iter().find(|e| e.id == id).map(to_snapshot))
}

/// Operations on `repo`, newest first.
pub fn snapshots_for(repo: &Path) -> Vec<OpSnapshot> {
    with(|r| r.entries.iter().rev().filter(|e| e.repo == repo).map(to_snapshot).collect())
}

fn to_snapshot(e: &Entry) -> OpSnapshot {
    OpSnapshot {
        id: e.id,
        repo: e.repo.clone(),
        kind: e.kind.clone(),
        state: e.state.clone(),
        summary: e.summary.clone(),
        attention: e.attention,
        commands: e.recorder.snapshot(),
        notes: e.notes.clone(),
        progress: e.progress.clone(),
        duration: e.started.map(|s| e.finished.unwrap_or_else(Instant::now) - s),
    }
}

/// Results of operations whose tab no longer exists (for the app to show).
pub fn take_orphaned_results() -> Vec<OrphanedResult> {
    with(|r| std::mem::take(&mut r.orphans))
}

/// Hands a result to the app instead of a tab that is going away.
pub fn push_orphan(o: OrphanedResult) {
    with(|r| r.orphans.push(o));
}

/// Number of operations finished on `repo` so far.
pub fn generation(repo: &Path) -> u64 {
    with(|r| r.generations.get(repo).copied().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(rx: &crossbeam_channel::Receiver<Msg>) -> (OpId, anyhow::Result<OpOutcome>) {
        loop {
            if let Msg::OpDone(id, _, r) = rx.recv_timeout(Duration::from_secs(10)).expect("op finished") {
                return (id, r);
            }
        }
    }

    #[test]
    fn records_states_and_outcomes() {
        let repo = PathBuf::from("/test/registry/records");
        let (tx, rx) = unbounded();
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let base = GitRunner::new(dir.path());
        let b2 = base.clone();
        let paths = RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
        let id = submit(&repo, &paths, OpKind::Stage, tx.clone(), ctx.clone(), Box::new(move |c| {
            let git = c.git(&b2);
            git.output(["--version"])?;
            git.output(["rev-parse", "HEAD"])?; // fails: not a repository
            Ok(OpOutcome::default())
        }))
        .unwrap();
        let (done, result) = wait_for(&rx);
        assert_eq!(done, id);
        assert!(result.is_err());
        let snap = snapshot(id).unwrap();
        assert!(matches!(snap.state, OpState::Finished(Outcome::Failed(_))));
        assert_eq!(snap.commands.len(), 2, "both commands recorded despite the failure");
        assert!(!active_for(&repo));

        // A queued operation cancelled before it starts never runs.
        let (block_tx, block_rx) = crossbeam_channel::bounded::<()>(0);
        let gen_before = generation(&repo);
        let first = submit(&repo, &paths, OpKind::Stage, tx.clone(), ctx.clone(), Box::new(move |_| {
            let _ = block_rx.recv();
            Ok(OpOutcome::default())
        }))
        .unwrap();
        let second = submit(&repo, &paths, OpKind::Unstage, tx.clone(), ctx.clone(), Box::new(|_| panic!("must not run")))
            .unwrap();
        assert!(active_for(&repo));
        cancel(second);
        block_tx.send(()).unwrap();
        let (a, ra) = wait_for(&rx);
        let (b, rb) = wait_for(&rx);
        assert_eq!((a, b), (first, second));
        assert!(ra.is_ok());
        assert!(rb.unwrap_err().downcast_ref::<Cancelled>().is_some());
        assert_eq!(snapshot(second).unwrap().state, OpState::Finished(Outcome::Cancelled));
        assert_eq!(generation(&repo), gen_before + 2, "each finished operation counts");
        let all = snapshots_for(&repo);
        assert_eq!(all.first().map(|s| s.id), Some(second), "newest first");
        let _ = base;
    }
}
