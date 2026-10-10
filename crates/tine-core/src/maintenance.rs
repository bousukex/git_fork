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
