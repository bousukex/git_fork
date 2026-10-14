//! Interactive rebase: the commits to rebase, the plan the user makes for
//! them (actions, order, messages), and running it through git.
//!
//! Git asks an editor for the todo list and for commit messages; Tine is
//! that editor ([`sequence_editor`], [`message_editor`], run by the `tine`
//! binary without its window). What they hand back lives in a session
//! directory in the worktree's git dir, from the start until git's rebase
//! really ends — also across stops (`edit`, conflicts) and restarts.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::{
    CancelToken, GitRunner,
    integrate::{Step, run_step},
    model::{Oid, RepoState},
    status::{has_conflicts, repo_state},
};

/// Most commits a plan may have.
pub const MAX_COMMITS: usize = 1000;
/// Most bytes of commit messages read for a plan.
pub const MAX_MESSAGE_BYTES: usize = 16 << 20;

/// One commit to rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseCommit {
    pub id: Oid,
    pub subject: String,
    /// The whole message.
    pub message: String,
    pub author: String,
    pub author_time: i64,
    /// Reachable from a remote-tracking branch (as last fetched).
    pub published: bool,
}

/// The commits after `base` up to HEAD, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    /// `None`: from the root (`--root`).
    pub base: Option<Oid>,
    pub head: Oid,
    /// The checked-out branch as a full ref (`None`: detached HEAD).
    pub branch: Option<String>,
    pub commits: Vec<RebaseCommit>,
}

/// `refs/heads/x` → `x` (for showing).
pub fn branch_name(full: &str) -> &str {
    full.strip_prefix("refs/heads/").unwrap_or(full)
}

/// Why a range can't be rebased interactively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeProblem {
    NotAnAncestor,
    Merges(Vec<Oid>),
    /// More than [`MAX_COMMITS`] commits.
    TooMany,
    /// The commit messages are larger than [`MAX_MESSAGE_BYTES`].
    TooLarge,
    Empty,
}

impl std::fmt::Display for RangeProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeProblem::NotAnAncestor => f.write_str("the commit isn't part of the current branch's history"),
            RangeProblem::Merges(ids) => write!(f, "merge commits can't be rebased interactively here ({})", ids[0].short()),
            RangeProblem::TooMany => write!(f, "more than {MAX_COMMITS} commits: too many for an interactive rebase here"),
            RangeProblem::TooLarge => f.write_str("the commit messages are too large for an interactive rebase here"),
            RangeProblem::Empty => f.write_str("there are no commits after this one"),
        }
    }
}

impl std::error::Error for RangeProblem {}

fn oid(s: &str) -> Result<Oid> {
    Oid::from_hex(s.trim().as_bytes()).ok_or_else(|| anyhow!("not an object id: {s:?}"))
}

fn rev_spec(base: Option<Oid>, head: Oid) -> String {
    match base {
        Some(b) => format!("{}..{}", b.to_hex(), head.to_hex()),
        None => head.to_hex(),
    }
}

/// HEAD's commit and branch (full ref) now.
pub fn head_now(git: &GitRunner) -> Result<(Oid, Option<String>)> {
    let head = oid(&String::from_utf8_lossy(&git.output(["rev-parse", "--verify", "HEAD^{commit}"])?))?;
    // Exit code 1: detached.
    let branch = String::from_utf8_lossy(&git.output_with(["symbolic-ref", "-q", "HEAD"], &[1])?).trim().to_owned();
    Ok((head, (!branch.is_empty()).then_some(branch)))
}

/// Reads the commits after `base` (all of them with `None`) up to HEAD.
/// Every query after the first uses the HEAD commit read then (not `HEAD`,
/// which may move meanwhile).
pub fn load_range(git: &GitRunner, base: Option<Oid>, cancel: &CancelToken) -> Result<Range> {
    let git = git.clone().with_cancel(cancel.clone());
    let (head, branch) = head_now(&git)?;
    if let Some(b) = base {
        // Exit code 1: not an ancestor.
        if !git.command(["merge-base", "--is-ancestor", &b.to_hex(), &head.to_hex()]).status()?.success() {
            return Err(RangeProblem::NotAnAncestor.into());
        }
    }
    let spec = rev_spec(base, head);
    // Counting stops after one too many (a huge history isn't walked).
    let limit = format!("--max-count={}", MAX_COMMITS + 1);
    let count: usize = String::from_utf8_lossy(&git.output(["rev-list", "--count", &limit, &spec])?).trim().parse()?;
    if count > MAX_COMMITS {
        return Err(RangeProblem::TooMany.into());
    }
    if count == 0 {
        return Err(RangeProblem::Empty.into());
    }
    // Every commit of the range (not only the first-parent line).
    // Read up to the limit only (one huge message isn't held whole).
    let args = ["log", "-z", "--reverse", "--format=%H%x1f%P%x1f%an%x1f%at%x1f%B", &spec];
    let Some(out) = git.output_limited(args, MAX_MESSAGE_BYTES, cancel)? else {
        return Err(RangeProblem::TooLarge.into());
    };
    let mut commits = Vec::with_capacity(count);
    let mut merges = Vec::new();
    for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let f: Vec<&str> = rec.splitn(5, '\x1f').collect();
        let [id, parents, author, time, message] = f[..] else { bail!("unexpected log output") };
        let id = oid(id)?;
        if parents.split_whitespace().count() > 1 {
            merges.push(id);
        }
        let message = message.trim_end().to_owned();
        commits.push(RebaseCommit {
            id,
            subject: message.lines().next().unwrap_or_default().to_owned(),
            message,
            author: author.to_owned(),
            author_time: time.parse().unwrap_or(0),
            published: false,
        });
    }
    if !merges.is_empty() {
        return Err(RangeProblem::Merges(merges).into());
    }
    // One call: the commits no remote-tracking branch reaches.
    let unpublished: HashSet<Oid> = String::from_utf8_lossy(&git.output(["rev-list", &spec, "--not", "--remotes"])?)
        .lines()
        .filter_map(|l| Oid::from_hex(l.as_bytes()))
        .collect();
    for c in &mut commits {
        c.published = !unpublished.contains(&c.id);
    }
    Ok(Range { base, head, branch, commits })
}

/// What to do with a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Pick,
    Reword,
    Edit,
    /// Meld into the commit before, combining the messages.
    Squash,
    /// Meld into the commit before, dropping this message.
    Fixup,
    Drop,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Edit => "edit",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
        }
    }

    fn parse(word: &str) -> Option<Action> {
        Some(match word {
            "pick" | "p" => Action::Pick,
            "reword" | "r" => Action::Reword,
            "edit" | "e" => Action::Edit,
            "squash" | "s" => Action::Squash,
            "fixup" | "f" => Action::Fixup,
            "drop" | "d" => Action::Drop,
            _ => return None,
        })
    }

    fn melds(self) -> bool {
        matches!(self, Action::Squash | Action::Fixup)
    }
}

/// Which message a file of the session holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKey {
    /// The new message of a reworded commit.
    Reword(Oid),
    /// The message of a squash group, keyed by its last member (where git
    /// asks for it).
    Group(Oid),
}

impl MessageKey {
    fn file_name(self) -> String {
        match self {
            MessageKey::Reword(id) => format!("reword-{}", id.to_hex()),
            MessageKey::Group(id) => format!("group-{}", id.to_hex()),
        }
    }
}

/// What the user wants done: the commits in their new order (oldest
/// first), each with an action, and the messages to use: every reworded
/// commit's, and the combined messages of squash groups the user wrote
/// (other groups get git's own combination, made from the commits as they
/// really are when git asks — after amending at an `edit`, say).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub items: Vec<(Oid, Action)>,
    pub messages: HashMap<MessageKey, String>,
}

/// A commit with the squash/fixup commits melded into it (indices into
/// the plan's items; drops in between don't end a group, as in git).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub anchor: usize,
    pub members: Vec<usize>,
}

impl Group {
    /// Where git asks for the combined message: the last member.
    pub fn terminal(&self) -> Option<usize> {
        self.members.last().copied()
    }

    pub fn has_squash(&self, plan: &Plan) -> bool {
        self.members.iter().any(|&m| plan.items[m].1 == Action::Squash)
    }
}

impl Plan {
    /// Every commit picked, in the range's order.
    pub fn new(range: &Range) -> Plan {
        Plan { items: range.commits.iter().map(|c| (c.id, Action::Pick)).collect(), messages: HashMap::new() }
    }

    /// The groups, in order. A squash/fixup with nothing before it to meld
    /// into is left out (the plan isn't valid then).
    pub fn groups(&self) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        for (i, (_, a)) in self.items.iter().enumerate() {
            match a {
                Action::Drop => {}
                a if a.melds() => {
                    if let Some(g) = groups.last_mut() {
                        g.members.push(i);
                    }
                }
                _ => groups.push(Group { anchor: i, members: Vec::new() }),
            }
        }
        groups
    }

    /// The messages the rebase uses: each reworded commit's, and those
    /// written for squash groups (keyed by the group's last member).
    pub fn needed_messages(&self) -> Vec<MessageKey> {
        let mut keys: Vec<MessageKey> =
            self.items.iter().filter(|(_, a)| *a == Action::Reword).map(|(id, _)| MessageKey::Reword(*id)).collect();
        for g in self.groups() {
            if g.has_squash(self)
                && let Some(t) = g.terminal()
                && self.messages.contains_key(&MessageKey::Group(self.items[t].0))
            {
                keys.push(MessageKey::Group(self.items[t].0));
            }
        }
        keys
    }

    /// git's combined message for a squash group as planned: the anchor's
    /// message (reworded, if it is) and those of the squashed commits (not
    /// of the fixups). Shown as the starting point; unless the user changes
    /// it, git combines the messages itself.
    pub fn default_group_message(&self, group: &Group, range: &Range) -> String {
        let message = |id: Oid| range.commits.iter().find(|c| c.id == id).map(|c| c.message.clone()).unwrap_or_default();
        let (anchor, action) = self.items[group.anchor];
        let first = match self.messages.get(&MessageKey::Reword(anchor)) {
            Some(m) if action == Action::Reword => m.clone(),
            _ => message(anchor),
        };
        std::iter::once(first)
            .chain(group.members.iter().filter(|&&m| self.items[m].1 == Action::Squash).map(|&m| message(self.items[m].0)))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// What is wrong with the plan for `range` (empty: it can run).
    pub fn problems(&self, range: &Range) -> Vec<String> {
        let mut problems = Vec::new();
        let want: HashSet<Oid> = range.commits.iter().map(|c| c.id).collect();
        let mut seen = HashSet::new();
        for (id, _) in &self.items {
            if !want.contains(id) {
                problems.push(format!("{} isn't one of the commits being rebased", id.short()));
            } else if !seen.insert(*id) {
                problems.push(format!("{} appears twice", id.short()));
            }
        }
        if let Some(missing) = want.iter().find(|id| !seen.contains(id)) {
            problems.push(format!("{} is missing", missing.short()));
        }
        match self.items.iter().find(|(_, a)| *a != Action::Drop) {
            None => problems.push("Every commit would be dropped".into()),
            Some((id, a)) if a.melds() => {
                problems.push(format!("{} can't be squashed or fixed up: there is no commit below it to meld into", id.short()))
            }
            Some(_) => {}
        }
        for key in self.needed_messages() {
            if self.messages.get(&key).is_none_or(|m| m.trim().is_empty()) {
                problems.push(match key {
                    MessageKey::Reword(id) => format!("The new message of {} is empty", id.short()),
                    MessageKey::Group(id) => format!("The message of the commits squashed up to {} is empty", id.short()),
                });
            }
        }
        problems
    }

    /// The todo list for git (full ids; drops stay as `drop` lines).
    pub fn todo(&self, range: &Range) -> String {
        let subject = |id: Oid| range.commits.iter().find(|c| c.id == id).map(|c| c.subject.replace(['\n', '\r'], " ")).unwrap_or_default();
        let mut out = String::new();
        for (id, a) in &self.items {
            out.push_str(&format!("{} {} {}\n", a.word(), id.to_hex(), subject(*id)));
        }
        out
    }
}

// --- The session ---------------------------------------------------------

/// Where the session of the worktree whose git dir is `git_dir` lives.
pub fn session_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("tine-rebase")
}

/// What Tine started (kept until git's rebase really ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// The worktree's git dir (where `rebase-merge` is).
    pub git_dir: PathBuf,
    pub head: Oid,
    /// The branch rebased (full ref; `None`: detached HEAD).
    pub branch: Option<String>,
    pub base: Option<Oid>,
    pub items: Vec<(Oid, Action)>,
    /// The messages written for the rebase (a missing file is an error).
    pub messages: Vec<MessageKey>,
    /// Local changes stashed before starting (re-applied once it ends).
    pub autostash: Option<Oid>,
    /// The rebased branch's newest reflog entry when it started (`None`:
    /// no reflog, or detached): the rebase's end is the entry right after.
    pub reflog: Option<String>,
}

impl Session {
    fn plan(&self) -> Plan {
        Plan { items: self.items.clone(), messages: HashMap::new() }
    }

    fn encode(&self) -> String {
        let mut s = format!("id {}\nhead {}\n", self.id, self.head.to_hex());
        for k in &self.messages {
            s.push_str(&match k {
                MessageKey::Reword(id) => format!("message reword {}\n", id.to_hex()),
                MessageKey::Group(id) => format!("message group {}\n", id.to_hex()),
            });
        }
        if let Some(b) = &self.branch {
            s.push_str(&format!("branch {b}\n"));
        }
        s.push_str(&match self.base {
            Some(b) => format!("base {}\n", b.to_hex()),
            None => "base root\n".into(),
        });
        if let Some(a) = self.autostash {
            s.push_str(&format!("autostash {}\n", a.to_hex()));
        }
        if let Some(r) = &self.reflog {
            s.push_str(&format!("reflog {r}\n"));
        }
        for (id, a) in &self.items {
            s.push_str(&format!("item {} {}\n", a.word(), id.to_hex()));
        }
        s
    }

    fn decode(text: &str, git_dir: &Path) -> Result<Session> {
        let (mut id, mut head, mut branch, mut base, mut autostash, mut reflog, mut items, mut messages) =
            (None, None, None, None, None, None, Vec::new(), Vec::new());
        for line in text.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "id" => id = Some(value.to_owned()),
                "head" => head = Some(oid(value)?),
                "branch" => branch = Some(value.to_owned()),
                "base" => base = Some(if value == "root" { None } else { Some(oid(value)?) }),
                "autostash" => autostash = Some(oid(value)?),
                "reflog" => reflog = Some(value.to_owned()),
                "message" => {
                    let (kind, id) = value.split_once(' ').ok_or_else(|| anyhow!("bad session line {line:?}"))?;
                    messages.push(match kind {
                        "reword" => MessageKey::Reword(oid(id)?),
                        "group" => MessageKey::Group(oid(id)?),
                        _ => bail!("bad session line {line:?}"),
                    });
                }
                "item" => {
                    let (a, id) = value.split_once(' ').ok_or_else(|| anyhow!("bad session line {line:?}"))?;
                    items.push((oid(id)?, Action::parse(a).ok_or_else(|| anyhow!("bad action {a:?}"))?));
                }
                _ => {}
            }
        }
        Ok(Session {
            id: id.ok_or_else(|| anyhow!("session without id"))?,
            git_dir: git_dir.to_path_buf(),
            head: head.ok_or_else(|| anyhow!("session without head"))?,
            branch,
            base: base.ok_or_else(|| anyhow!("session without base"))?,
            items,
            messages,
            autostash,
            reflog,
        })
    }
}

/// Writes `path` all at once (a crash leaves the old content or the new).
fn write_atomically(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The file in git's `rebase-merge` marking the rebase as this session's
/// (written by the sequence editor while git starts it).
const MARKER: &str = "tine-session";

/// The rebase in progress was started by `session` (not merely a session
/// left behind by one that ended outside Tine). A marker that can't be
/// read is an error, not "someone else's".
pub fn owns_current_rebase(session: &Session) -> Result<bool> {
    let path = session.git_dir.join("rebase-merge").join(MARKER);
    match std::fs::read_to_string(&path) {
        Ok(id) => Ok(id.trim() == session.id),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// The rebase in progress in this worktree is a session's of Tine's (`false`
/// also when that can't be read).
pub fn owns_rebase_in_progress(git_dir: &Path) -> bool {
    matches!(read_session(git_dir), Ok(Some(s)) if owns_current_rebase(&s).unwrap_or(false))
}

/// The session of this worktree, if one is there.
pub fn read_session(git_dir: &Path) -> Result<Option<Session>> {
    let path = session_dir(git_dir).join("session");
    match std::fs::read_to_string(&path) {
        Ok(text) => Session::decode(&text, git_dir).map(Some).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Removes the session (git's rebase is over).
pub fn clear_session(git_dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(session_dir(git_dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The branch (or detached HEAD) moved on from where it started: the
    /// rebase left it at `head`.
    Rebased { session: Session, head: Oid },
    /// Back where it started (aborted, or nothing changed), or the branch
    /// is gone.
    Unchanged { session: Session },
    /// Ended outside Tine, where it ended can't be told from what came
    /// after (so nothing is offered to undo); or left behind: the rebase in
    /// progress isn't this session's.
    Stale { session: Session },
}

impl Ended {
    pub fn session(&self) -> &Session {
        match self {
            Ended::Rebased { session, .. } | Ended::Unchanged { session } | Ended::Stale { session } => session,
        }
    }

    pub fn session_mut(&mut self) -> &mut Session {
        match self {
            Ended::Rebased { session, .. } | Ended::Unchanged { session } | Ended::Stale { session } => session,
        }
    }
}

/// The newest `n` entries of `refname`'s reflog, newest first, as (the
/// entry, its new value, its message). Empty when there is none.
fn reflog(git: &GitRunner, refname: &str, n: usize) -> Result<Vec<(String, Oid, String)>> {
    let out = git.output(["log", "-g", &format!("-n{n}"), "--date=raw", "--format=%H %gd%x00%gs", refname, "--"])?;
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let (entry, message) = line.split_once('\0').ok_or_else(|| anyhow!("unexpected reflog line {line:?}"))?;
        let new = oid(entry.split(' ').next().unwrap_or_default())?;
        entries.push((entry.to_owned(), new, message.to_owned()));
    }
    Ok(entries)
}

/// The branch's reflog shows the session's rebase ending at `tip`, and
/// nothing since: git's "rebase (finish)" entry right after the one it
/// started at.
fn ended_at(git: &GitRunner, session: &Session, tip: Oid) -> bool {
    let (Some(branch), Some(start)) = (&session.branch, &session.reflog) else { return false };
    match reflog(git, branch, 2).as_deref() {
        Ok([(_, new, message), (before, ..)]) => {
            *new == tip && before == start && message.starts_with("rebase") && message.contains("(finish)")
        }
        _ => false,
    }
}

/// If this worktree's session is over — its rebase ended (also outside
/// Tine), or the rebase in progress is someone else's — removes it and
/// says how it ended. Judged by the rebased branch's tip (not wherever
/// HEAD is now). `by_tine`: Tine's own command just ended it (the tip is
/// where the rebase left it); otherwise that is taken only from the
/// branch's reflog. Anything that can't be read keeps the session (an
/// error).
pub fn finish_session(git: &GitRunner, git_dir: &Path, by_tine: bool) -> Result<Option<Ended>> {
    let Some(mut session) = read_session(git_dir)? else { return Ok(None) };
    if repo_state(git_dir) == RepoState::Rebasing {
        if owns_current_rebase(&session)? {
            return Ok(None);
        }
        clear_session(git_dir)?;
        return Ok(Some(Ended::Stale { session }));
    }
    let rev = format!("{}^{{commit}}", session.branch.as_deref().unwrap_or("HEAD"));
    // Exit code 1: the branch is gone.
    let tip = String::from_utf8_lossy(&git.output_with(["rev-parse", "-q", "--verify", &rev], &[1])?).trim().to_owned();
    let tip = if tip.is_empty() { None } else { Some(oid(&tip)?) };
    // A stash applied and dropped meanwhile isn't offered again.
    if let Some(stash) = session.autostash
        && crate::stash::index_of(git, stash)?.is_none()
    {
        session.autostash = None;
    }
    let ended = match tip {
        Some(head) if head != session.head && (by_tine || ended_at(git, &session, head)) => Ended::Rebased { session, head },
        Some(head) if head != session.head => Ended::Stale { session },
        _ => Ended::Unchanged { session },
    };
    clear_session(git_dir)?;
    Ok(Some(ended))
}

// --- Running --------------------------------------------------------------

/// The program git runs as editor: the `tine` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub program: PathBuf,
}

pub const SEQUENCE_EDITOR_FLAG: &str = "--tine-sequence-editor";
pub const MESSAGE_EDITOR_FLAG: &str = "--tine-message-editor";
pub const DIR_ENV: &str = "TINE_REBASE_DIR";
pub const SESSION_ENV: &str = "TINE_REBASE_SESSION";

/// `path` quoted for the shell git runs editors through.
fn shell_quoted(path: &Path) -> OsString {
    let mut out = b"'".to_vec();
    for &b in path.as_os_str().as_bytes() {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

fn editor_command(editor: &Editor, flag: &str) -> OsString {
    let mut cmd = shell_quoted(&editor.program);
    cmd.push(" ");
    cmd.push(flag);
    cmd
}

fn with_editor(git: &GitRunner, session: &Session, editor: &Editor) -> GitRunner {
    git.clone()
        .with_env("GIT_SEQUENCE_EDITOR", editor_command(editor, SEQUENCE_EDITOR_FLAG))
        .with_env("GIT_EDITOR", editor_command(editor, MESSAGE_EDITOR_FLAG))
        .with_env(DIR_ENV, session_dir(&session.git_dir))
        .with_env(SESSION_ENV, &session.id)
}

/// The runner for continuing or skipping the rebase in progress: with Tine
/// as git's editor if Tine started it (its messages then apply), else
/// `git` as is.
pub fn rebase_runner(git: &GitRunner, git_dir: &Path, editor: &Editor) -> Result<GitRunner> {
    match read_session(git_dir)? {
        Some(s) if owns_current_rebase(&s)? => Ok(with_editor(git, &s, editor)),
        _ => Ok(git.clone()),
    }
}

/// A random hex name (for temporary files that mustn't collide).
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut bytes);
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>() + &format!("{nanos:x}")
}

/// Starts the rebase of `plan` (made for `range`). Right before, checks
/// that nothing is in progress and that HEAD, the branch and the commits
/// are still those the plan was made for. `autostash`: local changes were
/// stashed for it (recorded before git runs).
pub fn start(git: &GitRunner, git_dir: &Path, range: &Range, plan: &Plan, editor: &Editor, autostash: Option<Oid>) -> Result<Step> {
    if repo_state(git_dir) != RepoState::Clean || has_conflicts(git)? {
        bail!("another operation is in progress");
    }
    let now = load_range(git, range.base, &CancelToken::default())?;
    if now.head != range.head || now.branch != range.branch {
        bail!("HEAD moved since the interactive rebase was planned; plan it again");
    }
    let problems = plan.problems(&now);
    if let Some(p) = problems.first() {
        bail!("{p}");
    }
    // Where the branch's reflog is now: its next entry is this rebase's end
    // (none readable: an end outside Tine can't be undone, nothing worse).
    let reflog = range.branch.as_deref().and_then(|b| reflog(git, b, 1).ok()?.into_iter().next()).map(|(entry, ..)| entry);
    // Nothing in progress: a session left behind is stale.
    clear_session(git_dir)?;
    let dir = session_dir(git_dir);
    std::fs::create_dir_all(dir.join("msg"))?;
    let session = Session {
        id: random_id(),
        git_dir: git_dir.to_path_buf(),
        head: range.head,
        branch: range.branch.clone(),
        base: range.base,
        items: plan.items.clone(),
        messages: plan.needed_messages(),
        autostash,
        reflog,
    };
    std::fs::write(dir.join("todo"), plan.todo(range))?;
    for key in &session.messages {
        std::fs::write(dir.join("msg").join(key.file_name()), &plan.messages[key])?;
    }
    write_atomically(&dir.join("session"), session.encode().as_bytes())?;

    let runner = with_editor(git, &session, editor);
    // Options pinned so the user's rebase.* settings don't change what
    // runs (and every one of them exists in git 2.43).
    let mut args = vec!["rebase", "-i", "--no-rebase-merges", "--no-update-refs", "--no-autosquash", "--no-autostash", "--no-fork-point"];
    let base = range.base.map(|b| b.to_hex());
    match &base {
        Some(b) => args.push(b),
        None => args.push("--root"),
    }
    let result = run_step(&runner, git_dir, &args, RepoState::Rebasing);
    // Never started (the editor failed, git refused…): no session to keep.
    if repo_state(git_dir) != RepoState::Rebasing && result.is_err() {
        let _ = clear_session(git_dir);
    }
    result
}

// --- Tine as git's editor -------------------------------------------------

/// The session an editor call belongs to: `dir` must be a worktree's
/// session directory holding session `session_id`, and `file` exactly the
/// file git hands that editor (`expected`, relative to the git dir; no
/// symlink games).
fn editor_session(file: &Path, dir: &Path, session_id: &str, expected: &str) -> Result<Session> {
    let canon = |p: &Path| p.canonicalize().with_context(|| format!("can't find {}", p.display()));
    if dir.file_name().is_none_or(|n| n != "tine-rebase") {
        bail!("{} isn't a session directory", dir.display());
    }
    let git_dir = canon(dir.parent().ok_or_else(|| anyhow!("bad session directory"))?)?;
    let session = read_session(&git_dir)?.ok_or_else(|| anyhow!("no session in {}", dir.display()))?;
    if session.id != session_id {
        bail!("the session in {} isn't this rebase's", dir.display());
    }
    if canon(file)? != canon(&git_dir.join(expected))? {
        bail!("{} isn't the file of this rebase", file.display());
    }
    Ok(session)
}

/// Run as `GIT_SEQUENCE_EDITOR`: replaces git's todo list with the plan,
/// and marks the rebase as the session's.
pub fn sequence_editor(file: &Path, dir: &Path, session_id: &str) -> Result<()> {
    let session = editor_session(file, dir, session_id, "rebase-merge/git-rebase-todo")?;
    std::fs::copy(dir.join("todo"), file).with_context(|| format!("writing {}", file.display()))?;
    std::fs::write(session.git_dir.join("rebase-merge").join(MARKER), &session.id)?;
    Ok(())
}

/// One command of the rebase as git has it (done or still to do).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Command {
    action: Action,
    /// `-C` / `-c` of a fixup.
    flag: Option<String>,
    id: String,
}

impl Command {
    /// The same command (ids abbreviated alike or not).
    fn same(&self, other: &Command) -> bool {
        self.action == other.action && self.flag == other.flag && (self.id.starts_with(&other.id) || other.id.starts_with(&self.id))
    }
}

fn commands_in(text: &str) -> Option<Vec<Command>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        // exec, break, label… are no part of a plan of Tine's.
        let action = Action::parse(words.next()?)?;
        let mut id = words.next().unwrap_or_default();
        let mut flag = None;
        if id == "-C" || id == "-c" {
            flag = Some(id.to_owned());
            id = words.next().unwrap_or_default();
        }
        out.push(Command { action, flag, id: id.to_ascii_lowercase() });
    }
    Some(out)
}

/// The commands git has done and has still to do, if they are those of
/// the session's plan (in order; drops aside, which git may not record).
/// `None`: the rebase isn't (or is no longer) the plan — e.g. its todo was
/// edited outside Tine.
fn plan_position(session: &Session) -> Result<Option<(usize, Command)>> {
    let read = |name: &str| match std::fs::read_to_string(session.git_dir.join("rebase-merge").join(name)) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    };
    let (Some(done), Some(todo)) = (commands_in(&read("done")?), commands_in(&read("git-rebase-todo")?)) else { return Ok(None) };
    let kept = |c: &&Command| c.action != Action::Drop;
    let todo: Vec<&Command> = todo.iter().filter(kept).collect();
    let tried: Vec<&Command> = done.iter().filter(kept).collect();
    // A command git couldn't start (e.g. an untracked file in its way) goes
    // back to the front of the todo and stays in done as well: such an
    // attempt — followed by the same command — isn't one of its own.
    let done: Vec<&Command> = tried
        .iter()
        .enumerate()
        .filter(|&(i, c)| tried.get(i + 1).or(todo.first()).is_none_or(|next| !next.same(c)))
        .map(|(_, c)| *c)
        .collect();
    let plan: Vec<&(Oid, Action)> = session.items.iter().filter(|(_, a)| *a != Action::Drop).collect();
    if done.len() + todo.len() != plan.len() {
        return Ok(None);
    }
    for (c, (id, a)) in done.iter().chain(&todo).zip(&plan) {
        if c.flag.is_some() || c.action != *a || c.id.len() < 4 || !id.to_hex().starts_with(&c.id) {
            return Ok(None);
        }
    }
    let Some(last) = done.last() else { return Ok(None) };
    Ok(Some((done.len() - 1, (*last).clone())))
}

/// The message file for the command git is at, if the plan has one for it
/// (`None`: git's own message). A message the plan has whose file is gone
/// is an error, not git's message.
fn planned_message(session: &Session, dir: &Path) -> Result<Option<String>> {
    let Some((at, cmd)) = plan_position(session)? else { return Ok(None) };
    let plan = session.plan();
    let kept: Vec<usize> = (0..plan.items.len()).filter(|&i| plan.items[i].1 != Action::Drop).collect();
    let item = kept[at];
    let id = plan.items[item].0;
    let key = match cmd.action {
        Action::Reword => MessageKey::Reword(id),
        Action::Squash | Action::Fixup => {
            match plan.groups().into_iter().find(|g| g.terminal() == Some(item)) {
                Some(g) if g.has_squash(&plan) => MessageKey::Group(id),
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    if !session.messages.contains(&key) {
        return Ok(None);
    }
    let path = dir.join("msg").join(key.file_name());
    let message = std::fs::read_to_string(&path).with_context(|| format!("the message for {} is missing ({})", id.short(), path.display()))?;
    Ok(Some(message))
}

/// Run as `GIT_EDITOR` during the rebase: puts the user's message for the
/// commit git is at into `file`; otherwise leaves git's message.
pub fn message_editor(file: &Path, dir: &Path, session_id: &str) -> Result<()> {
    let session = editor_session(file, dir, session_id, "COMMIT_EDITMSG")?;
    if let Some(message) = planned_message(&session, dir)? {
        std::fs::write(file, message).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

/// The commit of the `reword` git is at, if the plan has a message for it.
fn planned_reword(session: &Session) -> Result<Option<Oid>> {
    let Some((at, cmd)) = plan_position(session)? else { return Ok(None) };
    if cmd.action != Action::Reword || planned_message(session, &session_dir(&session.git_dir))?.is_none() {
        return Ok(None);
    }
    let kept: Vec<Oid> = session.items.iter().filter(|(_, a)| *a != Action::Drop).map(|(id, _)| *id).collect();
    Ok(kept.get(at).copied())
}

/// For keeping a commit that became empty at a `reword`: the message the
/// plan has for it (a file to commit with), if Tine started this rebase.
pub fn empty_reword_message(git_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(session) = read_session(git_dir)? else { return Ok(None) };
    if !owns_current_rebase(&session)? {
        return Ok(None);
    }
    Ok(planned_reword(&session)?.map(|id| session_dir(git_dir).join("msg").join(MessageKey::Reword(id).file_name())))
}

/// Before continuing: a `reword` that git committed under the old message
/// but couldn't reword (a hook refused) is reworded now. git stops there and
/// leaves the rewording to whoever continues (`rebase --continue` alone
/// keeps the old message) — with an `amend` file when the commit stayed
/// the same, without one when it was made anew. So it is recognised by
/// HEAD: git made it with the commit's author, date and message (not so
/// the commit before, when git stopped before making one, nor one amended
/// with another message since). Done only if Tine started this rebase and
/// the plan has a message for the commit; `git` is the rebase's runner
/// (Tine as the editor, so the message goes through git's cleanup and hooks
/// as it would have). `Ok(false)`: nothing to do.
pub fn finish_reword(git: &GitRunner, git_dir: &Path) -> Result<bool> {
    let Some(session) = read_session(git_dir)? else { return Ok(false) };
    if !owns_current_rebase(&session)? {
        return Ok(false);
    }
    let Some(id) = planned_reword(&session)? else { return Ok(false) };
    if has_conflicts(git)? {
        return Ok(false);
    }
    let info = |rev: &str| -> Result<String> {
        let out = git.output(["log", "-1", "--date=raw", "--format=%an%x00%ae%x00%ad%x00%B", rev, "--"])?;
        Ok(String::from_utf8_lossy(&out).trim_end().to_owned())
    };
    if info("HEAD")? != info(&id.to_hex())? {
        return Ok(false);
    }
    git.output(["commit", "--amend", "--allow-empty"])?;
    Ok(true)
}

/// The last command git did in the rebase in progress (`None`: none).
pub fn last_command(git_dir: &Path) -> Option<Action> {
    let done = std::fs::read_to_string(git_dir.join("rebase-merge/done")).ok()?;
    commands_in(&done).and_then(|c| c.last().map(|c| c.action))
}

/// The `tine` binary as git's editor: `tine --tine-…-editor <file>` with the
/// session in the environment. `None` when not started that way.
pub fn run_editor(args: &[OsString]) -> Option<i32> {
    let [flag, file] = args else { return None };
    let sequence = flag == SEQUENCE_EDITOR_FLAG;
    if !sequence && flag != MESSAGE_EDITOR_FLAG {
        return None;
    }
    let (Some(dir), Some(id)) = (std::env::var_os(DIR_ENV), std::env::var(SESSION_ENV).ok()) else {
        eprintln!("tine: {} needs {DIR_ENV} and {SESSION_ENV}", flag.to_string_lossy());
        return Some(1);
    };
    let (file, dir) = (Path::new(file), Path::new(&dir));
    let result = if sequence { sequence_editor(file, dir, &id) } else { message_editor(file, dir, &id) };
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tine: {e:#}");
            1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Oid {
        Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap()
    }

    fn range(n: u8) -> Range {
        Range {
            base: Some(id(0)),
            head: id(n),
            branch: Some("main".into()),
            commits: (1..=n)
                .map(|i| RebaseCommit {
                    id: id(i),
                    subject: format!("c{i}"),
                    message: format!("c{i}\n\nbody {i}"),
                    author: "a".into(),
                    author_time: 0,
                    published: false,
                })
                .collect(),
        }
    }

    fn plan(actions: &[(u8, Action)]) -> Plan {
        Plan { items: actions.iter().map(|&(n, a)| (id(n), a)).collect(), messages: HashMap::new() }
    }

    #[test]
    fn groups_skip_drops_and_end_at_the_last_member() {
        use Action::*;
        let p = plan(&[(1, Pick), (2, Squash), (3, Drop), (4, Fixup), (5, Reword), (6, Fixup), (7, Edit)]);
        let g = p.groups();
        assert_eq!(g, [Group { anchor: 0, members: vec![1, 3] }, Group { anchor: 4, members: vec![5] }, Group { anchor: 6, members: vec![] }]);
        // The squash group's message is asked at its last member (a fixup).
        assert_eq!(g[0].terminal(), Some(3));
        assert!(g[0].has_squash(&p) && !g[1].has_squash(&p));
        // A group's message is only used once written.
        assert_eq!(p.needed_messages(), [MessageKey::Reword(id(5))]);
        let mut p = p;
        p.messages.insert(MessageKey::Group(id(4)), "written".into());
        assert_eq!(p.needed_messages(), [MessageKey::Reword(id(5)), MessageKey::Group(id(4))]);
    }

    #[test]
    fn default_group_messages_follow_git() {
        use Action::*;
        let r = range(4);
        let mut p = plan(&[(1, Reword), (2, Squash), (3, Fixup), (4, Squash)]);
        p.messages.insert(MessageKey::Reword(id(1)), "new first".into());
        let g = &p.groups()[0];
        assert_eq!(p.default_group_message(g, &r), "new first\n\nc2\n\nbody 2\n\nc4\n\nbody 4", "fixup messages left out");
    }

    #[test]
    fn problems_are_found() {
        use Action::*;
        let r = range(3);
        assert!(plan(&[(1, Pick), (2, Pick), (3, Pick)]).problems(&r).is_empty());
        let has = |p: Plan, what: &str| p.problems(&r).iter().any(|m| m.contains(what));
        assert!(has(plan(&[(1, Drop), (2, Drop), (3, Drop)]), "Every commit"));
        assert!(has(plan(&[(1, Drop), (2, Squash), (3, Pick)]), "no commit below"), "the first kept one can't meld");
        assert!(has(plan(&[(1, Pick), (2, Pick)]), "missing"));
        assert!(has(plan(&[(1, Pick), (2, Pick), (2, Pick), (3, Pick)]), "twice"));
        assert!(has(plan(&[(1, Pick), (2, Pick), (3, Pick), (9, Pick)]), "isn't one of"));
        assert!(has(plan(&[(1, Reword), (2, Pick), (3, Pick)]), "new message"));
        // A squash group needs no message (git combines them) unless one
        // was written, which can't be empty.
        let mut p = plan(&[(1, Pick), (2, Squash), (3, Pick)]);
        assert!(p.problems(&r).is_empty());
        p.messages.insert(MessageKey::Group(id(2)), " ".into());
        assert!(p.problems(&r).iter().any(|m| m.contains("squashed up to")));
        p.messages.insert(MessageKey::Group(id(2)), "both".into());
        assert!(p.problems(&r).is_empty());
    }

    #[test]
    fn todo_and_session_round_trip() {
        use Action::*;
        let r = range(2);
        let p = plan(&[(2, Pick), (1, Fixup)]);
        assert_eq!(p.todo(&r), format!("pick {} c2\nfixup {} c1\n", id(2).to_hex(), id(1).to_hex()));
        let s = Session {
            id: "abc".into(),
            git_dir: "/g".into(),
            head: id(9),
            branch: Some("refs/heads/feature/x y".into()),
            base: None,
            items: p.items.clone(),
            messages: vec![MessageKey::Reword(id(1)), MessageKey::Group(id(2))],
            autostash: Some(id(7)),
            reflog: Some(format!("{} refs/heads/feature/x y@{{1800000000 +0900}}", id(8).to_hex())),
        };
        assert_eq!(Session::decode(&s.encode(), Path::new("/g")).unwrap(), s);
    }

    #[test]
    fn git_commands_are_matched_against_the_plan() {
        use Action::*;
        let dir = tempfile::tempdir().unwrap();
        let rm = dir.path().join("rebase-merge");
        std::fs::create_dir_all(&rm).unwrap();
        let s = Session {
            id: "x".into(),
            git_dir: dir.path().into(),
            head: id(9),
            branch: None,
            base: None,
            items: vec![(id(1), Pick), (id(2), Drop), (id(3), Squash), (id(4), Reword)],
            messages: vec![],
            autostash: None,
            reflog: None,
        };
        let write = |done: &str, todo: &str| {
            std::fs::write(rm.join("done"), done).unwrap();
            std::fs::write(rm.join("git-rebase-todo"), todo).unwrap();
        };
        let h = |n: u8| id(n).to_hex();
        // Short commands and ids, drops recorded or not, comments.
        write(&format!("p {}\ns {} msg\n", &h(1)[..7], &h(3)[..7]), &format!("# comment\nr {} x\n", &h(4)[..10]));
        assert_eq!(plan_position(&s).unwrap().map(|(at, c)| (at, c.action)), Some((1, Squash)));
        write(&format!("pick {}\ndrop {}\nsquash {}\n", h(1), h(2), h(3)), &format!("reword {}\n", h(4)));
        assert_eq!(plan_position(&s).unwrap().map(|(at, _)| at), Some(1));
        // Put back by git (it couldn't start it): in done and in the todo;
        // then tried again (twice in done).
        write(&format!("pick {}\nsquash {}\n", h(1), h(3)), &format!("squash {}\nreword {}\n", h(3), h(4)));
        assert_eq!(plan_position(&s).unwrap().map(|(at, c)| (at, c.action)), Some((0, Pick)));
        write(&format!("pick {}\nsquash {}\nsquash {}\n", h(1), &h(3)[..7], h(3)), &format!("reword {}\n", h(4)));
        assert_eq!(plan_position(&s).unwrap().map(|(at, c)| (at, c.action)), Some((1, Squash)));
        // Edited outside Tine: a flag, another order, an exec, a missing
        // line, a command added again later.
        for (done, todo) in [
            (format!("pick {}\nsquash {}\npick {}\n", h(1), h(3), h(1)), format!("reword {}\n", h(4))),
            (format!("pick {}\nfixup -c {}\n", h(1), h(3)), format!("reword {}\n", h(4))),
            (format!("pick {}\nreword {}\n", h(1), h(4)), format!("squash {}\n", h(3))),
            (format!("pick {}\nexec make\nsquash {}\n", h(1), h(3)), format!("reword {}\n", h(4))),
            (format!("pick {}\n", h(1)), format!("reword {}\n", h(4))),
        ] {
            write(&done, &todo);
            assert_eq!(plan_position(&s).unwrap(), None, "{done}{todo}");
        }
    }

    #[test]
    fn editor_commands_quote_the_path() {
        let e = Editor { program: "/opt/my tools/it's/tine".into() };
        assert_eq!(editor_command(&e, SEQUENCE_EDITOR_FLAG), "'/opt/my tools/it'\\''s/tine' --tine-sequence-editor");
    }
}
