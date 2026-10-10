//! Fetch/Pull/Push toolbar buttons and the credential dialog, driven through
//! the AccessKit tree (no GPU needed).

mod common;

use std::{path::PathBuf, time::Duration};

use common::{git, settle};
use egui_kittest::{
    Harness,
    kittest::{NodeT as _, Queryable as _},
};
use tine::{
    app::TineApp,
    askpass,
    ops_registry,
    worker::{OpKind, OpOutcome},
};

static PROMPTS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Credential prompts are process-wide and hold back every app's dialogs:
/// the test that opens one runs alone.
fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
    PROMPTS.read().unwrap_or_else(|e| e.into_inner())
}

fn app(paths: Vec<PathBuf>) -> Harness<'static, TineApp> {
    Harness::builder().with_size([1200.0, 760.0]).build_eframe(move |cc| TineApp::new(cc, paths))
}

fn rev(dir: &std::path::Path, rev: &str) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["rev-parse", rev]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn remote_buttons_need_a_remote() {
    let _shared = shared();
    let repo = common::sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    for label in ["Fetch", "Pull", "Push"] {
        assert!(h.get_by_label(label).accesskit_node().is_disabled(), "{label}");
    }
}

#[test]
fn push_button_opens_the_dialog_and_pushes() {
    let _shared = shared();
    let (bare, mine) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut t = 1_880_000_000;
    git(bare.path(), &["init", "-q", "--bare", "-b", "main"], &mut t);
    git(mine.path(), &["clone", "-q", &format!("file://{}", bare.path().display()), "."], &mut t);
    git(mine.path(), &["config", "user.name", "山田 太郎"], &mut t);
    git(mine.path(), &["config", "user.email", "taro@example.com"], &mut t);
    std::fs::write(mine.path().join("f.txt"), "f\n").unwrap();
    git(mine.path(), &["add", "-A"], &mut t);
    git(mine.path(), &["commit", "-qm", "first"], &mut t);

    // A slow pre-push hook keeps the push running for a moment.
    let hook = mine.path().join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let mut h = app(vec![mine.path().into()]);
    settle(&mut h);
    for label in ["Fetch", "Pull", "Push"] {
        assert!(!h.get_by_label(label).accesskit_node().is_disabled(), "{label}");
    }
    h.get_by_label("Push").click();
    h.run_steps(2);
    assert!(h.query_by_label("Push main").is_some(), "the dialog is open");
    // The dialog's own Push button comes after the toolbar's.
    h.get_all_by_label("Push").last().unwrap().click();
    h.run_steps(3);
    for label in ["Fetch", "Pull", "Push", "Stash", "Branch"] {
        assert!(h.get_by_label(label).accesskit_node().is_disabled(), "{label} while pushing");
    }
    assert!(h.query_by_label("Cancel").is_some(), "the running push can be cancelled");
    let start = std::time::Instant::now();
    while rev(bare.path(), "main") != rev(mine.path(), "HEAD") {
        assert!(start.elapsed() < Duration::from_secs(10), "not pushed");
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(20));
    }
    settle(&mut h);
    assert!(h.query_by_label("Push main").is_none(), "the dialog closed");
}

#[test]
fn credential_dialog_sends_the_typed_password() {
    let _exclusive = PROMPTS.write().unwrap_or_else(|e| e.into_inner());
    // An operation that is waiting on a prompt (as git would through askpass).
    let dir = tempfile::tempdir().unwrap();
    let paths = tine_core::RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let (tx, _rx) = crossbeam_channel::unbounded();
    let (env_tx, env_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    ops_registry::submit(
        &PathBuf::from("/test/ui/askpass"),
        &paths,
        OpKind::Push("main".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            env_tx.send(askpass::env_for(ctx.id, "Push", std::path::Path::new("/home/me/project"), None).unwrap()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
            Ok(OpOutcome::default())
        }),
    )
    .unwrap();
    let env = env_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    let (socket, token) = (PathBuf::from(get(askpass::SOCKET_ENV)), get(askpass::TOKEN_ENV).into_string().unwrap());
    let asked = std::thread::spawn(move || {
        askpass::request(&socket, &token, "", "Password for 'https://alice@example.com': ").unwrap()
    });

    let repo = common::sample_repo();
    let mut h = app(vec![repo.path().into()]);
    settle(&mut h);
    // A tab dialog that comes up meanwhile waits behind the prompt.
    h.state_mut().active_tab_mut().unwrap().open_stash_dialog();
    let start = std::time::Instant::now();
    while h.query_by_label("Credentials needed").is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "dialog never showed");
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(h.query_by_label_contains("Password for 'https://alice@example.com'").is_some());
    assert!(h.query_by_label_contains("/home/me/project").is_some());
    assert!(h.query_by_label("Stash Changes").is_none(), "hidden while the prompt is open");
    let field = h.get_by_role(egui::accesskit::Role::PasswordInput);
    field.focus();
    field.type_text("pässwörd");
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert_eq!(asked.join().unwrap().as_deref(), Some("pässwörd"));
    assert!(h.query_by_label("Credentials needed").is_none());
    assert!(h.query_by_label("Stash Changes").is_some(), "the tab's dialog shows once the prompt is answered");
    release_tx.send(()).unwrap();
}

/// A file:// remote with one commit, my clone, and a colleague's clone.
fn remote_setup() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, i64) {
    let (bare, mine, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut t = 1_890_000_000;
    git(bare.path(), &["init", "-q", "--bare", "-b", "main"], &mut t);
    let url = format!("file://{}", bare.path().display());
    for d in [mine.path(), other.path()] {
        git(d, &["clone", "-q", &url, "."], &mut t);
        git(d, &["config", "user.name", "山田 太郎"], &mut t);
        git(d, &["config", "user.email", "taro@example.com"], &mut t);
    }
    std::fs::write(other.path().join("f.txt"), "base\n").unwrap();
    git(other.path(), &["add", "-A"], &mut t);
    git(other.path(), &["commit", "-qm", "base"], &mut t);
    git(other.path(), &["push", "-q", "origin", "main"], &mut t);
    git(mine.path(), &["pull", "-q", "origin", "main"], &mut t);
    (bare, mine, other, t)
}

fn other_pushes(other: &std::path::Path, file: &str, t: &mut i64) {
    std::fs::write(other.join(file), "x\n").unwrap();
    git(other, &["pull", "-q", "--rebase=false", "origin", "main"], t);
    git(other, &["add", "-A"], t);
    git(other, &["commit", "-qm", file], t);
    git(other, &["push", "-q", "origin", "main"], t);
}

fn wait(h: &mut Harness<'static, TineApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, TineApp>) -> bool) {
    let start = std::time::Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(10), "{what}");
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn fetch_and_pull_buttons() {
    let _shared = shared();
    let (_bare, mine, other, mut t) = remote_setup();
    let mut h = app(vec![mine.path().into()]);
    settle(&mut h);
    other_pushes(other.path(), "g.txt", &mut t);
    h.get_by_label("Fetch").click();
    h.run_steps(2);
    assert!(h.query_by_label("Remove remote branches that no longer exist (prune)").is_some(), "fetch dialog");
    h.get_all_by_label("Fetch").last().unwrap().click();
    let theirs = rev(other.path(), "HEAD");
    wait(&mut h, "fetched", |h| {
        let tab = h.state_mut().active_tab_mut().unwrap();
        tab.tracking_target("origin", "main").map(|o| o.to_hex()) == Some(theirs.clone())
    });
    settle(&mut h);
    h.get_by_label("Pull").click();
    h.run_steps(2);
    assert!(h.query_by_label("Pull into main").is_some(), "pull dialog");
    h.get_all_by_label("Pull").last().unwrap().click();
    wait(&mut h, "pulled", |_| rev(mine.path(), "HEAD") == theirs);
}

#[test]
fn force_push_keeps_the_commit_shown_when_the_dialog_opened() {
    let _shared = shared();
    let (bare, mine, other, mut t) = remote_setup();
    git(mine.path(), &["commit", "-q", "--amend", "-m", "rewritten"], &mut t);
    let mut h = app(vec![mine.path().into()]);
    settle(&mut h);
    let shown = rev(mine.path(), "origin/main");
    h.get_by_label("Push").click();
    h.run_steps(2);
    h.get_by_label("Force push (overwrite the remote branch)").click();
    h.run_steps(2);
    let hint = format!("only if it is still at {}", &shown[..8]);
    assert!(h.query_by_label_contains(&hint).is_some(), "the dialog names the commit it may replace");

    // While the dialog is open, a colleague pushes and a fetch picks it up.
    other_pushes(other.path(), "g.txt", &mut t);
    git(mine.path(), &["fetch", "-q"], &mut t);
    let theirs = rev(other.path(), "HEAD");
    wait(&mut h, "refreshed", |h| {
        let tab = h.state_mut().active_tab_mut().unwrap();
        tab.tracking_target("origin", "main").map(|o| o.to_hex()) == Some(theirs.clone())
    });
    assert!(h.query_by_label_contains(&hint).is_some(), "still the commit that was shown");
    h.get_by_label("Force Push").click();
    wait(&mut h, "push finished", |h| {
        let tab = h.state_mut().active_tab_mut().unwrap();
        !tab.errors.is_empty()
    });
    let tab = h.state_mut().active_tab_mut().unwrap();
    assert!(tab.errors.last().unwrap().contains("moved since you confirmed"), "{:?}", tab.errors);
    assert_eq!(rev(bare.path(), "main"), theirs, "the colleague's commit survives");
}

/// Starts an operation that waits until released, and returns a function
/// that asks a prompt through it (as git would).
fn waiting_operation(
    name: &str,
) -> (impl Fn(&str) -> std::thread::JoinHandle<Option<String>>, ops_registry::OpId, crossbeam_channel::Sender<()>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = tine_core::RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let (tx, _rx) = crossbeam_channel::unbounded();
    let (env_tx, env_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    let id = ops_registry::submit(
        &PathBuf::from(format!("/test/ui/{name}")),
        &paths,
        OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            env_tx.send(askpass::env_for(ctx.id, "Fetch", std::path::Path::new("/home/me/project"), None).unwrap()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            Ok(OpOutcome::default())
        }),
    )
    .unwrap();
    let env = env_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    let (socket, token) = (PathBuf::from(get(askpass::SOCKET_ENV)), get(askpass::TOKEN_ENV).into_string().unwrap());
    let ask = move |prompt: &str| {
        let (socket, token, prompt) = (socket.clone(), token.clone(), prompt.to_owned());
        std::thread::spawn(move || askpass::request(&socket, &token, "", &prompt).unwrap())
    };
    (ask, id, release_tx)
}

#[test]
fn typed_secrets_dont_outlive_their_prompt() {
    let _exclusive = PROMPTS.write().unwrap_or_else(|e| e.into_inner());
    let (ask, op, release) = waiting_operation("secrets");
    let mut h = app(Vec::new());
    let password = ask("Password for 'https://alice@example.com': ");
    wait(&mut h, "password prompt", |h| h.query_by_role(egui::accesskit::Role::PasswordInput).is_some());
    let field = h.get_by_role(egui::accesskit::Role::PasswordInput);
    field.focus();
    field.type_text("topsecret");
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert_eq!(password.join().unwrap().as_deref(), Some("topsecret"));
    assert!(!tine::ui::askpass::input_kept(&h.ctx), "forgotten once answered");

    // The next prompt's field can't undo its way back to the password.
    let user = ask("Username for 'https://example.com': ");
    wait(&mut h, "username prompt", |h| h.query_by_label_contains("Username for").is_some());
    let field = h.get_by_role(egui::accesskit::Role::TextInput);
    field.focus();
    h.run_steps(1);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    h.run_steps(2);
    let field = h.get_by_role(egui::accesskit::Role::TextInput);
    assert_eq!(field.value().unwrap_or_default(), "");
    field.type_text("alice");
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert_eq!(user.join().unwrap().as_deref(), Some("alice"));

    // A prompt that goes away with its operation takes the typed text along.
    let gone = ask("Password for 'https://alice@example.com': ");
    wait(&mut h, "third prompt", |h| h.query_by_role(egui::accesskit::Role::PasswordInput).is_some());
    let field = h.get_by_role(egui::accesskit::Role::PasswordInput);
    field.focus();
    field.type_text("half typed");
    h.run_steps(2);
    assert!(tine::ui::askpass::input_kept(&h.ctx));
    ops_registry::cancel(op);
    assert_eq!(gone.join().unwrap(), None);
    h.run_steps(2);
    assert!(h.query_by_label("Credentials needed").is_none());
    assert!(!tine::ui::askpass::input_kept(&h.ctx));
    release.send(()).unwrap();
}
