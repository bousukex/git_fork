//! External programs: a terminal or the file manager at the repository, a
//! file in its default application, a file shown in its folder.
//!
//! Each runs on a thread of its own that waits for it (so it is reaped);
//! how it ended comes back through [`take_results`], tagged with the work
//! tree it was started for — the app hands it to that tab, or shows it as a
//! notice if the tab was closed meanwhile.

use std::{
    ffi::{OsStr, OsString},
    io::Read as _,
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// What to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// A terminal in the work tree.
    Terminal,
    /// The work tree in the file manager.
    FileManager,
    /// A file of the work tree (relative path) in its default application.
    OpenFile(PathBuf),
    /// A file of the work tree, selected in the file manager.
    ShowFile(PathBuf),
}

impl Tool {
    pub fn label(&self) -> &'static str {
        match self {
            Tool::Terminal => "Open in Terminal",
            Tool::FileManager => "Open in File Manager",
            Tool::OpenFile(_) => "Open File",
            Tool::ShowFile(_) => "Show in File Manager",
        }
    }
}

/// Saved with the app state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    /// The terminal: program and arguments, `{dir}` standing for the work
    /// tree. Empty: found automatically ([`detect_terminal`]).
    pub terminal: Vec<String>,
}

const KEY: &str = "pengitto-tool-settings";

/// The settings in effect (set by the app each frame).
pub fn current(ctx: &egui::Context) -> ToolSettings {
    ctx.data(|d| d.get_temp(egui::Id::new(KEY))).unwrap_or_default()
}

pub fn set_current(ctx: &egui::Context, settings: ToolSettings) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(KEY), settings));
}

/// One program to run.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The action, for messages ("Open in Terminal").
    pub label: String,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub dir: PathBuf,
    /// For programs that stay (terminals): a non-zero exit within this long
    /// counts as a failure, a later one (closed after the shell's last
    /// command failed) is just noted. `None`: any non-zero exit fails
    /// (openers, which only hand the work over).
    pub startup: Option<Duration>,
    /// Tried when this one fails (e.g. opening the folder when the file
    /// manager can't select the file).
    pub fallback: Option<Box<Launch>>,
}

/// How a launch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The work tree it was started for.
    pub origin: PathBuf,
    /// `Ok(Some(note))`: ended in a way worth a line in the activity log.
    pub outcome: Result<Option<String>, String>,
}

static RESULTS: Mutex<Vec<ToolResult>> = Mutex::new(Vec::new());

/// Results that arrived since the last call.
pub fn take_results() -> Vec<ToolResult> {
    std::mem::take(&mut *RESULTS.lock().unwrap_or_else(|e| e.into_inner()))
}

fn report(result: ToolResult, ctx: &egui::Context) {
    RESULTS.lock().unwrap_or_else(|e| e.into_inner()).push(result);
    ctx.request_repaint();
}

/// Runs `tool` for the work tree `workdir` in the background.
pub fn run(ctx: &egui::Context, tool: &Tool, workdir: &Path) {
    match plan(tool, workdir, &current(ctx), &System) {
        Ok(launch) => start(launch, workdir.to_path_buf(), ctx.clone()),
        Err(e) => report(ToolResult { origin: workdir.to_path_buf(), outcome: Err(e) }, ctx),
    }
}

/// Starts `launch` on its own thread; the result is reported for `origin`.
pub fn start(launch: Launch, origin: PathBuf, ctx: egui::Context) {
    let spawned = std::thread::Builder::new().name("pengitto-tool".into()).spawn({
        let (launch, origin, ctx) = (launch.clone(), origin.clone(), ctx.clone());
        move || report(ToolResult { origin, outcome: execute(&launch) }, &ctx)
    });
    if let Err(e) = spawned {
        report(ToolResult { origin, outcome: Err(format!("{} failed: no thread to run it: {e}", launch.label)) }, &ctx);
    }
}

fn execute(l: &Launch) -> Result<Option<String>, String> {
    match execute_one(l) {
        Err(e) => match &l.fallback {
            Some(f) => execute(f).map_err(|fe| format!("{e}; {fe}")),
            None => Err(e),
        },
        ok => ok,
    }
}

fn execute_one(l: &Launch) -> Result<Option<String>, String> {
    let name = Path::new(&l.program).file_name().unwrap_or(&l.program).to_string_lossy().into_owned();
    let started = Instant::now();
    let mut child = Command::new(&l.program)
        .args(&l.args)
        .current_dir(&l.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Its own process group: a Ctrl+C for pengitto's terminal (or pengitto
        // quitting) leaves it alone.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{} failed: couldn't start {name} ({e})", l.label))?;
    // What it says on stderr, for the message. Read on another thread: a
    // program it starts may keep the pipe open long after it exits.
    let said = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut err) = child.stderr.take() {
        let said = said.clone();
        let _ = std::thread::Builder::new().name("pengitto-tool-stderr".into()).spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = err.read(&mut buf) {
                let mut s = said.lock().unwrap_or_else(|e| e.into_inner());
                if s.len() < 8192 {
                    s.extend_from_slice(&buf[..n]);
                }
            }
        });
    }
    let status = child.wait().map_err(|e| format!("{} failed: {name}: {e}", l.label))?;
    if status.success() {
        return Ok(None);
    }
    // Let the last words arrive.
    std::thread::sleep(Duration::from_millis(50));
    let how = match status.code() {
        Some(c) => format!("{name} exited with code {c}"),
        None => format!("{name} was stopped by a signal"),
    };
    if l.startup.is_some_and(|s| started.elapsed() > s) {
        return Ok(Some(format!("{}: {how}", l.label)));
    }
    let said = String::from_utf8_lossy(&said.lock().unwrap_or_else(|e| e.into_inner())).trim().to_owned();
    Err(if said.is_empty() { format!("{} failed: {how}", l.label) } else { format!("{} failed: {how}: {said}", l.label) })
}

/// A container's way to run host programs: the command, and whether the
/// host has a program.
pub type Host<'a> = (String, Box<dyn Fn(&str) -> bool + 'a>);

/// What the system offers (tests use their own).
pub trait Environment {
    fn desktop(&self) -> String;
    /// `$TERMINAL`.
    fn terminal_var(&self) -> Option<String>;
    /// `program` is on PATH.
    fn has(&self, program: &str) -> bool;
    /// Inside a container that can run host programs (distrobox, toolbox):
    /// the command that does it, and whether the host has `program`.
    fn host(&self) -> Option<Host<'_>>;
}

/// The real system.
pub struct System;

fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path)
        .any(|d| std::fs::metadata(d.join(program)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

impl Environment for System {
    fn desktop(&self) -> String {
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
    }

    fn terminal_var(&self) -> Option<String> {
        std::env::var("TERMINAL").ok().filter(|t| !t.trim().is_empty())
    }

    fn has(&self, program: &str) -> bool {
        on_path(program)
    }

    fn host(&self) -> Option<Host<'_>> {
        let in_container = Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists();
        let exec = ["distrobox-host-exec", "host-spawn"].into_iter().find(|p| on_path(p))?;
        in_container.then(|| {
            let has: Box<dyn Fn(&str) -> bool> =
                Box::new(|p: &str| ["/run/host/usr/bin", "/run/host/bin"].iter().any(|d| Path::new(d).join(p).exists()));
            (exec.to_owned(), has)
        })
    }
}

/// Terminals pengitto knows how to start in a folder.
const TERMINALS: &[(&str, &[&str])] = &[
    ("konsole", &["--workdir", "{dir}"]),
    ("ptyxis", &["--new-window", "--working-directory={dir}"]),
    ("kgx", &["--working-directory={dir}"]),
    ("gnome-terminal", &["--working-directory={dir}"]),
    ("xfce4-terminal", &["--working-directory={dir}"]),
    ("alacritty", &["--working-directory", "{dir}"]),
    ("kitty", &["--directory", "{dir}"]),
    ("foot", &["--working-directory={dir}"]),
    ("wezterm", &["start", "--cwd", "{dir}"]),
    ("xterm", &[]),
];

/// A terminal command for this desktop (`{dir}`: the folder), or `None`.
/// The desktop's own terminal first, then `$TERMINAL`, then the known
/// ones; inside a distrobox/toolbox container, the host's too (through its
/// host-exec command).
pub fn detect_terminal(env: &dyn Environment) -> Option<Vec<String>> {
    let desktop = env.desktop().to_ascii_uppercase();
    let preferred: &[&str] = if desktop.contains("KDE") {
        &["konsole"]
    } else if desktop.contains("GNOME") {
        &["ptyxis", "kgx", "gnome-terminal"]
    } else if desktop.contains("XFCE") {
        &["xfce4-terminal"]
    } else {
        &[]
    };
    let known = |name: &str| -> Vec<String> {
        let args = TERMINALS.iter().find(|(n, _)| *n == name).map_or(&[][..], |(_, a)| a);
        std::iter::once(name).chain(args.iter().copied()).map(str::to_owned).collect()
    };
    let mut candidates: Vec<Vec<String>> = preferred.iter().map(|n| known(n)).collect();
    if let Some(t) = env.terminal_var() {
        // Known: started in the folder by its option; else by the working
        // directory alone.
        candidates.push(if TERMINALS.iter().any(|(n, _)| *n == t) { known(&t) } else { vec![t] });
    }
    candidates.extend(TERMINALS.iter().map(|(n, _)| known(n)));
    let host = env.host();
    candidates.into_iter().find_map(|c| {
        if env.has(&c[0]) {
            return Some(c);
        }
        let (exec, has) = host.as_ref()?;
        has(&c[0]).then(|| std::iter::once(exec.clone()).chain(c).collect())
    })
}

/// `arg` with every `{dir}` replaced by `dir` (as bytes: any path works).
fn with_dir(arg: &str, dir: &Path) -> OsString {
    let mut out = Vec::new();
    let mut parts = arg.split("{dir}");
    out.extend_from_slice(parts.next().unwrap_or_default().as_bytes());
    for p in parts {
        out.extend_from_slice(dir.as_os_str().as_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    OsString::from_vec(out)
}

/// `file://` URI of an absolute path.
pub fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// How to run `tool` for the work tree `workdir`.
pub fn plan(tool: &Tool, workdir: &Path, settings: &ToolSettings, env: &dyn Environment) -> Result<Launch, String> {
    let launch = |program: &OsStr, args: Vec<OsString>| Launch {
        label: tool.label().to_owned(),
        program: program.to_owned(),
        args,
        dir: workdir.to_path_buf(),
        startup: (*tool == Tool::Terminal).then_some(Duration::from_secs(10)),
        fallback: None,
    };
    let open = |target: &Path| launch(OsStr::new("xdg-open"), vec![target.as_os_str().to_owned()]);
    match tool {
        Tool::Terminal => {
            let words = if settings.terminal.is_empty() {
                detect_terminal(env)
                    .ok_or_else(|| "Open in Terminal failed: no terminal found; choose one in Settings → Tools".to_owned())?
            } else {
                settings.terminal.clone()
            };
            let (program, args) = words.split_first().ok_or("Open in Terminal failed: no terminal set")?;
            Ok(launch(OsStr::new(program), args.iter().map(|a| with_dir(a, workdir)).collect()))
        }
        Tool::FileManager => Ok(open(workdir)),
        Tool::OpenFile(rel) => Ok(open(&workdir.join(rel))),
        Tool::ShowFile(rel) => {
            let file = workdir.join(rel);
            let mut show = launch(
                OsStr::new("dbus-send"),
                [
                    "--session",
                    "--print-reply",
                    // Don't wait the default 25 s for a file manager that
                    // doesn't answer: open the folder instead.
                    "--reply-timeout=5000",
                    "--dest=org.freedesktop.FileManager1",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                ]
                .iter()
                .map(OsString::from)
                .chain([OsString::from(format!("array:string:{}", file_uri(&file))), OsString::from("string:")])
                .collect(),
            );
            // Without a file manager that selects files: open the folder.
            show.fallback = Some(Box::new(open(file.parent().unwrap_or(workdir))));
            Ok(show)
        }
    }
}

/// Splits a command line into words like a shell would for plain quoting:
/// blanks separate words, '…' and "…" keep blanks, a backslash escapes the
/// next character (not inside '…').
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => w.push(c),
                        None => return Err("a ' quote isn't closed".into()),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => w.push(c),
                            Some(c) => {
                                w.push('\\');
                                w.push(c);
                            }
                            None => return Err("a \" quote isn't closed".into()),
                        },
                        Some(c) => w.push(c),
                        None => return Err("a \" quote isn't closed".into()),
                    }
                }
            }
            '\\' => match chars.next() {
                Some(c) => word.get_or_insert_with(String::new).push(c),
                None => return Err("a line can't end with \\".into()),
            },
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// The words as a command line [`split_words`] reads back the same.
pub fn join_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            // Quoted unless every character is plain: in particular any
            // blank `split_words` would split at (also U+3000, U+00A0…).
            let plain = |c: char| !c.is_whitespace() && (c.is_ascii_alphanumeric() || "-_./=:{},@+%".contains(c) || !c.is_ascii());
            if !w.is_empty() && w.chars().all(plain) {
                w.clone()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        desktop: &'static str,
        terminal: Option<&'static str>,
        local: &'static [&'static str],
        host: Option<&'static [&'static str]>,
    }

    impl Environment for Fake {
        fn desktop(&self) -> String {
            self.desktop.into()
        }
        fn terminal_var(&self) -> Option<String> {
            self.terminal.map(Into::into)
        }
        fn has(&self, program: &str) -> bool {
            self.local.contains(&program)
        }
        fn host(&self) -> Option<Host<'_>> {
            let host = self.host?;
            Some(("distrobox-host-exec".into(), Box::new(move |p: &str| host.contains(&p))))
        }
    }

    fn detect(f: Fake) -> Option<String> {
        detect_terminal(&f).map(|w| w.join(" "))
    }

    #[test]
    fn finds_the_desktops_terminal_also_on_the_host() {
        let kde = |local, host| Fake { desktop: "KDE", terminal: None, local, host };
        assert_eq!(detect(kde(&["xterm", "konsole"], None)).as_deref(), Some("konsole --workdir {dir}"));
        assert_eq!(detect(kde(&["xterm"], None)).as_deref(), Some("xterm"), "any known one");
        // In a distrobox: the host's konsole beats the container's xterm.
        assert_eq!(
            detect(kde(&["xterm"], Some(&["konsole"]))).as_deref(),
            Some("distrobox-host-exec konsole --workdir {dir}")
        );
        let gnome = Fake { desktop: "ubuntu:GNOME", terminal: None, local: &["gnome-terminal", "kgx"], host: None };
        assert_eq!(detect(gnome).as_deref(), Some("kgx --working-directory={dir}"));
        let var = Fake { desktop: "sway", terminal: Some("myterm"), local: &["myterm", "foot"], host: None };
        assert_eq!(detect(var).as_deref(), Some("myterm"), "$TERMINAL, started in the folder");
        assert_eq!(detect(Fake { desktop: "", terminal: None, local: &[], host: Some(&[]) }), None);
    }

    #[test]
    fn plans_with_the_folder_in_place() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = Path::new(OsStr::from_bytes(b"/tmp/my repo/\xff"));
        let settings = ToolSettings { terminal: vec!["term".into(), "--cwd={dir}".into(), "-e".into(), "a b".into()] };
        let none = Fake { desktop: "", terminal: None, local: &[], host: None };
        let l = plan(&Tool::Terminal, dir, &settings, &none).unwrap();
        assert_eq!(l.program, "term");
        let mut cwd = b"--cwd=".to_vec();
        cwd.extend_from_slice(dir.as_os_str().as_bytes());
        assert_eq!(l.args, [OsString::from_vec(cwd), "-e".into(), "a b".into()]);
        assert_eq!(l.dir, dir);
        assert!(plan(&Tool::Terminal, dir, &ToolSettings::default(), &none).unwrap_err().contains("Settings"));
        let show = plan(&Tool::ShowFile("a b/c,d.txt".into()), Path::new("/w"), &settings, &none).unwrap();
        assert_eq!(show.args[6], "array:string:file:///w/a%20b/c%2Cd.txt", "no comma for dbus-send to split at");
        // Only a terminal may end late without that being a failure.
        assert!(l.startup.is_some() && show.startup.is_none() && show.fallback.as_ref().unwrap().startup.is_none());
        let fallback = show.fallback.unwrap();
        assert_eq!((fallback.program.as_os_str(), fallback.args.as_slice()), (OsStr::new("xdg-open"), &[OsString::from("/w/a b")][..]));
    }

    #[test]
    fn command_lines_split_and_join() {
        assert_eq!(split_words(r#"konsole --workdir {dir}"#).unwrap(), ["konsole", "--workdir", "{dir}"]);
        assert_eq!(split_words(r#"a 'b c' "d \"e\"" f\ g ''"#).unwrap(), ["a", "b c", "d \"e\"", "f g", ""]);
        assert!(split_words("a 'b").is_err() && split_words("a \"b").is_err() && split_words("a\\").is_err());
        for words in [
            vec!["konsole", "--workdir", "{dir}"],
            vec!["a b", "it's", "", "日本語", "x=\"y\""],
            vec!["全角\u{3000}空白", "no\u{a0}break", "tab\there", "\\back"],
        ] {
            let words: Vec<String> = words.into_iter().map(Into::into).collect();
            assert_eq!(split_words(&join_words(&words)).unwrap(), words, "{}", join_words(&words));
        }
        assert_eq!(join_words(&["konsole".into(), "--workdir".into(), "{dir}".into()]), "konsole --workdir {dir}");
        // Any word, from characters that matter here, comes back the same.
        let alphabet: Vec<char> = "a-{}' \"\\\t\u{3000}\u{a0}日=".chars().collect();
        let mut seed = 7u64;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        for _ in 0..2000 {
            let words: Vec<String> = (0..1 + next(4)).map(|_| (0..next(6)).map(|_| alphabet[next(alphabet.len())]).collect()).collect();
            assert_eq!(split_words(&join_words(&words)).unwrap(), words, "{:?} → {}", words, join_words(&words));
        }
    }
}
