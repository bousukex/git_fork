//! The packages (M6), built for real: the AppImage (its layout, pengitto and
//! its helpers running from inside it — a rebase that stops and goes on, a
//! credential prompt — with and without FUSE, registered by install.sh) and
//! the RPM (its dependencies and files, an install and removal, a rebuild
//! from the source package with no network).
//!
//! Slow (release-like builds), so run on purpose:
//!   PENGITTO_TEST_APPIMAGETOOL=… PENGITTO_TEST_APPIMAGE_RUNTIME=… \
//!     cargo test -p pengitto --test m6_packages -- --ignored --test-threads=1
//! (the tools pinned in packaging/appimage/tools.lock).

mod common;

use std::{
    io::{BufRead as _, BufReader, Read as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use crossbeam_channel::unbounded;
use pengitto::{
    askpass::{self, Answer},
    ops_registry,
    worker::{Msg, OpKind, OpOutcome},
};
use pengitto_core::{
    CancelToken, GitRunner, RepoPaths,
    integrate::{Step, StopReason},
    interactive::{self, Action, Editor, MessageKey, Plan},
    model::{Oid, RepoState},
    sequencer,
};

const PENGITTO: &str = env!("CARGO_BIN_EXE_pengitto");

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

/// Builds the AppImage of the test build of pengitto into `out`.
fn appimage(out: &Path) -> PathBuf {
    ok(Command::new("sh")
        .arg(root().join("scripts/appimage.sh"))
        .args(["--binary", PENGITTO, "--out"])
        .arg(out)
        .env("APPIMAGETOOL", tool("PENGITTO_TEST_APPIMAGETOOL"))
        .env("APPIMAGE_RUNTIME", tool("PENGITTO_TEST_APPIMAGE_RUNTIME"))
        .output()
        .unwrap());
    let image = out.join(format!("pengitto-{}-x86_64.AppImage", env!("CARGO_PKG_VERSION")));
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
/// as git's editor (pengitto's helper).
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

/// `git credential fill`, its prompts answered through pengitto's askpass
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
#[ignore = "builds the AppImage; needs the pinned AppImage tools, and FUSE (fusermount3, /dev/fuse)"]
fn the_appimage_runs_pengitto_and_its_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let image = appimage(dir.path());
    // Its layout: pengitto, its menu entry and icons, the notices; no libraries.
    ok(Command::new(&image).arg("--appimage-extract").current_dir(dir.path()).output().unwrap());
    let app = dir.path().join("squashfs-root");
    for f in [
        "AppRun",
        "pengitto.desktop",
        "pengitto.png",
        "usr/bin/pengitto",
        "usr/share/doc/pengitto/LICENSE-MIT",
        "usr/share/doc/pengitto/LICENSE-APACHE",
        "usr/share/doc/pengitto/THIRD_PARTY_LICENSES",
        "usr/share/doc/pengitto/APPIMAGE_NOTICES",
    ] {
        assert!(app.join(f).is_file(), "{f}");
    }
    assert_eq!(std::fs::read_link(app.join(".DirIcon")).unwrap(), Path::new("pengitto.png"));
    assert!(!app.join("usr/lib").exists(), "no libraries bundled");
    ok(Command::new("desktop-file-validate").arg(app.join("pengitto.desktop")).output().unwrap());
    // The notices name each part of the pinned runtime, at the version its
    // own strings have.
    let runtime = std::fs::read(tool("PENGITTO_TEST_APPIMAGE_RUNTIME")).unwrap();
    let notices = std::fs::read_to_string(app.join("usr/share/doc/pengitto/APPIMAGE_NOTICES")).unwrap();
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
    // Self-checks, mounted (FUSE) and extracted. (The runtime leaves what
    // it extracts behind, in TMPDIR: here, this test's folder.)
    for extract in [false, true] {
        let mut cmd = Command::new(&image);
        cmd.arg("--self-check");
        if extract {
            cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1").env("TMPDIR", dir.path());
        }
        let all = ok(cmd.output().unwrap());
        assert!(all.contains("running from") && all.contains("helper: runs") && all.contains("environment: clean"), "{all}");
    }
    let extracted = |d: &Path| std::fs::read_dir(d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("appimage_extracted_"));
    assert!(extracted(dir.path()), "extracted into TMPDIR");
    // pengitto's helper as the GUI hands it to git: the very executable inside
    // the image (mounted, or extracted), while pengitto (here held in its
    // self-check) runs.
    for extract in [false, true] {
        let gate = dir.path().join("gate");
        std::fs::write(&gate, "").unwrap();
        let mut cmd = Command::new(&image);
        cmd.arg("--self-check").env("PENGITTO_SELF_CHECK_PAUSE", &gate).stdout(Stdio::piped());
        if extract {
            cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1").env("TMPDIR", dir.path());
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
    // One at a time (M7b), from the image too (mounted, and extracted): it
    // hands over to the one running (here a stand-in in this test) — its
    // mount namespace is this one's. No display it could open.
    let rt = dir.path().join("runtime");
    std::fs::create_dir(&rt).unwrap();
    std::fs::set_permissions(&rt, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let display = format!("pengitto-test-{}", std::process::id());
    let ns = std::fs::read_link("/proc/self/ns/mnt").unwrap().to_string_lossy().into_owned();
    let place = pengitto::instance::place(Some(rt.as_os_str()), &ns, Some(&display)).unwrap();
    let pengitto::instance::Claim::Primary(running) =
        pengitto::instance::claim(&place, &pengitto::instance::Request::default(), Duration::from_secs(5))
    else {
        panic!("not the one running")
    };
    running.link.ready(&egui::Context::default());
    for extract in [false, true] {
        let mut cmd = Command::new(&image);
        cmd.arg(dir.path())
            .env("XDG_RUNTIME_DIR", &rt)
            .env("WAYLAND_DISPLAY", &display)
            .env("XDG_ACTIVATION_TOKEN", "from-the-launcher")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_SOCKET");
        if extract {
            cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1").env("TMPDIR", dir.path());
        }
        let out = ok(cmd.output().unwrap());
        let got = running.link.take_requests();
        assert_eq!(got.len(), 1, "extract {extract}: {out}");
        assert_eq!(got[0].paths, [dir.path().to_path_buf()]);
        assert_eq!(got[0].token.as_deref(), Some("from-the-launcher"));
    }
    drop(running);
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
    let installed = home.path().join(".local/bin/pengitto");
    assert_eq!(std::fs::read(&installed).unwrap(), std::fs::read(&image).unwrap());
    assert!(ok(Command::new(&installed).arg("--version").output().unwrap()).starts_with("pengitto "));
    // It registers itself (M7c): mounted, the menu runs the copy as it is;
    // without FUSE, extracting itself. Either way that runs with nothing of
    // the installation's environment.
    let desktop = data.join("applications/pengitto.desktop");
    // (Extracting, into this test's folder; mounted, where the runtime
    // mounts it: it unmounts as the program ends, a little after — a
    // mount point here then would keep the folder from being removed.)
    let manage = |args: &[&str], env: &[(&str, &std::ffi::OsStr)]| {
        let mut c = Command::new(&image);
        c.args(args).env("HOME", home.path()).env("XDG_DATA_HOME", &data);
        for (k, v) in env {
            c.env(k, v);
        }
        if env.iter().any(|(k, _)| *k == "APPIMAGE_EXTRACT_AND_RUN") {
            c.env("TMPDIR", dir.path());
        }
        ok(c.output().unwrap())
    };
    let launched = |extracting: bool| {
        let words = exec_words(&std::fs::read_to_string(&desktop).unwrap());
        let upto = words.iter().position(|w| w == "--").unwrap();
        assert_eq!(words[upto..], ["--", "%F"]);
        assert_eq!(words[0] == "env", extracting, "{words:?}");
        let mut c = Command::new(&words[0]);
        c.args(&words[1..upto]).arg("--version").env_clear().env("HOME", home.path()).env("PATH", "/usr/bin:/bin");
        if extracting {
            c.env("TMPDIR", dir.path());
        }
        ok(c.output().unwrap())
    };
    let out = manage(&["--install", "--force", "--no-integration"], &[]);
    assert!(!out.contains("FUSE isn't usable"), "{out}");
    assert_eq!(std::fs::read(&installed).unwrap(), std::fs::read(&image).unwrap());
    assert!(launched(false).starts_with("pengitto "));
    let nofuse = dir.path().join("nofuse");
    std::fs::create_dir(&nofuse).unwrap();
    for d in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let n = e.file_name();
            if n != "fusermount3" && n != "fusermount" && !nofuse.join(&n).exists() {
                std::os::unix::fs::symlink(e.path(), nofuse.join(&n)).unwrap();
            }
        }
    }
    let out = manage(&["--install", "--no-integration"], &[("PATH", nofuse.as_os_str()), ("APPIMAGE_EXTRACT_AND_RUN", "1".as_ref())]);
    assert!(out.contains("FUSE isn't usable"), "{out}");
    assert!(launched(true).starts_with("pengitto "));
    // Taken away by the image too.
    manage(&["--uninstall"], &[]);
    assert!(!installed.exists() && !desktop.exists());
}

/// The words of a desktop file's Exec (plain ones: quotes taken off).
fn exec_words(desktop: &str) -> Vec<String> {
    let exec = desktop.lines().find_map(|l| l.strip_prefix("Exec=")).unwrap();
    exec.split(' ').map(|w| w.trim_matches('"').to_owned()).collect()
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

/// A stand-in for the package of pengitto's former name, 0.1.0: just its
/// executable and its menu entry.
fn former_rpm(dir: &Path, name: &str) -> PathBuf {
    let top = dir.join("former-rpm");
    std::fs::create_dir_all(&top).unwrap();
    let spec = top.join(format!("{name}.spec"));
    std::fs::write(
        &spec,
        format!(
            "%global debug_package %{{nil}}\nName: {name}\nVersion: 0.1.0\nRelease: 1\nSummary: stand-in\nLicense: MIT\nBuildArch: x86_64\n\
             %description\nstand-in\n%install\nmkdir -p %{{buildroot}}/usr/bin %{{buildroot}}/usr/share/applications\n\
             echo old > %{{buildroot}}/usr/bin/{name}\necho old > %{{buildroot}}/usr/share/applications/{name}.desktop\n\
             %files\n/usr/bin/{name}\n/usr/share/applications/{name}.desktop\n"
        ),
    )
    .unwrap();
    ok(Command::new("rpmbuild")
        .args(["-bb", "--nodeps", "--define"])
        .arg(format!("_topdir {}", top.display()))
        .args(["--define", "_build_id_links none"])
        .arg(&spec)
        .output()
        .unwrap());
    let rpm = top.join(format!("RPMS/x86_64/{name}-0.1.0-1.x86_64.rpm"));
    assert!(rpm.is_file());
    rpm
}

#[test]
#[ignore = "builds the RPMs (twice, the second time with no network)"]
fn the_rpm_installs_what_pengitto_needs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    snapshot(&src);
    let rpms = dir.path().join("rpms");
    ok(Command::new("sh").arg(src.join("scripts/rpm.sh")).arg("--out").arg(&rpms).output().unwrap());
    let version = env!("CARGO_PKG_VERSION");
    let rpm = rpms.join(format!("pengitto-{version}-1.x86_64.rpm"));
    let srpm = rpms.join(format!("pengitto-{version}-1.src.rpm"));
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
    let mut want: Vec<String> = ["/usr/bin/pengitto", "/usr/share/applications/pengitto.desktop"].map(String::from).to_vec();
    want.extend([16, 24, 32, 48, 64, 128, 256, 512].map(|n| format!("/usr/share/icons/hicolor/{n}x{n}/apps/pengitto.png")));
    want.push(format!("/usr/share/licenses/pengitto-{version}"));
    for f in ["LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_LICENSES"] {
        want.push(format!("/usr/share/licenses/pengitto-{version}/{f}"));
    }
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
    let all = ok(Command::new(unpacked.join("usr/bin/pengitto")).arg("--self-check").output().unwrap());
    assert!(all.contains("helper: runs"), "{all}");
    ok(Command::new("desktop-file-validate").arg(unpacked.join("usr/share/applications/pengitto.desktop")).output().unwrap());
    let desktop = std::fs::read_to_string(unpacked.join("usr/share/applications/pengitto.desktop")).unwrap();
    assert!(desktop.contains("Exec=\"/usr/bin/pengitto\" -- %F"), "{desktop}");
    let symbols = ok(Command::new("nm").arg(unpacked.join("usr/bin/pengitto")).output().unwrap());
    assert!(symbols.lines().count() > 1000, "symbols kept");
    // Installed into a root of its own, and removed: nothing left.
    let root_dir = dir.path().join("root");
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).arg("--initdb").output().unwrap());
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).args(["-i", "--nodeps", "--noscripts"]).arg(&rpm).output().unwrap());
    assert!(root_dir.join("usr/bin/pengitto").is_file());
    ok(Command::new("unshare").args(["-r", "rpm", "--root"]).arg(&root_dir).args(["-e", "pengitto"]).output().unwrap());
    assert!(!root_dir.join("usr/bin/pengitto").exists() && !root_dir.join("usr/share/applications/pengitto.desktop").exists());
    // It replaces the package of its former name (M7a): a small stand-in
    // for that package, installed first, goes with its files.
    let former = pengitto_core::legacy::APP;
    let obsoletes = query(&["-qp", "--obsoletes"]);
    assert!(obsoletes.lines().any(|l| l.trim() == format!("{former} < 0.2.0")), "{obsoletes}");
    let old_rpm = former_rpm(dir.path(), former);
    let root_dir = dir.path().join("root-upgrade");
    let rpm_root = |args: &[&str], file: Option<&Path>| {
        let mut c = Command::new("unshare");
        c.args(["-r", "rpm", "--root"]).arg(&root_dir).args(args);
        if let Some(f) = file {
            c.arg(f);
        }
        c.output().unwrap()
    };
    ok(rpm_root(&["--initdb"], None));
    ok(rpm_root(&["-i", "--nodeps", "--noscripts"], Some(&old_rpm)));
    assert!(root_dir.join("usr/bin").join(former).is_file());
    ok(rpm_root(&["-U", "--nodeps", "--noscripts"], Some(&rpm)));
    assert!(!rpm_root(&["-q", former], None).status.success(), "{former} is still installed");
    assert!(!root_dir.join("usr/bin").join(former).exists() && !root_dir.join(format!("usr/share/applications/{former}.desktop")).exists());
    assert!(root_dir.join("usr/bin/pengitto").is_file());
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
    let spec = std::fs::read_to_string(sources.join("pengitto.spec")).unwrap();
    assert!(spec.lines().any(|l| l == format!("Version:        {version}")), "{spec}");
    ok(Command::new("unshare")
        .args(["-rn", "rpmbuild", "-bb", "--nodeps", "--define"])
        .arg(format!("_topdir {}", rebuild.display()))
        .arg("--define")
        .arg(format!("_sourcedir {}", sources.display()))
        .arg(sources.join("pengitto.spec"))
        .output()
        .unwrap());
    let again = rebuild.join(format!("RPMS/x86_64/pengitto-{version}-1.x86_64.rpm"));
    assert!(again.is_file());
}
