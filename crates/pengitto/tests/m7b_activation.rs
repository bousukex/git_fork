//! Making the window's activator (M7b): asking the compositor for its
//! globals is given up on at the deadline, or as soon as it is told to stop
//! (the app waits for it before the window goes) — with a compositor that
//! never answers, played here on the other end of a socket.

use std::{
    io::{Read as _, Write as _},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use pengitto::activation::WaylandActivator;
use wayland_backend::client::Backend;

/// A connection to the compositor at the other end of `server`.
fn connection() -> (Backend, UnixStream) {
    let (client, server) = UnixStream::pair().unwrap();
    (Backend::connect(client).expect("libwayland-client"), server)
}

/// Makes the activator on `backend`'s connection (no surface: never reached,
/// the compositor never telling its globals).
fn make(backend: &Backend, deadline: Duration, stop: &AtomicBool) -> Result<(), String> {
    // SAFETY: the backend's live display; the surface is only used once the
    // globals are known, which never happens here.
    unsafe { WaylandActivator::new(backend.display_ptr().cast(), std::ptr::null_mut(), Instant::now() + deadline, stop) }.map(drop)
}

#[test]
fn a_compositor_that_never_answers_is_given_up_on_at_the_deadline() {
    let (backend, _server) = connection();
    let t = Instant::now();
    let e = make(&backend, Duration::from_millis(300), &AtomicBool::new(false)).unwrap_err();
    assert!(e.contains("in time"), "{e}");
    assert!(t.elapsed() >= Duration::from_millis(300) && t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn being_told_to_stop_ends_it_before_its_deadline() {
    let (backend, _server) = connection();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        s.store(true, SeqCst);
    });
    let t = Instant::now();
    let e = make(&backend, Duration::from_secs(60), &stop).unwrap_err();
    assert!(e.contains("stopped"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn a_compositor_without_xdg_activation_says_so() {
    let (backend, mut server) = connection();
    // Answers the roundtrip (wl_display.sync: its callback done), telling
    // no globals.
    std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 {
                return;
            }
            got.extend_from_slice(&buf[..n]);
            while got.len() >= 8 {
                let word = |i: usize| u32::from_ne_bytes(got[i..i + 4].try_into().unwrap());
                let (object, size, opcode) = (word(0), (word(4) >> 16) as usize, word(4) & 0xffff);
                if got.len() < size {
                    break;
                }
                if (object, opcode) == (1, 0) {
                    let callback = word(8);
                    let mut events = Vec::new();
                    for w in [callback, 12 << 16, 0, 1, (12 << 16) | 1, callback] {
                        events.extend_from_slice(&u32::to_ne_bytes(w));
                    }
                    let _ = server.write_all(&events);
                }
                got.drain(..size);
            }
        }
    });
    let e = make(&backend, Duration::from_secs(10), &AtomicBool::new(false)).unwrap_err();
    assert!(e.contains("no xdg_activation_v1"), "{e}");
}

// --- A compositor that has xdg_activation_v1 ---------------------------------

/// What the compositor heard.
#[derive(Default)]
struct Heard {
    /// xdg_activation_v1.activate: (token, surface).
    activations: Vec<(String, u32)>,
    destroyed: bool,
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

/// A string argument at `at`: its text and where the next argument is.
fn string(b: &[u8], at: usize) -> (String, usize) {
    let len = word(b, at) as usize;
    let text = String::from_utf8_lossy(&b[at + 4..at + 4 + len.saturating_sub(1)]).into_owned();
    (text, at + 4 + len.div_ceil(4) * 4)
}

fn message(object: u32, opcode: u32, args: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&object.to_ne_bytes());
    m.extend_from_slice(&((((8 + args.len()) as u32) << 16) | opcode).to_ne_bytes());
    m.extend_from_slice(args);
    m
}

fn string_arg(s: &str) -> Vec<u8> {
    let mut a = ((s.len() + 1) as u32).to_ne_bytes().to_vec();
    a.extend_from_slice(s.as_bytes());
    a.push(0);
    while !a.len().is_multiple_of(4) {
        a.push(0);
    }
    a
}

/// Plays a compositor with wl_compositor and xdg_activation_v1 on `server`:
/// tells its globals to each registry, answers each roundtrip, and notes
/// what is asked of xdg_activation_v1.
fn compositor(server: UnixStream, heard: Arc<std::sync::Mutex<Heard>>) {
    compositor_after(server, heard, Duration::ZERO, true);
}

/// The same, reading nothing for `delay` first; without xdg_activation_v1
/// unless `activation`.
fn compositor_after(mut server: UnixStream, heard: Arc<std::sync::Mutex<Heard>>, delay: Duration, activation_global: bool) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
        let mut activation = None;
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 {
                return;
            }
            got.extend_from_slice(&buf[..n]);
            while got.len() >= 8 {
                let (object, size, opcode) = (word(&got, 0), (word(&got, 4) >> 16) as usize, word(&got, 4) & 0xffff);
                if got.len() < size {
                    break;
                }
                let m: Vec<u8> = got.drain(..size).collect();
                let mut reply = Vec::new();
                match (object, opcode) {
                    // wl_display.sync: done, and the callback's id freed.
                    (1, 0) => {
                        reply.extend(message(word(&m, 8), 0, &0u32.to_ne_bytes()));
                        reply.extend(message(1, 1, &word(&m, 8).to_ne_bytes()));
                    }
                    // wl_display.get_registry: the globals.
                    (1, 1) => {
                        let globals: &[(u32, &str, u32)] =
                            if activation_global { &[(1, "wl_compositor", 4), (2, "xdg_activation_v1", 1)] } else { &[(1, "wl_compositor", 4)] };
                        for &(name, interface, version) in globals {
                            let mut args = name.to_ne_bytes().to_vec();
                            args.extend(string_arg(interface));
                            args.extend(version.to_ne_bytes());
                            reply.extend(message(word(&m, 8), 0, &args));
                        }
                    }
                    // xdg_activation_v1: destroy (0), activate (2).
                    (o, 0) if Some(o) == activation => heard.lock().unwrap().destroyed = true,
                    (o, 2) if Some(o) == activation => {
                        let (token, next) = string(&m, 8);
                        heard.lock().unwrap().activations.push((token, word(&m, next)));
                    }
                    // wl_registry.bind (name, interface, version, id):
                    // xdg_activation_v1's id noted.
                    (_, 0) if m.len() > 16 => {
                        let (interface, next) = string(&m, 12);
                        if interface == "xdg_activation_v1" {
                            activation = Some(word(&m, next + 4));
                        }
                    }
                    _ => {}
                }
                if !reply.is_empty() && server.write_all(&reply).is_err() {
                    return;
                }
            }
        }
    });
}

struct Client;

impl wayland_client::Dispatch<wayland_client::protocol::wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(Client: ignore wayland_client::protocol::wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wayland_client::protocol::wl_surface::WlSurface);

#[test]
fn the_window_is_activated_with_the_token_others_read_events_or_not() {
    use wayland_client::Proxy as _;
    let (backend, server) = connection();
    let heard = Arc::new(std::sync::Mutex::new(Heard::default()));
    compositor(server, heard.clone());
    // The app's own side of the connection (as winit has it): a surface.
    let conn = wayland_client::Connection::from_backend(backend.clone());
    let (globals, mut queue) = wayland_client::globals::registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wayland_client::protocol::wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).unwrap();
    let surface = compositor.create_surface(&qh, ());
    queue.roundtrip(&mut Client).unwrap();
    // Meanwhile another thread reads the connection's events now and
    // then (as winit's loop does).
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (conn, stop) = (conn.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut q = conn.new_event_queue::<Client>();
            while !stop.load(SeqCst) {
                let _ = q.dispatch_pending(&mut Client);
                if let Some(guard) = q.prepare_read() {
                    let _ = guard.read();
                }
            }
        })
    };
    for i in 0..5 {
        // SAFETY: the backend's display and a surface of it, both alive
        // while the activator is.
        let a = unsafe {
            WaylandActivator::new(backend.display_ptr().cast(), surface.id().as_ptr().cast(), Instant::now() + Duration::from_secs(10), &AtomicBool::new(false))
        }
        .unwrap_or_else(|e| panic!("made ({i}): {e}"));
        assert!(pengitto::instance::Activate::activate(&a, &format!("token-{i}")));
        drop(a);
    }
    stop.store(true, SeqCst);
    reader.join().unwrap();
    let t = Instant::now();
    while heard.lock().unwrap().activations.len() < 5 {
        assert!(t.elapsed() < Duration::from_secs(10), "not heard");
        std::thread::sleep(Duration::from_millis(10));
    }
    let heard = heard.lock().unwrap();
    let id = surface.id().protocol_id();
    assert_eq!(heard.activations, (0..5).map(|i| (format!("token-{i}"), id)).collect::<Vec<_>>());
    assert!(heard.destroyed, "destroyed when dropped");
}

#[test]
fn what_doesnt_happen_just_now_is_tried_again() {
    use pengitto::activation::transient;
    use wayland_client::backend::WaylandError;
    for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::Interrupted] {
        assert!(transient(&WaylandError::Io(kind.into())), "{kind:?}");
    }
    assert!(!transient(&WaylandError::Io(std::io::ErrorKind::BrokenPipe.into())));
    assert!(!transient(&WaylandError::Io(std::io::ErrorKind::ConnectionReset.into())));
}

#[test]
fn a_connection_that_cant_take_more_just_now_is_waited_for() {
    use std::os::fd::AsRawFd as _;
    let (client, server) = UnixStream::pair().unwrap();
    // As little room as the system allows, filled up below (the compositor
    // not reading for a while).
    let small: libc::c_int = 1;
    // SAFETY: a valid socket and option.
    unsafe {
        libc::setsockopt(client.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&raw const small).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t);
    }
    let backend = Backend::connect(client).unwrap();
    let heard = Arc::new(std::sync::Mutex::new(Heard::default()));
    compositor_after(server, heard, Duration::from_millis(500), false);
    let conn = wayland_client::Connection::from_backend(backend.clone());
    let queue = conn.new_event_queue::<Client>();
    let qh = queue.handle();
    let mut full = false;
    for _ in 0..200 {
        for _ in 0..20 {
            conn.display().sync(&qh, ());
        }
        match conn.flush() {
            Ok(()) => {}
            Err(e) if pengitto::activation::transient(&e) => {
                full = true;
                break;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(full, "the connection never filled up");
    // Asked all the same, once the compositor reads again (its answer: no
    // xdg_activation_v1 — never the full connection).
    let t = Instant::now();
    let e = make(&backend, Duration::from_secs(10), &AtomicBool::new(false)).unwrap_err();
    assert!(e.contains("no xdg_activation_v1"), "{e}");
    assert!(t.elapsed() >= Duration::from_millis(200), "it had to wait: {:?}", t.elapsed());
    drop(queue);
}

impl wayland_client::Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}
