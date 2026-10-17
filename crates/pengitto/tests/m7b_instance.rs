//! One pengitto at a time (M7b): the place and its checks, taking it or
//! handing over, the races (two at once, one starting or closing, one gone
//! without tidying up, one that doesn't answer), the limits of what is
//! asked, the activation without the UI; and the real executable handing
//! over (never reaching the real desktop: no display it could open).

use std::{
    ffi::OsStr,
    io::{Read as _, Write as _},
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pengitto::instance::{self, Activate, Claim, Place, Reply, Request, State};

const PENGITTO: &str = env!("CARGO_BIN_EXE_pengitto");

/// A private runtime folder of its own.
fn runtime() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn ns() -> String {
    std::fs::read_link("/proc/self/ns/mnt").unwrap().to_string_lossy().into_owned()
}

fn place_in(runtime: &Path, display: &str) -> Place {
    instance::place(Some(runtime.as_os_str()), &ns(), Some(display)).unwrap()
}

fn primary(place: &Place) -> instance::Primary {
    match instance::claim(place, &Request::default(), Duration::from_secs(5)) {
        Claim::Primary(p) => p,
        _ => panic!("not the one running"),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(20), "never: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn request(paths: &[&str]) -> Request {
    Request { cwd: "/".into(), paths: paths.iter().map(PathBuf::from).collect(), ..Default::default() }
}

// --- The place ----------------------------------------------------------------

#[test]
fn the_place_is_private_and_its_own() {
    let rt = runtime();
    let p = place_in(rt.path(), "wayland-0");
    let mode = std::fs::metadata(&p.dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // Again: the same; another display or namespace: another.
    assert_eq!(place_in(rt.path(), "wayland-0"), p);
    assert_ne!(place_in(rt.path(), "wayland-1"), p);
    assert_ne!(instance::place(Some(rt.path().as_os_str()), "mnt:[1]", Some("wayland-0")).unwrap(), p);
    // Not one at a time where it can't be safe or can't work.
    let none = |runtime: Option<&OsStr>, display: Option<&str>| instance::place(runtime, &ns(), display).unwrap_err();
    assert!(none(None, Some("w")).contains("no XDG_RUNTIME_DIR"));
    assert!(none(Some(OsStr::new("")), Some("w")).contains("no XDG_RUNTIME_DIR"));
    assert!(none(Some(OsStr::new("relative/run")), Some("w")).contains("isn't absolute"));
    assert!(none(Some(rt.path().as_os_str()), None).contains("no display"));
    let open = tempfile::tempdir().unwrap();
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(none(Some(open.path().as_os_str()), Some("w")).contains("isn't private"));
    let link = rt.path().join("link");
    std::os::unix::fs::symlink(rt.path(), &link).unwrap();
    assert!(none(Some(link.as_os_str()), Some("w")).contains("isn't a folder"));
    // The place itself: a link, or not private.
    let p = place_in(rt.path(), "wayland-9");
    std::fs::remove_dir(&p.dir).unwrap();
    std::os::unix::fs::symlink(rt.path(), &p.dir).unwrap();
    assert!(none(Some(rt.path().as_os_str()), Some("wayland-9")).contains("isn't a folder"));
    let p = place_in(rt.path(), "wayland-8");
    std::fs::set_permissions(&p.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(none(Some(rt.path().as_os_str()), Some("wayland-8")).contains("isn't private"));
    // Too long for a socket.
    let deep = rt.path().join("d".repeat(100));
    std::fs::create_dir(&deep).unwrap();
    std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(none(Some(deep.as_os_str()), Some("w")).contains("too long"));
}

// --- Taking it, handing over ----------------------------------------------------

#[test]
fn the_second_hands_over_once_the_first_is_ready() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let first = primary(&place);
    assert_eq!(first.link.state(), State::Starting);
    // While it starts, asked again and again.
    let p = place.clone();
    let second = std::thread::spawn(move || instance::claim(&p, &request(&["/a"]), Duration::from_secs(20)));
    wait_until("asked while starting", || first.link.answered() >= 2);
    assert!(first.link.take_requests().is_empty());
    first.link.ready(&egui::Context::default());
    assert!(matches!(second.join().unwrap(), Claim::Forwarded));
    assert_eq!(first.link.take_requests(), [request(&["/a"])]);
}

#[test]
fn of_several_at_once_one_runs() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..6 {
        let (p, tx) = (place.clone(), tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(instance::claim(&p, &request(&[&format!("/r{i}")]), Duration::from_secs(20)));
        });
    }
    let mut running = Vec::new();
    let mut forwarded = 0;
    for _ in 0..6 {
        match rx.recv().unwrap() {
            Claim::Primary(p) => {
                p.link.ready(&egui::Context::default());
                running.push(p);
            }
            Claim::Forwarded => forwarded += 1,
            _ => panic!("neither running nor handed over"),
        }
    }
    assert_eq!(running.len(), 1);
    assert_eq!(forwarded, 5);
    assert_eq!(running[0].link.take_requests().len(), 5);
}

#[test]
fn one_closing_is_taken_over_once_gone() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let first = primary(&place);
    first.link.ready(&egui::Context::default());
    assert!(first.link.begin_closing().is_empty());
    assert_eq!(first.link.state(), State::Closing);
    assert_eq!(instance::send(&place.socket(), &request(&[])).unwrap(), Reply::Closing);
    let p = place.clone();
    let second = std::thread::spawn(move || instance::claim(&p, &request(&["/b"]), Duration::from_secs(20)));
    wait_until("turned away while closing", || first.link.answered() >= 3);
    drop(first);
    let Claim::Primary(second) = second.join().unwrap() else { panic!("didn't take over") };
    second.link.ready(&egui::Context::default());
    assert!(second.link.take_requests().is_empty(), "its own paths it opens itself");
}

#[test]
fn what_was_asked_before_closing_keeps_it_open() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let first = primary(&place);
    first.link.ready(&egui::Context::default());
    assert_eq!(instance::send(&place.socket(), &request(&["/late"])).unwrap(), Reply::Accepted);
    // Closing now hands it back, and stays.
    assert_eq!(first.link.begin_closing(), [request(&["/late"])]);
    assert_eq!(first.link.state(), State::Ready);
    assert!(first.link.begin_closing().is_empty());
    assert_eq!(first.link.state(), State::Closing);
}

#[test]
fn one_gone_without_tidying_up_is_replaced() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    // Its socket left behind (as by a crash; its lock went with it).
    drop(std::os::unix::net::UnixListener::bind(place.socket()).unwrap());
    assert!(place.socket().exists());
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    assert_eq!(instance::send(&place.socket(), &request(&[])).unwrap(), Reply::Accepted);
    drop(p);
    assert!(!place.socket().exists(), "its own removed when it ends");
    // Something else where the socket goes: left alone, not one at a time.
    std::fs::write(place.socket(), "not a socket").unwrap();
    assert!(matches!(instance::claim(&place, &request(&[]), Duration::from_secs(5)), Claim::Off(why) if why.contains("isn't a socket")));
    assert_eq!(std::fs::read_to_string(place.socket()).unwrap(), "not a socket");
}

#[test]
fn a_socket_that_isnt_its_own_is_left() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let p = primary(&place);
    // Replaced meanwhile (another socket at the name): not removed.
    std::fs::remove_file(place.socket()).unwrap();
    let other = std::os::unix::net::UnixListener::bind(place.socket()).unwrap();
    drop(p);
    assert!(place.socket().exists());
    drop(other);
}

/// The lock held, the socket served by something that reads the request and
/// closes without an answer (an answer lost, or one that hangs).
fn mute_running_one(place: &Place) -> (std::fs::File, Arc<Mutex<usize>>) {
    use std::os::fd::AsRawFd as _;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.lock()).unwrap();
    // SAFETY: flock on an open file.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let listener = std::os::unix::net::UnixListener::bind(place.socket()).unwrap();
    let seen = Arc::new(Mutex::new(0));
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
            *s.lock().unwrap() += 1;
        }
    });
    (lock, seen)
}

#[test]
fn one_that_doesnt_answer_is_given_up_on() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let (_lock, seen) = mute_running_one(&place);
    let t = Instant::now();
    let Claim::Alone(notice) = instance::claim(&place, &request(&["/x"]), Duration::from_millis(600)) else { panic!("didn't give up") };
    assert!(notice.contains("didn't answer"), "{notice}");
    assert!(t.elapsed() < Duration::from_secs(5));
    // It got the request (it may do it too: allowed), and its socket stays.
    assert!(*seen.lock().unwrap() >= 1);
    assert!(place.socket().exists());
}

// --- Limits ----------------------------------------------------------------------

fn raw(place: &Place, bytes: &[u8]) -> Vec<u8> {
    let mut s = UnixStream::connect(place.socket()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    out
}

fn head(version: u8, len: u32) -> Vec<u8> {
    let mut h = b"PGTO".to_vec();
    h.push(version);
    h.extend_from_slice(&len.to_be_bytes());
    h
}

#[test]
fn what_is_asked_is_held_to_limits() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    // Not a request; another version; a size beyond reason (told without
    // waiting for it).
    assert_eq!(raw(&place, b"HELLO WORLD")[0], 5);
    assert_eq!(raw(&place, &head(2, 0))[0], 4);
    let t = Instant::now();
    assert_eq!(raw(&place, &head(1, u32::MAX))[0], 5);
    assert!(t.elapsed() < Duration::from_secs(1));
    // A token with a NUL; an unknown field.
    let mut bad = head(1, 6);
    bad.extend_from_slice(&[3, 0, 0, 0, 1, 0]);
    assert_eq!(raw(&place, &bad)[0], 5);
    let mut bad = head(1, 5);
    bad.extend_from_slice(&[9, 0, 0, 0, 0]);
    assert_eq!(raw(&place, &bad)[0], 5);
    // Someone that says how long and stops: cut off within the second.
    let t = Instant::now();
    assert!(raw(&place, &head(1, 100)).is_empty());
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // Nothing of it queued.
    assert!(p.link.take_requests().is_empty());
    // Queued up to 64, then busy.
    for i in 0..64 {
        assert_eq!(instance::send(&place.socket(), &request(&[&format!("/q{i}")])).unwrap(), Reply::Accepted);
    }
    assert_eq!(instance::send(&place.socket(), &request(&["/q64"])).unwrap(), Reply::Busy);
    assert_eq!(p.link.take_requests().len(), 64);
    // What this one can't ask: refused here, before asking.
    let bad = Request { token: Some("two words".into()), ..Default::default() };
    assert!(matches!(instance::claim(&place, &bad, Duration::from_secs(1)), Claim::Refused(_)));
}

#[test]
fn past_four_at_once_one_is_closed_right_away() {
    // (Connections that don't run out meanwhile: what is seen is the limit.)
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let Claim::Primary(p) = instance::claim_with(&place, &Request::default(), Duration::from_secs(5), Duration::from_secs(60)) else {
        panic!()
    };
    p.link.ready(&egui::Context::default());
    let stalled: Vec<UnixStream> = (0..4).map(|_| UnixStream::connect(place.socket()).unwrap()).collect();
    wait_until("four being read", || p.link.connections() == 4);
    let mut fifth = UnixStream::connect(place.socket()).unwrap();
    fifth.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 1];
    assert!(matches!(fifth.read(&mut buf), Ok(0)), "the fifth is closed (not read until it runs out)");
    assert_eq!(p.link.connections(), 4);
    drop(stalled);
    wait_until("the four let go", || p.link.connections() == 0);
}

/// The lock held, and a socket whose queue of connections is full (nothing
/// accepted).
fn full_running_one(place: &Place) -> (std::fs::File, UnixStream, i32) {
    use std::os::{fd::AsRawFd as _, unix::ffi::OsStrExt as _};
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.lock()).unwrap();
    // SAFETY: flock on an open file.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    // SAFETY: plain socket calls on a new socket and a valid address.
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (d, s) in addr.sun_path.iter_mut().zip(place.socket().as_os_str().as_bytes()) {
            *d = *s as libc::c_char;
        }
        assert_eq!(libc::bind(fd, (&raw const addr).cast(), std::mem::size_of::<libc::sockaddr_un>() as u32), 0);
        assert_eq!(libc::listen(fd, 0), 0);
        fd
    };
    // One waiting fills a queue of none.
    let waiting = UnixStream::connect(place.socket()).unwrap();
    (lock, waiting, fd)
}

#[test]
fn the_deadline_holds_whatever_the_other_end_does() {
    let rt = runtime();
    // A queue of connections full: not waited on past the deadline.
    let place = place_in(rt.path(), "full");
    let (_lock, _waiting, fd) = full_running_one(&place);
    let (tx, rx) = std::sync::mpsc::channel();
    let p = place.clone();
    std::thread::spawn(move || {
        let _ = tx.send(matches!(instance::claim(&p, &request(&["/x"]), Duration::from_millis(500)), Claim::Alone(_)));
    });
    assert!(rx.recv_timeout(Duration::from_secs(10)).expect("waited past its deadline"), "not on its own");
    // SAFETY: the socket made above.
    unsafe { libc::close(fd) };
    // One that reads slowly (a little now and then): sending is cut off.
    let place = place_in(rt.path(), "slow");
    use std::os::fd::AsRawFd as _;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.lock()).unwrap();
    // SAFETY: flock on an open file.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let listener = std::os::unix::net::UnixListener::bind(place.socket()).unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                while s.read(&mut buf).is_ok_and(|n| n > 0) {
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
        }
    });
    // (Close to a mebibyte: far more than the socket takes at once. Each
    // try may take a minute: only the deadline cuts it off.)
    let paths: Vec<String> = (0..240).map(|i| format!("/{i:04}{}", "p".repeat(3990))).collect();
    let big = request(&paths.iter().map(String::as_str).collect::<Vec<_>>());
    let (tx, rx) = std::sync::mpsc::channel();
    let p = place.clone();
    std::thread::spawn(move || {
        let claim = instance::claim_with(&p, &big, Duration::from_secs(2), Duration::from_secs(60));
        let _ = tx.send(matches!(claim, Claim::Alone(_)));
    });
    assert!(rx.recv_timeout(Duration::from_secs(10)).expect("sent past its deadline"), "not on its own");
}

#[test]
fn another_version_runs_on_its_own() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    // The lock held by one answering in another version.
    use std::os::fd::AsRawFd as _;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.lock()).unwrap();
    // SAFETY: flock on an open file.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let listener = std::os::unix::net::UnixListener::bind(place.socket()).unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 9];
            let _ = s.read_exact(&mut buf);
            let _ = s.write_all(&[4, 0, 3, b'v', b'9', b'!']);
        }
    });
    let Claim::Alone(notice) = instance::claim(&place, &request(&[]), Duration::from_secs(5)) else { panic!() };
    assert!(notice.contains("another version") && notice.contains("v9!"), "{notice}");
}

// --- Activation ---------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl Activate for Recorder {
    fn activate(&self, token: &str) -> bool {
        self.0.lock().unwrap().push(token.to_owned());
        true
    }
}

/// An activator whose requests don't go out.
struct Failing;

impl Activate for Failing {
    fn activate(&self, _: &str) -> bool {
        false
    }
}

/// An activator that notes when it is dropped (destroyed on the
/// compositor's connection).
struct Watched(Arc<std::sync::atomic::AtomicBool>);

impl Activate for Watched {
    fn activate(&self, _: &str) -> bool {
        true
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn the_activator_goes_with_the_window_not_after() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    // Closed by the app (on_exit, the window still there): dropped then.
    let p = primary(&place);
    let dropped = Arc::new(AtomicBool::new(false));
    assert!(p.link.publish_activator(Box::new(Watched(dropped.clone()))).is_ok());
    p.link.close();
    assert!(dropped.load(SeqCst) && !p.link.has_activator());
    // One made after that: not taken in (given back, to be dropped while
    // the window is there).
    let late = Arc::new(AtomicBool::new(false));
    let given_back = p.link.publish_activator(Box::new(Watched(late.clone())));
    assert!(given_back.is_err() && !p.link.has_activator());
    drop(given_back);
    assert!(late.load(SeqCst));
    drop(p);
    // Not closed by the app (it ended otherwise): when it goes, the
    // activator is let go of, not dropped (the connection may be gone).
    let p = primary(&place);
    let dropped = Arc::new(AtomicBool::new(false));
    assert!(p.link.publish_activator(Box::new(Watched(dropped.clone()))).is_ok());
    let link = p.link.clone();
    drop(p);
    assert!(!dropped.load(SeqCst) && !link.has_activator());
}

#[test]
fn the_window_is_activated_without_its_ui_running() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    let rec = Recorder::default();
    assert!(p.link.publish_activator(Box::new(rec.clone())).is_ok());
    // Taken: activated right then (nothing of the UI ran: nothing taken).
    let with = Request { token: Some("launcher-token-1".into()), ..request(&["/r"]) };
    assert_eq!(instance::send(&place.socket(), &with).unwrap(), Reply::Accepted);
    assert_eq!(*rec.0.lock().unwrap(), ["launcher-token-1"]);
    // No token: nothing to activate with (the UI asks for attention).
    assert_eq!(instance::send(&place.socket(), &request(&["/s"])).unwrap(), Reply::Accepted);
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    let taken = p.link.take_requests();
    assert_eq!(taken.iter().map(|r| r.activated).collect::<Vec<_>>(), [true, false], "the UI asks for attention for the second only");
    // The window gone: never again.
    p.link.close();
    assert_eq!(instance::send(&place.socket(), &with).unwrap(), Reply::Closing);
    assert_eq!(rec.0.lock().unwrap().len(), 1);
}

#[test]
fn a_token_taken_before_the_activator_is_used_once_it_is_made() {
    let rt = runtime();
    let place = place_in(rt.path(), "w");
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    // Ready, the activator not made yet: kept (the UI asks for attention
    // meanwhile).
    let early = Request { token: Some("early".into()), ..request(&["/e"]) };
    assert_eq!(instance::send(&place.socket(), &early).unwrap(), Reply::Accepted);
    assert!(!p.link.take_requests()[0].activated);
    let rec = Recorder::default();
    assert!(p.link.publish_activator(Box::new(rec.clone())).is_ok());
    assert_eq!(*rec.0.lock().unwrap(), ["early"]);
    // Used once.
    let later = Request { token: Some("later".into()), ..request(&["/l"]) };
    assert_eq!(instance::send(&place.socket(), &later).unwrap(), Reply::Accepted);
    assert_eq!(*rec.0.lock().unwrap(), ["early", "later"]);
    assert!(p.link.take_requests()[0].activated);
    drop(p);
    // A request that didn't go out: not activated (the UI asks for
    // attention).
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    assert!(p.link.publish_activator(Box::new(Failing)).is_ok());
    assert_eq!(instance::send(&place.socket(), &later).unwrap(), Reply::Accepted);
    assert!(!p.link.take_requests()[0].activated);
}

// --- The real executable --------------------------------------------------------------

/// The executable, with nothing of the real desktop: a runtime folder of its
/// own, a Wayland display that isn't there (and no X one).
fn pengitto(rt: &Path, display: &str, args: &[&str]) -> Command {
    let mut c = Command::new(PENGITTO);
    c.args(args)
        .env("XDG_RUNTIME_DIR", rt)
        .env("WAYLAND_DISPLAY", display)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID");
    c
}

#[test]
fn the_executable_hands_over_to_the_one_running() {
    let rt = runtime();
    let display = format!("pengitto-test-{}", std::process::id());
    let place = place_in(rt.path(), &display);
    let p = primary(&place);
    p.link.ready(&egui::Context::default());
    let repo = tempfile::tempdir().unwrap();
    let file = repo.path().join("file.txt");
    std::fs::write(&file, "").unwrap();
    let run = |args: &[&str], dir: &Path, token: Option<&str>| {
        let mut c = pengitto(rt.path(), &display, args);
        c.current_dir(dir);
        if let Some(t) = token {
            c.env("XDG_ACTIVATION_TOKEN", t);
        }
        c.output().unwrap()
    };
    // A folder, a file (its folder), a relative one, none; the token.
    for (args, want) in [
        (vec![repo.path().to_str().unwrap()], vec![repo.path().to_path_buf()]),
        (vec![file.to_str().unwrap()], vec![repo.path().to_path_buf()]),
        (vec!["."], vec![repo.path().join(".")]),
        (vec![], vec![]),
    ] {
        let o = run(&args, repo.path(), Some("tok-42"));
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        let got = p.link.take_requests();
        assert_eq!(got.len(), 1, "{args:?}");
        assert_eq!(got[0].paths, want, "{args:?}");
        assert_eq!(got[0].token.as_deref(), Some("tok-42"));
    }
    // What runs on its own hands nothing over: a pengitto of its own (here
    // its window can't open), the version, the self-check — which also finds
    // the launcher's token kept from what pengitto starts.
    let o = run(&["--new-instance", repo.path().to_str().unwrap()], repo.path(), None);
    assert!(!o.status.success(), "it ran on its own (and had no display)");
    let _ = run(&["--version"], repo.path(), None);
    let o = run(&["--self-check"], repo.path(), Some("tok-43"));
    let all = String::from_utf8_lossy(&o.stdout);
    assert!(all.contains("environment: clean"), "{all}");
    // (Asked for credentials by git, a prompt like an option.)
    let o = pengitto(rt.path(), &display, &["--new-instance"])
        .env("PENGITTO_ASKPASS_SOCKET", rt.path().join("none.sock"))
        .env("PENGITTO_ASKPASS_TOKEN", "x")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(p.link.take_requests().is_empty());
}

#[test]
fn the_executable_alone_fails_without_a_display() {
    // No one running: it is the one, and its window can't open (no
    // display): an error, not success, and nothing on the real desktop.
    let rt = runtime();
    let display = format!("pengitto-test-none-{}", std::process::id());
    let o = pengitto(rt.path(), &display, &[]).output().unwrap();
    assert!(!o.status.success());
    let place = place_in(rt.path(), &display);
    assert!(!place.socket().exists(), "its socket removed as it ends");
}
