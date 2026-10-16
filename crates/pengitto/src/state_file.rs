//! pengitto's own state (recent repositories, settings…) in a file of its own,
//! written so that it is always whole: to a new temporary file, synced, then
//! renamed over the old one. eframe's `app.ron` is truncated and rewritten
//! in place, so two Tines saving at once, or one stopped while saving, can
//! leave it broken (and everything in it lost); there only window and egui
//! state is kept.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use serde::{Serialize, de::DeserializeOwned};

/// The state file of the app (next to eframe's).
pub fn default_path() -> Option<PathBuf> {
    eframe::storage_dir("pengitto").map(|d| d.join("state.ron"))
}

/// Where pengitto kept its state under its former name, found as eframe
/// found it.
pub fn legacy_dir() -> Option<PathBuf> {
    eframe::storage_dir(pengitto_core::legacy::APP)
}

pub struct StateFile {
    path: PathBuf,
    /// The newest save written so far (an older one finishing later is
    /// dropped).
    written: Arc<Mutex<u64>>,
    saves: u64,
    pending: Vec<JoinHandle<()>>,
    /// Why nothing is saved to the file now (it couldn't be read, or kept
    /// aside: writing over it would lose it).
    held: Option<String>,
    /// (Tests: the next save waits for a word on this before it writes.)
    #[cfg(test)]
    hold_next: Option<std::sync::mpsc::Receiver<()>>,
}

impl StateFile {
    pub fn new(path: PathBuf) -> Self {
        StateFile {
            path,
            written: Arc::default(),
            saves: 0,
            pending: Vec::new(),
            held: None,
            #[cfg(test)]
            hold_next: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was saved, if the file is there and readable.
    pub fn load<T: DeserializeOwned>(&self) -> Option<T> {
        match read(&self.path) {
            Loaded::Found(v) => Some(v),
            _ => None,
        }
    }

    /// From now on nothing is saved to the file (`why` says why).
    pub fn hold(&mut self, why: String) {
        log::warn!("{} isn't saved to: {why}", self.path.display());
        self.held = Some(why);
    }

    /// Why nothing is saved to the file, if so.
    pub fn held(&self) -> Option<&str> {
        self.held.as_deref()
    }

    /// Saves `value` (in the background; the newest save wins).
    pub fn save<T: Serialize>(&mut self, value: &T) {
        if self.held.is_some() {
            return;
        }
        let text = match ron::ser::to_string_pretty(value, Default::default()) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("the state can't be written: {e}");
                return;
            }
        };
        self.saves += 1;
        let (n, path, written) = (self.saves, self.path.clone(), self.written.clone());
        self.pending.retain(|h| !h.is_finished());
        #[cfg(test)]
        let hold = self.hold_next.take();
        let job = std::thread::Builder::new().name("pengitto-state".into()).spawn(move || {
            #[cfg(test)]
            if let Some(hold) = hold {
                let _ = hold.recv();
            }
            let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
            if n < *last {
                return;
            }
            match write_whole(&path, text.as_bytes()) {
                Ok(()) => *last = n,
                Err(e) => log::warn!("{} can't be written: {e}", path.display()),
            }
        });
        match job {
            Ok(h) => self.pending.push(h),
            Err(e) => log::warn!("the state can't be saved: {e}"),
        }
    }

    /// Waits for the saves under way (before the app exits).
    pub fn flush(&mut self) {
        for h in self.pending.drain(..) {
            let _ = h.join();
        }
    }
}

/// What reading a state file found.
#[derive(Debug, PartialEq)]
pub enum Loaded<T> {
    Missing,
    /// There, but not readable (permissions…).
    Unreadable(String),
    /// Read, but not what is saved there (cut off, edited, not UTF-8…):
    /// what was read, to be kept.
    Broken { error: String, bytes: Vec<u8> },
    Found(T),
}

/// Reads the state saved at `path`.
pub fn read<T: DeserializeOwned>(path: &Path) -> Loaded<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => {
            log::warn!("{} can't be read: {e}", path.display());
            return Loaded::Unreadable(e.to_string());
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("{} is broken: not UTF-8", path.display());
            return Loaded::Broken { error: "not UTF-8".into(), bytes: e.into_bytes() };
        }
    };
    match ron::from_str(&text) {
        Ok(v) => Loaded::Found(v),
        Err(e) => {
            log::warn!("{} is broken: {e}", path.display());
            Loaded::Broken { error: e.to_string(), bytes: text.into_bytes() }
        }
    }
}

/// Keeps what was read from a broken state file in a new file next to it
/// (`<name>.broken-<seconds>`, never one that is there already, nor through
/// a link), so that the next save, which replaces the file, doesn't lose it.
/// The file itself is left where it is.
pub fn set_aside(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    set_aside_stamped(path, bytes, secs)
}

fn set_aside_stamped(path: &Path, bytes: &[u8], stamp: u64) -> std::io::Result<PathBuf> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 0..1000 {
        let aside = path.with_file_name(match n {
            0 => format!("{name}.broken-{stamp}"),
            n => format!("{name}.broken-{stamp}-{n}"),
        });
        // (create_new: an existing file or link at the name fails.)
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&aside) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                f.sync_all()?;
                return Ok(aside);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name to keep it under"))
}

/// Writes `bytes` to `path` so that `path` is either what it was or all of
/// `bytes`: a new file next to it (synced), renamed over it.
pub fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tmp = None;
    for _ in 0..16 {
        let candidate = dir.join(format!(".{name}.{}.tmp", pengitto_core::interactive::random_id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(f) => {
                tmp = Some((candidate, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((tmp_path, mut file)) = tmp else { return Err(std::io::Error::other("no free temporary name")) };
    let result = file.write_all(bytes).and_then(|()| file.sync_all()).and_then(|()| std::fs::rename(&tmp_path, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct S {
        n: u64,
        text: String,
    }

    #[test]
    fn saves_whole_files_the_newest_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.ron");
        let mut f = StateFile::new(path.clone());
        assert_eq!(f.load::<S>(), None);
        for n in 0..50 {
            f.save(&S { n, text: "x".repeat((n as usize % 7) * 1000) });
        }
        f.flush();
        assert_eq!(f.load::<S>().unwrap().n, 49, "the newest save wins");
        // Nothing left behind but the file.
        let names: Vec<_> = std::fs::read_dir(dir.path().join("sub")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["state.ron"]);
        // Two of them (two Tines) saving at once: always a whole file.
        let mut a = StateFile::new(path.clone());
        let mut b = StateFile::new(path.clone());
        for n in 0..40 {
            a.save(&S { n, text: "a".repeat(5000) });
            b.save(&S { n, text: "b".repeat(10) });
        }
        a.flush();
        b.flush();
        let s: S = f.load().expect("readable");
        assert!(s.text == "a".repeat(5000) || s.text == "b".repeat(10));
    }

    #[test]
    fn what_is_set_aside_never_writes_over_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.ron");
        std::fs::write(&path, b"broken").unwrap();
        // Someone else's file at the first name, a link at the second.
        std::fs::write(dir.path().join("state.ron.broken-7"), b"theirs").unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), dir.path().join("state.ron.broken-7-1")).unwrap();
        let a = set_aside_stamped(&path, b"read\xff", 7).unwrap();
        assert_eq!(a, dir.path().join("state.ron.broken-7-2"));
        assert_eq!(std::fs::read(&a).unwrap(), b"read\xff");
        // Twice (two pengittos reading the same broken file): two copies.
        let b = set_aside_stamped(&path, b"read\xff", 7).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&b).unwrap(), b"read\xff");
        assert_eq!(std::fs::read(dir.path().join("state.ron.broken-7")).unwrap(), b"theirs");
        assert!(!dir.path().join("elsewhere").exists(), "not written through the link");
        assert_eq!(std::fs::read(&path).unwrap(), b"broken", "the file itself left as it is");
        // Not UTF-8: broken, with what was read.
        std::fs::write(&path, b"(n: 1, text: \"\xff\")").unwrap();
        match read::<S>(&path) {
            Loaded::Broken { bytes, .. } => assert_eq!(bytes, b"(n: 1, text: \"\xff\")"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nothing_is_saved_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.ron");
        std::fs::write(&path, b"kept").unwrap();
        let mut f = StateFile::new(path.clone());
        f.hold("can't be read".into());
        f.save(&S { n: 1, text: "new".into() });
        f.flush();
        assert_eq!(std::fs::read(&path).unwrap(), b"kept");
        assert_eq!(f.held(), Some("can't be read"));
    }

    #[test]
    fn an_older_save_finishing_later_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = StateFile::new(dir.path().join("state.ron"));
        let (go, hold) = std::sync::mpsc::channel();
        f.hold_next = Some(hold);
        f.save(&S { n: 1, text: "older".into() });
        f.save(&S { n: 2, text: "newer".into() });
        // The newer one is written while the older one waits…
        let start = std::time::Instant::now();
        while f.load::<S>().map(|s| s.n) != Some(2) {
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "the newer save never came");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // …which then writes nothing.
        go.send(()).unwrap();
        f.flush();
        assert_eq!(f.load::<S>().unwrap(), S { n: 2, text: "newer".into() });
    }

    #[test]
    fn the_old_file_is_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.ron");
        write_whole(&path, b"old contents").unwrap();
        // Whoever still reads the old file (or was cut off while it was
        // being written) sees it whole: a new file takes its name.
        let mut old = std::fs::File::open(&path).unwrap();
        write_whole(&path, b"new").unwrap();
        let mut seen = String::new();
        std::io::Read::read_to_string(&mut old, &mut seen).unwrap();
        assert_eq!(seen, "old contents");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn a_failed_write_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.ron");
        write_whole(&path, b"old").unwrap();
        // The rename can't happen: a folder where the file goes.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("x"), "").unwrap();
        assert!(write_whole(&blocked, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
        // Unreadable contents: nothing loaded (the app starts afresh).
        std::fs::write(&path, "(n: 1, text: \"cut").unwrap();
        assert_eq!(StateFile::new(path).load::<S>(), None);
    }
}
