//! The user's global git configuration: who commits (user.name and
//! user.email). Git reads two global files — `~/.config/git/config`, then
//! `~/.gitconfig` (or `$GIT_CONFIG_GLOBAL` instead) — and the last value
//! wins; `git config --global` writes one of them. So values are read the
//! way commits see them, and every write is checked by reading it back.

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Result, anyhow, bail};

use crate::GitRunner;

/// Who commits, as configured globally (`None`: not set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// A runner for global settings (outside any repository).
pub fn global_runner() -> GitRunner {
    GitRunner::new(PathBuf::from("/"))
}

/// The global value in effect, and the file it comes from. (Not `--global
/// --get`: git before 2.46 then reads only one of the two files.)
fn get_global(git: &GitRunner, key: &str) -> Result<Option<(String, String)>> {
    // Exit code 1: the key isn't set anywhere.
    let out = git.output_with(["config", "-z", "--show-scope", "--show-origin", "--get-all", key], &[1])?;
    let fields: Vec<String> = out.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned()).collect();
    Ok(fields
        .chunks_exact(3)
        .rfind(|f| f[0] == "global")
        .map(|f| (f[1].strip_prefix("file:").unwrap_or(&f[1]).to_owned(), f[2].clone())))
}

fn get(git: &GitRunner, key: &str) -> Result<Option<String>> {
    Ok(get_global(git, key)?.map(|(_, v)| v).filter(|v| !v.is_empty()))
}

pub fn global_identity(git: &GitRunner) -> Result<Identity> {
    Ok(Identity { name: get(git, "user.name")?, email: get(git, "user.email")? })
}

/// What writing an identity did, field by field (one can fail alone).
#[derive(Debug)]
pub struct IdentityWrite {
    /// `None`: not asked to change.
    pub name: Option<Result<()>>,
    pub email: Option<Result<()>>,
}

impl IdentityWrite {
    pub fn all_ok(&self) -> bool {
        [&self.name, &self.email].iter().all(|r| r.as_ref().is_none_or(Result::is_ok))
    }
}

/// A value git can store on one line (surrounding blanks are dropped).
pub fn check_value(what: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.contains(['\n', '\r', '\0']) {
        bail!("{what} must be a single line");
    }
    Ok(value.to_owned())
}

fn set(git: &GitRunner, key: &str, value: &str) -> Result<()> {
    let value = check_value(key, value)?;
    if value.is_empty() {
        // Exit code 5: there was nothing to unset.
        git.output_with(["config", "--global", "--unset-all", key], &[5])?;
    } else {
        // `--` keeps a value like "-x" from being read as an option;
        // --replace-all also handles a key set more than once.
        git.output(["config", "--global", "--replace-all", "--", key, &value])?;
    }
    // git reads two global files (~/.gitconfig and ~/.config/git/config)
    // but writes one: a value in the other one may still apply. (If it
    // can't be read back, the write itself still worked.)
    let Ok(now) = get_global(git, key) else { return Ok(()) };
    match now {
        Some((origin, v)) if value.is_empty() => {
            Err(anyhow!("{key} is still \"{v}\", set in {origin} (only the main global file was changed)"))
        }
        Some((origin, v)) if v != value => Err(anyhow!("{key} still reads \"{v}\" from {origin}")),
        None if !value.is_empty() => Err(anyhow!("{key} didn't take effect")),
        _ => Ok(()),
    }
}

/// Writes the given fields (an empty value removes the setting), one after
/// the other. Writes from anywhere in the app go one at a time.
pub fn set_global_identity(git: &GitRunner, name: Option<&str>, email: Option<&str>) -> IdentityWrite {
    static WRITING: Mutex<()> = Mutex::new(());
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    IdentityWrite { name: name.map(|v| set(git, "user.name", v)), email: email.map(|v| set(git, "user.email", v)) }
}
