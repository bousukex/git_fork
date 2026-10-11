//! External programs (M4d): started with the right arguments and folder,
//! waited for, and how they ended reported.

use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use tine::tools::{self, Launch, Tool, ToolResult, ToolSettings};

/// Results come through one process-wide queue: one test at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The result for `origin` (waits up to 10 s).
fn result_for(origin: &Path) -> ToolResult {
    let start = Instant::now();
    loop {
        if let Some(r) = tools::take_results().into_iter().find(|r| r.origin == origin) {
            return r;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no result");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn launch(program: &Path, dir: &Path) -> Launch {
    Launch {
        label: "Open in Terminal".into(),
        program: program.into(),
        args: Vec::new(),
        dir: dir.into(),
        startup: Some(Duration::from_secs(10)),
        fallback: None,
    }
}

#[test]
fn arguments_and_folder_arrive_intact() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // A work tree whose name has a blank and a byte that isn't UTF-8.
    let workdir = tmp.path().join(OsStr::from_bytes(b"my repo \xff"));
    std::fs::create_dir(&workdir).unwrap();
    let out = tmp.path().join("out");
    let term = script(tmp.path(), "term", &format!("pwd > '{}'\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> '{}'; done", out.display(), out.display()));
    let settings = ToolSettings { terminal: vec![term.to_string_lossy().into(), "--cwd={dir}".into(), "a b".into(), "日本".into()] };
    let ctx = egui::Context::default();
    tools::start(tools::plan(&Tool::Terminal, &workdir, &settings, &tools::System).unwrap(), workdir.clone(), ctx);
    assert_eq!(result_for(&workdir).outcome, Ok(None));
    let written = std::fs::read(&out).unwrap();
    let (pwd, args) = written.split_at(written.iter().position(|&b| b == b'\n').unwrap() + 1);
    assert_eq!(&pwd[..pwd.len() - 1], workdir.as_os_str().as_bytes(), "started in the work tree");
    let args: Vec<&[u8]> = args.split(|&b| b == 0).filter(|a| !a.is_empty()).collect();
    let mut cwd = b"--cwd=".to_vec();
    cwd.extend_from_slice(workdir.as_os_str().as_bytes());
    assert_eq!(args, [cwd.as_slice(), b"a b", "日本".as_bytes()]);
}

#[test]
fn failures_say_why() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let failing = script(tmp.path(), "failing", "echo 'boom: no display' >&2\nexit 3");
    tools::start(launch(&failing, tmp.path()), tmp.path().into(), ctx.clone());
    let e = result_for(tmp.path()).outcome.unwrap_err();
    assert!(e.starts_with("Open in Terminal failed") && e.contains("exited with code 3") && e.contains("boom: no display"), "{e}");

    let missing = tmp.path().join("no-such-terminal");
    tools::start(launch(&missing, tmp.path()), tmp.path().into(), ctx.clone());
    let e = result_for(tmp.path()).outcome.unwrap_err();
    assert!(e.contains("couldn't start no-such-terminal"), "{e}");

    // A tool given in the settings that can't be split into words never
    // gets that far (the settings refuse it); one with no program at all:
    let empty = ToolSettings { terminal: vec![String::new()] };
    let planned = tools::plan(&Tool::Terminal, tmp.path(), &empty, &tools::System).unwrap();
    tools::start(planned, tmp.path().into(), ctx);
    assert!(result_for(tmp.path()).outcome.is_err());
}

#[test]
fn a_late_non_zero_exit_is_only_noted() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // A terminal closed after its shell's last command failed.
    let closed = script(tmp.path(), "closed", "sleep 0.4\nexit 1");
    let mut l = launch(&closed, tmp.path());
    l.startup = Some(Duration::from_millis(100));
    tools::start(l, tmp.path().into(), egui::Context::default());
    let note = result_for(tmp.path()).outcome.unwrap().unwrap();
    assert!(note.contains("closed exited with code 1"), "{note}");
}

#[test]
fn a_failed_launch_falls_back() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let marker = tmp.path().join("fallback-ran");
    let fallback = script(tmp.path(), "fallback", &format!("touch '{}'", marker.display()));
    let mut l = launch(&tmp.path().join("no-file-manager"), tmp.path());
    l.fallback = Some(Box::new(launch(&fallback, tmp.path())));
    tools::start(l, tmp.path().into(), ctx.clone());
    assert_eq!(result_for(tmp.path()).outcome, Ok(None));
    assert!(marker.exists());

    // An opener that fails late (D-Bus waiting for an answer) still falls
    // back: only terminals may end late without failing.
    std::fs::remove_file(&marker).unwrap();
    let slow = script(tmp.path(), "slow-dbus", "sleep 0.4\necho 'no reply' >&2\nexit 1");
    let mut l = launch(&slow, tmp.path());
    l.startup = None;
    l.fallback = Some(Box::new(launch(&fallback, tmp.path())));
    tools::start(l, tmp.path().into(), ctx.clone());
    assert_eq!(result_for(tmp.path()).outcome, Ok(None));
    assert!(marker.exists(), "fell back after a slow failure");
    let mut l = launch(&slow, tmp.path());
    l.startup = Some(Duration::from_millis(100));
    l.fallback = Some(Box::new(launch(&fallback, tmp.path())));
    tools::start(l, tmp.path().into(), ctx.clone());
    assert!(result_for(tmp.path()).outcome.unwrap().is_some(), "a terminal closed late is noted, not retried");

    // Both failing: both reasons.
    let mut l = launch(&tmp.path().join("no-file-manager"), tmp.path());
    l.fallback = Some(Box::new(launch(&tmp.path().join("no-xdg-open"), tmp.path())));
    tools::start(l, tmp.path().into(), ctx);
    let e = result_for(tmp.path()).outcome.unwrap_err();
    assert!(e.contains("no-file-manager") && e.contains("no-xdg-open"), "{e}");
}
