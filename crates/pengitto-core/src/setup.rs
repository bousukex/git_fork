//! What a fresh installation needs, checked (Setup): git and who commits,
//! HTTPS credentials, SSH, a Japanese font, and where pengitto runs — with
//! what to do about each (on Fedora, its package names; atomic desktops
//! too). Every program is run within a time ([`crate::bounded`]); a check
//! that couldn't tell says so rather than guessing. Nothing asks for or
//! uses a credential; only [`use_libsecret`] writes anything.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::{
    bounded::{self, Failed},
    config::{Scoped, parse_scoped},
    version::{GitVersion, MIN_GIT},
};

/// Where the checks look (the process's own surroundings, or a test's).
#[derive(Debug, Clone)]
pub struct Env {
    /// git, the program.
    pub git: OsString,
    /// Set (or, `None`, removed) for every program run.
    pub vars: Vec<(OsString, Option<OsString>)>,
    pub home: PathBuf,
    /// The root that `/etc/os-release` and the like are looked for in.
    pub root: PathBuf,
    /// `container` (set by podman, systemd-nspawn…).
    pub container_var: Option<String>,
    /// For each program run.
    pub timeout: Duration,
}

impl Env {
    pub fn from_process(git: OsString) -> Env {
        Env {
            git,
            vars: Vec::new(),
            home: std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from),
            root: PathBuf::from("/"),
            container_var: std::env::var("container").ok().filter(|v| !v.is_empty()),
            timeout: Duration::from_secs(5),
        }
    }

    /// A variable as the programs run see it.
    fn var(&self, name: &str) -> Option<OsString> {
        match self.vars.iter().rfind(|(k, _)| k == name) {
            Some((_, v)) => v.clone(),
            None => std::env::var_os(name),
        }
    }

    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut c = Command::new(program);
        c.current_dir("/").env("LC_ALL", "C");
        for (k, v) in &self.vars {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c
    }

    fn git(&self, args: &[&str]) -> Command {
        with_args(self.command(&self.git), args)
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

/// What a check found, or why it couldn't tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probed<T> {
    Known(T),
    Unknown(String),
}

/// Who commits: the values commits use (any scope; outside a repository).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Who {
    pub name: Option<Scoped>,
    pub email: Option<Scoped>,
}

/// The credential helpers in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Helpers {
    /// For any host (`credential.helper`).
    pub general: Vec<Scoped>,
    /// For the URLs that have their own (`credential.<url>.helper`): what
    /// git uses there, where that isn't `general`.
    pub by_url: Vec<(String, Vec<Scoped>)>,
}

impl Helpers {
    pub fn none(&self) -> bool {
        self.general.is_empty() && self.by_url.iter().all(|(_, h)| h.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// SSH_AUTH_SOCK isn't set.
    None,
    /// The agent, holding this many keys.
    Keys(usize),
    /// It couldn't be reached.
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
    pub agent: Probed<Agent>,
    /// Key files in ~/.ssh (`id_*` with a `.pub`).
    pub keys: Vec<String>,
}

/// What `/etc/os-release` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
    pub variant_id: Option<String>,
    pub pretty_name: Option<String>,
}

impl OsRelease {
    pub fn is(&self, id: &str) -> bool {
        self.id == id || self.id_like.iter().any(|l| l == id)
    }

    fn name(&self) -> String {
        self.pretty_name.clone().unwrap_or_else(|| self.id.clone())
    }
}

/// Where pengitto runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    pub os: Option<OsRelease>,
    /// An image-based system (`/usr` read-only; packages layered with
    /// rpm-ostree): Fedora's atomic desktops.
    pub atomic: bool,
    /// Inside a container: which kind.
    pub container: Option<String>,
    /// The host, seen from a distrobox/toolbox container.
    pub host: Option<OsRelease>,
    pub host_atomic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `Known(None)`: there is no git to run.
    pub git: Probed<Option<GitVersion>>,
    pub who: Probed<Who>,
    pub helpers: Probed<Helpers>,
    /// git-credential-libsecret, where git finds it.
    pub libsecret: Option<PathBuf>,
    pub ssh: Ssh,
    /// The first Japanese font family fontconfig has (`Known(None)`: none).
    pub japanese_font: Probed<Option<String>>,
    pub system: System,
}

/// Checks everything (at the same time; each program within the time).
pub fn check(env: &Env) -> Report {
    std::thread::scope(|s| {
        let git = s.spawn(|| git_version(env));
        let who = s.spawn(|| read_who(env));
        let helpers = s.spawn(|| read_helpers(env));
        let libsecret = s.spawn(|| find_libsecret(env));
        let ssh = s.spawn(|| ssh(env));
        let font = s.spawn(|| japanese_font(env));
        let git = joined(git);
        Report {
            who: unless_no_git(&git, joined(who)),
            helpers: unless_no_git(&git, joined(helpers)),
            libsecret: joined(libsecret),
            ssh: joined(ssh),
            japanese_font: joined(font),
            system: system(env),
            git,
        }
    })
}

fn joined<T>(h: std::thread::ScopedJoinHandle<'_, T>) -> T {
    h.join().unwrap_or_else(|_| panic!("a check panicked"))
}

/// (Without git, what it would read can't be told.)
fn unless_no_git<T>(git: &Probed<Option<GitVersion>>, p: Probed<T>) -> Probed<T> {
    match git {
        Probed::Known(None) => Probed::Unknown("there is no git".into()),
        _ => p,
    }
}

fn git_version(env: &Env) -> Probed<Option<GitVersion>> {
    match bounded::run(env.git(&["--version"]), env.timeout) {
        Err(Failed::Start(_)) => Probed::Known(None),
        Err(Failed::Time(e)) => Probed::Unknown(e),
        Ok(r) if !r.status.success() => Probed::Unknown(format!("git --version failed ({})", r.status)),
        Ok(r) => crate::version::parse(&r.stdout_text())
            .map_or_else(|| Probed::Unknown(format!("can't read the version in {:?}", r.stdout_text().trim())), |v| Probed::Known(Some(v))),
    }
}

/// `git config -z --show-scope --show-origin <args>`, read whole (else it
/// can't be told); exit 1 (nothing set) is nothing. (Outside any
/// repository: the system's and the user's settings, as a repository
/// without its own sees them.)
fn config(env: &Env, args: &[&str]) -> Result<Vec<Scoped>, String> {
    let mut all = vec!["config", "-z", "--show-scope", "--show-origin"];
    all.extend(args);
    let r = bounded::run(env.git(&all), env.timeout).map_err(|e| e.to_string())?;
    if !r.complete {
        return Err("git config's output was cut short (or didn't end in time)".into());
    }
    match r.status.code() {
        Some(0) => parse_scoped(&r.stdout),
        Some(1) => Ok(Vec::new()),
        _ => Err(format!("git config failed ({}): {}", r.status, String::from_utf8_lossy(&r.stderr).trim())),
    }
}

fn read_who(env: &Env) -> Probed<Who> {
    // The last value is the one used: empty, it isn't set (git refuses to
    // commit with it).
    let last = |key: &str| config(env, &["--get-all", key]).map(|v| v.into_iter().last().filter(|s| !s.value.is_empty()));
    match (last("user.name"), last("user.email")) {
        (Ok(name), Ok(email)) => Probed::Known(Who { name, email }),
        (Err(e), _) | (_, Err(e)) => Probed::Unknown(e),
    }
}

/// The helpers git uses: an empty value clears those before it.
fn in_effect(values: impl IntoIterator<Item = Scoped>) -> Vec<Scoped> {
    let mut kept = Vec::new();
    for v in values {
        if v.value.is_empty() {
            kept.clear();
        } else {
            kept.push(v);
        }
    }
    kept
}

/// Whether a `credential.<pattern>.*` setting applies to `url` (git's URL
/// matching, as far as URLs with their own settings go: the same, or one
/// under it).
fn covers(pattern: &str, url: &str) -> bool {
    let (p, u) = (pattern.trim_end_matches('/'), url.trim_end_matches('/'));
    u == p || u.strip_prefix(p).is_some_and(|rest| rest.starts_with('/'))
}

pub fn read_helpers(env: &Env) -> Probed<Helpers> {
    // All of them, in the order git reads them — for any host
    // (`credential.helper`) or for a URL — each `key LF value`: git applies
    // those that match a URL in that order, an empty one clearing the
    // list so far (whichever it was set for).
    let entries = match config(env, &["--get-regexp", r"^credential\.(.+\.)?helper$"]) {
        Ok(e) => e,
        Err(e) => return Probed::Unknown(e),
    };
    let entries: Vec<(Option<String>, Scoped)> = entries
        .into_iter()
        .map(|s| {
            let (key, value) = s.value.split_once('\n').unwrap_or((&s.value, ""));
            let url = key.strip_prefix("credential.").and_then(|k| k.strip_suffix(".helper")).filter(|u| !u.is_empty()).map(str::to_owned);
            (url, Scoped { value: value.to_owned(), ..s.clone() })
        })
        .collect();
    let general = in_effect(entries.iter().filter(|(u, _)| u.is_none()).map(|(_, s)| s.clone()));
    let mut urls: Vec<&String> = Vec::new();
    for u in entries.iter().filter_map(|(u, _)| u.as_ref()) {
        if !urls.contains(&u) {
            urls.push(u);
        }
    }
    let by_url = urls
        .into_iter()
        .filter_map(|url| {
            let used = in_effect(entries.iter().filter(|(p, _)| p.as_ref().is_none_or(|p| covers(p, url))).map(|(_, s)| s.clone()));
            (used != general).then(|| (url.clone(), used))
        })
        .collect();
    Probed::Known(Helpers { general, by_url })
}

fn with_args(mut c: Command, args: &[&str]) -> Command {
    c.args(args);
    c
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Where git would find git-credential-libsecret: its exec path, then PATH.
fn find_libsecret(env: &Env) -> Option<PathBuf> {
    const NAME: &str = "git-credential-libsecret";
    let exec_path = bounded::run(env.git(&["--exec-path"]), env.timeout)
        .ok()
        .filter(|r| r.status.success() && r.complete)
        .map(|r| PathBuf::from(r.stdout_text().trim_end_matches('\n')));
    let path = env.var("PATH").unwrap_or_default();
    exec_path.into_iter().chain(std::env::split_paths(&path)).map(|d| d.join(NAME)).find(|p| executable(p))
}

fn ssh(env: &Env) -> Ssh {
    let agent = match env.var("SSH_AUTH_SOCK").filter(|s| !s.is_empty()) {
        None => Probed::Known(Agent::None),
        Some(_) => match bounded::run(with_args(env.command("ssh-add"), &["-l"]), env.timeout) {
            Ok(r) if !r.complete => Probed::Unknown("ssh-add's output was cut short".into()),
            // 1: the agent has no keys.
            Ok(r) if r.status.code() == Some(0) => Probed::Known(Agent::Keys(r.stdout_text().lines().filter(|l| !l.trim().is_empty()).count())),
            Ok(r) if r.status.code() == Some(1) => Probed::Known(Agent::Keys(0)),
            Ok(r) if r.status.code() == Some(2) => Probed::Known(Agent::Unreachable),
            Ok(r) => Probed::Unknown(format!("ssh-add -l failed ({})", r.status)),
            Err(e) => Probed::Unknown(e.to_string()),
        },
    };
    let mut keys: Vec<String> = std::fs::read_dir(env.home.join(".ssh"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("id_") && !n.ends_with(".pub"))
        .filter(|n| env.home.join(".ssh").join(format!("{n}.pub")).is_file())
        .collect();
    keys.sort();
    Ssh { agent, keys }
}

fn japanese_font(env: &Env) -> Probed<Option<String>> {
    match bounded::run(with_args(env.command("fc-list"), &["-f", "%{family[0]}\n", ":lang=ja"]), env.timeout) {
        Ok(r) if !r.complete => Probed::Unknown("fc-list's output was cut short (or didn't end in time)".into()),
        Ok(r) if r.status.success() => {
            let mut families: Vec<String> = r.stdout_text().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
            families.sort();
            Probed::Known(families.into_iter().next())
        }
        Ok(r) => Probed::Unknown(format!("fc-list failed ({})", r.status)),
        Err(Failed::Start(_)) => Probed::Unknown("fontconfig's fc-list isn't there".into()),
        Err(e) => Probed::Unknown(e.to_string()),
    }
}

/// Reads os-release (`KEY=value`, values maybe quoted).
pub fn parse_os_release(text: &str) -> OsRelease {
    let mut os = OsRelease::default();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))).unwrap_or(v);
        let v = v.replace("\\\"", "\"").replace("\\\\", "\\");
        match k {
            "ID" => os.id = v,
            "ID_LIKE" => os.id_like = v.split_whitespace().map(str::to_owned).collect(),
            "VARIANT_ID" => os.variant_id = Some(v),
            "PRETTY_NAME" => os.pretty_name = Some(v),
            _ => {}
        }
    }
    os
}

/// Fedora's atomic (image-based) variants.
const ATOMIC_VARIANTS: [&str; 7] = ["silverblue", "kinoite", "sericea", "onyx", "cosmic-atomic", "budgie-atomic", "xfce-atomic"];

fn atomic(os: Option<&OsRelease>, booted: &Path) -> bool {
    booted.exists() || os.is_some_and(|o| o.is("fedora") && o.variant_id.as_deref().is_some_and(|v| ATOMIC_VARIANTS.contains(&v)))
}

fn os_at(env: &Env, paths: &[&str]) -> Option<OsRelease> {
    paths.iter().find_map(|p| std::fs::read_to_string(env.at(p)).ok()).map(|t| parse_os_release(&t))
}

pub fn system(env: &Env) -> System {
    let os = os_at(env, &["/etc/os-release", "/usr/lib/os-release"]);
    let container = if env.at("/run/.toolboxenv").exists() {
        Some("toolbox".to_owned())
    } else if env.at("/run/.containerenv").exists() {
        Some(env.container_var.clone().unwrap_or_else(|| "podman".into()))
    } else if env.at("/.dockerenv").exists() {
        Some("docker".to_owned())
    } else {
        env.container_var.clone()
    };
    let host = container.as_ref().and_then(|_| os_at(env, &["/run/host/etc/os-release", "/run/host/usr/lib/os-release"]));
    System {
        atomic: container.is_none() && atomic(os.as_ref(), &env.at("/run/ostree-booted")),
        host_atomic: host.is_some() && atomic(host.as_ref(), &env.at("/run/host/run/ostree-booted")),
        os,
        container,
        host,
    }
}

// --- What it means -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Problem,
    /// It couldn't be told.
    Unknown,
    /// For information.
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Settings → Git (who commits).
    GitSettings,
    /// Use libsecret as the credential helper ([`use_libsecret`]).
    UseLibsecret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub what: &'static str,
    pub status: Status,
    pub text: String,
    /// What to do (commands to run, where).
    pub advice: Vec<String>,
    pub action: Option<Action>,
}

#[derive(Clone, Copy)]
enum Need {
    Git,
    Libsecret,
    JapaneseFont,
}

/// How to install what is missing, where pengitto runs.
fn how_to_install(system: &System, need: Need) -> Vec<String> {
    let os = system.os.as_ref();
    let (fedora, debian) = (os.is_some_and(|o| o.is("fedora")), os.is_some_and(|o| o.is("debian") || o.is("ubuntu")));
    let mut advice = Vec::new();
    if fedora {
        let package = match need {
            Need::Git => "git-core",
            Need::Libsecret => "git-credential-libsecret",
            Need::JapaneseFont => "google-noto-sans-cjk-fonts",
        };
        if system.atomic {
            advice.push(format!("rpm-ostree install {package} (then restart the computer)"));
            if matches!(need, Need::Git) {
                advice.push("Or run pengitto in a distrobox container, which has git (see the README).".into());
            }
        } else {
            advice.push(format!("sudo dnf install {package}"));
        }
    } else if debian {
        match need {
            Need::Git => advice.push("sudo apt install git".into()),
            Need::JapaneseFont => advice.push("sudo apt install fonts-noto-cjk".into()),
            Need::Libsecret => advice.push("Debian and Ubuntu don't package it built: see /usr/share/doc/git/contrib/credential/libsecret".into()),
        }
    } else {
        advice.push(
            match need {
                Need::Git => "Install git 2.43 or newer with the system's package manager.",
                Need::Libsecret => "Install git's libsecret credential helper (git-credential-libsecret).",
                Need::JapaneseFont => "Install a Japanese font (Noto Sans CJK, for one).",
            }
            .into(),
        );
    }
    if system.container.is_some() {
        advice.push("(Inside the container pengitto runs in, not on the host.)".into());
    }
    advice
}

fn from(s: &Scoped) -> String {
    format!("{} ({}, {})", s.value, s.scope, s.origin)
}

/// What the report says, item by item.
pub fn rows(r: &Report) -> Vec<Row> {
    let row = |what, status, text: String| Row { what, status, text, advice: Vec::new(), action: None };
    let mut rows = Vec::new();
    rows.push(match &r.git {
        Probed::Known(Some(v)) if *v >= MIN_GIT => row("Git", Status::Ok, format!("git {v}")),
        Probed::Known(Some(v)) => Row {
            advice: how_to_install(&r.system, Need::Git),
            ..row("Git", Status::Problem, format!("git {v} is older than {}.{}, which pengitto needs", MIN_GIT.0, MIN_GIT.1))
        },
        Probed::Known(None) => Row { advice: how_to_install(&r.system, Need::Git), ..row("Git", Status::Problem, "There is no git: pengitto needs it.".into()) },
        Probed::Unknown(e) => row("Git", Status::Unknown, format!("Couldn't tell: {e}")),
    });
    rows.push(match &r.who {
        Probed::Known(Who { name: Some(n), email: Some(e) }) => row("Who commits", Status::Ok, format!("{} <{}> ({}, {})", n.value, e.value, n.scope, n.origin)),
        Probed::Known(w) => {
            let missing: Vec<&str> = [("user.name", &w.name), ("user.email", &w.email)].into_iter().filter(|(_, v)| v.is_none()).map(|(k, _)| k).collect();
            Row {
                action: Some(Action::GitSettings),
                ..row(
                    "Who commits",
                    Status::Problem,
                    format!("{} {} set: commits can't be made.", missing.join(" and "), if missing.len() > 1 { "aren't" } else { "isn't" }),
                )
            }
        }
        Probed::Unknown(e) => row("Who commits", Status::Unknown, format!("Couldn't tell: {e}")),
    });
    let list = |h: &[Scoped]| if h.is_empty() { "none".to_owned() } else { h.iter().map(|s| s.value.clone()).collect::<Vec<_>>().join(", then ") };
    let urls = |h: &Helpers| h.by_url.iter().map(|(u, s)| format!("{u}: {}", list(s))).collect::<Vec<_>>().join("; ");
    rows.push(match &r.helpers {
        Probed::Known(h) if !h.general.is_empty() => {
            let kept = format!("Kept by {}", h.general.iter().map(from).collect::<Vec<_>>().join(", then "));
            let text = if h.by_url.is_empty() { kept } else { format!("{kept}; for {}", urls(h)) };
            row("HTTPS credentials", Status::Ok, text)
        }
        Probed::Known(h) if !h.none() => row("HTTPS credentials", Status::Info, format!("Kept only for {}; other hosts ask each time.", urls(h))),
        Probed::Known(_) => Row {
            advice: if r.libsecret.is_some() { Vec::new() } else { how_to_install(&r.system, Need::Libsecret) },
            action: Some(Action::UseLibsecret),
            ..row(
                "HTTPS credentials",
                Status::Problem,
                "No credential helper: HTTPS passwords and tokens are asked for every time, and not kept.".into(),
            )
        },
        Probed::Unknown(e) => row("HTTPS credentials", Status::Unknown, format!("Couldn't tell: {e}")),
    });
    let agent = match &r.ssh.agent {
        Probed::Known(Agent::None) => "no ssh-agent (SSH_AUTH_SOCK isn't set)".to_owned(),
        Probed::Known(Agent::Keys(0)) => "ssh-agent, with no keys".to_owned(),
        Probed::Known(Agent::Keys(n)) => format!("ssh-agent, with {n} key{}", if *n == 1 { "" } else { "s" }),
        Probed::Known(Agent::Unreachable) => "ssh-agent can't be reached".to_owned(),
        Probed::Unknown(e) => format!("ssh-agent: couldn't tell ({e})"),
    };
    let keys = if r.ssh.keys.is_empty() { "no keys in ~/.ssh".to_owned() } else { format!("keys in ~/.ssh: {}", r.ssh.keys.join(", ")) };
    rows.push(row("SSH", Status::Info, format!("{agent}; {keys}")));
    rows.push(match &r.japanese_font {
        Probed::Known(Some(f)) => row("Japanese font", Status::Ok, f.clone()),
        Probed::Known(None) => Row {
            advice: how_to_install(&r.system, Need::JapaneseFont),
            ..row("Japanese font", Status::Problem, "None: Japanese file names and messages show as boxes.".into())
        },
        Probed::Unknown(e) => row("Japanese font", Status::Unknown, format!("Couldn't tell: {e}")),
    });
    let s = &r.system;
    let os = s.os.as_ref().map_or_else(|| "an unknown system".to_owned(), OsRelease::name);
    let text = match (&s.container, &s.host) {
        (None, _) => format!("{os}{}", if s.atomic { " (atomic: packages are layered with rpm-ostree)" } else { "" }),
        (Some(c), host) => format!(
            "{os}, in a container ({c}){}",
            host.as_ref().map(|h| format!("; the host: {}{}", h.name(), if s.host_atomic { " (atomic)" } else { "" })).unwrap_or_default()
        ),
    };
    rows.push(row("System", Status::Info, text));
    rows
}

/// What needs looking at (problems, and what couldn't be told).
pub fn problems(r: &Report) -> Vec<Row> {
    rows(r).into_iter().filter(|row| matches!(row.status, Status::Problem | Status::Unknown)).collect()
}

// --- Using libsecret ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperWrite {
    /// They aren't what was shown any more: nothing written (look again).
    Changed(Helpers),
    /// Written, and in effect.
    Done(Helpers),
    /// Not done (what they are now, if they could be read).
    Failed(String, Option<Helpers>),
}

/// Makes libsecret the global credential helper (`--add`: nothing else
/// replaced) — if the helpers are still as `shown` (none, as the button
/// is offered); read again after.
pub fn use_libsecret(env: &Env, shown: &Helpers) -> HelperWrite {
    let _one_at_a_time = crate::config::writing();
    match read_helpers(env) {
        Probed::Known(now) if now != *shown => return HelperWrite::Changed(now),
        Probed::Known(_) => {}
        Probed::Unknown(e) => return HelperWrite::Failed(e, None),
    }
    let written = bounded::run(env.git(&["config", "--global", "--add", "credential.helper", "libsecret"]), env.timeout);
    let now = match read_helpers(env) {
        Probed::Known(h) => Some(h),
        Probed::Unknown(_) => None,
    };
    match written {
        Err(e) => HelperWrite::Failed(e.to_string(), now),
        Ok(r) if !r.status.success() => HelperWrite::Failed(String::from_utf8_lossy(&r.stderr).trim().to_owned(), now),
        Ok(_) => match now {
            Some(h) if h.general.iter().any(|s| s.value == "libsecret") => HelperWrite::Done(h),
            Some(h) => HelperWrite::Failed("it was written, but isn't in effect (look at the configuration)".into(), Some(h)),
            None => HelperWrite::Failed("it was written, but couldn't be read back".into(), None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_as_systems_write_it() {
        let os = parse_os_release("NAME=\"Fedora Linux\"\nID=fedora\nVARIANT_ID=kinoite\nPRETTY_NAME=\"Fedora Linux 44 (KDE Plasma Desktop Edition)\"\n# a comment\n");
        assert_eq!((os.id.as_str(), os.variant_id.as_deref()), ("fedora", Some("kinoite")));
        assert_eq!(os.pretty_name.as_deref(), Some("Fedora Linux 44 (KDE Plasma Desktop Edition)"));
        assert!(atomic(Some(&os), Path::new("/nonexistent")));
        let ws = parse_os_release("ID=fedora\nVARIANT_ID=workstation\n");
        assert!(!atomic(Some(&ws), Path::new("/nonexistent")));
        let ubuntu = parse_os_release("ID=ubuntu\nID_LIKE=debian\nPRETTY_NAME='Ubuntu 24.04.3 LTS'\n");
        assert!(ubuntu.is("debian") && !ubuntu.is("fedora") && ubuntu.name() == "Ubuntu 24.04.3 LTS");
        let quoted = parse_os_release("PRETTY_NAME=\"a \\\"b\\\" c\"\nID=x\n");
        assert_eq!(quoted.pretty_name.as_deref(), Some("a \"b\" c"));
    }

    #[test]
    fn a_url_and_those_under_it() {
        assert!(covers("https://github.com", "https://github.com"));
        assert!(covers("https://github.com/", "https://github.com/org"));
        assert!(covers("https://github.com", "https://github.com/org/repo"));
        assert!(!covers("https://github.com", "https://github.community"));
        assert!(!covers("https://github.com/org", "https://github.com"));
    }

    #[test]
    fn an_empty_helper_clears_those_before_it() {
        let s = |v: &str| Scoped { scope: "global".into(), origin: "f".into(), value: v.into() };
        let values = |v: Vec<Scoped>| v.into_iter().map(|s| s.value).collect::<Vec<_>>();
        assert_eq!(values(in_effect([s("cache"), s(""), s("store"), s("x")])), ["store", "x"]);
        assert!(in_effect([s("cache"), s("")]).is_empty());
    }

    #[test]
    fn what_to_do_where() {
        let sys = |text: &str, container: Option<&str>| System {
            os: Some(parse_os_release(text)),
            atomic: atomic(Some(&parse_os_release(text)), Path::new("/nonexistent")) && container.is_none(),
            container: container.map(str::to_owned),
            ..Default::default()
        };
        let fedora = sys("ID=fedora\nVARIANT_ID=kde\n", None);
        assert_eq!(how_to_install(&fedora, Need::Git), ["sudo dnf install git-core"]);
        assert_eq!(how_to_install(&fedora, Need::Libsecret), ["sudo dnf install git-credential-libsecret"]);
        assert_eq!(how_to_install(&fedora, Need::JapaneseFont), ["sudo dnf install google-noto-sans-cjk-fonts"]);
        let kinoite = sys("ID=fedora\nVARIANT_ID=kinoite\n", None);
        let git = how_to_install(&kinoite, Need::Git);
        assert!(git[0].starts_with("rpm-ostree install git-core") && git[1].contains("distrobox"), "{git:?}");
        let boxed = sys("ID=fedora\nVARIANT_ID=container\n", Some("podman"));
        assert_eq!(how_to_install(&boxed, Need::JapaneseFont)[0], "sudo dnf install google-noto-sans-cjk-fonts");
        assert!(how_to_install(&boxed, Need::JapaneseFont)[1].contains("Inside the container"));
        assert_eq!(how_to_install(&sys("ID=ubuntu\nID_LIKE=debian\n", None), Need::Git), ["sudo apt install git"]);
        assert!(how_to_install(&sys("ID=arch\n", None), Need::Git)[0].contains("package manager"));
    }
}
