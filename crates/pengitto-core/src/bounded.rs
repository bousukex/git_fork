//! Running a program to check something, within a time: its start, its exit
//! and its output all count (a wrapper may leave a process behind that holds
//! the output open; what was written by then counts, said to be incomplete).
//! Its own process group, stopped as a whole once it is done with — whatever
//! it left behind, holding the output or not; its output kept up to a size.

use std::{
    io::Read as _,
    os::unix::process::CommandExt as _,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// What a program kept within its time said.
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// All of its output was read: to its end, within the time, none of it
    /// dropped (past [`MAX_OUTPUT`]) nor lost to a read error.
    pub complete: bool,
}

impl Ran {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Output kept, per stream (more is read and dropped).
pub const MAX_OUTPUT: usize = 1 << 20;

/// Why there is nothing to read (said in words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// It couldn't start (not there, not executable…).
    Start(String),
    /// It didn't start, or end, in time.
    Time(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Start(s) | Failed::Time(s) => f.write_str(s),
        }
    }
}

/// Stops the process group of `child` (its leader, not yet reaped, keeps the
/// group's id from being taken by another), then reaps it.
fn stop_group(child: &mut Child) -> Option<ExitStatus> {
    // SAFETY: a signal to the process group made for it; its leader is not
    // reaped yet (see above).
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    child.wait().ok()
}

/// Whether `child` has ended — without reaping it.
fn ended(child: &Child) -> std::io::Result<bool> {
    // SAFETY: a zeroed siginfo_t is valid; waitid only writes it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waits (without blocking, without reaping) for our own child.
    let r = unsafe { libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: filled by waitid (0: not ended yet).
    Ok(unsafe { info.si_pid() } != 0)
}

/// Starts `cmd`, by `deadline` (starting may itself hang: a file system
/// that doesn't answer). One started after it was given up on is stopped.
fn spawn_by(mut cmd: Command, deadline: Instant, name: &str) -> Result<Child, Failed> {
    #[derive(Default)]
    struct Handoff {
        done: bool,
        given_up: bool,
        child: Option<Result<Child, std::io::Error>>,
    }
    let shared = Arc::new((Mutex::new(Handoff::default()), Condvar::new()));
    let theirs = shared.clone();
    std::thread::Builder::new()
        .name("pengitto-spawn".into())
        .spawn(move || {
            let started = cmd.spawn();
            let (lock, cv) = &*theirs;
            let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
            match started {
                Ok(mut child) if h.given_up => {
                    stop_group(&mut child);
                }
                started => h.child = Some(started),
            }
            h.done = true;
            cv.notify_all();
        })
        .map_err(|e| Failed::Start(format!("can't run {name}: {e}")))?;
    let (lock, cv) = &*shared;
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    while !h.done {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            h.given_up = true;
            return Err(Failed::Time(format!("{name} didn't start in time")));
        }
        h = cv.wait_timeout(h, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    match h.child.take().expect("done") {
        Ok(child) => Ok(child),
        Err(e) => Err(Failed::Start(format!("can't run {name}: {e}"))),
    }
}

/// Runs `cmd` (stdin closed), giving up after `timeout` for the whole of
/// it: starting, ending, its output.
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Ran, Failed> {
    let deadline = Instant::now() + timeout;
    let name = std::path::Path::new(cmd.get_program()).file_name().unwrap_or(cmd.get_program()).to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group: whatever it leaves behind is stopped with it.
        .process_group(0);
    let mut child = spawn_by(cmd, deadline, &name)?;
    // (Read to the end, kept up to the limit; whether all of it was.)
    let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
    let keep = |mut from: Box<dyn std::io::Read + Send>| {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let (sink, done) = (kept.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            let mut whole = true;
            loop {
                match from.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut k = sink.lock().unwrap_or_else(|e| e.into_inner());
                        let room = MAX_OUTPUT.saturating_sub(k.len());
                        whole &= n <= room;
                        k.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        whole = false;
                        break;
                    }
                }
            }
            let _ = done.send(whole);
        });
        kept
    };
    let stdout = keep(Box::new(child.stdout.take().expect("piped")));
    let stderr = keep(Box::new(child.stderr.take().expect("piped")));
    let finished = loop {
        match ended(&child) {
            Ok(true) => break Ok(true),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(false) => break Ok(false),
            Err(e) => break Err(e),
        }
    };
    // Both outputs' ends, within what is left of the time.
    let complete = matches!(finished, Ok(true))
        && (0..2).all(|_| done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) == Ok(true));
    // Whatever is left of it goes (on every way out).
    let status = stop_group(&mut child);
    let take = |k: Arc<Mutex<Vec<u8>>>| std::mem::take(&mut *k.lock().unwrap_or_else(|e| e.into_inner()));
    match (finished, status) {
        (Ok(true), Some(status)) => Ok(Ran { status, stdout: take(stdout), stderr: take(stderr), complete }),
        (Err(e), _) => Err(Failed::Time(format!("{name}: {e}"))),
        _ => Err(Failed::Time(format!("{name} didn't answer in {timeout:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(body: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", body]);
        c
    }

    /// Whether no process has `marker` in its command line.
    fn none_with(marker: &str) -> bool {
        std::fs::read_dir("/proc").unwrap().flatten().all(|e| {
            let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
            !String::from_utf8_lossy(&cmd).contains(marker) || stat.contains(") Z ")
        })
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let t = Instant::now();
        while !done() {
            assert!(t.elapsed() < Duration::from_secs(5), "never: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn within_its_time_whatever_it_does() {
        let r = run(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(5)).unwrap();
        assert_eq!((r.stdout_text().as_str(), r.stderr.as_slice(), r.status.code()), ("out\n", &b"err\n"[..], Some(3)));
        assert!(r.complete);
        // Doesn't end: stopped, said so.
        let t = Instant::now();
        assert!(matches!(run(sh("exec sleep 10"), Duration::from_millis(200)), Err(Failed::Time(e)) if e.contains("didn't answer")));
        assert!(t.elapsed() < Duration::from_secs(3));
        // Ends, something left behind holding the output open: what it said
        // by then, within the time; and what was left is stopped.
        let t = Instant::now();
        let r = run(sh("echo said; sleep 10 & echo $! >&2"), Duration::from_millis(500)).unwrap();
        assert_eq!(r.stdout_text(), "said\n");
        assert!(!r.complete, "its output didn't end");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let pid: i32 = String::from_utf8_lossy(&r.stderr).trim().parse().unwrap();
        // (Reaped by init: gone, or a zombie for a moment.)
        let gone = || std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.contains(") Z "));
        let t = Instant::now();
        while !gone() {
            assert!(t.elapsed() < Duration::from_secs(5), "left running");
            std::thread::sleep(Duration::from_millis(20));
        }
        // A lot of output: kept up to the limit, all of it read, said to
        // be incomplete; up to it, complete.
        let r = run(sh("head -c 3000000 /dev/zero"), Duration::from_secs(10)).unwrap();
        assert!(r.status.success() && r.stdout.len() == MAX_OUTPUT && !r.complete);
        let r = run(sh(&format!("head -c {MAX_OUTPUT} /dev/zero")), Duration::from_secs(10)).unwrap();
        assert!(r.stdout.len() == MAX_OUTPUT && r.complete);
        assert!(matches!(run(Command::new("/nonexistent/x"), Duration::from_secs(1)), Err(Failed::Start(e)) if e.contains("can't run x")));
    }

    #[test]
    fn what_it_leaves_behind_goes_output_or_not() {
        // Left behind with its output closed: done at once, and stopped.
        let marker = format!("{}.{}17", std::process::id(), line!());
        let t = Instant::now();
        let r = run(sh(&format!("sleep {marker} </dev/null >/dev/null 2>&1 & echo done")), Duration::from_secs(5)).unwrap();
        assert!(r.complete && r.stdout_text() == "done\n" && t.elapsed() < Duration::from_secs(2));
        wait_for("what it left stopped", || none_with(&marker));
    }

    #[test]
    fn starting_that_hangs_is_given_up_on() {
        // (Its start held, as by a file system that doesn't answer.)
        let marker = format!("{}.{}29", std::process::id(), line!());
        let mut cmd = sh(&format!("exec sleep {marker}"));
        // SAFETY: only sleeps, in the child before it runs the program.
        unsafe {
            cmd.pre_exec(|| {
                std::thread::sleep(Duration::from_millis(1500));
                Ok(())
            });
        }
        let t = Instant::now();
        assert!(matches!(run(cmd, Duration::from_millis(300)), Err(Failed::Time(e)) if e.contains("didn't start")));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        // Started after all: stopped then.
        std::thread::sleep(Duration::from_millis(1600));
        wait_for("the late one stopped", || none_with(&marker));
    }
}
