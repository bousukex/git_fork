//! Credential prompts through the real `tine` binary acting as askpass.

use std::{path::PathBuf, time::Duration};

use crossbeam_channel::unbounded;
use tine::{
    askpass::{self, Answer, PromptKind},
    ops_registry::{self, OpState, Outcome},
    worker::{Msg, OpKind, OpOutcome},
};
use tine_core::{GitRunner, RepoPaths};

fn wait_prompt(op: ops_registry::OpId) -> askpass::Prompt {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(p) = askpass::pending_for(op) {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no prompt arrived");
}

/// Runs `git credential fill` for https://example.com as an operation.
fn credential_fill(repo: &str) -> (ops_registry::OpId, crossbeam_channel::Receiver<Msg>, tempfile::TempDir) {
    askpass::set_program(env!("CARGO_BIN_EXE_tine"));
    let dir = tempfile::tempdir().unwrap();
    let base = GitRunner::new(dir.path()).with_env("GIT_CONFIG_GLOBAL", "/dev/null").with_env("GIT_CONFIG_NOSYSTEM", "1");
    let paths = RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let (tx, rx) = unbounded();
    let id = ops_registry::submit(
        &PathBuf::from(repo),
        &paths,
        OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            let git = ctx.network(ctx.git(&base))?;
            let out = git.output_with_input(["credential", "fill"], b"protocol=https\nhost=example.com\n\n", &[])?;
            Ok(OpOutcome::new(String::from_utf8_lossy(&out).into_owned()))
        }),
    )
    .unwrap();
    (id, rx, dir)
}

fn result(rx: &crossbeam_channel::Receiver<Msg>) -> anyhow::Result<OpOutcome> {
    loop {
        if let Msg::OpDone(_, _, r) = rx.recv_timeout(Duration::from_secs(10)).expect("operation finished") {
            return r;
        }
    }
}

#[test]
fn username_then_password() {
    let (id, rx, _dir) = credential_fill("/test/askpass/ok");
    let user = wait_prompt(id);
    assert_eq!(user.op, id);
    assert!(user.text.starts_with("Username for 'https://example.com'"), "{}", user.text);
    assert_eq!(user.kind, PromptKind::Text);
    assert_eq!(user.op_label, "Fetch");
    assert_eq!(ops_registry::state(id), Some(OpState::AwaitingAuth));
    askpass::answer(user.id, Answer::Value("alice".into()));

    let pass = wait_prompt(id);
    assert!(pass.text.starts_with("Password for 'https://alice@example.com'"), "{}", pass.text);
    assert_eq!(pass.kind, PromptKind::Secret);
    askpass::answer(pass.id, Answer::Value("s3cret with spaces".into()));

    let out = result(&rx).unwrap();
    assert!(out.summary.contains("username=alice\n"), "{}", out.summary);
    assert!(out.summary.contains("password=s3cret with spaces\n"), "{}", out.summary);
    assert_eq!(ops_registry::state(id), Some(OpState::Finished(Outcome::Succeeded)));
}

#[test]
fn cancel_stops_the_operation_and_refuses_later_prompts() {
    let (id, rx, _dir) = credential_fill("/test/askpass/cancel");
    let user = wait_prompt(id);
    assert_eq!(user.op, id);
    let start = std::time::Instant::now();
    askpass::answer(user.id, Answer::Cancel);
    let err = result(&rx).unwrap_err();
    assert!(err.downcast_ref::<tine_core::Cancelled>().is_some(), "{err:#}");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(ops_registry::state(id), Some(OpState::Finished(Outcome::Cancelled)));
    assert_eq!(askpass::pending_for(id), None, "no second prompt");
}

#[test]
fn askpass_binary_without_a_known_token_answers_nothing() {
    askpass::set_program(env!("CARGO_BIN_EXE_tine"));
    // Start the server through a session we never use.
    let env = askpass::env_for(u64::MAX, "Fetch", std::path::Path::new("/"), None).unwrap();
    let socket = env.iter().find(|(k, _)| k == askpass::SOCKET_ENV).unwrap().1.clone();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tine"))
        .arg("Password: ")
        .env(askpass::SOCKET_ENV, &socket)
        .env(askpass::TOKEN_ENV, "0123456789abcdef")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    askpass::forget(u64::MAX);
}

#[test]
fn prompts_after_a_cancel_are_refused_without_asking() {
    let (tx, rx) = unbounded();
    let (env_tx, env_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    let dir = tempfile::tempdir().unwrap();
    let paths = RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let id = ops_registry::submit(
        &PathBuf::from("/test/askpass/refused"),
        &paths,
        OpKind::Push("x".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            env_tx.send(askpass::env_for(ctx.id, "Push", std::path::Path::new("/"), None).unwrap()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            Ok(OpOutcome::default())
        }),
    )
    .unwrap();
    let env = env_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    let (socket, token) = (PathBuf::from(get(askpass::SOCKET_ENV)), get(askpass::TOKEN_ENV).into_string().unwrap());

    let first = {
        let (socket, token) = (socket.clone(), token.clone());
        std::thread::spawn(move || askpass::request(&socket, &token, "", "Username: ").unwrap())
    };
    let prompt = wait_prompt(id);
    askpass::answer(prompt.id, Answer::Cancel);
    assert_eq!(first.join().unwrap(), None);

    // git asking again (e.g. for the password) gets no for an answer at once.
    let start = std::time::Instant::now();
    assert_eq!(askpass::request(&socket, &token, "", "Password: ").unwrap(), None);
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_eq!(askpass::pending_for(id), None);
    release_tx.send(()).unwrap();
    let _ = result(&rx);
}

/// Starts the server and returns its socket.
fn socket() -> PathBuf {
    askpass::set_program(env!("CARGO_BIN_EXE_tine"));
    let env = askpass::env_for(u64::MAX - 1, "Fetch", std::path::Path::new("/"), None).unwrap();
    askpass::forget(u64::MAX - 1);
    PathBuf::from(env.iter().find(|(k, _)| k == askpass::SOCKET_ENV).unwrap().1.clone())
}

#[test]
fn oversized_and_stalled_requests_are_dropped() {
    use std::io::{Read as _, Write as _};
    let socket = socket();
    // Too long: refused without asking.
    let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let _ = s.write_all(&vec![b'x'; 70 * 1024]);
    let _ = s.shutdown(std::net::Shutdown::Write);
    // "N", or a reset because the server hung up with the rest unread.
    let mut reply = Vec::new();
    match s.read_to_end(&mut reply) {
        Ok(_) => assert_eq!(reply, b"N"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
    }

    // Never finishes its request: the server hangs up after a few seconds.
    let mut stalled = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    stalled.write_all(b"half a tok").unwrap();
    stalled.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let start = std::time::Instant::now();
    let mut buf = Vec::new();
    let _ = stalled.read_to_end(&mut buf);
    assert!(start.elapsed() < Duration::from_secs(6), "{:?}", start.elapsed());
    assert!(buf.is_empty());
}

#[test]
fn prompt_of_a_vanished_asker_goes_away() {
    let (tx, _rx) = unbounded();
    let (env_tx, env_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    let dir = tempfile::tempdir().unwrap();
    let paths = RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let id = ops_registry::submit(
        &PathBuf::from("/test/askpass/vanished"),
        &paths,
        OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            env_tx.send(askpass::env_for(ctx.id, "Fetch", std::path::Path::new("/"), None).unwrap()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            Ok(OpOutcome::default())
        }),
    )
    .unwrap();
    let env = env_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    // Like the askpass process killed together with git: ask, then vanish.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tine"))
        .arg("Password: ")
        .env(askpass::SOCKET_ENV, get(askpass::SOCKET_ENV))
        .env(askpass::TOKEN_ENV, get(askpass::TOKEN_ENV))
        .spawn()
        .unwrap();
    wait_prompt(id);
    child.kill().unwrap();
    child.wait().unwrap();
    let start = std::time::Instant::now();
    while askpass::pending_for(id).is_some() {
        assert!(start.elapsed() < Duration::from_secs(3), "prompt still shown");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ops_registry::state(id), Some(OpState::Running), "no longer waiting for credentials");
    release_tx.send(()).unwrap();
}
