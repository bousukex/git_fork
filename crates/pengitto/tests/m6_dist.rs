//! Distribution (M6): the command line, the self-check (helpers that still
//! run after the executable was replaced or removed, no AppImage variables
//! passed on), the install and uninstall scripts (in throwaway homes), the
//! menu entry as launchers read it, the icons and the license list.

use std::{
    io::{BufRead as _, BufReader, Read as _},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const PENGITTO: &str = env!("CARGO_BIN_EXE_pengitto");

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// --- The command line and the self-check -----------------------------------

#[test]
fn options_without_a_window() {
    let run = |args: &[&str]| Command::new(PENGITTO).args(args).output().unwrap();
    let o = run(&["--version"]);
    assert!(o.status.success());
    assert_eq!(text(&o.stdout).trim(), format!("pengitto {}", env!("CARGO_PKG_VERSION")));
    let o = run(&["--help"]);
    assert!(o.status.success() && text(&o.stdout).contains("Usage: pengitto"));
    let o = run(&["--frobnicate"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("unknown option: --frobnicate"), "{}", text(&o.stderr));
    // The AppImage's own options, outside it (M7a).
    for opt in ["--install", "--uninstall"] {
        let o = run(&[opt]);
        assert_eq!(o.status.code(), Some(2));
        assert!(text(&o.stderr).contains(&format!("{opt} is for the AppImage")), "{}", text(&o.stderr));
    }
    // Asked for credentials by git: a prompt that looks like an option is
    // still a prompt (here the app isn't there to answer it).
    let o = Command::new(PENGITTO)
        .arg("--help")
        .env("PENGITTO_ASKPASS_SOCKET", "/nonexistent/askpass.sock")
        .env("PENGITTO_ASKPASS_TOKEN", "x")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(!text(&o.stdout).contains("Usage"), "{}", text(&o.stdout));
    assert!(text(&o.stderr).contains("askpass"), "{}", text(&o.stderr));
}

/// Runs `exe --self-check` held before it starts its helper; `meanwhile`
/// runs then (the executable's file may be replaced or removed).
fn self_check_holding(exe: &Path, meanwhile: impl FnOnce()) -> (bool, String) {
    let gate = exe.with_file_name("gate");
    std::fs::write(&gate, "").unwrap();
    let mut child = Command::new(exe)
        .arg("--self-check")
        .env("PENGITTO_SELF_CHECK_PAUSE", &gate)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(out.read_line(&mut line).unwrap() > 0, "ended early: {seen}");
        seen.push_str(&line);
        if line.starts_with("helper: /") {
            break;
        }
    }
    meanwhile();
    std::fs::remove_file(&gate).unwrap();
    out.read_to_string(&mut seen).unwrap();
    (child.wait().unwrap().success(), seen)
}

#[test]
fn the_helpers_are_the_running_version_whatever_happens_to_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("pengitto");
    // Replaced by another version while it runs (an update).
    std::fs::copy(PENGITTO, &exe).unwrap();
    let (ok, out) = self_check_holding(&exe, || {
        let new = exe.with_file_name("pengitto.new");
        executable(&new, "#!/bin/sh\necho 'pengitto 9.9.9'\n");
        std::fs::rename(&new, &exe).unwrap();
    });
    assert!(ok, "{out}");
    assert!(out.contains("helper: /proc/") && out.contains("helper: runs"), "{out}");
    // Removed while it runs (an uninstall).
    std::fs::copy(PENGITTO, &exe).unwrap();
    let (ok, out) = self_check_holding(&exe, || std::fs::remove_file(&exe).unwrap());
    assert!(ok && out.contains("helper: runs"), "{out}");
}

#[test]
fn appimage_variables_reach_nothing_pengitto_starts() {
    let o = Command::new(PENGITTO)
        .arg("--self-check")
        .env("APPIMAGE", "/somewhere/pengitto.AppImage")
        .env("APPDIR", "/tmp/.mount_pengittoX")
        .env("ARGV0", "pengitto.AppImage")
        .env("OWD", "/home")
        .output()
        .unwrap();
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("running from /somewhere/pengitto.AppImage"), "{out}");
    assert!(out.contains("environment: clean"), "{out}");
}

// --- The icons and the licenses ---------------------------------------------

#[test]
fn the_icons_are_the_picture_at_every_size() {
    for n in [16u32, 24, 32, 48, 64, 128, 256, 512] {
        let path = root().join(format!("packaging/icons/hicolor/{n}x{n}/apps/pengitto.png"));
        let icon = eframe::icon_data::from_png_bytes(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((icon.width, icon.height), (n, n), "{}", path.display());
        let px = |x: u32, y: u32| &icon.rgba[((y * n + x) * 4) as usize..][..4];
        // Rounded: the corners are see-through, the middle isn't.
        assert!(px(0, 0)[3] < 40 && px(n - 1, n - 1)[3] < 40, "{n}: corner {:?}", px(0, 0));
        assert_eq!(px(n / 2, n / 2)[3], 255, "{n}: middle");
        // The picture, not a flat fill.
        let colors: std::collections::HashSet<&[u8]> = icon.rgba.chunks(4).filter(|p| p[3] == 255).collect();
        assert!(colors.len() > if n >= 32 { 100 } else { 20 }, "{n}: {} colors", colors.len());
    }
    let window = eframe::icon_data::from_png_bytes(pengitto::WINDOW_ICON).unwrap();
    assert_eq!((window.width, window.height), (256, 256));
}

#[test]
fn the_license_list_is_current_and_complete() {
    let o = Command::new("python3").arg(root().join("scripts/third-party.py")).arg("--check").output().unwrap();
    assert!(o.status.success(), "{}{}", text(&o.stdout), text(&o.stderr));
    let list = pengitto::app::THIRD_PARTY_LICENSES;
    assert_eq!(list, std::fs::read_to_string(root().join("packaging/THIRD_PARTY_LICENSES")).unwrap());
    // pengitto's own: what Cargo.toml declares, a text for each.
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("\nlicense = \"MIT OR Apache-2.0\"\n"), "{manifest}");
    let mit = std::fs::read_to_string(root().join("LICENSE-MIT")).unwrap();
    assert!(mit.contains("Copyright (c) 2026 K2Y") && mit.contains("Permission is hereby granted, free of charge"));
    let apache = std::fs::read_to_string(root().join("LICENSE-APACHE")).unwrap();
    assert_eq!(apache, std::fs::read_to_string(root().join("packaging/licenses/Apache-2.0.txt")).unwrap());
    // Every crate in Cargo.lock that ends up in pengitto is there (a few kinds
    // of them, and the embedded assets' notices).
    for must in [
        "  egui 0.36.2 — MIT OR Apache-2.0",
        "  syntect 5.3.0",
        "  epaint_default_fonts 0.36.2",
        "UBUNTU FONT LICENCE",
        "SIL OPEN FONT LICENSE",
        "Bitstream Vera",
        "sublimehq/Packages",
        "Copyright (c) 2015 FichteFoll",
        "Apache License",
    ] {
        assert!(list.contains(must), "{must}");
    }
    // The crates are those of the build (cargo tree's, asked here apart
    // from the script): no more, no fewer.
    let tree = Command::new("cargo")
        .args(["tree", "--locked", "-p", "pengitto", "--target", "x86_64-unknown-linux-gnu", "-e", "normal,no-proc-macro"])
        .args(["--prefix", "none", "--format", "{p}"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(tree.status.success(), "{}", text(&tree.stderr));
    let built: std::collections::BTreeSet<String> = text(&tree.stdout)
        .lines()
        // (The workspace's own crates have their folder after them.)
        .filter(|l| !l.contains(" (/"))
        .filter_map(|l| {
            let mut w = l.split(' ');
            Some(format!("{} {}", w.next()?, w.next()?.strip_prefix('v')?))
        })
        .collect();
    let listed: std::collections::BTreeSet<String> = list
        .split("\nCRATES\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| !l.is_empty())
        .map(|l| l.trim_start().split(" — ").next().unwrap().to_owned())
        .collect();
    assert_eq!(listed, built);
    assert!(!listed.iter().any(|c| c.starts_with("egui_glow ")), "only tests have it");
    // Crates with only the standard text say whose they are.
    assert!(list.contains("  egui 0.36.2 — MIT OR Apache-2.0 — texts 2 — copyright: its authors, Emil Ernerfeldt"), "{}", &list[..2000]);
    // The check of texts against what crates declare: texts of alike
    // licenses told apart, exceptions with their texts, every part of the
    // assets.
    let probe = r#"
import importlib.util, sys
spec = importlib.util.spec_from_file_location("tp", sys.argv[1])
tp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tp)
MIT = "Permission is hereby granted, free of charge, to any person ... The above copyright notice and this\n permission notice shall be included in\n all copies"
APACHE = "Apache License\nVersion 2.0, January 2004\nTERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION"
LLVM = "--- LLVM Exceptions to the Apache 2.0 License ----"
BSD2 = "Redistribution and use in source and binary forms, with or without modification, are permitted"
BSD3 = BSD2 + " ... Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products"
ISC = "Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice"
ZERO_BSD = "Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted."
OFL = "SIL OPEN FONT LICENSE Version 1.1"
cases = [
    ("MIT", [("a", APACHE)], False),
    ("MIT", [("m", "Permission is hereby granted, free of charge")], False),
    ("MIT OR Apache-2.0", [("a", APACHE)], True),
    ("MIT OR Apache-2.0", [("a", "Licensed under the Apache License, Version 2.0")], False),
    ("MIT/Apache-2.0", [("m", MIT)], True),
    ("(MIT OR Apache-2.0) AND OFL-1.1", [("a", APACHE)], False),
    ("(MIT OR Apache-2.0) AND OFL-1.1", [("a", APACHE), ("o", OFL)], True),
    ("Apache-2.0 WITH LLVM-exception OR MIT", [("a", APACHE)], False),
    ("Apache-2.0 WITH LLVM-exception OR MIT", [("a", APACHE + "\n" + LLVM)], True),
    ("Apache-2.0 WITH LLVM-exception OR MIT", [("m", MIT)], True),
    ("BSD-3-Clause", [("b", BSD2)], False),
    ("BSD-3-Clause", [("b", BSD3)], True),
    ("BSD-2-Clause", [("b", BSD3)], False),
    ("BSD-2-Clause", [("b", BSD2)], True),
    ("ISC", [("z", ZERO_BSD)], False),
    ("0BSD", [("i", ISC)], False),
    ("0BSD", [("z", ZERO_BSD)], True),
]
for expr, texts, want in cases:
    got = tp.satisfied(expr, texts)
    assert got == want, (expr, texts, got)
for bad in ["Foo-1.0", "MIT WITH Foo-exception"]:
    try:
        tp.satisfied(bad, [("m", MIT)])
        raise AssertionError(bad)
    except SystemExit:
        pass
# The fonts without Hack's notice: missing.
fonts = [("UFL.txt", "UBUNTU FONT LICENCE Version 1.0"), ("OFL.txt", OFL),
         ("emoji.txt", "Copyright (c) 2014 John Slegers " + MIT)]
assert tp.missing_parts("epaint_default_fonts", fonts) == ["Hack"], tp.missing_parts("epaint_default_fonts", fonts)
fonts.append(("Hack.txt", "Source Foundry Authors " + MIT + " Bitstream Vera"))
assert tp.missing_parts("epaint_default_fonts", fonts) == []
print("ok")
"#;
    let o = Command::new("python3").arg("-c").arg(probe).arg(root().join("scripts/third-party.py")).output().unwrap();
    assert!(o.status.success() && text(&o.stdout).trim() == "ok", "{}{}", text(&o.stdout), text(&o.stderr));
    // Each part of the pinned runtime, at its version.
    let notices = std::fs::read_to_string(root().join("packaging/APPIMAGE_NOTICES")).unwrap();
    for must in [
        "type2-runtime, continuous build of commit 8f39b89e",
        "musl libc 1.2.5",
        "mimalloc 2.1.7",
        "Microsoft Corporation",
        "libfuse 3.15.0",
        "GNU LESSER GENERAL PUBLIC LICENSE",
        "squashfuse 0.5.2",
        "zstd 1.5.6",
        "zlib 1.3.2",
        "Jean-loup Gailly",
        "probonopd",
    ] {
        assert!(notices.contains(must), "{must}");
    }
}

// --- The AppImage's folder (the image itself: m6_packages) ------------------

#[test]
fn the_appimage_folder_and_its_pinned_tools() {
    let dir = tempfile::tempdir().unwrap();
    let run = |envs: &[(&str, &str)], args: &[&str]| {
        Command::new("sh").arg(root().join("scripts/appimage.sh")).args(["--binary", PENGITTO, "--out"]).arg(dir.path()).args(args).envs(envs.iter().copied()).output().unwrap()
    };
    ok(&run(&[], &["--appdir-only"]));
    let app = dir.path().join("pengitto.AppDir");
    let mode = |p: &str| std::fs::metadata(app.join(p)).unwrap().permissions().mode() & 0o111 != 0;
    assert!(mode("AppRun") && mode("usr/bin/pengitto"));
    assert_eq!(std::fs::read_link(app.join(".DirIcon")).unwrap(), Path::new("pengitto.png"));
    for f in [
        "pengitto.desktop",
        "pengitto.png",
        "usr/share/applications/pengitto.desktop",
        "usr/share/doc/pengitto/LICENSE-MIT",
        "usr/share/doc/pengitto/LICENSE-APACHE",
        "usr/share/doc/pengitto/THIRD_PARTY_LICENSES",
        "usr/share/doc/pengitto/APPIMAGE_NOTICES",
    ] {
        assert!(app.join(f).is_file(), "{f}");
    }
    for n in ICON_SIZES {
        assert!(app.join(format!("usr/share/icons/hicolor/{n}x{n}/apps/pengitto.png")).is_file());
    }
    assert!(!app.join("usr/lib").exists(), "no libraries bundled");
    let desktop = std::fs::read_to_string(app.join("pengitto.desktop")).unwrap();
    assert_eq!(exec_argv(&desktop), ["pengitto", "--", "%F"]);
    // Tools that aren't the pinned ones: refused, saying where to get them.
    let out = failed(&run(&[], &[]));
    assert!(out.contains("tools.lock") || out.contains("sha256"), "{out}");
    let out = failed(&run(&[("APPIMAGETOOL", "/bin/true"), ("APPIMAGE_RUNTIME", "/bin/true")], &[]));
    assert!(out.contains("isn't the pinned appimagetool"), "{out}");
}

// --- Installing --------------------------------------------------------------

/// A throwaway home: its data folder, and desktop cache tools that only
/// note how they were called.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let home = Home { dir: tempfile::tempdir().unwrap() };
        std::fs::create_dir_all(home.path("home")).unwrap();
        std::fs::create_dir_all(home.path("fakebin")).unwrap();
        for tool in ["update-desktop-database", "gtk-update-icon-cache", "kbuildsycoca6"] {
            executable(&home.path("fakebin").join(tool), &format!("#!/bin/sh\necho \"{tool} $*\" >> '{}'\n", home.path("calls").display()));
        }
        home
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn data(&self) -> PathBuf {
        self.path("data")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.path("calls")).unwrap_or_default()
    }

    fn command(&self, script: &str, args: &[&str], env: &[(&str, &Path)]) -> Command {
        let path = format!("{}:{}", self.path("fakebin").display(), std::env::var("PATH").unwrap());
        let mut c = Command::new("sh");
        c.arg(root().join("scripts").join(script))
            .args(args)
            .env("HOME", self.path("home"))
            .env("XDG_DATA_HOME", self.data())
            .env("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
            .env("PATH", path)
            .env("TMPDIR", self.dir.path());
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn run_with(&self, script: &str, args: &[&str], env: &[(&str, &Path)]) -> Output {
        self.command(script, args, env).output().unwrap()
    }

    fn run(&self, script: &str, args: &[&str]) -> Output {
        self.run_with(script, args, &[])
    }

    fn install(&self, args: &[&str]) -> Output {
        let mut all = vec!["--binary", PENGITTO];
        all.extend(args);
        self.run("install.sh", &all)
    }

    /// An install or uninstall started, its output kept.
    fn start(&self, script: &str, args: &[&str], env: &[(&str, &Path)]) -> std::process::Child {
        self.command(script, args, env).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }
}

/// A stand-in for pengitto that passes its checks (`body` for anything else).
fn fake_pengitto(path: &Path, version: &str, body: &str) {
    executable(
        path,
        &format!("#!/bin/sh\ncase $1 in\n--version) echo 'pengitto {version}'; exit 0 ;;\n--self-check) echo 'all fine'; exit 0 ;;\nesac\n{body}\n"),
    );
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the shell `pid` waits for the installations' lock (its `flock`
/// is running).
fn waits_for_the_lock(pid: u32) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().any(|e| {
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { return false };
        let Some((head, rest)) = stat.rsplit_once(") ") else { return false };
        let mut fields = rest.split(' ');
        let (state, ppid) = (fields.next(), fields.next());
        head.ends_with("(flock") && state != Some("Z") && ppid == Some(&pid.to_string())
    })
}

/// Whether a run would get the lock now.
fn lock_is_free(lock: &Path) -> bool {
    Command::new("flock").arg("-n").arg(lock).arg("true").status().unwrap().success()
}

fn finished(child: std::process::Child) -> Output {
    child.wait_with_output().unwrap()
}

fn ok(o: &Output) -> String {
    let out = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(o.status.success(), "{out}");
    out
}

fn failed(o: &Output) -> String {
    let out = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(!o.status.success(), "{out}");
    out
}

const ICON_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];

fn installed_files(bin: &Path, data: &Path) -> Vec<PathBuf> {
    let mut files = vec![bin.join("pengitto"), data.join("applications/pengitto.desktop")];
    files.extend(ICON_SIZES.map(|n| data.join(format!("icons/hicolor/{n}x{n}/apps/pengitto.png"))));
    files.extend(["LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_LICENSES"].map(|f| data.join("doc/pengitto").join(f)));
    files
}

/// Leftover temporary files of the installation, anywhere under `dir`.
fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() && !p.is_symlink() {
                todo.push(p);
            } else if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".pengitto-install")) {
                found.push(p);
            }
        }
    }
    found
}

/// The arguments the Exec key of a desktop file stands for, by the Desktop
/// Entry spec: string escapes undone first, then the quoting.
fn exec_argv(desktop: &str) -> Vec<String> {
    let value = desktop.lines().find_map(|l| l.strip_prefix("Exec=")).expect("an Exec key");
    let mut unescaped = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        unescaped.push(match c {
            '\\' => match chars.next() {
                Some('s') => ' ',
                Some('n') => '\n',
                Some('t') => '\t',
                Some('r') => '\r',
                Some('\\') => '\\',
                other => panic!("not a string escape: \\{other:?}"),
            },
            c => c,
        });
    }
    let (mut args, mut arg, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    let mut chars = unescaped.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => arg.push(chars.next().filter(|n| "\"`$\\".contains(*n)).expect("a quoting escape")),
            ' ' if !quoted => {
                if any || !arg.is_empty() {
                    args.push(std::mem::take(&mut arg));
                }
                any = false;
            }
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                arg.push('%');
            }
            c => arg.push(c),
        }
    }
    assert!(!quoted, "an unclosed quote");
    if any || !arg.is_empty() {
        args.push(arg);
    }
    args
}

#[test]
fn installs_into_the_data_folder_and_again_the_same() {
    let h = Home::new();
    let out = ok(&h.install(&[]));
    let (bin, data) = (h.path("home/.local/bin"), h.data());
    for f in installed_files(&bin, &data) {
        assert!(f.is_file(), "{} in\n{out}", f.display());
    }
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&bin.join("pengitto")), 0o755);
    assert_eq!(mode(&data.join("applications/pengitto.desktop")), 0o644);
    assert_eq!(std::fs::read(bin.join("pengitto")).unwrap(), std::fs::read(PENGITTO).unwrap());
    let desktop = std::fs::read_to_string(data.join("applications/pengitto.desktop")).unwrap();
    assert_eq!(exec_argv(&desktop), [bin.join("pengitto").to_str().unwrap(), "--", "%F"]);
    if Command::new("desktop-file-validate").arg("--help").output().is_ok() {
        ok(&Command::new("desktop-file-validate").arg(data.join("applications/pengitto.desktop")).output().unwrap());
    }
    // The record: every file, with its hash.
    let manifest = std::fs::read_to_string(data.join("pengitto/install-manifest")).unwrap();
    assert_eq!(manifest.lines().count(), installed_files(&bin, &data).len());
    assert!(out.contains("isn't on your PATH"), "{out}");
    // No cache was there: none is made (KDE's is refreshed).
    assert_eq!(h.calls().trim(), "kbuildsycoca6");
    // Again: the same, nothing left over.
    let before = std::fs::read(data.join("pengitto/install-manifest")).unwrap();
    ok(&h.install(&[]));
    assert_eq!(std::fs::read(data.join("pengitto/install-manifest")).unwrap(), before);
    assert!(leftovers(h.dir.path()).is_empty(), "{:?}", leftovers(h.dir.path()));
    // Caches that are there are kept current; none with --no-integration.
    std::fs::write(data.join("applications/mimeinfo.cache"), "").unwrap();
    std::fs::create_dir_all(data.join("icons/hicolor")).unwrap();
    std::fs::write(data.join("icons/hicolor/icon-theme.cache"), "").unwrap();
    std::fs::remove_file(h.path("calls")).unwrap();
    ok(&h.install(&[]));
    let calls = h.calls();
    assert!(calls.contains(&format!("update-desktop-database {}", data.join("applications").display())), "{calls}");
    assert!(calls.contains(&format!("gtk-update-icon-cache -q -f -t {}", data.join("icons/hicolor").display())), "{calls}");
    std::fs::remove_file(h.path("calls")).unwrap();
    ok(&h.install(&["--no-integration"]));
    assert_eq!(h.calls(), "");
}

#[test]
fn nothing_is_overwritten_that_pengitto_didnt_put_there() {
    let h = Home::new();
    let data = h.data();
    // Someone else's file where the menu entry goes.
    std::fs::create_dir_all(data.join("applications")).unwrap();
    std::fs::write(data.join("applications/pengitto.desktop"), "theirs").unwrap();
    let out = failed(&h.install(&[]));
    assert!(out.contains("weren't put there by pengitto") && out.contains("pengitto.desktop"), "{out}");
    assert!(!h.path("home/.local/bin/pengitto").exists(), "nothing installed");
    assert_eq!(std::fs::read_to_string(data.join("applications/pengitto.desktop")).unwrap(), "theirs");
    assert!(leftovers(h.dir.path()).is_empty());
    ok(&h.install(&["--force"]));
    assert_ne!(std::fs::read_to_string(data.join("applications/pengitto.desktop")).unwrap(), "theirs");
    // One of ours, changed since: not overwritten either.
    std::fs::write(data.join("doc/pengitto/THIRD_PARTY_LICENSES"), "edited").unwrap();
    let out = failed(&h.install(&[]));
    assert!(out.contains("THIRD_PARTY_LICENSES"), "{out}");
    assert_eq!(std::fs::read_to_string(data.join("doc/pengitto/THIRD_PARTY_LICENSES")).unwrap(), "edited");
    // A folder or a link in the way: refused.
    std::fs::remove_file(data.join("doc/pengitto/THIRD_PARTY_LICENSES")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", data.join("doc/pengitto/THIRD_PARTY_LICENSES")).unwrap();
    let out = failed(&h.install(&[]));
    assert!(out.contains("not a plain file"), "{out}");
}

#[test]
fn a_bad_executable_or_place_installs_nothing() {
    let h = Home::new();
    // An executable that doesn't start (as one needing a newer glibc).
    let broken = h.path("broken");
    executable(&broken, "#!/bin/sh\nexit 127\n");
    let out = failed(&h.run("install.sh", &["--binary", broken.to_str().unwrap()]));
    assert!(out.contains("doesn't start"), "{out}");
    assert!(!h.data().join("applications/pengitto.desktop").exists());
    // A place where a step fails before anything is put in place.
    let icons = h.data().join("icons/hicolor/512x512/apps");
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::set_permissions(&icons, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = failed(&h.install(&[]));
    std::fs::set_permissions(&icons, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!h.path("home/.local/bin/pengitto").exists(), "nothing put in place: {out}");
    assert!(leftovers(h.dir.path()).is_empty(), "{:?}", leftovers(h.dir.path()));
    // Paths a menu entry can't hold.
    for bad in ["with=equals", "with%percent", "with\nbreak", "with\ttab"] {
        let prefix = h.path(bad);
        let out = failed(&h.install(&["--prefix", prefix.to_str().unwrap()]));
        assert!(out.contains("can't"), "{bad:?}: {out}");
        assert!(!prefix.join("bin/pengitto").exists(), "{bad:?}");
    }
}

#[test]
fn an_installation_cut_short_is_finished_by_the_next() {
    let h = Home::new();
    ok(&h.install(&[]));
    let state = h.data().join("pengitto");
    // As if it stopped before its record was final.
    std::fs::rename(state.join("install-manifest"), state.join("install-manifest.pending")).unwrap();
    ok(&h.install(&[]));
    assert!(state.join("install-manifest").is_file() && !state.join("install-manifest.pending").exists());
    // A file an earlier version installed (and this one doesn't): removed
    // if it is as installed.
    let old = h.data().join("icons/hicolor/1024x1024/apps/pengitto.png");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, "old icon").unwrap();
    let hash = text(&Command::new("sha256sum").arg(&old).output().unwrap().stdout).split(' ').next().unwrap().to_owned();
    let mut manifest = std::fs::read_to_string(state.join("install-manifest")).unwrap();
    manifest.push_str(&format!("{hash}\t{}\n", old.display()));
    std::fs::write(state.join("install-manifest"), manifest).unwrap();
    ok(&h.install(&[]));
    assert!(!old.exists());
}

#[test]
fn installations_and_uninstallations_take_turns() {
    use std::os::unix::fs::MetadataExt as _;
    let h = Home::new();
    ok(&h.install(&[]));
    let lock = h.data().join("pengitto/install.lock");
    let inode = std::fs::metadata(&lock).unwrap().ino();
    // An uninstallation holding the lock, an installation waiting for it.
    let (gate_u, gate_i) = (h.path("gate-u"), h.path("gate-i"));
    std::fs::write(&gate_u, "").unwrap();
    std::fs::write(&gate_i, "").unwrap();
    let u = h.start("uninstall.sh", &[], &[("PENGITTO_INSTALL_PAUSE", &gate_u)]);
    wait_until("the uninstallation holds the lock", || !lock_is_free(&lock));
    let mut i = h.start("install.sh", &["--binary", PENGITTO], &[("PENGITTO_INSTALL_PAUSE", &gate_i)]);
    wait_until("the installation waits", || waits_for_the_lock(i.id()));
    std::fs::remove_file(&gate_u).unwrap();
    assert!(text(&ok_out(finished(u))).contains("Removed:"));
    // The installation has the lock now: the same one anyone else would
    // take (none goes ahead beside it).
    wait_until("the installation has the lock", || !waits_for_the_lock(i.id()) || i.try_wait().unwrap().is_some());
    assert!(!lock_is_free(&lock), "another run would go ahead beside the installation");
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
    std::fs::remove_file(&gate_i).unwrap();
    ok_out(finished(i));
    assert!(h.path("home/.local/bin/pengitto").is_file());

    // Nothing recorded yet (an installation under way): an uninstallation
    // waits for it, then takes what it installed.
    let h = Home::new();
    let lock = h.data().join("pengitto/install.lock");
    std::fs::write(&gate_i, "").unwrap();
    let i = h.start("install.sh", &["--binary", PENGITTO], &[("PENGITTO_INSTALL_PAUSE", &gate_i)]);
    wait_until("the installation holds the lock", || lock.exists() && !lock_is_free(&lock));
    let mut u = h.start("uninstall.sh", &[], &[]);
    wait_until("the uninstallation waits (or is done)", || waits_for_the_lock(u.id()) || u.try_wait().unwrap().is_some());
    assert!(u.try_wait().unwrap().is_none(), "it didn't wait: {}", text(&finished(u).stdout));
    std::fs::remove_file(&gate_i).unwrap();
    ok_out(finished(i));
    let out = text(&ok_out(finished(u)));
    assert!(out.contains("Removed:") && out.contains(".local/bin/pengitto"), "{out}");
    assert!(!h.path("home/.local/bin/pengitto").exists());
}

fn ok_out(o: Output) -> Vec<u8> {
    assert!(o.status.success(), "{}{}", text(&o.stdout), text(&o.stderr));
    o.stdout
}

#[test]
fn a_run_cut_short_after_another_keeps_what_each_put_there() {
    let h = Home::new();
    let bin = h.path("home/.local/bin/pengitto");
    ok(&h.install(&[]));
    let (b, c, d) = (h.path("pengitto-b"), h.path("pengitto-c"), h.path("pengitto-d"));
    fake_pengitto(&b, "0.0.2", "");
    fake_pengitto(&c, "0.0.3", "");
    fake_pengitto(&d, "0.0.4", "");
    // B stops having put its executable in place; C stops before putting
    // anything (its record written).
    for (exe, placed) in [(&b, "1"), (&c, "0")] {
        let stop = [("PENGITTO_INSTALL_STOP_AFTER", Path::new(placed))];
        let out = failed(&h.run_with("install.sh", &["--binary", exe.to_str().unwrap()], &stop));
        assert!(out.contains("stopped"), "{out}");
    }
    assert_eq!(std::fs::read(&bin).unwrap(), std::fs::read(&b).unwrap());
    // D finds B's executable still pengitto's own.
    ok(&h.run("install.sh", &["--binary", d.to_str().unwrap()]));
    assert_eq!(std::fs::read(&bin).unwrap(), std::fs::read(&d).unwrap());
    let state = h.data().join("pengitto");
    assert!(!state.join("install-manifest.pending").exists());
    let manifest = std::fs::read_to_string(state.join("install-manifest")).unwrap();
    assert_eq!(manifest.lines().count(), installed_files(bin.parent().unwrap(), &h.data()).len(), "{manifest}");
}

#[test]
fn an_executable_failing_its_self_check_installs_nothing() {
    let h = Home::new();
    let exe = h.path("pengitto-x");
    executable(&exe, "#!/bin/sh\ncase $1 in\n--version) echo 'pengitto 0.0.9' ;;\n--self-check) echo 'git: too old here'; exit 1 ;;\nesac\n");
    let out = failed(&h.run("install.sh", &["--binary", exe.to_str().unwrap()]));
    assert!(out.contains("self-check failed") && out.contains("git: too old here"), "{out}");
    for f in installed_files(&h.path("home/.local/bin"), &h.data()) {
        assert!(!f.exists(), "{}", f.display());
    }
    assert!(leftovers(h.dir.path()).is_empty(), "{:?}", leftovers(h.dir.path()));
    // A package's build (no record) doesn't run it: its build needn't have
    // git.
    let staging = h.path("buildroot/usr");
    ok(&h.run("install.sh", &["--binary", exe.to_str().unwrap(), "--prefix", staging.to_str().unwrap(), "--no-manifest", "--no-integration"]));
    assert!(staging.join("bin/pengitto").is_file());
}

#[test]
fn forced_or_not_nothing_goes_through_a_folder_or_a_link_to_one() {
    let h = Home::new();
    ok(&h.install(&[]));
    let data = h.data();
    let outside = h.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // A link to a folder where the licenses go: replaced with --force (the
    // folder it points to untouched).
    let licenses = data.join("doc/pengitto/THIRD_PARTY_LICENSES");
    std::fs::remove_file(&licenses).unwrap();
    std::os::unix::fs::symlink(&outside, &licenses).unwrap();
    let out = failed(&h.install(&[]));
    assert!(out.contains("not a plain file"), "{out}");
    ok(&h.install(&["--force"]));
    assert!(licenses.is_file() && !licenses.is_symlink());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "written into the folder the link pointed to");
    // A folder where the menu entry goes: never, --force or not.
    let entry = data.join("applications/pengitto.desktop");
    std::fs::remove_file(&entry).unwrap();
    std::fs::create_dir(&entry).unwrap();
    let out = failed(&h.install(&["--force"]));
    assert!(out.contains("folders where files go") && out.contains("pengitto.desktop"), "{out}");
    assert_eq!(std::fs::read_dir(&entry).unwrap().count(), 0);
    assert!(leftovers(h.dir.path()).is_empty(), "{:?}", leftovers(h.dir.path()));
}

#[test]
fn a_prefix_and_the_menus() {
    let h = Home::new();
    // A folder menus don't look in: said so.
    let prefix = h.path("opt");
    let out = ok(&h.install(&["--prefix", prefix.to_str().unwrap()]));
    assert!(prefix.join("bin/pengitto").is_file() && prefix.join("share/applications/pengitto.desktop").is_file());
    assert!(out.contains("menus don't look in"), "{out}");
    // Folders with spaces menus do look in: not said.
    let spaced = h.path("my apps");
    let share = spaced.join("share");
    std::fs::create_dir_all(&share).unwrap();
    let dirs = format!("/usr/share:{}", share.display());
    let out = ok(&h.run_with("install.sh", &["--binary", PENGITTO, "--prefix", spaced.to_str().unwrap()], &[("XDG_DATA_DIRS", Path::new(&dirs))]));
    assert!(!out.contains("menus don't look in"), "{out}");
    let out = ok(&h.run_with("install.sh", &["--binary", PENGITTO, "--prefix", spaced.to_str().unwrap()], &[("XDG_DATA_HOME", &share)]));
    assert!(!out.contains("menus don't look in"), "{out}");
    // Characters the menu entry must quote: read back as they were.
    let odd = h.path("a $b \"c\" `d` & \\e 日本 'q'");
    ok(&h.install(&["--prefix", odd.to_str().unwrap()]));
    let desktop = std::fs::read_to_string(odd.join("share/applications/pengitto.desktop")).unwrap();
    assert_eq!(exec_argv(&desktop), [odd.join("bin/pengitto").to_str().unwrap(), "--", "%F"]);
    // What a package does: another path to run, no record, no caches.
    let staging = h.path("buildroot/usr");
    ok(&h.install(&["--prefix", staging.to_str().unwrap(), "--exec", "/usr/bin/pengitto", "--no-manifest", "--no-integration"]));
    let desktop = std::fs::read_to_string(staging.join("share/applications/pengitto.desktop")).unwrap();
    assert_eq!(exec_argv(&desktop), ["/usr/bin/pengitto", "--", "%F"]);
    assert!(!staging.join("share/pengitto").exists(), "nothing of the installation's own");
}

#[test]
fn a_launcher_runs_it_with_the_files_as_they_are() {
    if Command::new("gio").arg("help").output().is_err() {
        eprintln!("skipped: no gio (apt: libglib2.0-bin)");
        return;
    }
    let h = Home::new();
    // A stand-in that notes its arguments (and passes as pengitto).
    let fake = h.path("fake-pengitto");
    let argv = h.path("argv");
    fake_pengitto(&fake, "0.0.0", &format!("for a in \"$0\" \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'", argv.display()));
    let prefix = h.path("a $b \"c\" `d` & \\e 日本 'q'");
    ok(&h.run("install.sh", &["--binary", fake.to_str().unwrap(), "--prefix", prefix.to_str().unwrap()]));
    let file = h.path("my file.txt");
    let dashed = h.path("-x");
    std::fs::write(&file, "").unwrap();
    std::fs::write(&dashed, "").unwrap();
    let o = Command::new("gio")
        .arg("launch")
        .arg(prefix.join("share/applications/pengitto.desktop"))
        .arg(&file)
        .arg(&dashed)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    let start = Instant::now();
    while !argv.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let got = std::fs::read_to_string(&argv).unwrap();
    let want = [prefix.join("bin/pengitto"), "--".into(), file, dashed].map(|p| format!("{}\n", p.display())).concat();
    assert_eq!(got, want);
}

#[test]
fn uninstalling_takes_only_what_was_installed_as_it_was() {
    let h = Home::new();
    ok(&h.install(&[]));
    let (bin, data) = (h.path("home/.local/bin"), h.data());
    std::fs::write(data.join("pengitto/state.ron"), "(recent: [])").unwrap();
    std::fs::write(data.join("pengitto/app.ron"), "{}").unwrap();
    std::fs::write(data.join("applications/other.desktop"), "someone else's").unwrap();
    std::fs::write(data.join("icons/hicolor/48x48/apps/pengitto.png"), "edited").unwrap();
    std::fs::write(data.join("applications/mimeinfo.cache"), "").unwrap();
    std::fs::remove_file(h.path("calls")).ok();
    let out = ok(&h.run("uninstall.sh", &[]));
    for f in installed_files(&bin, &data) {
        let edited = f.ends_with("48x48/apps/pengitto.png");
        assert_eq!(f.exists(), edited, "{}", f.display());
    }
    assert!(out.contains("Kept") && out.contains("48x48"), "{out}");
    assert!(out.contains("settings and recent repositories are kept"), "{out}");
    for kept in ["pengitto/state.ron", "pengitto/app.ron", "applications/other.desktop"] {
        assert!(data.join(kept).exists(), "{kept}");
    }
    assert!(!data.join("doc/pengitto").exists());
    // (The lock stays: see installations_and_uninstallations_take_turns.)
    assert!(data.join("pengitto/install.lock").exists());
    assert!(h.calls().contains("update-desktop-database"), "caches refreshed: {}", h.calls());
    // What was kept is still recorded: kept again, and --force takes it
    // (no installation in between).
    let manifest = std::fs::read_to_string(data.join("pengitto/install-manifest")).unwrap();
    assert_eq!(manifest.lines().count(), 1, "{manifest}");
    assert!(manifest.contains("48x48"), "{manifest}");
    let out = ok(&h.run("uninstall.sh", &[]));
    assert!(out.contains("Kept") && out.contains("48x48"), "{out}");
    let out = ok(&h.run("uninstall.sh", &["--force"]));
    assert!(out.contains("48x48") && !out.contains("Kept"), "{out}");
    assert!(!data.join("icons/hicolor/48x48/apps/pengitto.png").exists());
    assert!(!data.join("pengitto/install-manifest").exists());
    // Nothing recorded: nothing to do.
    let out = ok(&h.run("uninstall.sh", &[]));
    assert!(out.contains("isn't installed"), "{out}");
}

// --- Taking over from the former name (M7a) ---------------------------------

/// An installation under pengitto's former name, as its scripts left one: its
/// files, its record (`record`: the manifest, or only the record of a run cut
/// short), its lock and its settings.
struct Former {
    files: Vec<PathBuf>,
    dir: PathBuf,
}

fn sha256(path: &Path) -> String {
    text(&Command::new("sha256sum").arg(path).output().unwrap().stdout).split(' ').next().unwrap().to_owned()
}

fn former_install(h: &Home, record: &str) -> Former {
    let app = pengitto_core::legacy::APP;
    let data = h.data();
    let files = vec![
        h.path("home/.local/bin").join(app),
        data.join(format!("applications/{app}.desktop")),
        data.join(format!("icons/hicolor/48x48/apps/{app}.png")),
        data.join(format!("doc/{app}/THIRD_PARTY_LICENSES")),
    ];
    let dir = data.join(app);
    std::fs::create_dir_all(&dir).unwrap();
    let mut lines = String::new();
    for f in &files {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, format!("the former {}\n", f.display())).unwrap();
        lines.push_str(&format!("{}\t{}\n", sha256(f), f.display()));
    }
    std::fs::write(dir.join(record), lines).unwrap();
    std::fs::write(dir.join("install.lock"), "").unwrap();
    std::fs::write(dir.join("state.ron"), "(recent: [])").unwrap();
    Former { files, dir }
}

#[test]
fn the_former_installation_goes_once_pengittos_is_in_place() {
    use std::os::unix::fs::MetadataExt as _;
    let h = Home::new();
    let former = former_install(&h, "install-manifest");
    let inode = std::fs::metadata(former.dir.join("install.lock")).unwrap().ino();
    // One of its files changed since: it stays, and in its record.
    let changed = &former.files[2];
    std::fs::write(changed, "edited").unwrap();
    let out = ok(&h.install(&[]));
    for f in installed_files(&h.path("home/.local/bin"), &h.data()) {
        assert!(f.is_file(), "{}", f.display());
    }
    for f in &former.files {
        assert_eq!(f.exists(), f == changed, "{}", f.display());
    }
    assert!(out.contains("Removed what") && out.contains("Kept of") && out.contains(&changed.display().to_string()), "{out}");
    let record = std::fs::read_to_string(former.dir.join("install-manifest")).unwrap();
    assert_eq!(record.lines().count(), 1, "{record}");
    assert!(record.ends_with(&format!("\t{}\n", changed.display())), "{record}");
    // Its settings and its lock (the same file) stay; its empty doc folder
    // goes.
    assert!(former.dir.join("state.ron").is_file());
    assert_eq!(std::fs::metadata(former.dir.join("install.lock")).unwrap().ino(), inode);
    assert!(!former.files[3].parent().unwrap().exists());
    // Again: nothing more to do, the changed file still there.
    let out = ok(&h.install(&[]));
    assert!(!out.contains("Removed what") && changed.exists(), "{out}");
}

#[test]
fn the_former_installation_stays_if_pengittos_fails() {
    let h = Home::new();
    let former = former_install(&h, "install-manifest");
    let record = std::fs::read(former.dir.join("install-manifest")).unwrap();
    let exe = h.path("pengitto-x");
    executable(&exe, "#!/bin/sh\ncase $1 in\n--version) echo 'pengitto 0.0.9' ;;\n--self-check) echo 'no git'; exit 1 ;;\nesac\n");
    failed(&h.run("install.sh", &["--binary", exe.to_str().unwrap()]));
    for f in &former.files {
        assert!(f.is_file(), "{}", f.display());
    }
    assert_eq!(std::fs::read(former.dir.join("install-manifest")).unwrap(), record);
}

#[test]
fn tidying_the_former_one_cut_short_is_finished_next_time() {
    let h = Home::new();
    // Its record only of a run cut short, too.
    let former = former_install(&h, "install-manifest.pending");
    let stop = [("PENGITTO_INSTALL_STOP_AT", Path::new("legacy"))];
    let out = failed(&h.run_with("install.sh", &["--binary", PENGITTO], &stop));
    assert!(out.contains("stopped"), "{out}");
    assert!(h.path("home/.local/bin/pengitto").is_file(), "pengitto in place");
    assert!(former.files.iter().all(|f| f.exists()), "the former one not touched yet");
    ok(&h.install(&[]));
    assert!(former.files.iter().all(|f| !f.exists()));
    assert!(!former.dir.join("install-manifest.pending").exists() && !former.dir.join("install-manifest").exists());
}

#[test]
fn installing_waits_for_the_former_names_lock() {
    let h = Home::new();
    let former = former_install(&h, "install-manifest");
    let lock = former.dir.join("install.lock");
    let gate = h.path("gate");
    std::fs::write(&gate, "").unwrap();
    // Held as its own scripts would hold it.
    let mut holder = Command::new("flock")
        .arg(&lock)
        .args(["sh", "-c", &format!("while [ -e '{}' ]; do sleep 0.02; done", gate.display())])
        .spawn()
        .unwrap();
    wait_until("the lock is held", || !lock_is_free(&lock));
    let mut i = h.start("install.sh", &["--binary", PENGITTO], &[]);
    wait_until("the installation waits", || waits_for_the_lock(i.id()) || i.try_wait().unwrap().is_some());
    assert!(i.try_wait().unwrap().is_none(), "it didn't wait: {}", text(&finished(i).stderr));
    assert!(former.files.iter().all(|f| f.exists()));
    std::fs::remove_file(&gate).unwrap();
    holder.wait().unwrap();
    ok_out(finished(i));
    assert!(former.files.iter().all(|f| !f.exists()));
}

/// The former name's installation may begin after this one did (its folder
/// not there yet when this one took its locks): it is waited for all the
/// same before anything of it is touched.
#[test]
fn a_former_installation_that_comes_later_is_waited_for_too() {
    let h = Home::new();
    let (started, gate, held) = (h.path("started"), h.path("gate"), h.path("held"));
    std::fs::write(&gate, "").unwrap();
    // A pengitto whose self-check waits (this run is then past its locks).
    let exe = h.path("pengitto-slow");
    executable(
        &exe,
        &format!(
            "#!/bin/sh\ncase $1 in\n--version) echo 'pengitto 0.0.8' ;;\n--self-check) : > '{}'; while [ -e '{}' ]; do sleep 0.02; done; echo ok ;;\nesac\n",
            started.display(),
            gate.display()
        ),
    );
    let mut i = h.start("install.sh", &["--binary", exe.to_str().unwrap()], &[]);
    wait_until("the self-check runs", || started.exists());
    // Now the former name's installation, its lock held.
    let former = former_install(&h, "install-manifest.pending");
    let lock = former.dir.join("install.lock");
    std::fs::write(&held, "").unwrap();
    let mut holder = Command::new("flock")
        .arg(&lock)
        .args(["sh", "-c", &format!("while [ -e '{}' ]; do sleep 0.02; done", held.display())])
        .spawn()
        .unwrap();
    wait_until("the lock is held", || !lock_is_free(&lock));
    std::fs::remove_file(&gate).unwrap();
    wait_until("the installation waits", || waits_for_the_lock(i.id()) || i.try_wait().unwrap().is_some());
    assert!(i.try_wait().unwrap().is_none(), "it didn't wait: {}", text(&finished(i).stdout));
    assert!(former.files.iter().all(|f| f.exists()), "nothing of it touched meanwhile");
    std::fs::remove_file(&held).unwrap();
    holder.wait().unwrap();
    ok_out(finished(i));
    assert!(former.files.iter().all(|f| !f.exists()));
}
