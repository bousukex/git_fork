//! Credential prompts against a real HTTP remote with Basic auth (a small
//! python server running `git http-backend`). Skipped when python3 or
//! git-http-backend is missing.

mod common;

use std::{
    io::BufRead as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use common::{git, idle, poll_until};
use pengitto::{
    askpass::{self, Answer, Prompt, PromptKind},
    ops_registry::{self, OpState, Outcome},
    repo::RepoTab,
};

struct Server {
    child: Child,
    bare: PathBuf,
    mine: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start() -> Option<Server> {
    let backend = Command::new("git").args(["--exec-path"]).output().ok()?;
    let exec_path = PathBuf::from(String::from_utf8_lossy(&backend.stdout).trim());
    if !exec_path.join("git-http-backend").exists() || Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: needs python3 and git-http-backend");
        return None;
    }
    askpass::set_program(env!("CARGO_BIN_EXE_pengitto"));
    let (root, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let bare = root.path().join("repo.git");
    let mine = work.path().to_path_buf();
    let mut t = 1_850_000_000;
    git(root.path(), &["init", "-q", "--bare", "-b", "main", "repo.git"], &mut t);
    git(&mine, &["clone", "-q", bare.to_str().unwrap(), "."], &mut t);
    git(&mine, &["config", "user.name", "山田 太郎"], &mut t);
    git(&mine, &["config", "user.email", "taro@example.com"], &mut t);
    // No credential helper, whatever the user's global config says.
    git(&mine, &["config", "credential.helper", ""], &mut t);
    std::fs::write(mine.join("f.txt"), "base\n").unwrap();
    git(&mine, &["add", "-A"], &mut t);
    git(&mine, &["commit", "-qm", "base"], &mut t);
    git(&mine, &["push", "-q", "origin", "main"], &mut t);

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/auth_http.py");
    let mut child = Command::new("python3")
        .arg(script)
        .args([root.path().to_str().unwrap(), "alice", "s3cret"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = format!("http://127.0.0.1:{}/repo.git", line.trim());
    git(&mine, &["remote", "set-url", "origin", &url], &mut t);
    Some(Server { child, bare, mine, _dirs: (root, work) })
}

fn open(dir: &Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    assert!(poll_until(&mut tab, idle));
    tab
}

/// The running operation of `repo`.
fn active_op(repo: &Path) -> ops_registry::OpId {
    let start = std::time::Instant::now();
    loop {
        if let Some(s) = ops_registry::snapshots_for(repo).into_iter().find(|s| s.state.is_active()) {
            return s.id;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no operation started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Next prompt of `op`, polling `tab` meanwhile (if still open).
fn prompt_of(op: ops_registry::OpId, mut tab: Option<&mut RepoTab>) -> Prompt {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if let Some(t) = tab.as_deref_mut() {
            t.poll();
        }
        if let Some(p) = askpass::pending_for(op) {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no prompt for operation {op}");
}

fn log_in(op: ops_registry::OpId, tab: &mut RepoTab, password: &str) {
    let user = prompt_of(op, Some(tab));
    assert!(user.text.starts_with("Username for 'http://127.0.0.1:"), "{}", user.text);
    assert_eq!(user.kind, PromptKind::Text);
    assert_eq!(ops_registry::state(op), Some(OpState::AwaitingAuth));
    askpass::answer(user.id, Answer::Value("alice".into()));
    let pass = prompt_of(op, Some(tab));
    assert!(pass.text.starts_with("Password for 'http://alice@127.0.0.1:"), "{}", pass.text);
    assert_eq!(pass.kind, PromptKind::Secret);
    askpass::answer(pass.id, Answer::Value(password.into()));
}

#[test]
fn wrong_password_fails_with_advice_and_a_retry_works() {
    let Some(s) = start() else { return };
    let mut tab = open(&s.mine);
    tab.fetch(Some("origin".into()), false);
    let op = active_op(tab.repo_key());
    log_in(op, &mut tab, "wrong");
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    let e = tab.errors.last().unwrap().clone();
    assert!(e.contains("Authentication failed"), "{e}");
    assert!(e.contains("credential helper"), "{e}");
    assert_eq!(askpass::pending_for(op), None, "git doesn't ask a third time");

    tab.errors.clear();
    tab.fetch(Some("origin".into()), false);
    let op = active_op(tab.repo_key());
    log_in(op, &mut tab, "s3cret");
    assert!(poll_until(&mut tab, |t| idle(t) && t.activity.last().is_some_and(|a| a.contains("Fetched origin"))));
    assert!(tab.errors.is_empty(), "{:?}", tab.errors);
    assert_eq!(ops_registry::state(op), Some(OpState::Finished(Outcome::Succeeded)));
}

#[test]
fn cancelling_the_prompt_cancels_the_push() {
    let Some(s) = start() else { return };
    let mut tab = open(&s.mine);
    let mut t = 1_860_000_000;
    std::fs::write(s.mine.join("g.txt"), "g\n").unwrap();
    git(&s.mine, &["add", "-A"], &mut t);
    git(&s.mine, &["commit", "-qm", "unpushed"], &mut t);
    let head = String::from_utf8(Command::new("git").current_dir(&s.mine).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    assert!(poll_until(&mut tab, |t| idle(t) && t.head_commit().map(|o| o.to_hex()) == Some(head.trim().to_owned())));
    tab.push("main", "origin", "main", false, false, None);
    let op = active_op(tab.repo_key());
    let user = prompt_of(op, Some(&mut tab));
    askpass::answer(user.id, Answer::Cancel);
    assert!(poll_until(&mut tab, |t| idle(t) && !t.errors.is_empty()));
    assert_eq!(ops_registry::state(op), Some(OpState::Finished(Outcome::Cancelled)));
    assert_eq!(askpass::pending_for(op), None, "no password prompt after cancelling");
    let out = Command::new("git").current_dir(&s.bare).args(["log", "-1", "--format=%s", "main"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "base", "nothing pushed");
}

#[test]
fn prompt_outlives_its_tab() {
    let Some(s) = start() else { return };
    let mut tab = open(&s.mine);
    let mut t = 1_870_000_000;
    std::fs::write(s.mine.join("h.txt"), "h\n").unwrap();
    git(&s.mine, &["add", "-A"], &mut t);
    git(&s.mine, &["commit", "-qm", "pushed after the tab closed"], &mut t);
    tab.push("main", "origin", "main", false, false, None);
    let op = active_op(tab.repo_key());
    let user = prompt_of(op, Some(&mut tab));
    let workdir = tab.paths.workdir.clone();
    drop(tab);

    askpass::answer(user.id, Answer::Value("alice".into()));
    let pass = prompt_of(op, None);
    assert_eq!(pass.workdir, workdir, "the dialog says which repository asks");
    askpass::answer(pass.id, Answer::Value("s3cret".into()));
    let start = std::time::Instant::now();
    let orphan = loop {
        if let Some(o) = pengitto::worker::take_orphaned_results().into_iter().find(|o| o.origin == workdir) {
            break o;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "result never arrived");
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = orphan.result.expect("pushed");
    assert!(out.summary.contains("pushed main"), "{}", out.summary);
    let log = Command::new("git").current_dir(&s.bare).args(["log", "-1", "--format=%s", "main"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "pushed after the tab closed");
}
