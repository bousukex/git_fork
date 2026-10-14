//! The command line: what to do, from the arguments. The ways git starts
//! Tine as a helper (the editor of an interactive rebase, askpass) are told
//! apart before this, so a prompt that looks like an option stays a prompt.

use std::{ffi::OsString, path::PathBuf};

pub const USAGE: &str = "\
Usage: tine [OPTION]... [--] [PATH]...
Opens the git repositories at PATH (a file: the one it is in), or the ones
open last time.

Options:
  --self-check  check that git and Tine's own helpers can run, then exit
  --version     print the version and exit
  --help        print this help and exit
";

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    SelfCheck,
    /// The window, opening `paths` (none: the ones open last time).
    Run { ime_probe: bool, paths: Vec<PathBuf> },
}

/// The version line (`tine 0.1.0`).
pub fn version() -> String {
    format!("tine {}", env!("CARGO_PKG_VERSION"))
}

/// Reads the arguments (without the program name). An unknown option is
/// an error (the message says which); after `--`, everything is a path.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut ime_probe = false;
    let mut paths = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg.as_encoded_bytes().starts_with(b"-") && arg != "-" {
            match arg.to_str() {
                Some("--") => options = false,
                Some("--version" | "-V") => return Ok(Command::Version),
                Some("--help" | "-h") => return Ok(Command::Help),
                Some("--self-check") => return Ok(Command::SelfCheck),
                Some("--ime-probe") => ime_probe = true,
                _ => return Err(format!("unknown option: {}", arg.to_string_lossy())),
            }
            continue;
        }
        paths.push(folder_of(PathBuf::from(arg)));
    }
    Ok(Command::Run { ime_probe, paths })
}

/// A file to open stands for the folder it is in (a file manager may pass
/// one); anything else is taken as it is.
pub fn folder_of(path: PathBuf) -> PathBuf {
    if path.is_file() {
        match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        }
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn reads_the_options() {
        assert_eq!(run(&["--version"]), Ok(Command::Version));
        assert_eq!(run(&["-h"]), Ok(Command::Help));
        assert_eq!(run(&["--self-check"]), Ok(Command::SelfCheck));
        assert_eq!(run(&[]), Ok(Command::Run { ime_probe: false, paths: vec![] }));
        assert_eq!(run(&["--ime-probe"]), Ok(Command::Run { ime_probe: true, paths: vec![] }));
        assert_eq!(run(&["a", "b"]), Ok(Command::Run { ime_probe: false, paths: vec!["a".into(), "b".into()] }));
        assert!(run(&["--frobnicate"]).unwrap_err().contains("--frobnicate"));
        assert!(run(&["-x", "a"]).is_err());
        // After `--`, anything is a path.
        assert_eq!(
            run(&["--", "-x", "--version"]),
            Ok(Command::Run { ime_probe: false, paths: vec!["-x".into(), "--version".into()] })
        );
    }

    #[test]
    fn a_file_stands_for_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("README.md");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(folder_of(file), dir.path());
        assert_eq!(folder_of(dir.path().to_path_buf()), dir.path());
        assert_eq!(folder_of("missing/thing".into()), PathBuf::from("missing/thing"));
    }
}
