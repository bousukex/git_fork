//! Repository housekeeping that speeds up reading.

use std::path::PathBuf;

use anyhow::Result;
use bstr::ByteSlice;

use crate::runner::GitRunner;

fn git_path(git: &GitRunner, name: &str) -> Option<PathBuf> {
    let out = git.output(["rev-parse", "--path-format=absolute", "--git-path", name]).ok()?;
    Some(PathBuf::from(out.trim().to_str().ok()?))
}

/// Without a commit-graph, `git log --date-order` has to walk the entire
/// history before printing the first commit (seconds on large repositories).
pub fn has_commit_graph(git: &GitRunner) -> bool {
    ["objects/info/commit-graph", "objects/info/commit-graphs/commit-graph-chain"]
        .iter()
        .any(|p| git_path(git, p).is_some_and(|p| p.exists()))
}

/// Writes the same cache file `git gc` / `git maintenance` would.
pub fn write_commit_graph(git: &GitRunner) -> Result<()> {
    git.output(["commit-graph", "write", "--reachable"])?;
    Ok(())
}

/// Lock files left behind (e.g. by a git that was killed). Reported to the
/// user; never removed automatically, since another git may still run.
pub fn leftover_locks(paths: &crate::RepoPaths) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in [&paths.git_dir, &paths.common_dir] {
        for name in ["index.lock", "HEAD.lock", "packed-refs.lock", "config.lock"] {
            let p = dir.join(name);
            if p.exists() && !found.contains(&p) {
                found.push(p);
            }
        }
    }
    let mut stack = vec![paths.common_dir.join("refs")];
    if paths.git_dir != paths.common_dir {
        stack.push(paths.git_dir.join("refs"));
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "lock") {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}
