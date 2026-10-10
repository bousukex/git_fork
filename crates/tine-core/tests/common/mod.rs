//! Throwaway repositories for integration tests, isolated from the user's
//! git configuration and with deterministic timestamps.

#![allow(dead_code)]

use std::{
    cell::Cell,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

use tine_core::{CliReader, GitRunner};

pub struct TestRepo {
    dir: tempfile::TempDir,
    clock: Cell<i64>,
}

const ENV: [(&str, &str); 4] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test User"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
];

impl TestRepo {
    pub fn new() -> Self {
        let repo = TestRepo { dir: tempfile::tempdir().unwrap(), clock: Cell::new(1_700_000_000) };
        repo.git(["init", "-q", "-b", "main"]);
        repo
    }

    /// A second repository cloned from this one (for remote-tracking refs).
    pub fn clone_repo(&self) -> TestRepo {
        let repo = TestRepo { dir: tempfile::tempdir().unwrap(), clock: Cell::new(self.clock.get()) };
        let src = self.path().to_str().unwrap().to_owned();
        repo.git(["clone", "-q", &src, "."]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn runner(&self) -> GitRunner {
        ENV.iter().fold(GitRunner::new(self.path()), |g, (k, v)| g.with_env(*k, *v))
    }

    pub fn reader(&self) -> CliReader {
        CliReader::with_runner(self.runner()).unwrap()
    }

    /// Runs git and returns trimmed stdout; panics on failure.
    pub fn git<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (ok, stdout, stderr) = self.try_git(args);
        assert!(ok, "git failed: {stderr}");
        stdout
    }

    /// Runs git and returns (success, stdout, stderr).
    pub fn try_git<I, S>(&self, args: I) -> (bool, String, String)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let t = self.clock.get() + 60;
        self.clock.set(t);
        let date = format!("@{t} +0900");
        let out = Command::new("git")
            .current_dir(self.path())
            .args(args)
            .envs(ENV)
            .env("GIT_COMMITTER_NAME", "Test User")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub fn write(&self, name: impl AsRef<Path>, content: &str) -> PathBuf {
        let path = self.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Writes a file and commits everything; returns the new commit id.
    pub fn commit_file(&self, name: &str, content: &str, message: &str) -> String {
        self.write(name, content);
        self.git(["add", "-A"]);
        self.git(["commit", "-q", "-m", message]);
        self.git(["rev-parse", "HEAD"])
    }
}
