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
