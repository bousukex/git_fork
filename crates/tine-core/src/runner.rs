//! Spawning `git` with a consistent, machine-parseable environment.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result};

/// A git invocation that exited unsuccessfully.
#[derive(Debug, Clone)]
pub struct GitError {
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stderr: String,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = self.stderr.trim();
        if msg.is_empty() {
            write!(f, "git {} failed (exit {:?})", self.args.join(" "), self.code)
        } else {
            write!(f, "{msg}")
        }
    }
}

impl std::error::Error for GitError {}

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

/// Runs git commands against one repository.
#[derive(Debug, Clone)]
pub struct GitRunner {
    workdir: PathBuf,
    git: OsString,
    extra_env: Vec<(OsString, OsString)>,
}

impl GitRunner {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self { workdir: workdir.into(), git: "git".into(), extra_env: Vec::new() }
    }

    /// Adds an environment variable to every invocation (tests use this to
    /// isolate from the user's global git config).
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.extra_env.push((key.into(), value.into()));
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

    /// Runs to completion and returns stdout. `ok_codes` lists exit codes
    /// besides 0 that are not failures (e.g. 1 for `diff --no-index`).
    pub fn output_with<I, S>(&self, args: I, ok_codes: &[i32]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to run git in {}", self.workdir.display()))?;
        let code = out.status.code();
        if out.status.success() || code.is_some_and(|c| ok_codes.contains(&c)) {
            Ok(out.stdout)
        } else {
            Err(GitError {
                args: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
                code,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
            .into())
        }
    }

    /// Like [`Self::output_with`], feeding `input` to git's stdin.
    pub fn output_with_input<I, S>(&self, args: I, input: &[u8], ok_codes: &[i32]) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use std::io::Write as _;
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut child = self
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run git in {}", self.workdir.display()))?;
        // Write from another thread so a large output can't deadlock us.
        let mut stdin = child.stdin.take().expect("piped");
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output()?;
        let _ = writer.join();
        let code = out.status.code();
        if out.status.success() || code.is_some_and(|c| ok_codes.contains(&c)) {
            Ok(out.stdout)
        } else {
            Err(GitError {
                args: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
                code,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }
            .into())
        }
    }

    pub fn output<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with(args, &[])
    }

    /// Streams stdout split at `delim`, calling `on_record` for each record
    /// (without the delimiter). Stops early and kills git when `on_record`
    /// returns `false` or `cancel` fires, even while git is silent (a
    /// watchdog thread kills git's whole process group).
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
        use std::os::unix::process::CommandExt as _;

        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut child = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so helpers git spawns die with it.
            .process_group(0)
            .spawn()
            .with_context(|| format!("failed to run git in {}", self.workdir.display()))?;
        let pid = child.id();
        let stderr_thread = drain_stderr(&mut child);

        let done = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let (done, cancel) = (done.clone(), cancel.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if cancel.is_cancelled() {
                        kill_group(pid);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        };

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
            if buf.last() == Some(&delim) {
                buf.pop();
            }
            if !on_record(&buf) {
                stopped = true;
                break Ok(());
            }
        };
        stopped |= cancel.is_cancelled();
        if stopped || read_result.is_err() {
            kill_group(pid);
        }
        done.store(true, Ordering::Relaxed);
        drop(reader);
        let status = child.wait()?;
        let _ = watchdog.join();
        let stderr = stderr_thread.join().unwrap_or_default();
        read_result?;
        if stopped || status.success() {
            Ok(())
        } else {
            Err(GitError {
                args: args.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
                code: status.code(),
                stderr,
            }
            .into())
        }
    }
}

/// Kills a process group started with `process_group(0)` (group id == pid).
fn kill_group(pid: u32) {
    // SAFETY: plain syscall; a stale pid at worst hits an exited group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<String> {
    let mut stderr = child.stderr.take().expect("piped");
    std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stderr.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
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
}
