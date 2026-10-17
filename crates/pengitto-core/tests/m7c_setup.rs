//! The Setup check (M7c) with real git and its configuration in a scratch
//! place, stand-ins for the other programs (fontconfig, ssh-add), and a
//! scratch root for what the system says of itself.

mod common;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::write_executable;
use pengitto_core::setup::{self, Action, Agent, Env, HelperWrite, Probed, Status};

struct Place {
    dir: tempfile::TempDir,
}

impl Place {
    fn new() -> Place {
        let p = Place { dir: tempfile::tempdir().unwrap() };
        for d in ["home", "bin", "root/etc", "exec"] {
            std::fs::create_dir_all(p.path(d)).unwrap();
        }
        p
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn global(&self) -> PathBuf {
        self.path("gitconfig")
    }

    fn write(&self, p: &str, text: &str) {
        std::fs::create_dir_all(self.path(p).parent().unwrap()).unwrap();
        std::fs::write(self.path(p), text).unwrap();
    }

    /// A program the checks find first on PATH.
    fn program(&self, name: &str, body: &str) {
        write_executable(&self.path("bin").join(name), &format!("#!/bin/sh\n{body}\n"));
    }

    fn env(&self) -> Env {
        let path = format!("{}:{}", self.path("bin").display(), std::env::var("PATH").unwrap());
        let var = |k: &str, v: OsString| (OsString::from(k), Some(v));
        Env {
            git: "git".into(),
            vars: vec![
                var("GIT_CONFIG_GLOBAL", self.global().into()),
                var("GIT_CONFIG_SYSTEM", self.path("system").into()),
                (OsString::from("XDG_CONFIG_HOME"), Some(self.path("home/.config").into())),
                var("HOME", self.path("home").into()),
                var("PATH", path.into()),
                var("GIT_EXEC_PATH", self.path("exec").into()),
                (OsString::from("SSH_AUTH_SOCK"), None),
            ],
            home: self.path("home"),
            root: self.path("root"),
            container_var: None,
            timeout: Duration::from_secs(5),
        }
    }
}

fn row(r: &setup::Report, what: &str) -> setup::Row {
    setup::rows(r).into_iter().find(|row| row.what == what).unwrap_or_else(|| panic!("no row {what}"))
}

/// The helpers in effect: for any host (`scope:value`), then where a URL
/// has others (`url=value,…`).
fn helpers(r: &setup::Report) -> Vec<String> {
    match &r.helpers {
        Probed::Known(h) => h
            .general
            .iter()
            .map(|s| format!("{}:{}", s.scope, s.value))
            .chain(h.by_url.iter().map(|(u, s)| format!("{u}={}", s.iter().map(|s| s.value.as_str()).collect::<Vec<_>>().join(","))))
            .collect(),
        Probed::Unknown(e) => panic!("{e}"),
    }
}

#[test]
fn a_fresh_home_has_its_problems_told() {
    let p = Place::new();
    p.program("fc-list", "exit 0");
    let r = setup::check(&p.env());
    assert!(matches!(r.git, Probed::Known(Some(_))));
    assert_eq!(row(&r, "Git").status, Status::Ok);
    let who = row(&r, "Who commits");
    assert_eq!((who.status, who.action), (Status::Problem, Some(Action::GitSettings)));
    assert!(who.text.starts_with("user.name and user.email aren't set"), "{}", who.text);
    let https = row(&r, "HTTPS credentials");
    assert_eq!((https.status, https.action), (Status::Problem, Some(Action::UseLibsecret)));
    assert_eq!(r.libsecret, None);
    // No system told: generic advice.
    assert!(https.advice[0].contains("git-credential-libsecret"), "{:?}", https.advice);
    assert_eq!(row(&r, "Japanese font").status, Status::Problem);
    let ssh = row(&r, "SSH");
    assert_eq!(ssh.status, Status::Info);
    assert!(ssh.text.contains("no ssh-agent") && ssh.text.contains("no keys"), "{}", ssh.text);
    let problems: Vec<&str> = setup::problems(&r).iter().map(|r| r.what).collect();
    assert_eq!(problems, ["Who commits", "HTTPS credentials", "Japanese font"]);
}

#[test]
fn what_is_set_is_read_as_git_reads_it() {
    let p = Place::new();
    p.program("fc-list", "printf 'Noto Sans CJK JP\\nIPAGothic\\n'");
    std::fs::write(p.global(), "[user]\n\tname = Taro\n[include]\n\tpath = more\n[credential]\n\thelper = cache\n\thelper =\n\thelper = store\n").unwrap();
    p.write("more", "[user]\n\temail = taro@example.com\n[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n");
    p.write("system", "[credential]\n\thelper = osxkeychain\n");
    let r = setup::check(&p.env());
    let who = row(&r, "Who commits");
    assert_eq!(who.status, Status::Ok);
    // One missing: that one.
    std::fs::write(p.path("more"), "").unwrap();
    let only_name = row(&setup::check(&p.env()), "Who commits");
    assert!(only_name.text.starts_with("user.email isn't set"), "{}", only_name.text);
    p.write("more", "[user]\n\temail = taro@example.com\n[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n");
    assert!(who.text.starts_with("Taro <taro@example.com> (global, "), "{}", who.text);
    // The system's helper cleared by the empty one — and so is the one for
    // a URL (read before it, in the include): what git uses there too is
    // what follows.
    assert_eq!(helpers(&r), ["global:store"]);
    assert_eq!(row(&r, "HTTPS credentials").status, Status::Ok);
    assert_eq!(row(&r, "Japanese font").text, "IPAGothic");
    // Only for a URL: told, nothing offered.
    std::fs::write(p.global(), "[user]\n\tname = T\n\temail = t@x\n[include]\n\tpath = more\n").unwrap();
    let r = setup::check(&p.env());
    assert_eq!(helpers(&r), ["system:osxkeychain", "https://github.com=osxkeychain,!gh auth git-credential"]);
    std::fs::remove_file(p.path("system")).unwrap();
    let r = setup::check(&p.env());
    let https = row(&r, "HTTPS credentials");
    assert_eq!((https.status, https.action), (Status::Info, None));
    assert!(https.text.contains("only for https://github.com"), "{}", https.text);
}

#[test]
fn libsecret_where_git_finds_it_and_used_only_as_shown() {
    let p = Place::new();
    write_executable(&p.path("exec/git-credential-libsecret"), "#!/bin/sh\n");
    let env = p.env();
    let r = setup::check(&env);
    assert_eq!(r.libsecret.as_deref(), Some(p.path("exec/git-credential-libsecret").as_path()));
    assert!(row(&r, "HTTPS credentials").advice.is_empty(), "nothing to install");
    let Probed::Known(shown) = r.helpers.clone() else { panic!() };
    // Set elsewhere meanwhile: nothing written, what is there now told.
    std::fs::write(p.global(), "[credential]\n\thelper = store\n").unwrap();
    let HelperWrite::Changed(now) = setup::use_libsecret(&env, &shown) else { panic!() };
    assert_eq!(now.general[0].value, "store");
    assert_eq!(std::fs::read_to_string(p.global()).unwrap(), "[credential]\n\thelper = store\n");
    // As shown: added (nothing replaced), and in effect.
    std::fs::write(p.global(), "[user]\n\tname = T\n").unwrap();
    let HelperWrite::Done(now) = setup::use_libsecret(&env, &shown) else { panic!() };
    assert_eq!(now.general.iter().map(|s| s.value.as_str()).collect::<Vec<_>>(), ["libsecret"]);
    assert!(std::fs::read_to_string(p.global()).unwrap().contains("name = T"));
    // Not on PATH or in git's exec path: not found.
    std::fs::remove_file(p.path("exec/git-credential-libsecret")).unwrap();
    write_executable(&p.path("bin/git-credential-libsecret"), "#!/bin/sh\n");
    assert_eq!(setup::check(&env).libsecret.as_deref(), Some(p.path("bin/git-credential-libsecret").as_path()));
}

#[test]
fn a_write_that_fails_says_so() {
    let p = Place::new();
    // The global file can't be written (its folder isn't there).
    let mut env = p.env();
    env.vars.push(("GIT_CONFIG_GLOBAL".into(), Some(p.path("missing/folder/gitconfig").into())));
    let Probed::Known(shown) = setup::read_helpers(&env) else { panic!() };
    let HelperWrite::Failed(why, Some(now)) = setup::use_libsecret(&env, &shown) else { panic!() };
    assert!(!why.is_empty() && now.none(), "{why}");
}

#[test]
fn programs_that_dont_answer_are_told_as_such() {
    let p = Place::new();
    // A git that hangs, one that leaves something behind holding its
    // output, fonts that hang.
    p.program("fc-list", "exec sleep 30");
    let mut env = p.env();
    env.timeout = Duration::from_millis(400);
    let hanging = p.path("bin/hanging-git");
    write_executable(&hanging, "#!/bin/sh\nexec sleep 30\n");
    env.git = hanging.into();
    let t = Instant::now();
    let r = setup::check(&env);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    for what in ["Git", "Who commits", "HTTPS credentials", "Japanese font"] {
        let row = row(&r, what);
        assert_eq!(row.status, Status::Unknown, "{what}: {}", row.text);
        assert!(row.text.contains("didn't answer"), "{what}: {}", row.text);
    }
    let lingering = p.path("bin/lingering-git");
    write_executable(&lingering, "#!/bin/sh\necho 'git version 2.44.1'\nsleep 30 &\n");
    env.git = lingering.into();
    let t = Instant::now();
    assert_eq!(setup::check(&env).git, Probed::Known(Some(pengitto_core::version::GitVersion(2, 44, 1))));
    assert!(t.elapsed() < Duration::from_secs(5));
    // No git at all: told, and what it would read can't be told.
    env.git = p.path("bin/no-git").into();
    let r = setup::check(&env);
    assert_eq!(r.git, Probed::Known(None));
    assert_eq!(row(&r, "Git").status, Status::Problem);
    assert_eq!(row(&r, "Who commits").status, Status::Unknown);
    // No fontconfig: can't be told.
    let mut env = p.env();
    env.vars.push(("PATH".into(), Some("/nonexistent".into())));
    assert!(matches!(setup::check(&env).japanese_font, Probed::Unknown(e) if e.contains("fc-list isn't there")));
}

#[test]
fn the_agent_and_the_keys() {
    let p = Place::new();
    let mut env = p.env();
    env.vars.push(("SSH_AUTH_SOCK".into(), Some(p.path("agent.sock").into())));
    let agent = |p: &Place, env: &Env, body: &str| {
        p.program("ssh-add", body);
        setup::check(env).ssh.agent
    };
    assert_eq!(agent(&p, &env, "echo '256 SHA256:aa k1 (ED25519)'; echo '3072 SHA256:bb k2 (RSA)'"), Probed::Known(Agent::Keys(2)));
    assert_eq!(agent(&p, &env, "echo 'The agent has no identities.'; exit 1"), Probed::Known(Agent::Keys(0)));
    assert_eq!(agent(&p, &env, "exit 2"), Probed::Known(Agent::Unreachable));
    p.write("home/.ssh/id_ed25519", "k");
    p.write("home/.ssh/id_ed25519.pub", "k");
    p.write("home/.ssh/id_rsa", "no .pub");
    p.write("home/.ssh/known_hosts", "x");
    let r = setup::check(&env);
    assert_eq!(r.ssh.keys, ["id_ed25519"]);
    assert!(row(&r, "SSH").text.contains("keys in ~/.ssh: id_ed25519"));
}

fn system_of(files: &[(&str, &str)], container_var: Option<&str>) -> (setup::System, Vec<setup::Row>, Place) {
    let p = Place::new();
    for (f, text) in files {
        p.write(&format!("root/{f}"), text);
    }
    let mut env = p.env();
    env.container_var = container_var.map(str::to_owned);
    env.git = p.path("bin/no-git").into();
    let r = setup::check(&env);
    (r.system.clone(), setup::rows(&r), p)
}

#[test]
fn where_it_runs_and_what_to_do_there() {
    let kinoite = "ID=fedora\nVARIANT_ID=kinoite\nPRETTY_NAME=\"Fedora Linux 44 (KDE Plasma Desktop Edition)\"\n";
    let (s, rows, _p) = system_of(&[("etc/os-release", kinoite)], None);
    assert!(s.atomic && s.container.is_none());
    let git = rows.iter().find(|r| r.what == "Git").unwrap();
    assert!(git.advice[0].starts_with("rpm-ostree install git-core"), "{:?}", git.advice);
    let system = rows.iter().find(|r| r.what == "System").unwrap();
    assert!(system.text.contains("Fedora Linux 44") && system.text.contains("atomic"), "{}", system.text);
    // Booted from an image, whatever the variant says.
    let (s, _, _p) = system_of(&[("usr/lib/os-release", "ID=fedora\n"), ("run/ostree-booted", "")], None);
    assert!(s.atomic);
    let (s, rows, _p) = system_of(&[("etc/os-release", "ID=fedora\nVARIANT_ID=workstation\n")], None);
    assert!(!s.atomic);
    assert_eq!(rows.iter().find(|r| r.what == "Git").unwrap().advice, ["sudo dnf install git-core"]);
    // A distrobox on an atomic host: advice for inside it.
    let (s, rows, _p) = system_of(
        &[
            ("etc/os-release", "ID=ubuntu\nID_LIKE=debian\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\n"),
            ("run/.containerenv", "engine=\"podman\"\n"),
            ("run/host/etc/os-release", kinoite),
        ],
        Some("podman"),
    );
    assert_eq!((s.container.as_deref(), s.atomic, s.host_atomic), (Some("podman"), false, true));
    let system = rows.iter().find(|r| r.what == "System").unwrap();
    assert_eq!(system.text, "Ubuntu 24.04.3 LTS, in a container (podman); the host: Fedora Linux 44 (KDE Plasma Desktop Edition) (atomic)");
    let git = rows.iter().find(|r| r.what == "Git").unwrap();
    assert_eq!(git.advice, ["sudo apt install git", "(Inside the container pengitto runs in, not on the host.)"]);
    let (s, _, _p) = system_of(&[("etc/os-release", "ID=fedora\n"), ("run/.toolboxenv", ""), ("run/.containerenv", "")], None);
    assert_eq!(s.container.as_deref(), Some("toolbox"));
    let (s, rows, _p) = system_of(&[], None);
    assert!(s.os.is_none());
    assert!(rows.iter().any(|r| r.what == "System" && r.text == "an unknown system"));
}

#[test]
fn nothing_is_asked_of_a_credential_helper() {
    // A helper that would note being run: the check reads the
    // configuration only.
    let p = Place::new();
    let ran = p.path("ran");
    write_executable(&p.path("bin/git-credential-noting"), &format!("#!/bin/sh\ntouch '{}'\n", ran.display()));
    std::fs::write(p.global(), "[credential]\n\thelper = noting\n").unwrap();
    let r = setup::check(&p.env());
    assert_eq!(helpers(&r), ["global:noting"]);
    assert!(!Path::new(&ran).exists());
}

#[test]
fn helpers_for_a_url_as_git_applies_them() {
    let p = Place::new();
    let check = |p: &Place, global: &str| {
        std::fs::write(p.global(), global).unwrap();
        setup::check(&p.env())
    };
    // Set for a URL, then cleared for all: none anywhere.
    let r = check(&p, "[credential \"https://example.com\"]\n\thelper = cache\n[credential]\n\thelper =\n");
    assert!(helpers(&r).is_empty());
    let https = row(&r, "HTTPS credentials");
    assert_eq!((https.status, https.action), (Status::Problem, Some(Action::UseLibsecret)));
    // Cleared for all, then set for a URL: that URL's.
    let r = check(&p, "[credential]\n\thelper =\n[credential \"https://example.com\"]\n\thelper = cache\n");
    assert_eq!(helpers(&r), ["https://example.com=cache"]);
    assert_eq!(row(&r, "HTTPS credentials").status, Status::Info);
    // One under another: the outer one's first, then its own (an empty one
    // clearing them).
    let r = check(
        &p,
        "[credential \"https://github.com\"]\n\thelper = outer\n[credential \"https://github.com/org\"]\n\thelper =\n\thelper = inner\n[credential \"https://github.community\"]\n\thelper = other\n",
    );
    assert_eq!(helpers(&r), ["https://github.com=outer", "https://github.com/org=inner", "https://github.community=other"]);
    // Not cleared: the outer one's, then its own.
    let r = check(&p, "[credential \"https://github.com\"]\n\thelper = outer\n[credential \"https://github.com/org\"]\n\thelper = inner\n");
    assert_eq!(helpers(&r), ["https://github.com=outer", "https://github.com/org=outer,inner"]);
    // For all, and a URL clearing it for itself: told.
    let r = check(&p, "[credential]\n\thelper = store\n[credential \"https://example.com\"]\n\thelper =\n");
    assert_eq!(helpers(&r), ["global:store", "https://example.com="]);
    assert!(row(&r, "HTTPS credentials").text.contains("for https://example.com: none"), "{}", row(&r, "HTTPS credentials").text);
}

#[test]
fn a_name_set_empty_last_isnt_set() {
    let p = Place::new();
    p.write("system", "[user]\n\tname = Earlier\n\temail = e@x\n");
    std::fs::write(p.global(), "[user]\n\tname =\n").unwrap();
    let who = row(&setup::check(&p.env()), "Who commits");
    assert_eq!(who.status, Status::Problem);
    assert!(who.text.starts_with("user.name isn't set"), "{}", who.text);
}

#[test]
fn output_not_read_whole_cant_be_told() {
    let p = Place::new();
    let real = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let mut env = p.env();
    // A git that leaves something behind holding its output.
    let lingering = p.path("bin/lingering-git");
    write_executable(&lingering, &format!("#!/bin/sh\n{} \"$@\"\nsleep 30 &\n", real.trim()));
    env.git = lingering.into();
    env.timeout = Duration::from_millis(500);
    std::fs::write(p.global(), "[user]\n\tname = T\n\temail = t@x\n[credential]\n\thelper = store\n").unwrap();
    let r = setup::check(&env);
    for what in ["Who commits", "HTTPS credentials"] {
        let row = row(&r, what);
        assert_eq!(row.status, Status::Unknown, "{what}: {}", row.text);
        assert!(row.text.contains("cut short"), "{what}: {}", row.text);
    }
    // One whose records end halfway.
    let broken = p.path("bin/broken-git");
    write_executable(&broken, &format!("#!/bin/sh\ncase $1 in config) printf 'global\\000file:x\\000' ;; *) exec {} \"$@\" ;; esac\n", real.trim()));
    env.git = broken.into();
    env.timeout = Duration::from_secs(5);
    assert!(matches!(setup::check(&env).helpers, Probed::Unknown(e) if e.contains("cut short")));
    // More than is kept (an empty helper, many times, before the one that
    // counts): not taken for none.
    let mut many = String::from("[credential]\n");
    for _ in 0..40_000 {
        many.push_str("\thelper =\n");
    }
    many.push_str("\thelper = store\n");
    std::fs::write(p.global(), many).unwrap();
    let r = setup::check(&p.env());
    assert!(matches!(&r.helpers, Probed::Unknown(e) if e.contains("cut short")), "{:?}", r.helpers);
    assert_eq!(row(&r, "HTTPS credentials").status, Status::Unknown);
}
