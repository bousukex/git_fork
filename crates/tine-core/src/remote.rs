//! Fetch, pull and push. Credentials are not handled here: the caller
//! configures the runner's environment (credential helpers and ssh-agent
//! work as usual; prompts go to whatever `GIT_ASKPASS` it sets).

use std::{collections::HashMap, path::Path};

use anyhow::{Result, bail};
use bstr::ByteSlice;

use crate::{
    CancelToken, GitError,
    integrate::{Step, run_step_with},
    model::{Oid, RepoState},
    runner::GitRunner,
};

/// Which remotes to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    All,
    Remote(String),
}

/// Fetches with progress; `prune` removes remote-tracking branches that no
/// longer exist on the remote.
pub fn fetch(git: &GitRunner, target: &FetchTarget, prune: bool, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    let mut args = vec!["fetch", "--progress"];
    if prune {
        args.push("--prune");
    }
    match target {
        FetchTarget::All => args.push("--all"),
        FetchTarget::Remote(r) => {
            check_remote_name(r)?;
            args.extend(["--", r]);
        }
    }
    git.run_with_progress(&args, &CancelToken::default(), on_progress)?;
    Ok(())
}

fn check_remote_name(remote: &str) -> Result<()> {
    if remote.is_empty() || remote.starts_with('-') {
        bail!("invalid remote name '{remote}'");
    }
    Ok(())
}

/// `pull.rebase` as a yes/no default for the pull dialog.
pub fn pull_rebases_by_default(git: &GitRunner) -> bool {
    match git.output_with(["config", "--get", "pull.rebase"], &[1]) {
        Ok(v) => {
            let v = v.trim().to_str_lossy().to_ascii_lowercase();
            !(v.is_empty() || v == "false" || v == "no" || v == "off" || v == "0")
        }
        Err(_) => false,
    }
}

/// Pulls `branch` (its name on the remote) from `remote` into the current
/// branch, merging or rebasing explicitly. Conflicts are a normal result.
pub fn pull(
    git: &GitRunner,
    git_dir: &Path,
    remote: &str,
    branch: &str,
    rebase: bool,
    on_progress: &mut dyn FnMut(&str),
) -> Result<Step> {
    check_remote_name(remote)?;
    if branch.is_empty() || branch.starts_with('-') {
        bail!("invalid branch name '{branch}'");
    }
    let mode = if rebase { "--rebase=true" } else { "--rebase=false" };
    let args = ["pull", "--no-edit", "--progress", mode, remote, branch];
    let expect = if rebase { RepoState::Rebasing } else { RepoState::Merging };
    run_step_with(git, git_dir, expect, || git.run_with_progress(args, &CancelToken::default(), &mut *on_progress))
}

/// What to push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSpec {
    pub remote: String,
    /// Local branch name (without `refs/heads/`).
    pub branch: String,
    /// Branch name on the remote.
    pub remote_branch: String,
    pub set_upstream: bool,
    /// Tags to push (names without `refs/tags/`), each as its own refspec.
    pub tags: Vec<String>,
    /// Force push, but only if the remote branch is still where we last
    /// saw it: `Some(None)` means it must not exist yet.
    pub force_lease: Option<Option<Oid>>,
}

impl PushSpec {
    fn args(&self) -> Result<Vec<String>> {
        check_remote_name(&self.remote)?;
        for name in [&self.branch, &self.remote_branch].into_iter().chain(&self.tags) {
            if name.is_empty() || name.starts_with('-') || name.contains(':') {
                bail!("invalid ref name '{name}'");
            }
        }
        let mut args: Vec<String> = ["push", "--porcelain", "--progress"].map(String::from).into();
        if self.set_upstream {
            args.push("--set-upstream".into());
        }
        let dst = format!("refs/heads/{}", self.remote_branch);
        if let Some(lease) = self.force_lease {
            let expected = lease.map(|o| o.to_string()).unwrap_or_default();
            args.push(format!("--force-with-lease={dst}:{expected}"));
        }
        args.push(self.remote.clone());
        args.push(format!("refs/heads/{}:{dst}", self.branch));
        args.extend(self.tags.iter().map(|t| format!("refs/tags/{t}:refs/tags/{t}")));
        Ok(args)
    }
}

/// How the remote took one ref of a push (`git push --porcelain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdate {
    FastForward,
    Forced,
    New,
    Deleted,
    UpToDate,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefResult {
    /// The URL it was pushed to (a remote can have several push URLs).
    pub destination: String,
    pub update: RefUpdate,
    /// Destination ref on the remote, e.g. `refs/heads/main`.
    pub remote_ref: String,
    /// git's summary, e.g. `[rejected] (fetch first)`.
    pub summary: String,
}

impl RefResult {
    pub fn ok(&self) -> bool {
        self.update != RefUpdate::Rejected
    }

    /// Short name for display (`main`, `v1.0`).
    pub fn short(&self) -> &str {
        self.remote_ref
            .strip_prefix("refs/heads/")
            .or_else(|| self.remote_ref.strip_prefix("refs/tags/"))
            .unwrap_or(&self.remote_ref)
    }
}

/// Parses `git push --porcelain` stdout: one `flag\tfrom:to\tsummary` line
/// per ref between `To <url>` and `Done`.
pub fn parse_push_porcelain(out: &[u8]) -> Vec<RefResult> {
    let mut destination = String::new();
    out.lines()
        .filter_map(|line| {
            let line = line.to_str_lossy();
            if let Some(url) = line.strip_prefix("To ") {
                destination = url.to_owned();
                return None;
            }
            let mut f = line.splitn(3, '\t');
            let flag = f.next()?;
            let refs = f.next()?;
            let summary = f.next().unwrap_or_default().to_owned();
            let update = match flag {
                " " => RefUpdate::FastForward,
                "+" => RefUpdate::Forced,
                "*" => RefUpdate::New,
                "-" => RefUpdate::Deleted,
                "=" => RefUpdate::UpToDate,
                "!" => RefUpdate::Rejected,
                _ => return None,
            };
            let remote_ref = refs.rsplit_once(':').map_or(refs, |(_, to)| to).to_owned();
            Some(RefResult { destination: destination.clone(), update, remote_ref, summary })
        })
        .collect()
}

/// Result of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// git reported refs (some may be rejected). `failed` holds git's error
    /// when the rejections don't explain it, e.g. one of several push URLs
    /// of the remote could not be reached after others took the push.
    Reported { refs: Vec<RefResult>, failed: Option<String> },
    /// The connection failed after the transfer may have started; `verified`
    /// lists refs `ls-remote` showed at the pushed commit afterwards, and
    /// `detail` says what could not be confirmed.
    Unknown { verified: Vec<String>, detail: String },
}

/// Pushes per `spec`. Refusals by the remote come back as rejected refs; a
/// connection that broke mid-way is checked with `ls-remote`.
pub fn push(git: &GitRunner, spec: &PushSpec, on_progress: &mut dyn FnMut(&str)) -> Result<PushOutcome> {
    let mut wanted = vec![(format!("refs/heads/{}", spec.remote_branch), format!("refs/heads/{}", spec.branch))];
    wanted.extend(spec.tags.iter().map(|t| (format!("refs/tags/{t}"), format!("refs/tags/{t}"))));
    run_push(git, &spec.remote, &spec.args()?, &wanted, on_progress)
}

/// Pushes tags (each as its own refspec) without any branch.
pub fn push_tags(git: &GitRunner, remote: &str, tags: &[String], on_progress: &mut dyn FnMut(&str)) -> Result<PushOutcome> {
    check_remote_name(remote)?;
    let mut args: Vec<String> = ["push", "--porcelain", "--progress", remote].map(String::from).into();
    let mut wanted = Vec::new();
    for t in tags {
        if t.is_empty() || t.starts_with('-') || t.contains(':') {
            bail!("invalid tag name '{t}'");
        }
        args.push(format!("refs/tags/{t}:refs/tags/{t}"));
        wanted.push((format!("refs/tags/{t}"), format!("refs/tags/{t}")));
    }
    run_push(git, remote, &args, &wanted, on_progress)
}

/// Runs a push; `wanted` pairs each destination ref with the local ref
/// pushed to it (for checking the remote if the connection breaks).
fn run_push(
    git: &GitRunner,
    remote: &str,
    args: &[String],
    wanted: &[(String, String)],
    on_progress: &mut dyn FnMut(&str),
) -> Result<PushOutcome> {
    let mut transfer_started = false;
    let result = git.run_with_progress(args, &CancelToken::default(), &mut |line: &str| {
        if line.starts_with("Writing objects") || line.starts_with("Total ") || line.starts_with("remote:") {
            transfer_started = true;
        }
        on_progress(line);
    });
    let (stdout, err) = match result {
        Ok(out) => (out, None),
        Err(e) => match e.downcast_ref::<GitError>() {
            Some(g) => (g.stdout.clone().into_bytes(), Some(e)),
            None => return Err(e),
        },
    };
    let refs = parse_push_porcelain(&stdout);
    let Some(err) = err else { return Ok(PushOutcome::Reported { refs, failed: None }) };
    let message = format!("{err:#}");
    if !refs.is_empty() {
        let reported = stdout.lines().filter(|l| l.starts_with(b"To ")).count();
        let urls = push_url_count(git, remote);
        let explained = refs.iter().any(|r| !r.ok()) && reported >= urls;
        return Ok(PushOutcome::Reported { refs, failed: (!explained).then(|| error_line(&message)) });
    }
    let dropped = ["hung up", "unexpected disconnect", "RPC failed", "early EOF", "broken pipe", "Connection reset"]
        .iter()
        .any(|m| message.contains(m));
    if !(transfer_started || dropped) {
        // Failed before anything was sent (no such remote, auth, network).
        return Err(err);
    }
    // The remote may have applied the push: look at what it has now.
    let local: Vec<(String, Option<Oid>)> = wanted
        .iter()
        .map(|(remote_ref, local_ref)| {
            let oid = git.output(["rev-parse", "--verify", "-q", local_ref]).ok().and_then(|o| Oid::from_hex(o.trim()));
            (remote_ref.clone(), oid)
        })
        .collect();
    let remote_refs: Vec<&str> = local.iter().map(|(r, _)| r.as_str()).collect();
    match ls_remote(git, remote, &remote_refs) {
        Ok(have) => {
            let verified: Vec<String> = local
                .iter()
                .filter(|(r, oid)| oid.is_some() && have.get(r) == oid.as_ref())
                .map(|(r, _)| r.clone())
                .collect();
            let detail = if verified.len() == local.len() {
                format!("the connection failed ({}), but the remote has all pushed refs", first_line(&message))
            } else {
                format!("the connection failed ({}); the remote doesn't have every pushed ref", first_line(&message))
            };
            Ok(PushOutcome::Unknown { verified, detail })
        }
        // Cancelled (e.g. a credential prompt for ls-remote): not unknown.
        Err(e) if e.is::<crate::Cancelled>() => Err(e),
        Err(e) => Ok(PushOutcome::Unknown {
            verified: Vec::new(),
            detail: format!("the connection failed ({}) and checking the remote failed too: {e:#}", first_line(&message)),
        }),
    }
}

/// How many URLs `remote` pushes to (1 for a URL given directly).
fn push_url_count(git: &GitRunner, remote: &str) -> usize {
    git.output(["remote", "get-url", "--push", "--all", remote])
        .map(|o| o.lines().filter(|l| !l.is_empty()).count().max(1))
        .unwrap_or(1)
}

/// git's complaints in a failure message, without the progress output.
fn error_line(s: &str) -> String {
    let errors: Vec<&str> =
        s.lines().map(str::trim).filter(|l| l.starts_with("fatal:") || l.starts_with("error:")).collect();
    if errors.is_empty() { first_line(s).to_owned() } else { errors.join(" ") }
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim()
}

/// Deletes `remote_ref` (a full ref name) on `remote`.
pub fn delete_remote_ref(git: &GitRunner, remote: &str, remote_ref: &str, on_progress: &mut dyn FnMut(&str)) -> Result<()> {
    check_remote_name(remote)?;
    if !remote_ref.starts_with("refs/") {
        bail!("expected a full ref name, got '{remote_ref}'");
    }
    let out = git.run_with_progress(["push", "--porcelain", remote, "--delete", remote_ref], &CancelToken::default(), on_progress);
    match out {
        Ok(_) => Ok(()),
        Err(e) => {
            let refs = e.downcast_ref::<GitError>().map(|g| parse_push_porcelain(g.stdout.as_bytes())).unwrap_or_default();
            match refs.first() {
                Some(r) if !r.ok() => bail!("the remote refused to delete {}: {}", r.short(), r.summary),
                _ => Err(e),
            }
        }
    }
}

/// Object ids of `refs` on `remote` (refs it doesn't have are absent).
pub fn ls_remote(git: &GitRunner, remote: &str, refs: &[&str]) -> Result<HashMap<String, Oid>> {
    check_remote_name(remote)?;
    let mut args = vec!["ls-remote", "--", remote];
    args.extend(refs);
    let out = git.output(&args)?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (oid, name) = l.split_once_str("\t")?;
            Some((name.to_str_lossy().into_owned(), Oid::from_hex(oid)?))
        })
        .filter(|(name, _)| refs.contains(&name.as_str()))
        .collect())
}

/// Advice for failures that look like missing credentials.
pub fn auth_hint(git: &GitRunner, error: &str) -> Option<&'static str> {
    let auth = ["Authentication failed", "could not read Username", "could not read Password", "terminal prompts disabled", "Permission denied (publickey"]
        .iter()
        .any(|m| error.contains(m));
    if !auth {
        return None;
    }
    if error.contains("Permission denied (publickey") {
        return Some("Add your SSH key to ssh-agent (ssh-add) and to the server.");
    }
    let helper = git.output_with(["config", "--get", "credential.helper"], &[1]).map(|o| !o.trim().is_empty()).unwrap_or(false);
    (!helper).then_some(
        "No credential helper is set up, so credentials aren't remembered. For example: \
         git config --global credential.helper libsecret (or 'store' to keep them in a plain file).",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_lines() {
        let out = b"To /tmp/remote.git\n\
            =\trefs/heads/main:refs/heads/main\t[up to date]\n\
            *\trefs/tags/v1:refs/tags/v1\t[new tag]\n\
            !\trefs/heads/x:refs/heads/y\t[rejected] (fetch first)\n\
            +\trefs/heads/f:refs/heads/f\tabc...def (forced update)\n\
            \x20\trefs/heads/g:refs/heads/g\tabc..def\n\
            -\t:refs/heads/old\t[deleted]\n\
            Done\n";
        let r = parse_push_porcelain(out);
        let got: Vec<(RefUpdate, &str)> = r.iter().map(|r| (r.update, r.short())).collect();
        assert_eq!(
            got,
            [
                (RefUpdate::UpToDate, "main"),
                (RefUpdate::New, "v1"),
                (RefUpdate::Rejected, "y"),
                (RefUpdate::Forced, "f"),
                (RefUpdate::FastForward, "g"),
                (RefUpdate::Deleted, "old"),
            ]
        );
        assert_eq!(r[2].summary, "[rejected] (fetch first)");
    }

    #[test]
    fn push_arguments() {
        let spec = PushSpec {
            remote: "origin".into(),
            branch: "feat".into(),
            remote_branch: "feature".into(),
            set_upstream: true,
            tags: vec!["v1".into()],
            force_lease: Some(None),
        };
        assert_eq!(
            spec.args().unwrap(),
            [
                "push",
                "--porcelain",
                "--progress",
                "--set-upstream",
                "--force-with-lease=refs/heads/feature:",
                "origin",
                "refs/heads/feat:refs/heads/feature",
                "refs/tags/v1:refs/tags/v1"
            ]
        );
        let bad = PushSpec { branch: "-x".into(), ..spec };
        assert!(bad.args().is_err());
    }
}
