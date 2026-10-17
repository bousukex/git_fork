//! The running pengitto doing what others ask (M7b): repositories opened (or
//! switched to), and nothing taken left undone when it closes — asked just
//! before, or while quitting is asked or under way, it stays and does it.

mod common;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use common::{git, settle};
use egui_kittest::{Harness, kittest::Queryable as _};
use pengitto::{
    app::{AppOptions, MakeActivator, PengittoApp},
    instance::{self, Activate, Claim, Place, Reply, Request, State},
};

/// One test at a time (operations are counted process-wide).
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

struct Running {
    _rt: tempfile::TempDir,
    place: Place,
    primary: instance::Primary,
}

fn running() -> Running {
    let rt = tempfile::tempdir().unwrap();
    std::fs::set_permissions(rt.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let ns = std::fs::read_link("/proc/self/ns/mnt").unwrap().to_string_lossy().into_owned();
    let place = instance::place(Some(rt.path().as_os_str()), &ns, Some("w")).unwrap();
    let Claim::Primary(primary) = instance::claim(&place, &Request::default(), Duration::from_secs(5)) else { panic!() };
    Running { _rt: rt, place, primary }
}

fn harness(r: &Running) -> Harness<'static, PengittoApp> {
    harness_with(AppOptions { instance: Some(r.primary.link.clone()), ..Default::default() })
}

fn harness_with(options: AppOptions) -> Harness<'static, PengittoApp> {
    let mut h = Harness::builder()
        .with_size([1100.0, 700.0])
        .build_eframe(move |cc| PengittoApp::with_options(cc, vec![], None, options));
    settle(&mut h);
    h
}

fn repo(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join(name);
    std::fs::create_dir(&d).unwrap();
    let mut t = 1_800_000_000;
    git(&d, &["init", "-q", "-b", "main"], &mut t);
    std::fs::write(d.join("a"), "a\n").unwrap();
    git(&d, &["add", "-A"], &mut t);
    git(&d, &["commit", "-qm", "base"], &mut t);
    (dir, d)
}

fn ask(r: &Running, paths: &[&Path]) -> Reply {
    let req = Request { cwd: "/".into(), paths: paths.iter().map(|p| p.to_path_buf()).collect(), ..Default::default() };
    instance::send(&r.place.socket(), &req).unwrap()
}

fn until(h: &mut Harness<'static, PengittoApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, PengittoApp>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(15), "never: {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn close_window(h: &mut Harness<'static, PengittoApp>) {
    h.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    h.step();
}

#[test]
fn what_others_ask_is_opened_or_switched_to() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    let (_b, b) = repo("beta");
    // Not taken before the app runs.
    assert_eq!(ask(&r, &[&a]), Reply::Starting);
    let mut h = harness(&r);
    assert_eq!(r.primary.link.state(), State::Ready);
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    until(&mut h, "alpha open", |h| h.state().tab_count() == 1);
    assert_eq!(ask(&r, &[&b]), Reply::Accepted);
    until(&mut h, "beta open", |h| h.state().tab_count() == 2 && h.state().active_title() == Some("beta"));
    // Open already: switched to, not opened again.
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    until(&mut h, "back to alpha", |h| h.state().active_title() == Some("alpha"));
    h.run_steps(5);
    assert_eq!(h.state().tab_count(), 2);
    // Nothing to open: nothing changes.
    assert_eq!(ask(&r, &[]), Reply::Accepted);
    h.run_steps(3);
    assert_eq!(h.state().tab_count(), 2);
    // The window goes: no more taken.
    eframe::App::on_exit(h.state_mut());
    assert_eq!(r.primary.link.state(), State::Closing);
    assert_eq!(ask(&r, &[&a]), Reply::Closing);
}

#[test]
fn asked_just_before_closing_it_stays_and_does_it() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    let mut h = harness(&r);
    // Taken (not yet done by the UI), then the window is closed.
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    close_window(&mut h);
    until(&mut h, "opened instead", |h| h.state().tab_count() == 1);
    assert_eq!(r.primary.link.state(), State::Ready, "still taking requests");
    // Closed again with nothing asked: closing, others turned away.
    close_window(&mut h);
    assert_eq!(r.primary.link.state(), State::Closing);
    assert_eq!(ask(&r, &[&a]), Reply::Closing);
}

#[test]
fn asked_while_stopping_operations_to_quit_it_stays_and_does_it() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    let mut h = harness(&r);
    // Something runs (until stopped).
    let (tx, _rx) = crossbeam_channel::unbounded();
    let paths = pengitto_core::RepoPaths { workdir: "/m7b/quit".into(), git_dir: "/m7b/quit".into(), common_dir: "/m7b/quit".into() };
    let (go_on, wait) = crossbeam_channel::bounded::<()>(0);
    pengitto::ops_registry::submit(
        Path::new("/m7b/quit"),
        &paths,
        pengitto::worker::OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Stopped, but not quite ended until let go.
            let _ = wait.recv();
            Err(pengitto_core::Cancelled.into())
        }),
    )
    .unwrap();
    close_window(&mut h);
    h.run_steps(3);
    h.get_by_label("Stop and Quit").click();
    until(&mut h, "stopping", |h| h.state().stopping());
    // Asked meanwhile: quitting called off, the repository opened.
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    until(&mut h, "opened instead of quitting", |h| !h.state().stopping() && h.state().tab_count() == 1);
    assert!(h.state().notices().iter().any(|n| n.contains("Quitting was called off")), "{:?}", h.state().notices());
    // What was stopped ends; it stays.
    drop(go_on);
    until(&mut h, "the operation ended", |_| pengitto::ops_registry::active_ops().is_empty());
    h.run_steps(5);
    assert!(!h.state().quitting());
    assert_eq!(r.primary.link.state(), State::Ready);
}

#[test]
fn asked_while_quitting_is_asked_it_stays() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    let mut h = harness(&r);
    let (tx, _rx) = crossbeam_channel::unbounded();
    let paths = pengitto_core::RepoPaths { workdir: "/m7b/ask".into(), git_dir: "/m7b/ask".into(), common_dir: "/m7b/ask".into() };
    let id = pengitto::ops_registry::submit(
        Path::new("/m7b/ask"),
        &paths,
        pengitto::worker::OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(|ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(pengitto_core::Cancelled.into())
        }),
    )
    .unwrap();
    close_window(&mut h);
    h.run_steps(3);
    assert!(h.state().quit_asked());
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    until(&mut h, "opened, the question gone", |h| !h.state().quit_asked() && h.state().tab_count() == 1);
    assert!(h.query_by_label("Stop and Quit").is_none());
    pengitto::ops_registry::cancel(id);
    until(&mut h, "the operation ended", |_| pengitto::ops_registry::active_ops().is_empty());
}

/// The app's logic alone, as eframe runs it while the window is hidden
/// (minimized, on another desktop): no UI — what it sends the window.
fn hidden_logic(h: &mut Harness<'static, PengittoApp>, close: bool) -> Vec<egui::ViewportCommand> {
    let info = egui::ViewportInfo {
        minimized: Some(true),
        events: if close { vec![egui::ViewportEvent::Close] } else { vec![] },
        ..Default::default()
    };
    let raw = egui::RawInput { viewports: std::iter::once((egui::ViewportId::ROOT, info)).collect(), ..Default::default() };
    let ctx = h.ctx.clone();
    let mut frame = eframe::Frame::_new_kittest();
    let out = ctx.run_logic(&raw, |ctx| eframe::App::logic(h.state_mut(), ctx, &mut frame));
    out.viewport_commands.get(&egui::ViewportId::ROOT).cloned().unwrap_or_default()
}

#[test]
fn what_is_asked_is_done_while_the_window_is_hidden() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    let (_b, b) = repo("beta");
    let mut h = harness(&r);
    // Taken and done with the UI not running: opened, the window asked to
    // come back (no activator: not Wayland).
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    let sent = hidden_logic(&mut h, false);
    assert!(sent.contains(&egui::ViewportCommand::Minimized(false)) && sent.contains(&egui::ViewportCommand::Focus), "{sent:?}");
    assert!(r.primary.link.take_requests().is_empty(), "taken by the app");
    // Closed (hidden) as something is asked: it stays and does it.
    assert_eq!(ask(&r, &[&b]), Reply::Accepted);
    let sent = hidden_logic(&mut h, true);
    assert!(sent.contains(&egui::ViewportCommand::CancelClose), "{sent:?}");
    assert_eq!(r.primary.link.state(), State::Ready);
    // Both opened, the UI never running.
    hidden_until(&mut h, "both open", |h| h.state().tab_count() == 2);
    // Closed again with nothing asked: closing.
    let sent = hidden_logic(&mut h, true);
    assert!(!sent.contains(&egui::ViewportCommand::CancelClose), "{sent:?}");
    assert_eq!(r.primary.link.state(), State::Closing);
    assert_eq!(ask(&r, &[&a]), Reply::Closing);
}

/// Runs the hidden logic until `done` (never the UI).
fn hidden_until(h: &mut Harness<'static, PengittoApp>, what: &str, mut done: impl FnMut(&mut Harness<'static, PengittoApp>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(15), "never: {what}");
        hidden_logic(h, false);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_repository_asked_for_keeps_the_window_open_until_it_is_found() {
    let _t = turn();
    let r = running();
    let (_a, a) = repo("alpha");
    // Its discovery waits: git reads its configuration, which includes a
    // FIFO (opening it waits for a writer).
    let gate = a.parent().unwrap().join("gate");
    let made = std::process::Command::new("mkfifo").arg(&gate).status().unwrap();
    assert!(made.success());
    let config = a.join(".git/config");
    let before = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{before}[include]\n\tpath = {}\n", gate.display())).unwrap();
    let mut h = harness(&r);
    assert_eq!(ask(&r, &[&a]), Reply::Accepted);
    hidden_logic(&mut h, false);
    // Closed while it is found: it stays (whatever the frames meanwhile).
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(100));
        let sent = hidden_logic(&mut h, true);
        assert!(sent.contains(&egui::ViewportCommand::CancelClose), "{sent:?}");
    }
    assert_eq!((h.state().tab_count(), r.primary.link.state()), (0, State::Ready));
    // Found (the gate let go, the include gone for what git runs next): a
    // tab, the UI never running; then it may close.
    std::fs::write(&config, before).unwrap();
    drop(std::fs::OpenOptions::new().write(true).open(&gate).unwrap());
    hidden_until(&mut h, "opened", |h| h.state().tab_count() == 1);
    let sent = hidden_logic(&mut h, true);
    assert!(!sent.contains(&egui::ViewportCommand::CancelClose), "{sent:?}");
    assert_eq!(r.primary.link.state(), State::Closing);
}

/// An activator that records what it was asked, and whether it was dropped.
#[derive(Clone, Default)]
struct Probe {
    tokens: Arc<Mutex<Vec<String>>>,
    dropped: Arc<AtomicBool>,
}

struct ProbeActivator(Probe);

impl Activate for ProbeActivator {
    fn activate(&self, token: &str) -> bool {
        self.0.tokens.lock().unwrap().push(token.to_owned());
        true
    }
}

impl Drop for ProbeActivator {
    fn drop(&mut self) {
        self.0.dropped.store(true, SeqCst);
    }
}

fn making(probe: &Probe) -> MakeActivator {
    let probe = probe.clone();
    MakeActivator(Arc::new(move |_| Ok(Box::new(ProbeActivator(probe.clone())) as Box<dyn Activate>)))
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_launchers_token_is_used_once_one_at_a_time_or_not() {
    let _t = turn();
    // Not one at a time: the window's own token all the same.
    let probe = Probe::default();
    let mut h = harness_with(AppOptions { launch_token: Some("own".into()), make_activator: Some(making(&probe)), ..Default::default() });
    wait_for("the own token used", || !probe.tokens.lock().unwrap().is_empty());
    h.run_steps(5);
    assert_eq!(*probe.tokens.lock().unwrap(), ["own"]);
    eframe::App::on_exit(h.state_mut());
    assert!(probe.dropped.load(SeqCst), "nothing kept it");
    drop(h);
    // One at a time: the own token, then the others' with the same activator.
    let r = running();
    let probe = Probe::default();
    let mut h = harness_with(AppOptions {
        instance: Some(r.primary.link.clone()),
        launch_token: Some("own".into()),
        make_activator: Some(making(&probe)),
        ..Default::default()
    });
    wait_for("the activator handed over", || r.primary.link.has_activator());
    assert_eq!(*probe.tokens.lock().unwrap(), ["own"]);
    let with = Request { cwd: "/".into(), token: Some("theirs".into()), ..Default::default() };
    assert_eq!(instance::send(&r.place.socket(), &with).unwrap(), Reply::Accepted);
    assert_eq!(*probe.tokens.lock().unwrap(), ["own", "theirs"]);
    // Activated: nothing more asked of the window.
    let sent = hidden_logic(&mut h, false);
    assert!(!sent.contains(&egui::ViewportCommand::Focus), "{sent:?}");
    h.run_steps(5);
    assert_eq!(probe.tokens.lock().unwrap().len(), 2, "the own token once");
    eframe::App::on_exit(h.state_mut());
    assert!(probe.dropped.load(SeqCst) && !r.primary.link.has_activator());
}

#[test]
fn the_activator_being_made_is_stopped_when_the_app_goes_without_on_exit() {
    let _t = turn();
    let r = running();
    let probe = Probe::default();
    let (started, gave_up) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (p, s, g) = (probe.clone(), started.clone(), gave_up.clone());
    // Made only once told to stop (as one waiting on the compositor), and
    // a while after that (what it was doing, finished).
    let make = MakeActivator(Arc::new(move |stop: &AtomicBool| {
        s.store(true, SeqCst);
        let start = Instant::now();
        while !stop.load(SeqCst) {
            if start.elapsed() > Duration::from_secs(10) {
                g.store(true, SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(300));
        Ok(Box::new(ProbeActivator(p.clone())) as Box<dyn Activate>)
    }));
    let h = harness_with(AppOptions {
        instance: Some(r.primary.link.clone()),
        launch_token: Some("own".into()),
        make_activator: Some(make),
        ..Default::default()
    });
    wait_for("being made", || started.load(SeqCst));
    // Dropped as eframe drops it (before the window), on_exit not run: told
    // to stop and waited for — what it made dropped unused.
    drop(h);
    assert!(!gave_up.load(SeqCst), "told to stop");
    assert!(probe.dropped.load(SeqCst), "waited for");
    assert!(probe.tokens.lock().unwrap().is_empty(), "not used");
    assert!(!r.primary.link.has_activator());
    assert_eq!(ask(&r, &[]), Reply::Closing);
}
