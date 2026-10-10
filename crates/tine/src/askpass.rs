//! Credential prompts of git and ssh, answered in Tine's own dialog.
//!
//! Remote operations run git with `GIT_ASKPASS` / `SSH_ASKPASS` pointing at
//! the tine executable. Started that way, tine runs as a small client
//! ([`run_client`]) instead of the GUI: it forwards the prompt over a Unix
//! socket (in a 0700 directory) to the running app, which shows it and
//! sends back the answer. Every operation gets its own random token;
//! requests with an unknown token are refused. Once the user cancels a
//! prompt, the operation is cancelled and later prompts from it are refused
//! without asking again. Prompts live here, not in a tab, so they can be
//! answered after the tab that started the operation was closed.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use crossbeam_channel::{Sender, bounded};

use crate::ops_registry::{self, OpId};

pub const SOCKET_ENV: &str = "TINE_ASKPASS_SOCKET";
pub const TOKEN_ENV: &str = "TINE_ASKPASS_TOKEN";

/// What kind of answer a prompt wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// ssh's unknown host key question: "yes", "no" or the fingerprint.
    HostKey,
    /// Yes/no (`SSH_ASKPASS_PROMPT=confirm`); the exit status is the answer.
    Confirm,
    /// Password, passphrase, token, PIN: masked.
    Secret,
    /// Username or anything else: shown (can be masked).
    Text,
}

/// Decides the kind from the prompt text and `SSH_ASKPASS_PROMPT`.
pub fn classify(prompt: &str, ssh_prompt: &str) -> PromptKind {
    let lower = prompt.to_lowercase();
    if lower.contains("(yes/no") {
        // OpenSSH asks this with echo on and no SSH_ASKPASS_PROMPT.
        PromptKind::HostKey
    } else if ssh_prompt == "confirm" {
        PromptKind::Confirm
    } else if ["password", "passphrase", "token", "pin", "secret"].iter().any(|w| lower.contains(w)) {
        PromptKind::Secret
    } else {
        PromptKind::Text
    }
}

/// A prompt waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub id: u64,
    pub op: OpId,
    /// What asked, e.g. "Push" in `/home/me/project`.
    pub op_label: String,
    pub workdir: PathBuf,
    pub text: String,
    pub kind: PromptKind,
}

/// The user's answer to a [`Prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Value(String),
    /// "No" to a yes/no question; the operation goes on and git decides.
    Decline,
    /// Stop the whole operation.
    Cancel,
}

struct Waiting {
    prompt: Prompt,
    reply: Sender<Answer>,
}

struct Session {
    op: OpId,
    op_label: String,
    workdir: PathBuf,
    ctx: Option<egui::Context>,
}

#[derive(Default)]
struct State {
    socket: Option<PathBuf>,
    program: Option<PathBuf>,
    sessions: HashMap<String, Session>,
    refused: HashSet<OpId>,
    waiting: Vec<Waiting>,
    next_id: u64,
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let mut s = STATE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    f(&mut s)
}

/// The executable git runs as askpass (tests point it at the built `tine`).
pub fn set_program(path: impl Into<PathBuf>) {
    let path = path.into();
    with(|s| s.program = Some(path));
}

static SOCKET_DIR: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn remove_socket_dir() {
    if let Some(dir) = SOCKET_DIR.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Longest request a client may send (token, kind and prompt text).
const MAX_REQUEST: u64 = 64 * 1024;
/// How long a client may take to send its request; waiting for the user
/// to answer is not limited.
const IPC_TIMEOUT: Duration = Duration::from_secs(3);

/// Starts listening (once per process); returns the socket path. Concurrent
/// first calls (operations of different repositories) start one server.
fn ensure_server() -> std::io::Result<PathBuf> {
    static START: Mutex<()> = Mutex::new(());
    let _starting = START.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = with(|s| s.socket.clone()) {
        return Ok(p);
    }
    use std::os::unix::fs::DirBuilderExt as _;
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let dir = base.join(format!("tine-{uid}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let socket = dir.join("askpass.sock");
    let listener = UnixListener::bind(&socket)?;
    if SOCKET_DIR.set(dir).is_ok() {
        // SAFETY: registers a plain function that only removes a directory.
        unsafe {
            libc::atexit(remove_socket_dir);
        }
    }
    std::thread::Builder::new().name("tine-askpass".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = std::thread::Builder::new().name("tine-askpass-conn".into()).spawn(move || serve(stream));
        }
    })?;
    with(|s| s.socket = Some(socket.clone()));
    Ok(socket)
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        // Not expected on Linux; still unguessable enough across processes.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        bytes[..16].copy_from_slice(&t.as_nanos().to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Environment that routes credential prompts of operation `op` to the
/// app. Fails if prompts can't be set up (the operation then fails with
/// that reason instead of with a confusing "terminal prompts disabled").
pub fn env_for(
    op: OpId,
    op_label: &str,
    workdir: &Path,
    ctx: Option<egui::Context>,
) -> anyhow::Result<Vec<(String, OsString)>> {
    use anyhow::Context as _;
    let socket = ensure_server().context("can't set up credential prompts")?;
    let program = match with(|s| s.program.clone()) {
        Some(p) => p,
        None => std::env::current_exe().context("can't find the tine executable for credential prompts")?,
    };
    let token = random_token();
    with(|s| {
        s.sessions.insert(
            token.clone(),
            Session { op, op_label: op_label.to_owned(), workdir: workdir.to_path_buf(), ctx },
        )
    });
    Ok(vec![
        ("GIT_ASKPASS".into(), program.clone().into()),
        ("SSH_ASKPASS".into(), program.into()),
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        (SOCKET_ENV.into(), socket.into()),
        (TOKEN_ENV.into(), token.into()),
    ])
}

/// Forgets operation `op` once it finished.
pub fn forget(op: OpId) {
    let replies: Vec<Sender<Answer>> = with(|s| {
        s.sessions.retain(|_, sess| sess.op != op);
        s.refused.remove(&op);
        let (gone, keep): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut s.waiting).into_iter().partition(|w| w.prompt.op == op);
        s.waiting = keep;
        gone.into_iter().map(|w| w.reply).collect()
    });
    for r in replies {
        let _ = r.send(Answer::Cancel);
    }
}

/// The prompt to show now, if any (oldest first).
pub fn pending() -> Option<Prompt> {
    with(|s| s.waiting.first().map(|w| w.prompt.clone()))
}

/// The oldest prompt of operation `op`, if it is waiting for one.
pub fn pending_for(op: OpId) -> Option<Prompt> {
    with(|s| s.waiting.iter().find(|w| w.prompt.op == op).map(|w| w.prompt.clone()))
}

/// Answers prompt `id`. [`Answer::Cancel`] cancels its operation and
/// refuses its later prompts.
pub fn answer(id: u64, answer: Answer) {
    let (replies, cancel_op) = with(|s| {
        let Some(i) = s.waiting.iter().position(|w| w.prompt.id == id) else { return (Vec::new(), None) };
        let w = s.waiting.remove(i);
        let op = w.prompt.op;
        if answer != Answer::Cancel {
            return (vec![w.reply], None);
        }
        s.refused.insert(op);
        // Other prompts of the same operation are moot now.
        let (gone, keep): (Vec<Waiting>, Vec<Waiting>) = std::mem::take(&mut s.waiting).into_iter().partition(|w| w.prompt.op == op);
        s.waiting = keep;
        (std::iter::once(w.reply).chain(gone.into_iter().map(|g| g.reply)).collect(), Some(op))
    });
    // Cancel the operation before git hears "no": otherwise git may fail
    // and the operation finish as a plain failure before the cancel lands.
    if let Some(op) = cancel_op {
        ops_registry::cancel(op);
    }
    for r in replies {
        let _ = r.send(answer.clone());
    }
}

/// Request: `token NUL ssh-prompt-kind NUL prompt`. Reply: `Y` + value, or `N`.
fn serve(mut stream: UnixStream) {
    let mut req = Vec::new();
    // A client that stalls or floods is dropped without being asked about.
    let _ = stream.set_read_timeout(Some(IPC_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IPC_TIMEOUT));
    if (&stream).take(MAX_REQUEST + 1).read_to_end(&mut req).is_err() {
        return;
    }
    if req.len() as u64 > MAX_REQUEST {
        let _ = stream.write_all(b"N");
        return;
    }
    let mut parts = req.splitn(3, |&b| b == 0);
    let (Some(token), Some(hint), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
        let _ = stream.write_all(b"N");
        return;
    };
    let token = String::from_utf8_lossy(token).into_owned();
    let hint = String::from_utf8_lossy(hint).into_owned();
    let text = String::from_utf8_lossy(text).into_owned();
    let (tx, rx) = bounded(1);
    let started = with(|s| {
        let sess = s.sessions.get(&token)?;
        if s.refused.contains(&sess.op) {
            return None;
        }
        let (op, ctx) = (sess.op, sess.ctx.clone());
        if hint == "none" {
            // Information only (e.g. "touch your security key"); ssh doesn't wait.
            return Some((op, ctx, None));
        }
        s.next_id += 1;
        let id = s.next_id;
        let prompt = Prompt {
            id: s.next_id,
            op,
            op_label: sess.op_label.clone(),
            workdir: sess.workdir.clone(),
            kind: classify(&text, &hint),
            text: text.clone(),
        };
        s.waiting.push(Waiting { prompt, reply: tx });
        Some((op, ctx, Some(id)))
    });
    let Some((op, ctx, waiting_id)) = started else {
        let _ = stream.write_all(b"N");
        return;
    };
    let Some(id) = waiting_id else {
        ops_registry::set_progress(op, text.trim());
        let _ = stream.write_all(b"Y");
        return;
    };
    ops_registry::set_awaiting_auth(op, true);
    if let Some(c) = &ctx {
        c.request_repaint();
    }
    let answer = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(a) => break a,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break Answer::Cancel,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Cancelled from the toolbar, or finished: stop waiting.
                let active = ops_registry::state(op).is_some_and(|s| s.is_active() && s != ops_registry::OpState::Cancelling);
                if !active {
                    with(|s| s.waiting.retain(|w| w.prompt.op != op));
                    break Answer::Cancel;
                }
                // The asking process is gone (killed with git): drop its prompt.
                if peer_closed(&stream) {
                    with(|s| s.waiting.retain(|w| w.prompt.id != id));
                    break Answer::Cancel;
                }
            }
        }
    };
    let still_waiting = with(|s| s.waiting.iter().any(|w| w.prompt.op == op));
    if !still_waiting {
        ops_registry::set_awaiting_auth(op, false);
    }
    if let Some(c) = &ctx {
        c.request_repaint();
    }
    let reply: Vec<u8> = match answer {
        Answer::Value(v) => [b"Y".as_slice(), v.as_bytes()].concat(),
        Answer::Decline | Answer::Cancel => b"N".to_vec(),
    };
    let _ = stream.write_all(&reply);
}

/// Whether the client closed its end completely (not just its sending half,
/// which it does after the request).
fn peer_closed(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;
    let mut fd = libc::pollfd { fd: stream.as_raw_fd(), events: 0, revents: 0 };
    // SAFETY: one valid pollfd, zero timeout.
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// Sends one prompt to the app; `None` means declined or cancelled.
pub fn request(socket: &Path, token: &str, ssh_prompt: &str, prompt: &str) -> std::io::Result<Option<String>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&[token.as_bytes(), b"\0", ssh_prompt.as_bytes(), b"\0", prompt.as_bytes()].concat())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(match reply.split_first() {
        Some((b'Y', value)) => Some(String::from_utf8_lossy(value).into_owned()),
        _ => None,
    })
}

/// If this process was started by git or ssh as askpass, answers the prompt
/// through the running app and returns the exit code; otherwise `None`.
pub fn run_client() -> Option<i32> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let token = std::env::var(TOKEN_ENV).ok()?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let prompt = match args.as_slice() {
        [p] => p.to_string_lossy().into_owned(),
        [] => String::new(),
        _ => return None,
    };
    let hint = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    Some(match request(Path::new(&socket), &token, &hint, &prompt) {
        Ok(Some(value)) => {
            if hint != "confirm" {
                println!("{value}");
            }
            0
        }
        Ok(None) => 1,
        Err(e) => {
            eprintln!("tine askpass: {e}");
            1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_kinds() {
        let host_key = "The authenticity of host 'example.com (192.0.2.1)' can't be established.\n\
            ED25519 key fingerprint is SHA256:abc.\n\
            Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        assert_eq!(classify(host_key, ""), PromptKind::HostKey);
        assert_eq!(classify("Allow use of key?", "confirm"), PromptKind::Confirm);
        assert_eq!(classify("Enter passphrase for key '/home/u/.ssh/id_ed25519': ", ""), PromptKind::Secret);
        assert_eq!(classify("Password for 'https://u@example.com': ", ""), PromptKind::Secret);
        assert_eq!(classify("u@example.com's password: ", ""), PromptKind::Secret);
        assert_eq!(classify("Enter PIN for authenticator: ", ""), PromptKind::Secret);
        assert_eq!(classify("Username for 'https://example.com': ", ""), PromptKind::Text);
        assert_eq!(classify("Verification code: ", ""), PromptKind::Text);
    }

    #[test]
    fn unknown_tokens_are_refused() {
        let socket = ensure_server().unwrap();
        assert_eq!(request(&socket, "not-a-token", "", "Password: ").unwrap(), None);
        let mode = std::fs::metadata(socket.parent().unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(mode.permissions().mode() & 0o777, 0o700);
    }
}
