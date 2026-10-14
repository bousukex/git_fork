//! The packages (M6), built for real: the AppImage (its layout, Tine and
//! its helpers running from inside it — a rebase that stops and goes on, a
//! credential prompt — with and without FUSE, registered by install.sh) and
//! the RPM (its dependencies and files, an install and removal, a rebuild
//! from the source package with no network).
//!
//! Slow (release-like builds), so run on purpose:
//!   TINE_TEST_APPIMAGETOOL=… TINE_TEST_APPIMAGE_RUNTIME=… \
//!     cargo test -p tine --test m6_packages -- --ignored --test-threads=1
//! (the tools pinned in packaging/appimage/tools.lock).

mod common;

use std::{
    io::{BufRead as _, BufReader, Read as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use crossbeam_channel::unbounded;
use tine::{
    askpass::{self, Answer},
    ops_registry,
    worker::{Msg, OpKind, OpOutcome},
};
use tine_core::{
    CancelToken, GitRunner, RepoPaths,
    integrate::{Step, StopReason},
    interactive::{self, Action, Editor, MessageKey, Plan},
    model::{Oid, RepoState},
    sequencer,
};

const TINE: &str = env!("CARGO_BIN_EXE_tine");

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn ok(o: Output) -> String {
    let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(o.status.success(), "{all}");
    all
}

fn tool(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("set {var} to the pinned tool (packaging/appimage/tools.lock)"))
}

/// Builds the AppImage of the test build of tine into `out`.
fn appimage(out: &Path) -> PathBuf {
    ok(Command::new("sh")
        .arg(root().join("scripts/appimage.sh"))
        .args(["--binary", TINE, "--out"])
        .arg(out)
        .env("APPIMAGETOOL", tool("TINE_TEST_APPIMAGETOOL"))
        .env("APPIMAGE_RUNTIME", tool("TINE_TEST_APPIMAGE_RUNTIME"))
        .output()
        .unwrap());
    let image = out.join(format!("Tine-{}-x86_64.AppImage", env!("CARGO_PKG_VERSION")));
    assert!(image.is_file());
    image
}

fn runner(d: &Path) -> GitRunner {
    GitRunner::new(d).with_env("GIT_CONFIG_GLOBAL", "/dev/null").with_env("GIT_CONFIG_NOSYSTEM", "1")
}

fn out(d: &Path, args: &[&str]) -> String {
    let o = Command::new("git").current_dir(d).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", text(&o.stderr));
    text(&o.stdout)
}

/// A rebase that stops at an edit and goes on, rewording, with `program`
/// as git's editor (Tine's helper).
fn rebase_through(program: &Path) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    common::git(d, &["init", "-q", "-b", "main"], &mut t);
    common::git(d, &["config", "user.name", "T"], &mut t);
    common::git(d, &["config", "user.email", "t@example.com"], &mut t);
    for f in ["base", "f1", "f2"] {
        std::fs::write(d.join(f), f).unwrap();
        common::git(d, &["add", "-A"], &mut t);
        common::git(d, &["commit", "-qm", f], &mut t);
    }
    let base = Oid::from_hex(out(d, &["rev-parse", "HEAD~2"]).trim().as_bytes()).unwrap();
    let editor = Editor { program: program.to_path_buf() };
    let git_dir = d.join(".git");
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut plan = Plan { items: vec![(r.commits[0].id, Action::Edit), (r.commits[1].id, Action::Reword)], messages: Default::default() };
    plan.messages.insert(MessageKey::Reword(r.commits[1].id), "f2, reworded through the AppImage".into());
    let step = interactive::start(&runner(d), &git_dir, &r, &plan, &editor, None).unwrap();
    assert!(matches!(step, Step::Stopped(StopReason::Edit(_))), "{step:?}");
    // On again, as the app does it (the session found again).
    let s = interactive::rebase_runner(&runner(d), &git_dir, &editor).unwrap();
    interactive::finish_reword(&s, &git_dir).unwrap();
    assert_eq!(sequencer::continue_op(&s, &git_dir, RepoState::Rebasing).unwrap(), Step::Done);
    assert_eq!(out(d, &["log", "-1", "--format=%s"]).trim(), "f2, reworded through the AppImage");
}

/// `git credential fill`, its prompts answered through Tine's askpass
/// (`program`).
fn credentials_through(program: &Path) {
    askpass::set_program(program);
    let dir = tempfile::tempdir().unwrap();
    let base = runner(dir.path());
    let paths = RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let (tx, rx) = unbounded();
    let id = ops_registry::submit(
        &PathBuf::from("/test/m6/appimage-askpass"),
        &paths,
        OpKind::Fetch("origin".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            let git = ctx.network(ctx.git(&base))?;
            let out = git.output_with_input(["credential", "fill"], b"protocol=https\nhost=example.com\n\n", &[])?;
            Ok(OpOutcome::new(String::from_utf8_lossy(&out).into_owned()))
        }),
    )
    .unwrap();
    for answer in ["alice", "s3cret"] {
        let start = std::time::Instant::now();
        let prompt = loop {
            if let Some(p) = askpass::pending_for(id) {
                break p;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "no prompt");
            std::thread::sleep(Duration::from_millis(10));
        };
        askpass::answer(prompt.id, Answer::Value(answer.into()));
    }
    let summary = loop {
        if let Msg::OpDone(_, _, r) = rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            break r.unwrap().summary;
        }
    };
    assert!(summary.contains("username=alice\n") && summary.contains("password=s3cret\n"), "{summary}");
}

#[test]
#[ignore = "builds the AppImage; needs the pinned AppImage tools"]
fn the_appimage_runs_tine_and_its_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let image = appimage(dir.path());
    // Its layout: Tine, its menu entry and icons, the notices; no libraries.
    ok(Command::new(&image).arg("--appimage-extract").current_dir(dir.path()).output().unwrap());
    let app = dir.path().join("squashfs-root");
    for f in ["AppRun", "tine.desktop", "tine.png", "usr/bin/tine", "usr/share/doc/tine/THIRD_PARTY_LICENSES", "usr/share/doc/tine/APPIMAGE_NOTICES"] {
        assert!(app.join(f).is_file(), "{f}");
    }
    assert_eq!(std::fs::read_link(app.join(".DirIcon")).unwrap(), Path::new("tine.png"));
    assert!(!app.join("usr/lib").exists(), "no libraries bundled");
    ok(Command::new("desktop-file-validate").arg(app.join("tine.desktop")).output().unwrap());
    // The notices name each part of the pinned runtime, at the version its
    // own strings have.
    let runtime = std::fs::read(tool("TINE_TEST_APPIMAGE_RUNTIME")).unwrap();
    let notices = std::fs::read_to_string(app.join("usr/share/doc/tine/APPIMAGE_NOTICES")).unwrap();
    for (inside, noticed) in [
        ("type2-runtime/commit/8f39b89", "commit 8f39b89e"),
        ("musl libc", "musl libc 1.2.5"),
        ("mimalloc", "mimalloc 2.1.7"),
        ("libfuse", "libfuse 3.15.0"),
        ("squashfuse 0.5.2", "squashfuse 0.5.2"),
        ("1.5.6", "zstd 1.5.6"),
        ("inflate 1.3.2", "zlib 1.3.2"),
    ] {
        assert!(runtime.windows(inside.len()).any(|w| w == inside.as_bytes()), "{inside} in the runtime");
        assert!(notices.contains(noticed), "{noticed} in the notices");
    }
    // Self-checks, mounted (FUSE) and extracted.
    for extract in [false, true] {
        let mut cmd = Command::new(&image);
        cmd.arg("--self-check");
        if extract {
            cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
        }
        let all = ok(cmd.output().unwrap());
        assert!(all.contains("running from") && all.contains("helper: runs") && all.contains("environment: clean"), "{all}");
    }
    // Tine's helper as the GUI hands it to git: the very executable inside
    // the image (mounted, or extracted), while Tine (here held in its
    // self-check) runs.
    for extract in [false, true] {
        let gate = dir.path().join("gate");
        std::fs::write(&gate, "").unwrap();
        let mut cmd = Command::new(&image);
        cmd.arg("--self-check").env("TINE_SELF_CHECK_PAUSE", &gate).stdout(Stdio::piped());
        if extract {
            cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
        }
        let mut held = cmd.spawn().unwrap();
        let mut lines = BufReader::new(held.stdout.take().unwrap());
        let helper = loop {
            let mut line = String::new();
            assert!(lines.read_line(&mut line).unwrap() > 0);
            if let Some(p) = line.trim().strip_prefix("helper: ") {
                break PathBuf::from(p);
            }
        };
        assert!(helper.starts_with("/proc/"), "{}", helper.display());
        rebase_through(&helper);
        credentials_through(&helper);
        std::fs::remove_file(&gate).unwrap();
        let mut rest = String::new();
        lines.read_to_string(&mut rest).unwrap();
        assert!(held.wait().unwrap().success(), "extract {extract}: {rest}");
    }
    // Registered by install.sh: the menu entry runs the AppImage.
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    ok(Command::new("sh")
        .arg(root().join("scripts/install.sh"))
        .arg("--binary")
        .arg(&image)
        .args(["--no-integration"])
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .output()
        .unwrap());
    let installed = home.path().join(".local/bin/tine");
    assert_eq!(std::fs::read(&installed).unwrap(), std::fs::read(&image).unwrap());
    assert!(ok(Command::new(&installed).arg("--version").output().unwrap()).starts_with("tine "));
}

/// A repository of what the working tree has (committed or not, without
/// what git ignores), committed: what a release of it would be.
fn snapshot(to: &Path) {
    let files = Command::new("git").current_dir(root()).args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).output().unwrap();
    for f in files.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let f = Path::new(std::str::from_utf8(f).unwrap());
        let (from, dest) = (root().join(f), to.join(f));
        if from.is_file() {
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(&from, &dest).unwrap();
        }
    }
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "snapshot"]] {
        ok(Command::new("git").current_dir(to).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap());
    }
}

#[test]
#[ignore = "builds the RPMs (twice, the second time with no network)"]
fn the_rpm_installs_what_tine_needs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    snapshot(&src);
    let rpms = dir.path().join("rpms");
    ok(Command::new("sh").arg(src.join("scripts/rpm.sh")).arg("--out").arg(&rpms).output().unwrap());
    let version = env!("CARGO_PKG_VERSION");
    let rpm = rpms.join(format!("tine-{version}-1.x86_64.rpm"));
    let srpm = rpms.join(format!("tine-{version}-1.src.rpm"));
    let query = |args: &[&str]| ok(Command::new("rpm").args(args).arg(&rpm).output().unwrap());
    let requires = query(&["-qp", "--requires"]);
    for need in [
        "git-core >= 2.43",
        "fontconfig",
        "hicolor-icon-theme",
        "libwayland-client.so.0()(64bit)",
        "libxkbcommon.so.0()(64bit)",
        "(libvulkan.so.1()(64bit) or libEGL.so.1()(64bit))",
        "libc.so.6(GLIBC_2.39)(64bit)",
    ] {
        assert!(requires.lines().any(|l| l.trim() == need), "{need} in\n{requires}");
    }
    let recommends = query(&["-qp", "--recommends"]);
    for want in ["libX11.so.6()(64bit)", "xdg-utils", "dbus-tools", "xdg-desktop-portal"] {
        assert!(recommends.lines().any(|l| l.trim() == want), "{want}");
    }
    let mut files: Vec<String> = query(&["-qlp"]).lines().filter(|l| !l.contains("/.build-id")).map(Into::into).collect();
    files.sort();
    let mut want: Vec<String> = ["/usr/bin/tine", "/usr/share/applications/tine.desktop"].map(String::from).to_vec();
    want.extend([16, 24, 32, 48, 64, 128, 256, 512].map(|n| format!("/usr/share/icons/hicolor/{n}x{n}/apps/tine.png")));
    want.push(format!("/usr/share/licenses/tine-{version}"));
    want.push(format!("/usr/share/licenses/tine-{version}/THIRD_PARTY_LICENSES"));
    want.sort();
    assert_eq!(files, want);
    // Unpacked: it runs (helpers and all), its entry is valid, the
    // function names are still there for backtraces.
    let unpacked = dir.path().join("unpacked");
    std::fs::create_dir(&unpacked).unwrap();
    ok(Command::new("sh")
        .arg("-c")
        .arg(format!("rpm2cpio '{}' | cpio -idm --quiet", rpm.display()))
        .current_dir(&unpacked)
        .output()
        .unwrap());
    let all = ok(Command::new(unpacked.join("usr/bin/tine")).arg("--self-check").output().unwrap());
    assert!(all.contains("helper: runs"), "{all}");
    ok(Command::new("desktop-file-validate").arg(unpacked.join("usr/share/applications/tine.desktop")).output().unwrap());
    let desktop = std::fs::read_to_string(unpacked.join("usr/share/applications/tine.desktop")).unwrap();
    assert!(desktop.contains("Exec=\"/usr/bin/tine\" -- %F"), "{desktop}");
    let symbols = ok(Command::new("nm").arg(unpacked.join("usr/bin/tine")).output().unwrap());
    assert!(symbols.lines().count() > 1000, "symbols kept");
    // Installed into a root of its own, and removed: nothing left.
    let root_dir = dir.path().join("root");
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).arg("--initdb").output().unwrap());
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).args(["-i", "--nodeps", "--noscripts"]).arg(&rpm).output().unwrap());
    assert!(root_dir.join("usr/bin/tine").is_file());
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).args(["-e", "tine"]).output().unwrap());
    assert!(!root_dir.join("usr/bin/tine").exists() && !root_dir.join("usr/share/applications/tine.desktop").exists());
    // Rebuilt from the source package alone, with no network (unpacked
    // first: rpm can't give its files their owners in the namespace).
    let rebuild = dir.path().join("rebuild");
    let sources = rebuild.join("SOURCES");
    std::fs::create_dir_all(&sources).unwrap();
    ok(Command::new("sh")
        .arg("-c")
        .arg(format!("rpm2cpio '{}' | cpio -idm --quiet", srpm.display()))
        .current_dir(&sources)
        .output()
        .unwrap());
    // (Its spec has the version: nothing more to tell rpmbuild than where.)
    let spec = std::fs::read_to_string(sources.join("tine.spec")).unwrap();
    assert!(spec.lines().any(|l| l == format!("Version:        {version}")), "{spec}");
    ok(Command::new("unshare")
        .args(["-rn", "rpmbuild", "-bb", "--nodeps", "--define"])
        .arg(format!("_topdir {}", rebuild.display()))
        .arg("--define")
        .arg(format!("_sourcedir {}", sources.display()))
        .arg(sources.join("tine.spec"))
        .output()
        .unwrap());
    let again = rebuild.join(format!("RPMS/x86_64/tine-{version}-1.x86_64.rpm"));
    assert!(again.is_file());
}
