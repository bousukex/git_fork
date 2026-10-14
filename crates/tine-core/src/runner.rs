//! Spawning `git` with a consistent, machine-parseable environment.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

/// A git invocation that exited unsuccessfully.
#[derive(Debug, Clone)]
pub struct GitError {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stderr: String,
    /// Some commands explain failures on stdout (e.g. "nothing to commit").
    pub stdout: String,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = if self.stderr.trim().is_empty() { self.stdout.trim() } else { self.stderr.trim() };
        if msg.is_empty() {
            write!(f, "git {} failed (exit {:?})", self.args.join(" "), self.code)
        } else {
            write!(f, "{msg}")
        }
    }
}

impl std::error::Error for GitError {}

/// The command was stopped because its [`CancelToken`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Set when the caller is no longer interested in a long-running command.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One finished git invocation, as shown in the Output panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub args: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub duration: Duration,
    pub cancelled: bool,
}

/// Collects [`CommandRecord`]s of every command run through a runner that
/// carries it; survives failures part-way through multi-command operations.
#[derive(Debug, Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<CommandRecord>>>);

/// Output kept per stream in a record (some commands print a lot).
const RECORD_LIMIT: usize = 64 * 1024;

fn clip(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(&bytes[..bytes.len().min(RECORD_LIMIT)]).into_owned();
    if bytes.len() > RECORD_LIMIT { s + "\n…(truncated)" } else { s }
}

impl Recorder {
    pub fn snapshot(&self) -> Vec<CommandRecord> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn push(&self, rec: CommandRecord) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(rec);
    }
}

/// Runs git commands against one repository.
#[derive(Debug, Clone)]
pub struct GitRunner {
    workdir: PathBuf,
    git: OsString,
    extra_env: Vec<(OsString, OsString)>,
    recorder: Option<Recorder>,
    /// Stops commands run through this runner (and refuses new ones).
    cancel: Option<CancelToken>,
}

/// How long a cancelled git gets to exit after SIGTERM (it removes its lock
/// files on SIGTERM) before it is killed.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Stops a process group started with `process_group(0)` (group id == pid),
/// in the background (see [`stop_group_and_wait`]): reads that end early
/// don't wait for it.
fn stop_group(pid: u32) {
    std::thread::spawn(move || stop_group_and_wait(pid));
}

/// SIGTERM to the group now, SIGKILL if anything of it is still around after
/// [`TERM_GRACE`]; returns once nothing of it is left (or a moment after the
/// SIGKILL). Its output may be closed long before (a hook that closed it
/// and ignores SIGTERM): a stopped command ends only after this, so nothing
/// it started outlives it — or Tine, which quits once its operations ended.
fn stop_group_and_wait(pid: u32) {
    let pgid = -(pid as libc::pid_t);
    // SAFETY: plain syscalls on a process group we created; signal 0 only
    // checks whether any process of it is left.
    let gone = || unsafe { libc::kill(pgid, 0) } != 0;
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if gone() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while !gone() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The watchdog of a command that stops when its token fires.
struct Watch {
    done: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    fn none() -> Self {
        Watch { done: Arc::default(), stopped: Arc::default(), thread: None }
    }

    /// The command has ended: the watchdog is let go — once, if it stopped
    /// the command, nothing the command started is left. Whether it did.
    fn finish(mut self) -> bool {
        self.done.store(true, Ordering::Relaxed);
        let stopped = self.stopped.load(Ordering::Relaxed);
        if stopped && let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        stopped
    }
}

impl GitRunner {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self { workdir: workdir.into(), git: "git".into(), extra_env: Vec::new(), recorder: None, cancel: None }
    }

    /// Adds an environment variable to every invocation (tests use this to
    /// isolate from the user's global git config).
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Records every command run through the returned runner.
    pub fn with_recorder(mut self, recorder: Recorder) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Makes every command run through the returned runner stop when
    /// `cancel` fires, and refuse to start once it has.
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// The same runner without cancellation, for recovery steps that must
    /// complete even after the user cancelled (e.g. restoring a stash).
    pub fn uncancellable(&self) -> Self {
        Self { cancel: None, ..self.clone() }
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// Runs `program` instead of the `git` on PATH (tests use a wrapper
    /// that fails on purpose).
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.git = program.into();
        self
    }

    /// Same configuration, different directory.
    pub fn with_workdir(&self, workdir: impl Into<PathBuf>) -> Self {
        Self { workdir: workdir.into(), ..self.clone() }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// A `git` command preconfigured for parsing: stable locale, unquoted
    /// non-ASCII paths, no pager/editor/terminal prompts, and no optional
    /// index locks (so background refreshes never block the user's own git).
    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.git);
        cmd.current_dir(&self.workdir)
            .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
            .args(args)
            .env("LC_ALL", "C.UTF-8")
            .env("LANGUAGE", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            // File names are never globs for us (`*` and `[` are legal in names).
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .stdin(Stdio::null());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    fn record(&self, args: &[OsString], stdout: &[u8], stderr: &[u8], code: Option<i32>, started: Instant, cancelled: bool) {
        if let Some(r) = &self.recorder {
            r.push(CommandRecord {
                args: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
                stdout: clip(stdout),
                stderr: clip(stderr),
                code,
                duration: started.elapsed(),
                cancelled,
            });
        }
    }

    fn error(args: &[OsString], code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> anyhow::Error {
        GitError {
            args: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
            code,
            stderr: String::from_utf8_lossy(stderr).into_owned(),
            stdout: String::from_utf8_lossy(stdout).into_owned(),
        }
        .into()
    }

    /// Runs to completion and returns stdout. `ok_codes` lists exit codes
    /// besides 0 that are not failures (e.g. 1 for `diff --no-index`).
    pub fn output_with<I, S>(&self, args: I, ok_codes: &[i32]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.capture(&args, None, ok_codes)
    }

    /// Like [`Self::output_with`], feeding `input` to git's stdin.
    pub fn output_with_input<I, S>(&self, args: I, input: &[u8], ok_codes: &[i32]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.capture(&args, Some(input), ok_codes)
    }

    /// Runs a command to completion, capturing its output; cancellable when
    /// the runner carries a token.
    fn capture(&self, args: &[OsString], input: Option<&[u8]>, ok_codes: &[i32]) -> Result<Vec<u8>> {
        use std::io::Write as _;
        self.check_cancelled()?;
        let started = Instant::now();
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let (mut child, watch) = match &self.cancel {
            Some(token) => self.spawn_cancellable_cmd(cmd, token)?,
            None => (cmd.spawn().with_context(|| format!("failed to run git in {}", self.workdir.display()))?, Watch::none()),
        };
        // Write from another thread so a large output can't deadlock us.
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped");
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            })
        });
        let out = child.wait_with_output();
        let stopped = watch.finish();
        if let Some(w) = writer {
            let _ = w.join();
        }
        let out = out?;
        let code = out.status.code();
        // A failure right as the token fired (e.g. a credential prompt the
        // user cancelled made git give up) is a cancellation too.
        let was_stopped = stopped || (!out.status.success() && self.cancel.as_ref().is_some_and(CancelToken::is_cancelled));
        self.record(args, &out.stdout, &out.stderr, code, started, was_stopped);
        if was_stopped {
            return Err(Cancelled.into());
        }
        if out.status.success() || code.is_some_and(|c| ok_codes.contains(&c)) {
            Ok(out.stdout)
        } else {
            Err(Self::error(args, code, &out.stdout, &out.stderr))
        }
    }

    pub fn output<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with(args, &[])
    }

    /// Spawns git in its own process group with a watchdog that stops the
    /// group when `cancel` fires (finish the watch once git was waited for).
    fn spawn_cancellable(&self, args: &[OsString], cancel: &CancelToken) -> Result<(Child, Watch)> {
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.spawn_cancellable_cmd(cmd, cancel)
    }

    fn spawn_cancellable_cmd(&self, mut cmd: Command, cancel: &CancelToken) -> Result<(Child, Watch)> {
        use std::os::unix::process::CommandExt as _;
        let child = cmd
            // Own process group, so helpers git spawns are stopped with it.
            .process_group(0)
            .spawn()
            .with_context(|| format!("failed to run git in {}", self.workdir.display()))?;
        let pid = child.id();
        let done = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (d, s, c) = (done.clone(), stopped.clone(), cancel.clone());
        let thread = std::thread::spawn(move || {
            while !d.load(Ordering::Relaxed) {
                if c.is_cancelled() {
                    s.store(true, Ordering::Relaxed);
                    stop_group_and_wait(pid);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok((child, Watch { done, stopped, thread: Some(thread) }))
    }

    /// Streams stdout split at `delim`, calling `on_record` for each record
    /// (without the delimiter). Stops early when `on_record` returns `false`
    /// or `cancel` fires, even while git is silent. Stopping is not an error
    /// here (used for reading history).
    pub fn stream_records<I, S>(
        &self,
        args: I,
        delim: u8,
        cancel: &CancelToken,
        mut on_record: impl FnMut(&[u8]) -> bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.stream_terminated(args, delim, cancel, |rec, _| on_record(rec))
    }

    /// Like [`Self::stream_records`]; also says whether each record ended
    /// with `delim` (only the last one can lack it: output cut off).
    pub fn stream_terminated<I, S>(
        &self,
        args: I,
        delim: u8,
        cancel: &CancelToken,
        mut on_record: impl FnMut(&[u8], bool) -> bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let started = Instant::now();
        let (mut child, watch) = self.spawn_cancellable(&args, cancel)?;
        let mut recorded_stdout = Vec::new();
        let pid = child.id();
        let stderr_thread = drain(child.stderr.take().expect("piped"));
        let mut reader = BufReader::with_capacity(1 << 16, child.stdout.take().expect("piped"));
        let mut buf = Vec::with_capacity(512);
        let mut stopped = false;
        let read_result = loop {
            if cancel.is_cancelled() {
                stopped = true;
                break Ok(());
            }
            buf.clear();
            match reader.read_until(delim, &mut buf) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
            if self.recorder.is_some() && recorded_stdout.len() <= RECORD_LIMIT {
                recorded_stdout.extend_from_slice(&buf);
            }
            let terminated = buf.last() == Some(&delim);
            if terminated {
                buf.pop();
            }
            if !on_record(&buf, terminated) {
                stopped = true;
                break Ok(());
            }
        };
        stopped |= cancel.is_cancelled();
        if stopped || read_result.is_err() {
            stop_group(pid);
        }
        drop(reader);
        let status = child.wait()?;
        watch.finish();
        let stderr = stderr_thread.join().unwrap_or_default();
        self.record(&args, &recorded_stdout, &stderr, status.code(), started, stopped);
        read_result?;
        if stopped || status.success() {
            Ok(())
        } else {
            Err(Self::error(&args, status.code(), b"", &stderr))
        }
    }

    /// Runs to completion and returns stdout — unless it grows past `limit`
    /// bytes: then git is stopped as soon as that is known and `Ok(None)`
    /// returned (no more than the limit and one read is ever held).
    pub fn output_limited<I, S>(&self, args: I, limit: usize, cancel: &CancelToken) -> Result<Option<Vec<u8>>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_limited_with(args, limit, cancel, &[])
    }

    /// [`Self::output_limited`] with exit codes besides 0 that are not
    /// failures.
    pub fn output_limited_with<I, S>(&self, args: I, limit: usize, cancel: &CancelToken, ok_codes: &[i32]) -> Result<Option<Vec<u8>>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.check_cancelled()?;
        let started = Instant::now();
        let (mut child, watch) = self.spawn_cancellable(&args, cancel)?;
        let pid = child.id();
        let stderr_thread = drain(child.stderr.take().expect("piped"));
        let mut stdout = child.stdout.take().expect("piped");
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        let mut too_large = false;
        let read_result = loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            }
            if out.len() > limit {
                too_large = true;
                stop_group(pid);
                break Ok(());
            }
        };
        if read_result.is_err() {
            stop_group(pid);
        }
        drop(stdout);
        let status = child.wait()?;
        let stopped = watch.finish();
        let stderr = stderr_thread.join().unwrap_or_default();
        let was_stopped = stopped || cancel.is_cancelled();
        self.record(&args, &out, &stderr, status.code(), started, was_stopped || too_large);
        if was_stopped {
            return Err(Cancelled.into());
        }
        read_result?;
        if too_large {
            return Ok(None);
        }
        if status.success() || status.code().is_some_and(|c| ok_codes.contains(&c)) {
            Ok(Some(out))
        } else {
            Err(Self::error(&args, status.code(), &out, &stderr))
        }
    }

    /// Runs a long command (fetch, push, …) that reports progress on stderr.
    /// Each progress line (split at `\r` and `\n`) goes to `on_progress`.
    /// Returns stdout on success, [`Cancelled`] if `cancel` fired, or a
    /// [`GitError`] with the full output.
    pub fn run_with_progress<I, S>(&self, args: I, cancel: &CancelToken, mut on_progress: impl FnMut(&str)) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.check_cancelled()?;
        let started = Instant::now();
        // Either token stops the command.
        let both = CancelToken::default();
        {
            let (b, c, own) = (both.clone(), cancel.clone(), self.cancel.clone());
            std::thread::spawn(move || {
                while !b.is_cancelled() {
                    if c.is_cancelled() || own.as_ref().is_some_and(CancelToken::is_cancelled) {
                        b.cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
        }
        let (mut child, watch) = self.spawn_cancellable(&args, &both)?;
        let stdout_thread = drain(child.stdout.take().expect("piped"));
        let mut stderr = child.stderr.take().expect("piped");
        let mut all_err = Vec::new();
        let mut line = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            all_err.extend_from_slice(&chunk[..n]);
            for &b in &chunk[..n] {
                if b == b'\r' || b == b'\n' {
                    if !line.is_empty() {
                        on_progress(&String::from_utf8_lossy(&line));
                        line.clear();
                    }
                } else {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            on_progress(&String::from_utf8_lossy(&line));
        }
        let status = child.wait()?;
        let stopped = watch.finish();
        let was_stopped = stopped || (!status.success() && (cancel.is_cancelled() || self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)));
        // Ends the helper thread above.
        both.cancel();
        let stdout = stdout_thread.join().unwrap_or_default();
        self.record(&args, &stdout, &all_err, status.code(), started, was_stopped);
        if was_stopped {
            return Err(Cancelled.into());
        }
        if status.success() { Ok(stdout) } else { Err(Self::error(&args, status.code(), &stdout, &all_err)) }
    }
}

fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = r.read_to_end(&mut s);
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitRunner::new(dir.path());
        let err = git.output(["rev-parse", "HEAD"]).unwrap_err();
        let err = err.downcast::<GitError>().unwrap();
        assert!(err.stderr.contains("not a git repository"), "{}", err.stderr);
    }

    /// A process is gone (or only waits to be reaped).
    fn dead(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('Z')))
    }

    #[test]
    fn a_stopped_command_ends_after_all_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        // A hook of git's (here the fsmonitor one, of a read-only command)
        // that lets go of its output and ignores SIGTERM: git itself ends at
        // once, the hook stays. (Not an alias: git passes the signal on to
        // those and waits for them.)
        let hook = dir.path().join("stubborn");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nexec >/dev/null 2>&1 </dev/null\ntrap '' TERM\necho $$ > '{}'\nwhile :; do sleep 0.05; done\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(dir.path()).status().unwrap();
        assert!(ok.success());
        let monitor = format!("core.fsmonitor={}", hook.display());
        let cancel = CancelToken::default();
        let git = GitRunner::new(dir.path()).with_cancel(cancel.clone());
        let c = cancel.clone();
        let p = pid_file.clone();
        std::thread::spawn(move || {
            while !p.exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(100));
            c.cancel();
        });
        let err = git.output(["-c", monitor.as_str(), "status"]).unwrap_err();
        assert!(err.is::<Cancelled>(), "{err:#}");
        // Ended only once the child was gone too (killed after the grace).
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(dead(pid.trim()), "the child outlived the command");
    }

    #[test]
    fn cancel_stops_a_silent_command_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitRunner::new(dir.path());
        let cancel = CancelToken::default();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            c2.cancel();
        });
        let start = std::time::Instant::now();
        // An alias that stays silent for a long time (and spawns a child
        // shell that also holds stdout open).
        git.stream_records(["-c", "alias.slow=!sleep 30", "slow"], b'\n', &cancel, |_| true).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(3), "{:?}", start.elapsed());
    }

    #[test]
    fn stream_can_stop_early() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitRunner::new(dir.path());
        let mut seen = 0;
        // `git help -a` prints plenty of lines without needing a repository.
        git.stream_records(["help", "-a"], b'\n', &CancelToken::default(), |_| {
            seen += 1;
            seen < 3
        })
        .unwrap();
        assert_eq!(seen, 3);
    }

    #[test]
    fn progress_lines_are_split_at_carriage_returns() {
        let dir = tempfile::tempdir().unwrap();
        let git = GitRunner::new(dir.path());
        let mut lines = Vec::new();
        let out = git
            .run_with_progress(
                ["-c", r"alias.p=!printf 'a 10%%\rb 50%%\rc 100%%\ndone\n' >&2; echo out", "p"],
                &CancelToken::default(),
                |l| lines.push(l.to_owned()),
            )
            .unwrap();
        assert_eq!(lines, ["a 10%", "b 50%", "c 100%", "done"]);
        assert_eq!(out, b"out\n");
    }

    #[test]
    fn progress_command_reports_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        let git = GitRunner::new(dir.path()).with_recorder(rec.clone());
        let cancel = CancelToken::default();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let start = Instant::now();
        let err = git.run_with_progress(["-c", "alias.slow=!sleep 30", "slow"], &cancel, |_| {}).unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(3));
        let records = rec.snapshot();
        assert_eq!(records.len(), 1);
        assert!(records[0].cancelled);
    }

    #[test]
    fn recorder_keeps_every_command_including_failures() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        let git = GitRunner::new(dir.path()).with_recorder(rec.clone());
        let _ = git.output(["--version"]);
        let _ = git.output(["rev-parse", "HEAD"]);
        let records = rec.snapshot();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].code, Some(0));
        assert!(records[0].stdout.starts_with("git version"));
        assert_ne!(records[1].code, Some(0));
        assert!(records[1].stderr.contains("not a git repository"));
    }

    #[test]
    fn runner_token_cancels_plain_commands() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::default();
        let git = GitRunner::new(dir.path()).with_cancel(cancel.clone());
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let start = Instant::now();
        let err = git.output(["-c", "alias.slow=!sleep 30", "slow"]).unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(3));
        // Once cancelled, nothing new starts…
        let err = git.output(["--version"]).unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some());
        // …except through the uncancellable copy used for recovery.
        assert!(git.uncancellable().output(["--version"]).is_ok());
    }

    #[test]
    fn stream_stdout_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::default();
        let git = GitRunner::new(dir.path()).with_recorder(rec.clone());
        git.stream_records(["--version"], b'\n', &CancelToken::default(), |_| true).unwrap();
        assert!(rec.snapshot()[0].stdout.starts_with("git version"));
    }

    #[test]
    fn sigterm_lets_git_finish_cleanly() {
        // A command that traps SIGTERM and exits quickly must not be SIGKILLed.
        let dir = tempfile::tempdir().unwrap();
        let git = GitRunner::new(dir.path());
        let cancel = CancelToken::default();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let marker = dir.path().join("cleaned");
        let script = format!("!trap 'touch {} ; exit 1' TERM; sleep 30 & wait", marker.display());
        let err = git.run_with_progress(["-c", &format!("alias.t={script}"), "t"], &cancel, |_| {}).unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some());
        std::thread::sleep(Duration::from_millis(100));
        assert!(marker.exists(), "the TERM handler must have run");
    }
}
