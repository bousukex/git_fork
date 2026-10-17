//! One pengitto at a time (for a user, on a desktop, in a mount namespace):
//! a second one hands its paths (and the launcher's activation token) to the
//! one running, which opens them and comes to the front.
//!
//! The first to take `instance.lock` (in `$XDG_RUNTIME_DIR/pengitto-<id>/`,
//! held while it runs) serves `instance.sock`. A request is taken as soon as
//! it is queued while the running one is ready (not once its UI has handled
//! it: a minimized window may not draw, and so not run its UI, until it is
//! activated); its window is activated from the thread that took it. One
//! about to close turns requests away, and the second one tries again until
//! it can take the lock itself.

use std::{
    collections::VecDeque,
    ffi::OsStr,
    io::{Read as _, Write as _},
    os::unix::{
        ffi::OsStrExt as _,
        fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long a second pengitto tries to reach the first (then it runs on its
/// own).
pub const DEADLINE: Duration = Duration::from_secs(5);
/// A whole connection (request and reply), at most.
const CONNECTION: Duration = Duration::from_secs(1);
const MAX_CONNECTIONS: usize = 4;
const MAX_QUEUE: usize = 64;
const MAX_BODY: usize = 1 << 20;
const MAX_PATHS: usize = 256;
const MAX_PATH: usize = 4096;
const MAX_TOKEN: usize = 256;
const MAX_REASON: usize = 256;
/// `sun_path`, with its NUL.
const SOCKET_PATH: usize = 108;

const MAGIC: &[u8; 4] = b"PGTO";
pub const VERSION: u8 = 1;

/// What a second pengitto asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub cwd: PathBuf,
    /// Absolute.
    pub paths: Vec<PathBuf>,
    /// The launcher's activation token (Wayland).
    pub token: Option<String>,
    /// The launcher's startup id (X11).
    pub startup_id: Option<String>,
    /// (Not sent.) Its token brought the window to the front when it was
    /// taken: nothing else to do for that.
    pub activated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Accepted,
    /// Still starting: ask again.
    Starting,
    /// About to close: ask again (and take over once it has).
    Closing,
    /// Too much asked at once: ask again.
    Busy,
    BadVersion(String),
    BadRequest(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Closing,
}

/// Brings the window to the front with a launcher's token; whether the
/// request went out.
pub trait Activate: Send {
    fn activate(&self, token: &str) -> bool;
}

/// What activates the window, once it is made.
#[derive(Default)]
struct Activation {
    activator: Option<Box<dyn Activate>>,
    /// The latest token taken before there was an activator (used once
    /// there is).
    pending: Option<String>,
    /// The window is going: nothing more is activated or taken in.
    closed: bool,
}

struct Queue {
    state: State,
    requests: VecDeque<Request>,
}

// (Locks taken in this order: the queue, then the activation.)
struct Shared {
    queue: Mutex<Queue>,
    activation: Mutex<Activation>,
    ctx: Mutex<Option<egui::Context>>,
    /// Requests answered so far (whatever the answer).
    answered: AtomicUsize,
    /// Connections being read now.
    open: AtomicUsize,
}

/// The running pengitto's side, for its app: what was asked of it.
#[derive(Clone)]
pub struct Link(Arc<Shared>);

impl Default for Link {
    fn default() -> Self {
        Link(Arc::new(Shared {
            queue: Mutex::new(Queue { state: State::Starting, requests: VecDeque::new() }),
            activation: Mutex::new(Activation::default()),
            ctx: Mutex::new(None),
            answered: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
        }))
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Link({:?})", self.state())
    }
}

impl Link {
    pub fn state(&self) -> State {
        locked(&self.0.queue).state
    }

    /// The app runs (its first frame): requests are taken from now on.
    pub fn ready(&self, ctx: &egui::Context) {
        *locked(&self.0.ctx) = Some(ctx.clone());
        let mut q = locked(&self.0.queue);
        if q.state == State::Starting {
            q.state = State::Ready;
        }
    }

    /// Requests answered so far (whatever the answer).
    pub fn answered(&self) -> usize {
        self.0.answered.load(Ordering::SeqCst)
    }

    /// Connections being read now.
    pub fn connections(&self) -> usize {
        self.0.open.load(Ordering::SeqCst)
    }

    pub fn take_requests(&self) -> Vec<Request> {
        locked(&self.0.queue).requests.drain(..).collect()
    }

    /// The app is about to close: from now on requests are turned away —
    /// unless some came already: then it stays (they are handed back to be
    /// handled), so nothing taken is left undone.
    pub fn begin_closing(&self) -> Vec<Request> {
        let mut q = locked(&self.0.queue);
        if q.requests.is_empty() {
            q.state = State::Closing;
        }
        q.requests.drain(..).collect()
    }

    /// Takes in the window's activator, made: a token taken before it is
    /// used now. Given back when the window is going already (to be dropped
    /// by the caller, while the window is still there).
    pub fn publish_activator(&self, activator: Box<dyn Activate>) -> Result<(), Box<dyn Activate>> {
        let mut a = locked(&self.0.activation);
        if a.closed {
            return Err(activator);
        }
        if let Some(token) = a.pending.take() {
            activator.activate(&token);
        }
        a.activator = Some(activator);
        Ok(())
    }

    pub fn has_activator(&self) -> bool {
        locked(&self.0.activation).activator.is_some()
    }

    /// The window is going: closing, nothing activated from now on (it
    /// waits for an activation under way), the activator dropped (the
    /// caller makes sure the window is still there).
    pub fn close(&self) {
        let mut q = locked(&self.0.queue);
        q.state = State::Closing;
        let mut a = locked(&self.0.activation);
        a.closed = true;
        a.pending = None;
        drop(a.activator.take());
    }

    /// Closing, once the window may be gone: an activator still there (the
    /// app didn't close it) is let go of, not dropped (that would talk to a
    /// connection that may be gone).
    fn close_late(&self) {
        let mut q = locked(&self.0.queue);
        q.state = State::Closing;
        let mut a = locked(&self.0.activation);
        a.closed = true;
        if let Some(activator) = a.activator.take() {
            std::mem::forget(activator);
        }
    }

    /// What the thread taking requests does with one: queued (the window
    /// activated with its token right then — its UI may not run until it
    /// is — or the token kept for when it can be).
    fn take(&self, mut request: Request) -> Reply {
        {
            let mut q = locked(&self.0.queue);
            match q.state {
                State::Starting => return Reply::Starting,
                State::Closing => return Reply::Closing,
                State::Ready if q.requests.len() >= MAX_QUEUE => return Reply::Busy,
                State::Ready => {}
            }
            if let Some(token) = &request.token {
                let mut a = locked(&self.0.activation);
                match &a.activator {
                    Some(activator) => request.activated = activator.activate(token),
                    None => a.pending = Some(token.clone()),
                }
            }
            q.requests.push_back(request);
        }
        if let Some(ctx) = locked(&self.0.ctx).as_ref() {
            ctx.request_repaint();
        }
        Reply::Accepted
    }
}

// --- Where -----------------------------------------------------------------

/// The folder of the instance (its lock and socket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub dir: PathBuf,
}

impl Place {
    pub fn lock(&self) -> PathBuf {
        self.dir.join("instance.lock")
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("instance.sock")
    }
}

/// The place for this session, from the environment: `$XDG_RUNTIME_DIR`,
/// the mount namespace and the display; why not, if there is none.
pub fn place_from_env() -> Result<Place, String> {
    let ns = std::fs::read_link("/proc/self/ns/mnt").map_err(|e| format!("the mount namespace can't be read: {e}"))?;
    let display = std::env::var("WAYLAND_DISPLAY").ok().filter(|d| !d.is_empty()).or_else(|| std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()));
    place(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &ns.to_string_lossy(), display.as_deref())
}

/// The place under `runtime` for the mount namespace `ns` and `display`
/// (made if needed): only in a runtime folder that is the user's own,
/// private, and no link; the same for the place itself.
pub fn place(runtime: Option<&OsStr>, ns: &str, display: Option<&str>) -> Result<Place, String> {
    let runtime = runtime.filter(|r| !r.is_empty()).map(Path::new).ok_or("no XDG_RUNTIME_DIR")?;
    if !runtime.is_absolute() {
        return Err(format!("XDG_RUNTIME_DIR isn't absolute: {}", runtime.display()));
    }
    private_folder(runtime)?;
    let display = display.ok_or("no display (WAYLAND_DISPLAY, DISPLAY)")?;
    // (FNV-1a: short, stable.)
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in ns.bytes().chain([0]).chain(display.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let dir = runtime.join(format!("pengitto-{h:016x}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{} can't be made: {e}", dir.display())),
    }
    private_folder(&dir)?;
    let place = Place { dir };
    if place.socket().as_os_str().len() >= SOCKET_PATH {
        return Err(format!("{} is too long for a socket", place.socket().display()));
    }
    Ok(place)
}

fn private_folder(dir: &Path) -> Result<(), String> {
    let m = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !m.file_type().is_dir() {
        Err(format!("{} isn't a folder", dir.display()))
    } else if m.uid() != me {
        Err(format!("{} isn't this user's", dir.display()))
    } else if m.permissions().mode() & 0o777 != 0o700 {
        Err(format!("{} isn't private (0700)", dir.display()))
    } else {
        Ok(())
    }
}

// --- Taking the place, or handing over ---------------------------------------

/// What came of trying to be the one running.
pub enum Claim {
    /// This one runs, serving the others.
    Primary(Primary),
    /// Handed over to the one running: this one is done.
    Forwarded,
    /// Runs on its own (the one running couldn't take it; told why).
    Alone(String),
    /// What this one asked can't be asked (a fault: exit).
    Refused(String),
    /// Not one at a time here (why, for the log).
    Off(String),
}

/// Being the one running: the lock held, the socket served (until dropped).
pub struct Primary {
    pub link: Link,
    _lock: std::fs::File,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket: PathBuf,
    /// The socket made (device, inode): only that one is removed.
    made: (u64, u64),
}

impl Drop for Primary {
    fn drop(&mut self) {
        self.link.close_late();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Only ours, while the lock is still held, nothing served any more.
        if let Ok(m) = std::fs::symlink_metadata(&self.socket)
            && (m.dev(), m.ino()) == self.made
        {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

/// Becomes the one running at `place`, or hands `request` to it; tries
/// until `deadline`.
pub fn claim(place: &Place, request: &Request, deadline: Duration) -> Claim {
    claim_with(place, request, deadline, CONNECTION)
}

/// [`claim`], a connection (asking, or being asked) taking up to
/// `connection` (tests: one that doesn't run out meanwhile).
pub fn claim_with(place: &Place, request: &Request, deadline: Duration, connection: Duration) -> Claim {
    if let Err(why) = check_request(request) {
        return Claim::Refused(why);
    }
    let start = Instant::now();
    let end = start + deadline;
    let lock = match std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.lock()) {
        Ok(f) => f,
        Err(e) => return Claim::Off(format!("{}: {e}", place.lock().display())),
    };
    loop {
        if try_lock(&lock) {
            return match serve(place, lock, connection) {
                Ok(p) => Claim::Primary(p),
                Err(why) => Claim::Off(why),
            };
        }
        // (Each try within what is left of the deadline.)
        match send_by(&place.socket(), request, (Instant::now() + connection).min(end)) {
            Ok(Reply::Accepted) => return Claim::Forwarded,
            Ok(Reply::BadVersion(why)) => {
                return Claim::Alone(format!(
                    "Another pengitto of another version is running ({why}): close it, then start pengitto again to have one."
                ));
            }
            Ok(Reply::BadRequest(why)) => return Claim::Refused(why),
            // Starting, closing, busy; or not reachable just now.
            Ok(_) | Err(_) => {}
        }
        if start.elapsed() >= deadline {
            return Claim::Alone("Another pengitto didn't answer: this one runs on its own.".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn try_lock(f: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd as _;
    // SAFETY: flock on an open file.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Serves `place`, its lock held.
fn serve(place: &Place, lock: std::fs::File, connection: Duration) -> Result<Primary, String> {
    let socket = place.socket();
    // One left by a pengitto that ended without removing it (the lock is
    // ours now): gone. Anything else there: not ours to remove.
    match std::fs::symlink_metadata(&socket) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(&socket).map_err(|e| format!("{}: {e}", socket.display()))?,
        Ok(_) => return Err(format!("{} is there and isn't a socket", socket.display())),
        Err(_) => {}
    }
    let listener = UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let m = std::fs::symlink_metadata(&socket).map_err(|e| e.to_string())?;
    let link = Link::default();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (link, stop) = (link.clone(), stop.clone());
        std::thread::Builder::new()
            .name("pengitto-instance".into())
            .spawn(move || accept(&listener, &link, &stop, connection))
            .map_err(|e| e.to_string())?
    };
    Ok(Primary { link, _lock: lock, stop, thread: Some(thread), socket, made: (m.dev(), m.ino()) })
}

use std::os::unix::fs::FileTypeExt as _;

fn accept(listener: &UnixListener, link: &Link, stop: &AtomicBool, connection: Duration) {
    use std::os::fd::AsRawFd as _;
    let open = &link.0.open;
    while !stop.load(Ordering::SeqCst) {
        let mut fds = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fds, 1, 100) } <= 0 {
            continue;
        }
        let Ok((stream, _)) = listener.accept() else { continue };
        // Too many at once: closed right away (they ask again).
        if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            continue;
        }
        open.fetch_add(1, Ordering::SeqCst);
        let link = link.clone();
        let spawned = std::thread::Builder::new().name("pengitto-request".into()).spawn(move || {
            answer(stream, &link, connection);
            link.0.open.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Reads one request (within `connection`) and answers it.
fn answer(mut stream: UnixStream, link: &Link, connection: Duration) {
    let _ = stream.set_nonblocking(false);
    let deadline = Instant::now() + connection;
    let reply = match read_request(&mut stream, deadline) {
        Ok(request) => link.take(request),
        Err(Bad::Version(v)) => Reply::BadVersion(format!("asked in version {v}, this one speaks {VERSION}")),
        Err(Bad::Request(why)) => Reply::BadRequest(why),
        Err(Bad::Io) => return,
    };
    let _ = write_with_deadline(&mut stream, &encode_reply(&reply), deadline);
    link.0.answered.fetch_add(1, Ordering::SeqCst);
}

// --- Asking -----------------------------------------------------------------

/// Sends `request` to the socket at `path`; its reply.
pub fn send(path: &Path, request: &Request) -> std::io::Result<Reply> {
    send_by(path, request, Instant::now() + CONNECTION)
}

/// [`send`], all of it (connecting, sending, the reply) by `deadline`.
fn send_by(path: &Path, request: &Request, deadline: Instant) -> std::io::Result<Reply> {
    let mut stream = connect_by(path, deadline)?;
    write_with_deadline(&mut stream, &encode_request(request), deadline)?;
    read_reply(&mut stream, deadline)
}

/// Connects without waiting past `deadline` (a socket whose queue of
/// connections is full answers at once: try again later).
fn connect_by(path: &Path, deadline: Instant) -> std::io::Result<UnixStream> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: a zeroed sockaddr_un is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: a new socket; owned by the stream at once.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is ours, valid.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    // SAFETY: addr is a valid sockaddr_un of that length.
    if unsafe { libc::connect(stream.as_raw_fd(), (&raw const addr).cast(), len as libc::socklen_t) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        let mut fds = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
        let ms = remaining(deadline)?.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fds, 1, ms.max(1)) } <= 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        if let Some(e) = stream.take_error()? {
            return Err(e);
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() { Err(std::io::ErrorKind::TimedOut.into()) } else { Ok(left) }
}

/// Writes all of `bytes` by `deadline` (however slowly the other end
/// reads): without blocking, waiting for room with poll — a blocking write
/// waits afresh for each piece it sends (the socket's timeout bounds each
/// wait, not the write), so one reading a little now and then keeps it
/// going.
fn write_with_deadline(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> std::io::Result<()> {
    stream.set_nonblocking(true)?;
    let written = write_nonblocking_by(stream, bytes, deadline);
    stream.set_nonblocking(false)?;
    written
}

fn write_nonblocking_by(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    while !bytes.is_empty() {
        let left = remaining(deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut fds = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                let ms = left.as_millis().min(i32::MAX as u128) as i32;
                // SAFETY: one valid pollfd. (Woken, timed out or
                // interrupted: the loop looks again.)
                unsafe { libc::poll(&mut fds, 1, ms.max(1)) };
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_exact_by(stream: &mut UnixStream, buf: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    let mut at = 0;
    while at < buf.len() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(&mut buf[at..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => at += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// --- The format -------------------------------------------------------------
//
// Request: "PGTO", version (1 byte), body length (u32, big-endian), body:
// fields of tag (1 byte), length (u32), bytes. Tags: 1 work dir, 2 a path
// (repeated), 3 activation token, 4 startup id. Reply: kind (1 byte), reason
// length (u16), reason.

const TAG_CWD: u8 = 1;
const TAG_PATH: u8 = 2;
const TAG_TOKEN: u8 = 3;
const TAG_STARTUP_ID: u8 = 4;

enum Bad {
    Version(u8),
    Request(String),
    Io,
}

/// The size of the request's body, encoded (as [`encode_request`] makes it).
fn body_len(r: &Request) -> usize {
    let field = |b: &[u8]| 5 + b.len();
    field(r.cwd.as_os_str().as_bytes())
        + r.paths.iter().map(|p| field(p.as_os_str().as_bytes())).sum::<usize>()
        + [&r.token, &r.startup_id].into_iter().flatten().map(|t| field(t.as_bytes())).sum::<usize>()
}

/// What can't be sent (checked before sending, as when received).
fn check_request(r: &Request) -> Result<(), String> {
    let len = body_len(r);
    if len > MAX_BODY {
        return Err(format!("a request of {len} bytes, more than {MAX_BODY}"));
    }
    if r.paths.len() > MAX_PATHS {
        return Err(format!("more than {MAX_PATHS} paths"));
    }
    for p in std::iter::once(&r.cwd).chain(&r.paths) {
        let b = p.as_os_str().as_bytes();
        if b.len() > MAX_PATH || b.contains(&0) {
            return Err(format!("a path too long, or with a NUL: {}", p.display()));
        }
    }
    for t in [&r.token, &r.startup_id].into_iter().flatten() {
        check_token(t.as_bytes())?;
    }
    Ok(())
}

fn check_token(t: &[u8]) -> Result<(), String> {
    if t.is_empty() || t.len() > MAX_TOKEN || !t.iter().all(|b| (0x21..=0x7e).contains(b)) {
        return Err("an activation token that isn't one".into());
    }
    Ok(())
}

fn encode_request(r: &Request) -> Vec<u8> {
    let mut body = Vec::new();
    let mut field = |tag: u8, bytes: &[u8]| {
        body.push(tag);
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(bytes);
    };
    field(TAG_CWD, r.cwd.as_os_str().as_bytes());
    for p in &r.paths {
        field(TAG_PATH, p.as_os_str().as_bytes());
    }
    if let Some(t) = &r.token {
        field(TAG_TOKEN, t.as_bytes());
    }
    if let Some(t) = &r.startup_id {
        field(TAG_STARTUP_ID, t.as_bytes());
    }
    debug_assert_eq!(body.len(), body_len(r));
    let mut out = MAGIC.to_vec();
    out.push(VERSION);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend(body);
    out
}

fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<Request, Bad> {
    let mut head = [0u8; 9];
    read_exact_by(stream, &mut head, deadline).map_err(|_| Bad::Io)?;
    if &head[..4] != MAGIC {
        return Err(Bad::Request("not a request".into()));
    }
    if head[4] != VERSION {
        return Err(Bad::Version(head[4]));
    }
    let len = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
    // (Checked before anything is set aside for it.)
    if len > MAX_BODY {
        return Err(Bad::Request(format!("a request of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    read_exact_by(stream, &mut body, deadline).map_err(|_| Bad::Io)?;
    decode_body(&body).map_err(Bad::Request)
}

fn decode_body(body: &[u8]) -> Result<Request, String> {
    let mut r = Request::default();
    let mut at = 0;
    let mut cwd = false;
    while at < body.len() {
        if body.len() - at < 5 {
            return Err("a cut-off field".into());
        }
        let tag = body[at];
        let len = u32::from_be_bytes([body[at + 1], body[at + 2], body[at + 3], body[at + 4]]) as usize;
        at += 5;
        if len > body.len() - at {
            return Err("a field longer than the request".into());
        }
        let bytes = &body[at..at + len];
        at += len;
        let path = || -> Result<PathBuf, String> {
            if bytes.len() > MAX_PATH || bytes.contains(&0) {
                return Err("a path too long, or with a NUL".into());
            }
            Ok(PathBuf::from(OsStr::from_bytes(bytes)))
        };
        match tag {
            TAG_CWD if !cwd => {
                r.cwd = path()?;
                cwd = true;
            }
            TAG_PATH if r.paths.len() < MAX_PATHS => r.paths.push(path()?),
            TAG_PATH => return Err(format!("more than {MAX_PATHS} paths")),
            TAG_TOKEN | TAG_STARTUP_ID => {
                check_token(bytes)?;
                let t = String::from_utf8_lossy(bytes).into_owned();
                if tag == TAG_TOKEN { r.token = Some(t) } else { r.startup_id = Some(t) }
            }
            _ => return Err(format!("an unknown field ({tag})")),
        }
    }
    Ok(r)
}

fn encode_reply(r: &Reply) -> Vec<u8> {
    let (kind, reason) = match r {
        Reply::Accepted => (0, ""),
        Reply::Starting => (1, ""),
        Reply::Closing => (2, ""),
        Reply::Busy => (3, ""),
        Reply::BadVersion(w) => (4, w.as_str()),
        Reply::BadRequest(w) => (5, w.as_str()),
    };
    let reason = &reason.as_bytes()[..reason.len().min(MAX_REASON)];
    let mut out = vec![kind];
    out.extend_from_slice(&(reason.len() as u16).to_be_bytes());
    out.extend_from_slice(reason);
    out
}

fn read_reply(stream: &mut UnixStream, deadline: Instant) -> std::io::Result<Reply> {
    let mut head = [0u8; 3];
    read_exact_by(stream, &mut head, deadline)?;
    let len = usize::from(u16::from_be_bytes([head[1], head[2]]));
    if len > MAX_REASON {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut reason = vec![0u8; len];
    read_exact_by(stream, &mut reason, deadline)?;
    let reason = String::from_utf8_lossy(&reason).into_owned();
    Ok(match head[0] {
        0 => Reply::Accepted,
        1 => Reply::Starting,
        2 => Reply::Closing,
        3 => Reply::Busy,
        4 => Reply::BadVersion(reason),
        5 => Reply::BadRequest(reason),
        _ => return Err(std::io::ErrorKind::InvalidData.into()),
    })
}

// --- The launcher's tokens ------------------------------------------------------

static TOKENS: std::sync::OnceLock<(Option<String>, Option<String>)> = std::sync::OnceLock::new();

/// Notes and removes the launcher's activation token (`XDG_ACTIVATION_TOKEN`,
/// Wayland) and startup id (`DESKTOP_STARTUP_ID`, X11): what pengitto starts
/// mustn't get them.
///
/// # Safety
/// Call it before any other thread starts (it changes the environment).
pub unsafe fn take_launch_tokens() {
    let take = |name: &str| {
        let v = std::env::var(name).ok().filter(|t| check_token(t.as_bytes()).is_ok());
        // SAFETY: the caller promises no other thread runs yet.
        unsafe { std::env::remove_var(name) };
        v
    };
    let _ = TOKENS.set((take("XDG_ACTIVATION_TOKEN"), take("DESKTOP_STARTUP_ID")));
}

/// The launcher's activation token and startup id (see
/// [`take_launch_tokens`]).
pub fn launch_tokens() -> (Option<String>, Option<String>) {
    TOKENS.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_go_both_ways() {
        let r = Request {
            cwd: "/home/x".into(),
            paths: vec!["/a b".into(), PathBuf::from(OsStr::from_bytes(b"/caf\xe9"))],
            token: Some("tok_123".into()),
            startup_id: None,
            activated: false,
        };
        let bytes = encode_request(&r);
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(decode_body(&bytes[9..]).unwrap(), r);
        for reply in [Reply::Accepted, Reply::Starting, Reply::Closing, Reply::Busy, Reply::BadVersion("v".into()), Reply::BadRequest("x".into())] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_all(&encode_reply(&reply)).unwrap();
            assert_eq!(read_reply(&mut b, Instant::now() + CONNECTION).unwrap(), reply);
        }
        // What can't be asked.
        for bad in [
            Request { token: Some("with space".into()), ..Default::default() },
            Request { token: Some("nul\0".into()), ..Default::default() },
            Request { token: Some("x".repeat(MAX_TOKEN + 1)), ..Default::default() },
            Request { paths: vec![PathBuf::from(OsStr::from_bytes(b"/a\0b"))], ..Default::default() },
            Request { paths: vec![PathBuf::from("/x".repeat(MAX_PATH))], ..Default::default() },
            Request { paths: vec![PathBuf::from("/p"); MAX_PATHS + 1], ..Default::default() },
        ] {
            assert!(check_request(&bad).is_err(), "{bad:?}");
        }
        // The whole of it: up to the limit, not one byte more (each field
        // within its own).
        let mut big = Request { cwd: "/".into(), paths: Vec::new(), ..Default::default() };
        let path = |n: usize| PathBuf::from(format!("/{}", "p".repeat(n - 1)));
        while body_len(&big) + 5 + MAX_PATH <= MAX_BODY {
            big.paths.push(path(MAX_PATH));
        }
        let room = MAX_BODY - body_len(&big) - 5;
        big.paths.push(path(room));
        assert_eq!(body_len(&big), MAX_BODY);
        assert_eq!(encode_request(&big).len() - 9, MAX_BODY);
        assert!(check_request(&big).is_ok());
        big.paths.last_mut().unwrap().push("x");
        assert!(check_request(&big).unwrap_err().contains("more than"));
        // Received broken: told, not panicking.
        assert!(decode_body(&[TAG_PATH, 0, 0, 0, 9, b'/']).is_err());
        assert!(decode_body(&[TAG_TOKEN, 0, 0, 0, 1, 0]).is_err());
        assert!(decode_body(&[9, 0, 0, 0, 0]).is_err());
        assert!(decode_body(&[TAG_CWD, 0, 0]).is_err());
    }
}
