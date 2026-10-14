//! Tine's own state (recent repositories, settings…) in a file of its own,
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
    eframe::storage_dir("tine").map(|d| d.join("state.ron"))
}

pub struct StateFile {
    path: PathBuf,
    /// The newest save written so far (an older one finishing later is
    /// dropped).
    written: Arc<Mutex<u64>>,
    saves: u64,
    pending: Vec<JoinHandle<()>>,
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
            #[cfg(test)]
            hold_next: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was saved, if the file is there and readable.
    pub fn load<T: DeserializeOwned>(&self) -> Option<T> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match ron::from_str(&text) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("{} can't be read: {e}", self.path.display());
                None
            }
        }
    }

    /// Saves `value` (in the background; the newest save wins).
    pub fn save<T: Serialize>(&mut self, value: &T) {
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
        let job = std::thread::Builder::new().name("tine-state".into()).spawn(move || {
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

/// Writes `bytes` to `path` so that `path` is either what it was or all of
/// `bytes`: a new file next to it (synced), renamed over it.
pub fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tmp = None;
    for _ in 0..16 {
        let candidate = dir.join(format!(".{name}.{}.tmp", tine_core::interactive::random_id()));
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
