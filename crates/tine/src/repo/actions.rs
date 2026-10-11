//! Branch, history, stash and conflict operations of a repository tab.
//!
//! Every action checks whether it may run now ([`RepoTab::block_reason`]),
//! and every operation re-checks the repository state on the queue right
//! before it runs (the UI's status may be stale). Operations that can't be
//! undone cleanly with uncommitted changes stash them first, with the user's
//! consent. Recovery state (a stash conflict, stashed changes waiting to be
//! re-applied) is only cleared once the operation finishing it succeeded.

use std::collections::BTreeSet;

use bstr::BString;
use tine_core::{
    GitRunner,
    autostash::{self, AutoStash},
    branches::{self, Created, DeleteCheck, RemoteCheckout},
    integrate::{self, DirtyWorkTree, ResetMode, Step},
    remote,
    model::{GitRef, Oid, RepoState},
    sequencer::{self, Resolution},
    stash,
};

use super::{
    RepoTab, View,
    dialog::{CheckoutTarget, Dialog, Integration},
};
use crate::worker::{FollowUp, NeedsDecision, OpKind, OpOutcome};

/// What an action needs from the repository state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// Only that no other operation is running (and state is fresh).
    Idle,
    /// Also no merge/rebase/… in progress and no unresolved conflicts.
    CleanState,
}

/// A stash whose application stopped with conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashConflict {
    pub oid: Oid,
    /// No tracked file had changes before, so the apply can be undone.
    pub was_clean_before: bool,
    /// Untracked files the apply created (the only ones undo removes).
    pub created_untracked: Vec<BString>,
    /// The stash holds local changes set aside for an operation: undoing
    /// the apply leaves them waiting to be applied again.
    pub autostash: bool,
}

/// Turns a step result into an outcome; conflicts and empty steps become
/// follow-ups rather than errors.
fn step_outcome(step: Step, done: String, what: &str, state: RepoState) -> OpOutcome {
    match step {
        Step::Done => OpOutcome::new(done).moving_refs(),
        Step::Stopped(tine_core::integrate::StopReason::Edit(id)) => {
            OpOutcome::new(format!("{what} stopped at {} to edit it", id.short())).moving_refs().with(FollowUp::EditStop)
        }
        Step::Stopped(_) => OpOutcome::new(format!("{what} stopped; continue when ready")).moving_refs(),
        Step::Conflicts => OpOutcome::new(format!("{what} stopped with conflicts")).moving_refs().with(FollowUp::Conflicts),
        Step::Empty => {
            OpOutcome::new(format!("{what}: the change is already present")).moving_refs().with(FollowUp::Empty(state))
        }
    }
}

/// The runner for continuing or skipping: with Tine as git's editor if Tine
/// started the rebase in progress (its messages then apply).
fn rebase_runner(git: &GitRunner, git_dir: &std::path::Path, state: RepoState) -> anyhow::Result<GitRunner> {
    if state != RepoState::Rebasing {
        return Ok(git.clone());
    }
    let editor = tine_core::interactive::Editor { program: crate::askpass::program()? };
    tine_core::interactive::rebase_runner(git, git_dir, &editor)
}

/// How a command that may end an interactive rebase relates to it.
#[derive(Clone, Copy)]
struct RebaseEnd {
    /// The rebase was Tine's own when the command ran (so the branch is
    /// where the command left it).
    by_tine: bool,
    /// The command already dealt with the stash taken for the rebase.
    stash_handled: bool,
}

impl RebaseEnd {
    /// A command continuing (or ending) the rebase in progress.
    fn of_rebase_in_progress(git_dir: &std::path::Path) -> Self {
        RebaseEnd { by_tine: tine_core::interactive::owns_rebase_in_progress(git_dir), stash_handled: false }
    }
}

/// Adds the end of an interactive rebase (if it just ended) to `out`.
fn with_rebase_end(git: &GitRunner, git_dir: &std::path::Path, out: OpOutcome, how: RebaseEnd) -> OpOutcome {
    // Even after a cancel: it only reads where the branch is and tidies up.
    match tine_core::interactive::finish_session(&git.uncancellable(), git_dir, how.by_tine) {
        Ok(Some(mut e)) => {
            if how.stash_handled {
                e.session_mut().autostash = None;
            }
            out.with(FollowUp::RebaseEnded(Box::new(e)))
        }
        Ok(None) => out,
        // Kept for another try (status refreshes, F5).
        Err(e) => out.with(FollowUp::RebaseEndFailed(format!("{e:#}"))),
    }
}

/// Adds advice to errors that look like missing credentials.
pub(super) fn with_auth_hint(git: &GitRunner, e: anyhow::Error) -> anyhow::Error {
    if e.is::<tine_core::Cancelled>() {
        return e;
    }
    match remote::auth_hint(git, &format!("{e:#}")) {
        Some(hint) => anyhow::anyhow!("{e:#}\n{hint}"),
        None => e,
    }
}

/// Git refused to switch because local changes would be overwritten.
fn blocked_by_local_changes(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("would be overwritten") || m.contains("commit your changes or stash them")
}

fn run_checkout(git: &GitRunner, target: &CheckoutTarget) -> anyhow::Result<()> {
    match target {
        CheckoutTarget::Branch(b) => branches::switch(git, b),
        CheckoutTarget::Track { local, remote } => branches::create_tracking(git, local, remote),
        CheckoutTarget::Detached { target, .. } => branches::switch_detached(git, target),
    }
}

impl RepoTab {
    /// Why an action can't run now, if it can't.
    pub fn block_reason(&self, needs: Needs) -> Option<&'static str> {
        if self.op_active() {
            return Some("Waiting for the current operation");
        }
        if !self.state_fresh() {
            return Some("Refreshing the repository state");
        }
        if needs == Needs::CleanState {
            if self.status.state != RepoState::Clean {
                return Some("Finish or abort the operation in progress first");
            }
            if self.stash_conflict.is_some() || !self.status.conflicted.is_empty() {
                return Some("Resolve the conflicts first");
            }
        }
        None
    }

    /// Reports why an action can't run, if it can't.
    pub(super) fn allowed(&mut self, needs: Needs) -> bool {
        match self.block_reason(needs) {
            Some(why) => {
                self.error(why.to_owned());
                false
            }
            None => true,
        }
    }

    pub(super) fn current_branch(&self) -> String {
        match &self.refs.head {
            Some(tine_core::model::Head::Branch { name, .. } | tine_core::model::Head::Unborn { name }) => name.clone(),
            _ => "HEAD".into(),
        }
    }

    // --- Checkout -----------------------------------------------------------

    /// Switches to `target`; when local changes block it, asks whether to
    /// stash them.
    pub fn checkout(&mut self, target: CheckoutTarget) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Checkout(target.label()), move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            match run_checkout(git, &target) {
                Ok(()) => Ok(OpOutcome::new(format!("Checked out {}", target.label())).moving_refs()),
                Err(e) if blocked_by_local_changes(&e) => Err(NeedsDecision::StashAndSwitch(target).into()),
                Err(e) => Err(e),
            }
        });
    }

    pub fn checkout_branch(&mut self, name: &str) {
        self.checkout(CheckoutTarget::Branch(name.to_owned()));
    }

    /// Checks out a remote branch: switches to the local branch tracking it,
    /// creates one, or asks when the name is taken.
    pub fn checkout_remote(&mut self, remote: &GitRef) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        match branches::plan_remote_checkout(&self.refs, remote) {
            RemoteCheckout::SwitchExisting(local) => self.checkout_branch(&local),
            RemoteCheckout::CreateTracking(local) => self.create_tracking(&local, &remote.short_name),
            RemoteCheckout::NameTaken { local, upstream, suggestion } => {
                self.show_dialog(Dialog::RemoteNameTaken {
                    remote: remote.short_name.clone(),
                    local,
                    upstream,
                    new_name: suggestion,
                })
            }
        }
    }

    pub fn create_tracking(&mut self, local: &str, remote: &str) {
        self.checkout(CheckoutTarget::Track { local: local.to_owned(), remote: remote.to_owned() });
    }

    pub fn request_detach(&mut self, target: &str, label: &str) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::Detach { target: target.into(), label: label.into() });
        }
    }

    pub fn checkout_detached(&mut self, target: &str, label: &str) {
        self.checkout(CheckoutTarget::Detached { target: target.to_owned(), label: label.to_owned() });
    }

    /// Stashes local changes (untracked files included, since they can
    /// block a switch too), switches, and re-applies them.
    pub fn stash_and_switch(&mut self, target: CheckoutTarget) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Checkout(target.label()), move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            let label = format!("switching to {}", target.label());
            let (result, report) = autostash::run(git, &git_dir, &label, true, || run_checkout(git, &target));
            match result {
                Ok(()) => Ok(OpOutcome::new(format!("Checked out {}", target.label()))
                    .moving_refs()
                    .with(FollowUp::AutoStash(report))),
                Err(e) => match report.describe() {
                    Some(where_) => Err(e.context(where_)),
                    None => Err(e),
                },
            }
        });
    }

    // --- Branches and tags --------------------------------------------------

    pub fn open_create_branch(&mut self, start: &str, start_label: &str) {
        if self.allowed(Needs::Idle) {
            self.show_dialog(Dialog::CreateBranch {
                start: start.into(),
                start_label: start_label.into(),
                name: String::new(),
                checkout: self.block_reason(Needs::CleanState).is_none(),
            });
        }
    }

    pub fn create_branch(&mut self, name: &str, start: &str, checkout: bool) {
        if !self.allowed(if checkout { Needs::CleanState } else { Needs::Idle }) {
            return;
        }
        let (name, start) = (name.trim().to_owned(), start.to_owned());
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::CreateBranch(name.clone()), move |git| {
            if checkout {
                integrate::ensure_ready(git, &git_dir, true)?;
            }
            Ok(match branches::create_branch(git, &name, &start, checkout)? {
                Created::Created => OpOutcome::new(format!("Created branch {name}")),
                Created::CreatedAndSwitched => OpOutcome::new(format!("Created and switched to {name}")),
                Created::CreatedNotSwitched(why) => {
                    anyhow::bail!("created branch {name}, but could not switch to it: {why}")
                }
            }
            .moving_refs())
        });
    }

    pub fn rename_branch(&mut self, old: &str, new: &str) {
        // A rebase records the branch name; renaming during one breaks it.
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let (old, new) = (old.to_owned(), new.trim().to_owned());
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::RenameBranch(old.clone()), move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            branches::rename_branch(git, &old, &new)?;
            Ok(OpOutcome::new(format!("Renamed {old} to {new}")).moving_refs())
        });
    }

    pub fn request_rename_branch(&mut self, name: &str) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::RenameBranch { old: name.into(), new: name.into() });
        }
    }

    pub fn request_delete_branch(&mut self, name: &str) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::DeleteBranch { name: name.into(), force: false });
        }
    }

    /// Deletes a branch after checking why it may or may not be deleted;
    /// unmerged branches come back as a request to confirm `force`.
    pub fn delete_branch(&mut self, name: &str, force: bool) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let name = name.to_owned();
        let refs = self.refs.clone();
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::DeleteBranch(name.clone()), move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            match branches::check_delete(git, &refs, &name)? {
                DeleteCheck::Safe => {}
                DeleteCheck::Unmerged if force => {}
                DeleteCheck::Unmerged => return Err(NeedsDecision::ForceDeleteBranch(name).into()),
                DeleteCheck::Current => anyhow::bail!("'{name}' is the current branch"),
                DeleteCheck::InOtherWorktree => anyhow::bail!("'{name}' is checked out in another worktree"),
                DeleteCheck::Missing => anyhow::bail!("'{name}' no longer exists"),
            }
            branches::delete_branch(git, &name, force)?;
            Ok(OpOutcome::new(format!("Deleted branch {name}")).moving_refs())
        });
    }

    pub fn open_create_tag(&mut self, target: &str, target_label: &str) {
        if self.allowed(Needs::Idle) {
            self.show_dialog(Dialog::CreateTag {
                target: target.into(),
                target_label: target_label.into(),
                name: String::new(),
                message: String::new(),
            });
        }
    }

    pub fn create_tag(&mut self, name: &str, target: &str, message: &str) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let (name, target, message) = (name.trim().to_owned(), target.to_owned(), message.to_owned());
        self.run_op(OpKind::Tag(name.clone()), move |git| {
            branches::create_tag(git, &name, &target, Some(&message))?;
            Ok(OpOutcome::new(format!("Created tag {name}")).moving_refs())
        });
    }

    pub fn request_delete_tag(&mut self, name: &str) {
        if self.allowed(Needs::Idle) {
            self.show_dialog(Dialog::DeleteTag { name: name.into() });
        }
    }

    pub fn delete_tag(&mut self, name: &str) {
        if !self.allowed(Needs::Idle) {
            return;
        }
        let name = name.to_owned();
        self.run_op(OpKind::DeleteTag(name.clone()), move |git| {
            branches::delete_tag(git, &name)?;
            Ok(OpOutcome::new(format!("Deleted tag {name}")).moving_refs())
        });
    }

    // --- Merge, rebase, cherry-pick, revert ---------------------------------

    /// `target` should be a full ref name (or an object id) so a tag with the
    /// same short name can't be picked by mistake; `label` is for display.
    pub fn request_merge(&mut self, target: &str, label: &str) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::Merge {
                target: target.into(),
                label: label.into(),
                mode: Default::default(),
                stash_first: self.status.has_tracked_changes(),
            });
        }
    }

    pub fn request_rebase(&mut self, onto: &str, label: &str) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::Rebase {
                onto: onto.into(),
                label: label.into(),
                stash_first: self.status.has_tracked_changes(),
            });
        }
    }

    pub fn request_cherry_pick(&mut self, commit: &str) {
        self.request_integration(Integration::CherryPick { commit: commit.into() });
    }

    pub fn request_revert(&mut self, commit: &str) {
        self.request_integration(Integration::Revert { commit: commit.into() });
    }

    /// Runs right away on a clean tree; with local changes, asks first.
    fn request_integration(&mut self, action: Integration) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        if self.status.has_tracked_changes() {
            self.show_dialog(Dialog::DirtyTree { action });
        } else {
            self.integrate(action, false);
        }
    }

    /// Runs a merge/rebase/cherry-pick/revert, optionally with local changes
    /// stashed around it. Without stashing, local changes found right before
    /// running turn into a question instead (aborting could not restore them).
    pub fn integrate(&mut self, action: Integration, stash_first: bool) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        if !stash_first && self.status.has_tracked_changes() {
            self.error("Commit or stash your local changes first".into());
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        let current = self.current_branch();
        let kind = match &action {
            Integration::Merge { target, .. } => OpKind::Merge(target.clone()),
            Integration::Rebase { onto } => OpKind::Rebase(onto.clone()),
            Integration::CherryPick { commit } => OpKind::CherryPick(commit.clone()),
            Integration::Revert { commit } => OpKind::Revert(commit.clone()),
            Integration::Pull { remote, branch, .. } => OpKind::Pull(format!("{remote}/{branch}")),
            Integration::InteractiveRebase { .. } => OpKind::InteractiveRebase,
        };
        self.run_op_ctx(kind, move |ctx, git| {
            if let Err(e) = integrate::ensure_ready(git, &git_dir, stash_first) {
                return Err(if e.is::<DirtyWorkTree>() { NeedsDecision::LocalChanges(action).into() } else { e });
            }
            let run = |stash: Option<tine_core::model::Oid>| -> anyhow::Result<OpOutcome> {
                Ok(match &action {
                    Integration::Merge { target, mode } => {
                        let step = integrate::merge(git, &git_dir, target, *mode)?;
                        step_outcome(step, format!("Merged {target} into {current}"), "Merge", RepoState::Merging)
                    }
                    Integration::Rebase { onto } => {
                        let step = integrate::rebase(git, &git_dir, onto)?;
                        step_outcome(step, format!("Rebased {current} onto {onto}"), "Rebase", RepoState::Rebasing)
                    }
                    Integration::CherryPick { commit } => {
                        let step = integrate::cherry_pick(git, &git_dir, commit)?;
                        step_outcome(step, format!("Cherry-picked {commit}"), "Cherry-pick", RepoState::CherryPicking)
                    }
                    Integration::Revert { commit } => {
                        let step = integrate::revert(git, &git_dir, commit)?;
                        step_outcome(step, format!("Reverted {commit}"), "Revert", RepoState::Reverting)
                    }
                    Integration::InteractiveRebase { range, plan } => {
                        let editor = tine_core::interactive::Editor { program: crate::askpass::program()? };
                        // The stash is recorded with the session before git runs.
                        let step = tine_core::interactive::start(git, &git_dir, range, plan, &editor, stash)?;
                        let kept = plan.items.iter().filter(|(_, a)| *a != tine_core::interactive::Action::Drop).count();
                        let done = format!("Rebased {current} interactively ({kept} commits)");
                        step_outcome(step, done, "Interactive rebase", RepoState::Rebasing)
                    }
                    Integration::Pull { remote, branch, rebase } => {
                        let net = ctx.network(git.clone())?;
                        let before = git.output(["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default();
                        let step = remote::pull(&net, &git_dir, remote, branch, *rebase, &mut |l| ctx.progress(l))
                            .map_err(|e| with_auth_hint(git, e))?;
                        let after = git.output(["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default();
                        let done = if step == Step::Done && before == after {
                            format!("{current} is already up to date with {remote}/{branch}")
                        } else {
                            format!("Pulled {remote}/{branch} into {current}")
                        };
                        let state = if *rebase { RepoState::Rebasing } else { RepoState::Merging };
                        step_outcome(step, done, "Pull", state)
                    }
                })
            };
            // Only Tine's own interactive rebase ends a session here.
            let by_tine = matches!(action, Integration::InteractiveRebase { .. });
            if !stash_first {
                return run(None).map(|out| with_rebase_end(git, &git_dir, out, RebaseEnd { by_tine, stash_handled: false }));
            }
            let (result, report) = autostash::run_with_stash(git, &git_dir, &action.describe(), false, run);
            // The stash is re-applied (or kept, as reported) by now.
            let result = result.map(|out| with_rebase_end(git, &git_dir, out, RebaseEnd { by_tine, stash_handled: true }));
            match result {
                Ok(out) => Ok(if report.stash().is_some() { out.with(FollowUp::AutoStash(report)) } else { out }),
                Err(e) => match report.describe() {
                    Some(where_) => Err(e.context(where_)),
                    None => Err(e),
                },
            }
        });
    }

    // --- Reset --------------------------------------------------------------

    pub fn request_reset(&mut self, target: Oid, label: &str) {
        self.request_reset_mode(target, label, ResetMode::Mixed, None);
    }

    /// The reset dialog with `mode` chosen; with `guard`, only while HEAD is
    /// still where (and on what) the guard says.
    pub fn request_reset_mode(&mut self, target: Oid, label: &str, mode: ResetMode, guard: Option<super::dialog::ResetGuard>) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let lost = match integrate::hard_reset_losses(&self.git, &target.to_hex()) {
            Ok(l) => l,
            Err(e) => {
                self.error(format!("Could not check what a reset would discard: {e:#}"));
                return;
            }
        };
        self.show_dialog(Dialog::Reset { target, label: label.into(), mode, lost, changed: false, guard });
    }

    /// Resets the current branch. A hard reset first re-checks that it would
    /// discard exactly what the user confirmed (`confirmed_lost`).
    pub fn reset(&mut self, target: Oid, mode: ResetMode, confirmed_lost: Vec<String>, guard: Option<super::dialog::ResetGuard>) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let branch = self.current_branch();
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Reset(target.short()), move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            if let Some(g) = &guard {
                let (head, on) = tine_core::interactive::head_now(git)?;
                if head != g.head || on != g.branch {
                    anyhow::bail!("the branch moved since the rebase finished; nothing was reset");
                }
            }
            if mode == ResetMode::Hard {
                let lost = integrate::hard_reset_losses(git, &target.to_hex())?;
                if lost != confirmed_lost {
                    return Err(NeedsDecision::ResetLossesChanged { target, lost, guard }.into());
                }
            }
            integrate::reset(git, &target.to_hex(), mode)?;
            let out = OpOutcome::new(format!("Reset {branch} to {} ({mode:?})", target.short())).moving_refs();
            Ok(if guard.is_some() { out.with(FollowUp::RebaseUndone) } else { out })
        });
    }

    // --- Stash --------------------------------------------------------------

    pub fn open_stash_dialog(&mut self) {
        if self.allowed(Needs::CleanState) {
            self.show_dialog(Dialog::Stash { message: String::new(), include_untracked: true, keep_index: false });
        }
    }

    pub fn stash_push(&mut self, message: &str, include_untracked: bool, keep_index: bool) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let message = message.to_owned();
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Stash, move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            match stash::push(git, &message, include_untracked, keep_index)? {
                Some(oid) => Ok(OpOutcome::new(format!("Stashed changes as {}", oid.short())).moving_refs()),
                None => anyhow::bail!("there are no local changes to stash"),
            }
        });
    }

    /// Applies (and with `pop`, then drops) a stash, identified by its id.
    pub fn stash_apply(&mut self, oid: Oid, pop: bool) {
        if !self.allowed(Needs::CleanState) {
            return;
        }
        let pending = self.pending_autostash == Some(oid);
        let git_dir = self.paths.git_dir.clone();
        self.run_op(if pop { OpKind::StashPop } else { OpKind::StashApply }, move |git| {
            integrate::ensure_ready(git, &git_dir, true)?;
            let applied = if pop { stash::pop(git, oid)? } else { stash::apply(git, oid)? };
            if applied.conflicts {
                // Applied (with conflicts): no longer waiting — unless the
                // apply is undone.
                let mut out = OpOutcome::new(format!("Applying stash {} conflicted", oid.short())).moving_refs().with(
                    FollowUp::StashConflict(StashConflict {
                        oid,
                        was_clean_before: applied.was_clean_before,
                        created_untracked: applied.created_untracked,
                        autostash: pending,
                    }),
                );
                if pending {
                    out = out.with(FollowUp::ClearPendingAutostash(oid));
                }
                return Ok(out);
            }
            let verb = if pop && applied.complete() { "Popped" } else { "Applied" };
            let note = if applied.index_restored {
                String::new()
            } else {
                format!("; staged changes came back unstaged, so stash {} was kept", oid.short())
            };
            let mut out = OpOutcome::new(format!("{verb} stash {}{note}", oid.short())).moving_refs();
            if pending {
                out = out.with(FollowUp::ClearPendingAutostash(oid));
            }
            Ok(out)
        });
    }

    /// Stashes that must not be dropped right now, with the reason.
    pub fn stash_drop_block(&self, oid: Oid) -> Option<&'static str> {
        if self.pending_autostash == Some(oid) {
            return Some("This stash holds your changes from before the last operation; apply it first");
        }
        if self.stash_conflict.as_ref().is_some_and(|c| c.oid == oid) {
            return Some("Finish the conflicted apply of this stash first");
        }
        self.block_reason(Needs::CleanState)
    }

    pub fn request_drop_stash(&mut self, oid: Oid, label: &str) {
        match self.stash_drop_block(oid) {
            Some(why) => self.error(why.into()),
            None => self.show_dialog(Dialog::DropStash { oid, label: label.into() }),
        }
    }

    pub fn drop_stash(&mut self, oid: Oid) {
        if let Some(why) = self.stash_drop_block(oid) {
            self.error(why.into());
            return;
        }
        self.run_op(OpKind::StashDrop, move |git| {
            stash::drop(git, oid)?;
            Ok(OpOutcome::new(format!("Dropped stash {}", oid.short())).moving_refs())
        });
    }

    /// Ends a stash conflict once the files are resolved: unstages what the
    /// apply staged (only when the tree was clean before, so nothing of the
    /// user's is unstaged) and optionally drops the stash.
    pub fn finish_stash_conflict(&mut self, drop_stash: bool) {
        let Some(sc) = self.stash_conflict.clone() else { return };
        if self.block_reason(Needs::Idle).is_some() {
            return;
        }
        if !self.status.conflicted.is_empty() {
            self.error("Resolve the remaining conflicts first".into());
            return;
        }
        self.run_op(OpKind::FinishStashConflict, move |git| {
            if sc.was_clean_before {
                git.output(["reset", "-q"])?;
            }
            if drop_stash {
                stash::drop(git, sc.oid)?;
            }
            let summary = if drop_stash { "Stash applied and dropped" } else { "Stash applied (kept)" };
            Ok(OpOutcome::new(summary).moving_refs().with(FollowUp::ClearStashConflict))
        });
    }

    pub fn request_undo_stash_apply(&mut self) {
        if let Some(sc) = &self.stash_conflict
            && sc.was_clean_before
            && self.block_reason(Needs::Idle).is_none()
        {
            self.show_dialog(Dialog::UndoStashApply { oid: sc.oid });
        }
    }

    pub fn undo_stash_apply(&mut self) {
        let Some(sc) = self.stash_conflict.clone() else { return };
        if !sc.was_clean_before || self.block_reason(Needs::Idle).is_some() {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::UndoStashApply, move |git| {
            stash::undo_apply(git, &git_dir, &sc.created_untracked)?;
            let out = OpOutcome::new(format!("Undid applying stash {} (the stash is kept)", sc.oid.short()))
                .with(FollowUp::ClearStashConflict);
            // Changes set aside for an operation wait in their stash again.
            Ok(if sc.autostash { out.with(FollowUp::PendingAutostash(sc.oid)) } else { out })
        });
    }

    /// Re-applies changes that were stashed before an operation which then
    /// stopped half-way (cleared once that succeeds).
    pub fn apply_pending_autostash(&mut self) {
        if let Some(oid) = self.pending_autostash {
            self.stash_apply(oid, true);
        }
    }

    /// Finishes conflicts of unknown origin (e.g. a stash applied on the
    /// command line): optionally unstages exactly the files that were
    /// conflicted, which resolving had staged.
    pub fn finish_generic_conflict(&mut self, unstage: bool) {
        let Some(paths) = self.generic_conflict.clone() else { return };
        if !self.status.conflicted.is_empty() || self.block_reason(Needs::Idle).is_some() {
            return;
        }
        if !unstage {
            self.generic_conflict = None;
            return;
        }
        let paths: Vec<BString> = paths.into_iter().collect();
        self.run_op(OpKind::UnstageResolved, move |git| {
            use std::os::unix::ffi::OsStringExt as _;
            let mut args: Vec<std::ffi::OsString> = vec!["reset".into(), "-q".into(), "--".into()];
            args.extend(paths.iter().map(|p| std::ffi::OsString::from_vec(p.to_vec())));
            git.output(&args)?;
            Ok(OpOutcome::new(format!("Unstaged {} resolved file(s)", paths.len())).with(FollowUp::ClearGenericConflict))
        });
    }

    // --- Continuing multi-step operations and resolving conflicts -----------

    pub fn continue_op(&mut self) {
        let state = self.status.state;
        if self.block_reason(Needs::Idle).is_some() || !self.status.conflicted.is_empty() {
            return;
        }
        if state == RepoState::Merging {
            // A merge finishes with a commit (message prefilled from MERGE_MSG).
            self.commit();
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Continue, move |git| {
            let how = RebaseEnd::of_rebase_in_progress(&git_dir);
            let runner = rebase_runner(git, &git_dir, state)?;
            if state == RepoState::Rebasing {
                tine_core::interactive::finish_reword(&runner, &git_dir)?;
            }
            let step = sequencer::continue_op(&runner, &git_dir, state)?;
            Ok(with_rebase_end(git, &git_dir, step_outcome(step, format!("{state:?} continued"), "Continue", state), how))
        });
    }

    pub fn request_abort(&mut self) {
        if self.block_reason(Needs::Idle).is_none() && self.status.state != RepoState::Clean {
            self.show_dialog(Dialog::Abort { state: self.status.state });
        }
    }

    pub fn abort(&mut self, state: RepoState) {
        if self.block_reason(Needs::Idle).is_some() {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Abort, move |git| {
            let how = RebaseEnd::of_rebase_in_progress(&git_dir);
            sequencer::abort(git, state)?;
            Ok(with_rebase_end(git, &git_dir, OpOutcome::new(format!("Aborted {state:?}")).moving_refs(), how))
        });
    }

    pub fn skip(&mut self) {
        let state = self.status.state;
        if self.block_reason(Needs::Idle).is_some()
            || !matches!(state, RepoState::Rebasing | RepoState::CherryPicking | RepoState::Reverting)
        {
            return;
        }
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::Skip, move |git| {
            let how = RebaseEnd::of_rebase_in_progress(&git_dir);
            let runner = rebase_runner(git, &git_dir, state)?;
            let step = sequencer::skip(&runner, &git_dir, state)?;
            Ok(with_rebase_end(git, &git_dir, step_outcome(step, "Skipped the current commit".into(), "Skip", state), how))
        });
    }

    /// Keeps an empty cherry-pick/revert step as an empty commit.
    pub fn commit_empty_step(&mut self) {
        if self.block_reason(Needs::Idle).is_some() {
            return;
        }
        let (git_dir, state) = (self.paths.git_dir.clone(), self.status.state);
        self.run_op(OpKind::Continue, move |git| {
            // A reworded commit that became empty keeps its new message.
            let message = if state == RepoState::Rebasing { tine_core::interactive::empty_reword_message(&git_dir)? } else { None };
            sequencer::commit_empty(git, message.as_deref())?;
            Ok(OpOutcome::new("Recorded an empty commit").moving_refs())
        });
    }

    /// Resolves conflicted files. Marking files resolved that still contain
    /// conflict markers asks for confirmation first.
    pub fn resolve(&mut self, paths: Vec<BString>, how: Resolution) {
        if self.block_reason(Needs::Idle).is_some() || paths.is_empty() {
            return;
        }
        if how == Resolution::MarkResolved
            && paths.iter().any(|p| sequencer::has_conflict_markers(&self.paths.workdir, p))
        {
            self.show_dialog(Dialog::MarkResolved { paths });
            return;
        }
        self.resolve_confirmed(paths, how);
    }

    pub fn resolve_confirmed(&mut self, paths: Vec<BString>, how: Resolution) {
        let n = paths.len();
        self.run_op(OpKind::Resolve, move |git| {
            sequencer::resolve(git, &paths, how)?;
            Ok(OpOutcome::new(format!("Resolved {n} file(s) ({how:?})")))
        });
    }

    pub fn open_mergetool(&mut self, path: BString) {
        if self.block_reason(Needs::Idle).is_some() {
            return;
        }
        self.run_op(OpKind::MergeTool, move |git| {
            sequencer::mergetool(git, &path)?;
            Ok(OpOutcome::new(format!("Merge tool finished for {path}")))
        });
    }

    // --- Results ---------------------------------------------------------------

    /// Reacts to what a finished operation left to do.
    pub(super) fn handle_follow_up(&mut self, f: FollowUp) {
        match f {
            FollowUp::Conflicts => self.view = View::LocalChanges,
            FollowUp::Empty(state) => self.show_dialog(Dialog::EmptyStep { state }),
            FollowUp::AutoStash(report) => {
                match &report {
                    AutoStash::Kept(oid) | AutoStash::NotRestored { oid, .. } | AutoStash::RestoreFailed(oid, _) => {
                        self.pending_autostash = Some(*oid)
                    }
                    AutoStash::Conflicted { oid, created_untracked } => {
                        self.stash_conflict = Some(StashConflict {
                            oid: *oid,
                            was_clean_before: true,
                            created_untracked: created_untracked.clone(),
                            autostash: true,
                        });
                        self.view = View::LocalChanges;
                    }
                    AutoStash::RestoredUnstaged(_) | AutoStash::NotNeeded | AutoStash::Restored => {}
                }
                if let Some(text) = report.describe() {
                    if matches!(report, AutoStash::RestoreFailed(..) | AutoStash::NotRestored { .. }) {
                        self.error(text);
                    } else {
                        self.activity.push(text);
                    }
                }
            }
            FollowUp::StashConflict(sc) => {
                self.stash_conflict = Some(sc);
                self.view = View::LocalChanges;
            }
            FollowUp::ClearStashConflict => self.stash_conflict = None,
            FollowUp::ClearPendingAutostash(oid) => {
                if self.pending_autostash == Some(oid) {
                    self.pending_autostash = None;
                }
            }
            FollowUp::PendingAutostash(oid) => self.pending_autostash = Some(oid),
            FollowUp::ClearGenericConflict => self.generic_conflict = None,
            FollowUp::EditStop => self.view = View::LocalChanges,
            FollowUp::RebaseEnded(ended) => self.rebase_ended(*ended),
            FollowUp::RebaseEndFailed(e) => self.error(format!("Couldn't tidy up after the interactive rebase (press F5 to try again): {e}")),
            FollowUp::RebaseUndone => self.rebase_done = None,
        }
    }

    /// Turns a request for a decision into the matching dialog.
    pub(super) fn handle_decision(&mut self, d: NeedsDecision) {
        self.show_dialog(match d {
            NeedsDecision::ForceDeleteBranch(name) => Dialog::DeleteBranch { name, force: true },
            NeedsDecision::StashAndSwitch(target) => Dialog::StashAndSwitch { target },
            NeedsDecision::LocalChanges(action) => Dialog::DirtyTree { action },
            NeedsDecision::ResetLossesChanged { target, lost, guard } => Dialog::Reset {
                guard,
                target,
                label: target.short(),
                mode: ResetMode::Hard,
                lost,
                changed: true,
            },
        });
    }

    /// Paths currently conflicted, for recording conflicts of unknown origin.
    pub(super) fn conflicted_paths(&self) -> BTreeSet<BString> {
        self.status.conflicted.iter().map(|f| f.path.clone()).collect()
    }
}
