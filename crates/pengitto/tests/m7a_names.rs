//! No trace of pengitto's former name is left (M7a), in what the repository
//! has (tracked or new, not ignored): file contents and paths, generated
//! files, Cargo.lock and the license texts included. Kept only where the
//! former name is needed to take over from it: `pengitto_core::legacy`, the
//! fixtures written in its format, and the lines between `legacy-names`
//! markers of a few files.

use std::{path::Path, process::Command};

/// The former name (put together so that this file isn't a trace itself).
const FORMER: &str = concat!("ti", "ne");

const MARKER: &str = concat!("legacy-names", ":");
const BEGIN: &str = concat!("legacy-names", ":begin");
const END: &str = concat!("legacy-names", ":end");

/// Whole files about the former name.
const EXEMPT_FILES: &[&str] = &["crates/pengitto-core/src/legacy.rs"];
const EXEMPT_DIRS: &[&str] = &["crates/pengitto/tests/fixtures/legacy/"];
/// The only files that may mark lines for the former name.
const MARKED_FILES: &[&str] = &["README.md", "packaging/rpm/pengitto.spec", "scripts/install.sh"];

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Where `text` has the former name as a word or part of a name: not after
/// a letter, not before a lower-case one (`x_y`, `XyZ`, `X_Y`, `x-y`, `x.y`
/// all count; `routine` doesn't).
fn traces(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    lower
        .match_indices(FORMER)
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = i.checked_sub(1).map(|j| bytes[j]);
            let after = bytes.get(i + FORMER.len()).copied();
            !before.is_some_and(|b| b.is_ascii_alphabetic()) && !after.is_some_and(|b| b.is_ascii_lowercase())
        })
        .collect()
}

/// A line that is a marker, whole (a comment line of its own): whether it
/// begins a marked part.
fn marker(line: &str) -> Option<bool> {
    let t = line.trim();
    [("# ", ""), ("<!-- ", " -->")].iter().find_map(|(open, close)| {
        let rest = t.strip_prefix(open)?.strip_suffix(close)?;
        (rest == BEGIN).then_some(true).or((rest == END).then_some(false))
    })
}

/// The traces in a file (outside marked parts), or what is wrong with its
/// markers: only in the files allowed, comment lines of their own, each part
/// begun before it ends and ended before another begins.
fn scan(path: &str, text: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut open = false;
    for (n, line) in text.lines().enumerate() {
        let at = format!("{path}:{}", n + 1);
        if line.contains(MARKER) {
            let Some(begin) = marker(line) else { return Err(format!("{at}: a marker must be a comment line of its own")) };
            if !MARKED_FILES.contains(&path) {
                return Err(format!("{at}: may not mark lines for the former name"));
            }
            if begin == open {
                return Err(format!("{at}: {}", if begin { "begins inside a marked part" } else { "ends no marked part" }));
            }
            open = begin;
        } else if !open && !traces(line).is_empty() {
            found.push(format!("{at}: {}", line.trim()));
        }
    }
    if open { Err(format!("{path}: a marked part isn't closed")) } else { Ok(found) }
}

#[test]
fn markers_are_held_to_their_rules() {
    let (begin, end) = (format!("# {BEGIN}"), format!("# {END}"));
    let old = |s: &str| s.replace("@", FORMER);
    let ok = format!("a\n{begin}\n{}\n{end}\nb {}\n", old("@ here"), old("@ there"));
    assert_eq!(scan("scripts/install.sh", &ok).unwrap().len(), 1, "outside the part only");
    assert_eq!(scan("README.md", &format!("<!-- {BEGIN} -->\n{}\n<!-- {END} -->\n", old("@"))).unwrap().len(), 0);
    for (path, text, why) in [
        ("crates/x.rs", format!("{begin}\n{end}\n"), "may not mark"),
        ("crates/x.rs", format!("{}\n{end}\n", old("const S: &str = \"@\";")), "may not mark"),
        ("scripts/install.sh", format!("{end}\n"), "ends no marked part"),
        ("scripts/install.sh", format!("{begin}\n{begin}\n{end}\n"), "begins inside"),
        ("scripts/install.sh", format!("{begin}\n{}\n", old("@")), "isn't closed"),
        ("scripts/install.sh", format!("{} # {END}\n", old("X=@")), "comment line of its own"),
        ("README.md", format!("text <!-- {BEGIN} -->\n"), "comment line of its own"),
    ] {
        let e = scan(path, &text).unwrap_err();
        assert!(e.contains(why), "{path}: {e}");
    }
}

#[test]
fn traces_are_found_as_meant() {
    let t = |s: &str| traces(&s.replace("@", FORMER)).len();
    for found in ["@", "@_core", "@-core", "@App", "@::app", "--@-sequence-editor", ".@-install", "@.desktop", "bin/@"] {
        assert_eq!(t(found), 1, "{found}");
    }
    assert_eq!(t(&FORMER.to_uppercase()), 1);
    assert_eq!(t(&format!("{}{}", FORMER[..1].to_uppercase(), &FORMER[1..])), 1);
    for not in ["rou@", "pris@", "@s", "Valen@"] {
        assert_eq!(t(not), 0, "{not}");
    }
}

#[test]
fn nothing_is_left_of_the_former_name() {
    let root = root();
    let files = Command::new("git").args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).current_dir(&root).output().unwrap();
    assert!(files.status.success());
    let mut found = Vec::new();
    let mut checked = 0;
    for path in files.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8(path.to_vec()).unwrap();
        if !root.join(&path).is_file() {
            continue; // (removed, not staged yet)
        }
        let exempt = EXEMPT_FILES.contains(&path.as_str()) || EXEMPT_DIRS.iter().any(|d| path.starts_with(d));
        if !exempt && !traces(&path).is_empty() {
            found.push(format!("{path}: in its path"));
        }
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else { continue };
        checked += 1;
        if exempt {
            continue;
        }
        found.extend(scan(&path, &text).unwrap_or_else(|e| panic!("{e}")));
    }
    assert!(checked > 150, "only {checked} files read");
    assert!(found.is_empty(), "the former name is still there:\n{}", found.join("\n"));
}
