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
    /// returns `false` or `cancel` fires.
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
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut child = self
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run git in {}", self.workdir.display()))?;
        let stderr_thread = drain_stderr(&mut child);
        let mut reader = BufReader::with_capacity(1 << 16, child.stdout.take().expect("piped"));
        let mut buf = Vec::with_capacity(512);
        let mut stopped = false;
        loop {
            if cancel.is_cancelled() {
                stopped = true;
                break;
            }
            buf.clear();
            let n = reader.read_until(delim, &mut buf)?;
            if n == 0 {
                break;
            }
            if buf.last() == Some(&delim) {
                buf.pop();
            }
            if !on_record(&buf) {
                stopped = true;
                break;
            }
        }
        if stopped {
            let _ = child.kill();
        }
        drop(reader);
        let status = child.wait()?;
        let stderr = stderr_thread.join().unwrap_or_default();
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
