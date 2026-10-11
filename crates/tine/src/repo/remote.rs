//! Fetch, pull and push of a repository tab. Network commands get the
//! app's credential prompts ([`crate::askpass`]) and report progress.

use tine_core::{
    model::{Head, Oid, RefKind},
    remote::{self, FetchTarget, PushOutcome, PushSpec, RefResult, RefUpdate},
};

use super::{
    Needs, RepoTab,
    actions::with_auth_hint,
    dialog::{Dialog, Integration},
};
use crate::{
    ops_registry::UnknownResult,
    worker::{OpKind, OpOutcome},
};

/// Splits `origin/feature/x` into (`origin`, `feature/x`), preferring the
/// longest configured remote name that matches.
pub fn split_remote_branch(remotes: &[String], short: &str) -> Option<(String, String)> {
    remotes
        .iter()
        .filter(|r| short.len() > r.len() + 1 && short.starts_with(r.as_str()) && short.as_bytes()[r.len()] == b'/')
        .max_by_key(|r| r.len())
        .map(|r| (r.clone(), short[r.len() + 1..].to_owned()))
}

/// What went through and what was rejected, e.g. ("pushed main, v1 ·
/// up to date: v0", ["v2 [rejected] (already exists)"]). With several push
/// URLs, each ref names its destination.
fn describe_refs(refs: &[RefResult]) -> (String, Vec<String>) {
    let several = refs.iter().any(|r| r.destination != refs[0].destination);
    let name = |r: &RefResult| {
        if several { format!("{} ({})", r.short(), r.destination) } else { r.short().to_owned() }
    };
    let names = |pred: &dyn Fn(&RefResult) -> bool| -> Vec<String> { refs.iter().filter(|r| pred(r)).map(name).collect() };
    let pushed = names(&|r| r.ok() && r.update != RefUpdate::UpToDate);
    let same = names(&|r| r.update == RefUpdate::UpToDate);
    let rejected: Vec<String> = refs.iter().filter(|r| !r.ok()).map(|r| format!("{} {}", name(r), r.summary)).collect();
    let mut parts = Vec::new();
    if !pushed.is_empty() {
        parts.push(format!("pushed {}", pushed.join(", ")));
    }
    if !same.is_empty() {
        parts.push(format!("up to date: {}", same.join(", ")));
    }
    (parts.join(" · "), rejected)
}

/// Turns a push result into an outcome: rejected refs, or a failure for
/// another push URL, make it an error that still says what went through.
fn push_outcome(remote: &str, out: PushOutcome) -> anyhow::Result<OpOutcome> {
    match out {
        PushOutcome::Reported { refs, failed } => {
            let (done, rejected) = describe_refs(&refs);
            if rejected.is_empty() && failed.is_none() {
                let summary = if done.is_empty() { format!("Nothing to push to {remote}") } else { format!("{remote}: {done}") };
                return Ok(OpOutcome::new(summary).moving_refs());
            }
            let mut msg = if rejected.is_empty() {
                format!("Pushing to {remote} failed")
            } else {
                format!("{remote} rejected {}", rejected.join("; "))
            };
            if let Some(f) = &failed {
                msg = format!("{msg} ({f})");
            }
            if !done.is_empty() {
                msg = format!("{msg}. Other refs went through ({done}).");
            }
            if failed.is_some() && !done.is_empty() {
                msg.push_str("\nSome push URLs of this remote got the push and others didn't; push again once the problem is fixed.");
            }
            if refs.iter().any(|r| r.summary.contains("fetch first") || r.summary.contains("non-fast-forward")) {
                msg.push_str("\nThe remote has commits you don't have: pull (or fetch and integrate) first, or force push.");
            }
            if refs.iter().any(|r| r.summary.contains("stale info")) {
                msg.push_str("\nThe remote branch moved since you confirmed the force push; fetch and review it first.");
            }
            Err(anyhow::anyhow!(msg))
        }
        PushOutcome::Unknown { verified, detail } => {
            let seen = if verified.is_empty() { String::new() } else { format!(" On the remote now: {}.", verified.join(", ")) };
            Err(UnknownResult(format!("Push to {remote} may or may not have happened: {detail}.{seen} Fetch to check.")).into())
        }
    }
}

impl RepoTab {
    fn head_branch(&self) -> Option<&str> {
        match &self.refs.head {
            Some(Head::Branch { name, .. }) => Some(name),
            _ => None,
        }
    }

    fn upstream_of(&self, branch: &str) -> Option<&str> {
        self.refs
            .refs
            .iter()
            .find(|r| r.kind == RefKind::LocalBranch && r.short_name == branch)
            .and_then(|r| r.upstream.as_deref())
    }

    /// Why fetch/pull/push can't be offered at all (beyond being busy).
    pub fn remote_block(&self, needs: Needs, needs_branch: bool) -> Option<&'static str> {
        if self.refs.remotes.is_empty() {
            return Some("This repository has no remotes");
        }
        if needs_branch && self.head_branch().is_none() {
            return Some("Check out a branch first");
        }
        self.block_reason(needs)
    }

    // --- Fetch ----------------------------------------------------------------

    pub fn open_fetch_dialog(&mut self, remote: Option<String>) {
        if self.remote_block(Needs::Idle, false).is_none() {
            self.show_dialog(Dialog::Fetch { remote, prune: false });
        }
    }

    pub fn fetch(&mut self, remote: Option<String>, prune: bool) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let (target, label) = match remote {
            Some(r) => (FetchTarget::Remote(r.clone()), r),
            None => (FetchTarget::All, "all remotes".to_owned()),
        };
        self.run_op_ctx(OpKind::Fetch(label.clone()), move |ctx, git| {
            let net = ctx.network(git.clone())?;
            remote::fetch(&net, &target, prune, &mut |l| ctx.progress(l)).map_err(|e| with_auth_hint(git, e))?;
            Ok(OpOutcome::new(format!("Fetched {label}")).moving_refs())
        });
    }

    // --- Pull -----------------------------------------------------------------

    /// Opens the pull dialog for the current branch; `from` preselects a
    /// remote branch (e.g. `origin/main`), otherwise its upstream.
    pub fn open_pull_dialog(&mut self, from: Option<String>) {
        if let Some(why) = self.remote_block(Needs::CleanState, true) {
            self.error(format!("Can't pull: {why}"));
            return;
        }
        let branch = self.head_branch().unwrap_or_default().to_owned();
        let remote_branch = from.or_else(|| self.upstream_of(&branch).map(str::to_owned)).unwrap_or_else(|| {
            // No upstream: a remote branch of the same name, if any.
            self.refs
                .refs
                .iter()
                .filter(|r| r.kind == RefKind::RemoteBranch)
                .find(|r| split_remote_branch(&self.refs.remotes, &r.short_name).is_some_and(|(_, b)| b == branch))
                .map(|r| r.short_name.clone())
                .unwrap_or_default()
        });
        self.show_dialog(Dialog::Pull {
            remote_branch,
            rebase: remote::pull_rebases_by_default(&self.git),
            stash_first: self.status.has_tracked_changes(),
        });
    }

    /// Pulls `remote_branch` (e.g. `origin/main`) into the current branch.
    pub fn pull(&mut self, remote_branch: &str, rebase: bool, stash_first: bool) {
        let Some((remote, branch)) = split_remote_branch(&self.refs.remotes, remote_branch) else {
            self.error(format!("'{remote_branch}' is not a branch of a configured remote"));
            return;
        };
        if let Some(why) = self.remote_block(Needs::CleanState, true) {
            self.error(format!("Can't pull: {why}"));
            return;
        }
        self.integrate(Integration::Pull { remote, branch, rebase }, stash_first);
    }

    // --- Push -----------------------------------------------------------------

    /// Opens the push dialog for `branch` (the current one by default).
    pub fn open_push_dialog(&mut self, branch: Option<String>) {
        let Some(branch) = branch.or_else(|| self.head_branch().map(str::to_owned)) else {
            self.error("Can't push: check out a branch first".into());
            return;
        };
        if let Some(why) = self.remote_block(Needs::Idle, false) {
            self.error(format!("Can't push: {why}"));
            return;
        }
        let upstream = self.upstream_of(&branch).and_then(|u| split_remote_branch(&self.refs.remotes, u));
        let (remote, remote_branch) = match &upstream {
            Some((r, b)) => (r.clone(), b.clone()),
            None => {
                let remote = if self.refs.remotes.iter().any(|r| r == "origin") {
                    "origin".to_owned()
                } else {
                    self.refs.remotes[0].clone()
                };
                (remote, branch.clone())
            }
        };
        let lease = (remote.clone(), remote_branch.clone(), self.tracking_target(&remote, &remote_branch));
        self.show_dialog(Dialog::Push {
            branch,
            remote,
            remote_branch,
            set_upstream: upstream.is_none(),
            tags: false,
            force: false,
            lease,
        });
    }

    /// Where the remote-tracking ref of `remote/branch` points now (what a
    /// force push expects to overwrite).
    pub fn tracking_target(&self, remote: &str, branch: &str) -> Option<Oid> {
        let full = format!("refs/remotes/{remote}/{branch}");
        self.refs.refs.iter().find(|r| r.full_name == full).map(|r| r.target)
    }

    /// Pushes `branch` to `remote_branch` on `remote`. `force_lease`: force
    /// push, overwriting the remote branch only if it is still at the given
    /// commit (`Some(None)`: only if it doesn't exist), i.e. what the user
    /// saw when confirming.
    pub fn push(
        &mut self,
        branch: &str,
        remote: &str,
        remote_branch: &str,
        set_upstream: bool,
        tags: bool,
        force_lease: Option<Option<Oid>>,
    ) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let spec = PushSpec {
            remote: remote.to_owned(),
            branch: branch.to_owned(),
            remote_branch: remote_branch.trim().to_owned(),
            set_upstream,
            tags: if tags {
                self.refs.refs.iter().filter(|r| r.kind == RefKind::Tag).map(|r| r.short_name.clone()).collect()
            } else {
                Vec::new()
            },
            force_lease,
        };
        let label = format!("{branch} → {remote}/{}", spec.remote_branch);
        self.run_op_ctx(OpKind::Push(label), move |ctx, git| {
            let net = ctx.network(git.clone())?;
            let out = remote::push(&net, &spec, &mut |l| ctx.progress(l)).map_err(|e| with_auth_hint(git, e))?;
            push_outcome(&spec.remote, out)
        });
    }

    pub fn push_tag(&mut self, tag: &str, remote: &str) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let (tag, remote) = (tag.to_owned(), remote.to_owned());
        self.run_op_ctx(OpKind::Push(format!("tag {tag} → {remote}")), move |ctx, git| {
            let net = ctx.network(git.clone())?;
            let out = remote::push_tags(&net, &remote, std::slice::from_ref(&tag), &mut |l| ctx.progress(l))
                .map_err(|e| with_auth_hint(git, e))?;
            push_outcome(&remote, out)
        });
    }

    // --- Deleting on the remote ---------------------------------------------

    pub fn request_delete_remote_ref(&mut self, remote: &str, full_ref: &str) {
        if self.remote_block(Needs::Idle, false).is_none() {
            self.show_dialog(Dialog::DeleteRemoteRef { remote: remote.into(), full_ref: full_ref.into() });
        }
    }

    pub fn delete_remote_ref(&mut self, remote: &str, full_ref: &str) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let (remote, full_ref) = (remote.to_owned(), full_ref.to_owned());
        let short = full_ref.strip_prefix("refs/heads/").or_else(|| full_ref.strip_prefix("refs/tags/")).unwrap_or(&full_ref).to_owned();
        self.run_op_ctx(OpKind::DeleteRemoteRef(format!("{remote}: {short}")), move |ctx, git| {
            let net = ctx.network(git.clone())?;
            remote::delete_remote_ref(&net, &remote, &full_ref, &mut |l| ctx.progress(l)).map_err(|e| with_auth_hint(git, e))?;
            Ok(OpOutcome::new(format!("Deleted {short} on {remote}")).moving_refs())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_branch_names() {
        let remotes = vec!["origin".to_owned(), "up".to_owned(), "up/stream".to_owned()];
        assert_eq!(split_remote_branch(&remotes, "origin/feature/x"), Some(("origin".into(), "feature/x".into())));
        assert_eq!(split_remote_branch(&remotes, "up/stream/main"), Some(("up/stream".into(), "main".into())));
        assert_eq!(split_remote_branch(&remotes, "up/main"), Some(("up".into(), "main".into())));
        assert_eq!(split_remote_branch(&remotes, "origin"), None);
        assert_eq!(split_remote_branch(&remotes, "other/main"), None);
    }
}
