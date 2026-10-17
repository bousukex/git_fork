//! Bringing the window to the front with a launcher's activation token, on
//! Wayland (`xdg_activation_v1`): winit only uses a token when it makes a
//! window, so this runs on winit's own connection to the compositor, with
//! a queue of its own, borrowing the window's surface.

use std::{
    ffi::c_void,
    os::fd::AsRawFd as _,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy as _, QueueHandle,
    backend::{Backend, ObjectId, WaylandError},
    protocol::{wl_callback, wl_registry, wl_surface::WlSurface},
};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{self, XdgActivationV1};

use crate::instance::Activate;

/// What the compositor said while the activator is made.
#[derive(Default)]
struct State {
    /// xdg_activation_v1's global (name, version).
    activation: Option<(u32, u32)>,
    /// The compositor answered everything asked before (all its globals
    /// told).
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, _: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == XdgActivationV1::interface().name
        {
            state.activation = Some((name, version));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.done = true;
        }
    }
}

impl Dispatch<XdgActivationV1, ()> for State {
    fn event(_: &mut Self, _: &XdgActivationV1, _: xdg_activation_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// A read or write that didn't happen just now — another thread (winit's)
/// read the events first; the socket full; a signal — and is tried again.
pub fn transient(e: &WaylandError) -> bool {
    matches!(e, WaylandError::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted))
}

/// Waits (at most `left`, and 50 ms) for the connection to be ready for
/// `events`.
fn wait_for(conn: &Connection, events: libc::c_short, left: Duration) {
    let fd = conn.backend().poll_fd().as_raw_fd();
    let mut fds = libc::pollfd { fd, events, revents: 0 };
    let ms = left.min(Duration::from_millis(50)).as_millis() as i32;
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fds, 1, ms.max(1)) };
}

/// Sends what is queued, waiting for room (by `deadline`, unless stopped).
fn flush_by(conn: &Connection, deadline: Instant, stop: &AtomicBool) -> Result<(), String> {
    loop {
        match conn.flush() {
            Ok(()) => return Ok(()),
            Err(e) if transient(&e) => {}
            Err(e) => return Err(e.to_string()),
        }
        if stop.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("the compositor didn't take what was sent in time".into());
        }
        wait_for(conn, libc::POLLOUT, left);
    }
}

/// Activates the window whose surface it borrowed.
pub struct WaylandActivator {
    conn: Connection,
    activation: XdgActivationV1,
    /// Borrowed: winit's, not managed here (nothing destroys it here).
    surface: WlSurface,
    /// The queue of what is made here (no events come after the start).
    _queue: EventQueue<State>,
}

impl WaylandActivator {
    /// On the connection of `display`, for `surface` (one of its): asks the
    /// compositor for its globals and waits for the answer — until
    /// `deadline`, or `stop` is set (looking every 50 ms).
    ///
    /// # Safety
    /// `display` must be a live `wl_display` and `surface` a live
    /// `wl_surface` of it, both for as long as this runs and the activator
    /// exists (the app stops this, and drops the activator, before its
    /// window goes).
    pub unsafe fn new(display: *mut c_void, surface: *mut c_void, deadline: Instant, stop: &AtomicBool) -> Result<Self, String> {
        // A guest on winit's connection (dropping it doesn't close it).
        // SAFETY: the caller's.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let _sync = conn.display().sync(&qh, ());
        let mut state = State::default();
        loop {
            queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
            if state.done {
                break;
            }
            if stop.load(Ordering::SeqCst) {
                return Err("stopped".into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the compositor didn't answer in time".into());
            }
            flush_by(&conn, deadline, stop)?;
            // (No guard: events came meanwhile, to be dispatched first.)
            let Some(guard) = queue.prepare_read() else { continue };
            let mut fds = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let ms = left.min(Duration::from_millis(50)).as_millis() as i32;
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut fds, 1, ms.max(1)) } > 0 {
                match guard.read() {
                    // (Nothing read, as another thread read it first: the
                    // loop looks again.)
                    Err(e) if !transient(&e) => return Err(e.to_string()),
                    _ => {}
                }
            }
            // (Else the guard, dropped, gives up its read.)
        }
        let (name, version) = state.activation.ok_or("the compositor has no xdg_activation_v1")?;
        let activation: XdgActivationV1 = registry.bind(name, version.min(1), &qh, ());
        // SAFETY: the caller's; borrowed (from_ptr doesn't take it over).
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }.map_err(|e| format!("the surface: {e}"))?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| format!("the surface: {e}"))?;
        flush_by(&conn, deadline, stop)?;
        Ok(WaylandActivator { conn, activation, surface, _queue: queue })
    }
}

impl Activate for WaylandActivator {
    fn activate(&self, token: &str) -> bool {
        self.activation.activate(token.to_owned(), &self.surface);
        // (Not sent within the time, the socket full: queued all the same,
        // it goes with winit's next flush.)
        match flush_by(&self.conn, Instant::now() + Duration::from_millis(500), &AtomicBool::new(false)) {
            Ok(()) => true,
            Err(e) if e.contains("in time") => true,
            Err(e) => {
                log::warn!("activating the window: {e}");
                false
            }
        }
    }
}

impl Drop for WaylandActivator {
    fn drop(&mut self) {
        self.activation.destroy();
        let _ = self.conn.flush();
    }
}
