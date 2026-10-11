//! Interactive rebase from a tab: the plan being edited in the dialog,
//! starting it, and following it through stops to its end.

use std::collections::HashMap;

use tine_core::{
    integrate::StopReason,
    interactive::{self, Action, Ended, MessageKey, Plan, Range},
    model::{Oid, RepoState},
};

use super::{
    Needs, RepoTab,
    dialog::{Dialog, Integration},
};
use crate::worker::{FollowUp, OpKind, OpOutcome};

/// The plan being made in the interactive rebase dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseDialog {
    pub range: Range,
    pub plan: Plan,
    /// Selected item (index into `plan.items`).
    pub selected: Option<usize>,
    /// Combined messages the user wrote, by the commit the group melds into
    /// (kept while that commit heads no squash group: they apply again if
    /// it does once more).
    group_edits: HashMap<Oid, String>,
    pub stash_first: bool,
}

impl RebaseDialog {
    pub fn new(range: Range) -> Self {
        let plan = Plan::new(&range);
        let selected = (!plan.items.is_empty()).then(|| plan.items.len() - 1);
        RebaseDialog { range, plan, selected, group_edits: HashMap::new(), stash_first: true }
    }

    /// Back to every commit picked in the original order.
    pub fn reset(&mut self) {
        *self = RebaseDialog { stash_first: self.stash_first, ..RebaseDialog::new(self.range.clone()) };
    }

    pub fn commit(&self, id: Oid) -> Option<&interactive::RebaseCommit> {
        self.range.commits.iter().find(|c| c.id == id)
    }

    pub fn set_action(&mut self, i: usize, action: Action) {
        let Some(item) = self.plan.items.get_mut(i) else { return };
        item.1 = action;
        let id = item.0;
        if action == Action::Reword && !self.plan.messages.contains_key(&MessageKey::Reword(id)) {
            let original = self.commit(id).map(|c| c.message.clone()).unwrap_or_default();
            self.plan.messages.insert(MessageKey::Reword(id), original);
        }
        self.sync_groups();
    }

    /// Moves item `from` to position `to` (both indices into the items).
    pub fn move_item(&mut self, from: usize, to: usize) {
        let n = self.plan.items.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let item = self.plan.items.remove(from);
        self.plan.items.insert(to, item);
        if self.selected == Some(from) {
            self.selected = Some(to);
        }
        self.sync_groups();
    }

    pub fn reword_message(&mut self, id: Oid) -> &mut String {
        self.plan.messages.entry(MessageKey::Reword(id)).or_default()
    }

    pub fn set_group_message(&mut self, anchor: Oid, message: String) {
        self.group_edits.insert(anchor, message);
        self.sync_groups();
    }

    /// The reword message changed: the default combined messages follow.
    pub fn reword_changed(&mut self) {
        self.sync_groups();
    }

    /// Recomputes the combined messages after the groups may have changed.
    /// Only messages the user wrote are used (keyed by the group's last
    /// member); other groups get git's own combination at the time.
    fn sync_groups(&mut self) {
        self.plan.messages.retain(|k, _| matches!(k, MessageKey::Reword(_)));
        for g in self.plan.groups() {
            if !g.has_squash(&self.plan) {
                continue;
            }
            let anchor = self.plan.items[g.anchor].0;
            let Some(terminal) = g.terminal() else { continue };
            if let Some(m) = self.group_edits.get(&anchor) {
                self.plan.messages.insert(MessageKey::Group(self.plan.items[terminal].0), m.clone());
            }
        }
    }

    /// The combined message shown for the group melding into `anchor`:
    /// the user's, or what git would make of the commits as planned.
    pub fn group_text(&self, anchor: Oid) -> Option<String> {
        let group = self.plan.groups().into_iter().find(|g| self.plan.items[g.anchor].0 == anchor && g.has_squash(&self.plan))?;
        Some(match self.group_edits.get(&anchor) {
            Some(m) => m.clone(),
            None => self.plan.default_group_message(&group, &self.range),
        })
    }

    /// Messages the user wrote for groups that don't exist now (their
    /// commit heads no squash group): kept, to come back or be copied.
    pub fn set_aside(&self) -> Vec<(Oid, String)> {
        let anchors: Vec<Oid> =
            self.plan.groups().into_iter().filter(|g| g.has_squash(&self.plan)).map(|g| self.plan.items[g.anchor].0).collect();
        let mut out: Vec<(Oid, String)> =
            self.group_edits.iter().filter(|(a, _)| !anchors.contains(a)).map(|(a, m)| (*a, m.clone())).collect();
        out.sort_by_key(|(a, _)| *a);
        out
    }

    pub fn discard_set_aside(&mut self, anchor: Oid) {
        self.group_edits.remove(&anchor);
    }

    /// Goes back to git's combined message for this group.
    pub fn reset_group_message(&mut self, anchor: Oid) {
        self.group_edits.remove(&anchor);
        self.sync_groups();
    }

    /// The anchor of the squash group item `i` belongs to (itself if it is
    /// one).
    pub fn group_anchor(&self, i: usize) -> Option<Oid> {
        self.plan
            .groups()
            .into_iter()
            .find(|g| g.has_squash(&self.plan) && (g.anchor == i || g.members.contains(&i)))
            .map(|g| self.plan.items[g.anchor].0)
    }

    pub fn group_edited(&self, anchor: Oid) -> bool {
        self.group_edits.contains_key(&anchor)
    }

    pub fn problems(&self) -> Vec<String> {
        self.plan.problems(&self.range)
    }

    pub fn published(&self) -> usize {
        self.range.commits.iter().filter(|c| c.published).count()
    }
}

/// An interactive rebase that finished (for "Undo Rebase…").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseDone {
    /// Where it started.
    pub previous: Oid,
    /// The branch rebased (full ref; `None`: detached HEAD).
    pub branch: Option<String>,
    /// Where it ended (undo only while HEAD is still there).
    pub head: Oid,
}

impl RepoTab {
    /// Reads the commits after `base` (all of them with `None`) and opens
    /// the interactive rebase dialog for them.
    pub fn open_interactive_rebase(&mut self, base: Option<Oid>) {
        if let Some(why) = self.block_reason(Needs::CleanState) {
            self.error(format!("Can't rebase interactively: {why}"));
            return;
        }
        self.stop_rebase_range();
        let cancel = tine_core::CancelToken::default();
        self.rebase_range_cancel = Some(cancel.clone());
        self.rebase_range_seq = self.gens.bump();
        self.worker.rebase_range(self.rebase_range_seq, base, cancel);
    }

    /// Forgets (and stops) reading commits for the rebase dialog.
    pub(super) fn stop_rebase_range(&mut self) {
        self.rebase_range_seq = 0;
        if let Some(c) = self.rebase_range_cancel.take() {
            c.cancel();
        }
    }

    pub(super) fn on_rebase_range(&mut self, result: anyhow::Result<Range>) {
        self.stop_rebase_range();
        // Something else came up meanwhile: it isn't replaced.
        if self.modal_open() {
            return;
        }
        match result {
            Ok(range) => self.show_dialog(Dialog::InteractiveRebase(Box::new(RebaseDialog::new(range)))),
            Err(e) => self.error(format!("Can't rebase interactively: {e:#}")),
        }
    }

    /// Starts the rebase planned in `d`.
    pub fn start_interactive_rebase(&mut self, d: RebaseDialog) {
        let stash = d.stash_first && self.status.has_tracked_changes();
        self.integrate(Integration::InteractiveRebase { range: Box::new(d.range), plan: Box::new(d.plan) }, stash);
    }

    /// Stopped at an `edit` of this commit (and nothing conflicts): it can
    /// be amended, or commits added.
    pub fn edit_stop(&self) -> Option<Oid> {
        match self.rebase_stop {
            Some(StopReason::Edit(id)) if self.status.state == RepoState::Rebasing && self.status.conflicted.is_empty() => Some(id),
            _ => None,
        }
    }

    /// Follows the rebase in progress (why it stopped, whether Tine started
    /// it) and notices that a session is over (also outside Tine, or left
    /// behind by a rebase someone else started), tidying up after it.
    pub(super) fn check_rebase_session(&mut self) {
        let rebasing = self.status.state == RepoState::Rebasing;
        self.rebase_stop =
            (rebasing && self.status.conflicted.is_empty()).then(|| tine_core::integrate::stop_reason(&self.paths.git_dir));
        // Leaving an edit stop (or reaching another one): an amend armed for
        // it must not carry over.
        let stop = self.edit_stop();
        if stop != self.last_edit_stop {
            self.last_edit_stop = stop;
            self.cancel_amend();
        }
        // A session (or its marker) that can't be read is reported — not
        // taken for none, or for someone else's, and nothing is tidied away
        // (F5 tries again).
        let read = interactive::read_session(&self.paths.git_dir).and_then(|s| match s {
            Some(s) => interactive::owns_current_rebase(&s).map(|owned| Some((s, owned))),
            None => Ok(None),
        });
        let (session, owned) = match read {
            Ok(Some((s, owned))) => (Some(s), owned),
            Ok(None) => (None, false),
            Err(e) => {
                let e = format!("{e:#}");
                if self.rebase_session_error.as_deref() != Some(e.as_str()) {
                    self.error(format!("Can't read the interactive rebase Tine started (F5 tries again): {e}"));
                    self.rebase_session_error = Some(e);
                }
                return;
            }
        };
        self.rebase_session_error = None;
        self.rebase_session = rebasing && owned;
        if let Some(stash) = session.as_ref().and_then(|s| s.autostash).filter(|_| owned) {
            // Also after a restart while stopped.
            self.pending_autostash.get_or_insert(stash);
        }
        let Some(session) = session.filter(|_| !rebasing || !owned) else { return };
        // Once per session (a failure is reported, not retried each
        // refresh; F5 tries again).
        if self.rebase_finish_tried.as_deref() == Some(session.id.as_str()) || self.busy() {
            return;
        }
        self.rebase_finish_tried = Some(session.id);
        let git_dir = self.paths.git_dir.clone();
        self.run_op(OpKind::FinishRebase, move |git| {
            let mut out = OpOutcome::default();
            // Ended outside Tine's commands: where it ended comes from the
            // branch's reflog.
            if let Some(e) = interactive::finish_session(git, &git_dir, false)? {
                out = out.with(FollowUp::RebaseEnded(Box::new(e)));
            }
            Ok(out)
        });
    }

    pub(super) fn rebase_ended(&mut self, ended: Ended) {
        let session = ended.session().clone();
        let on = session.branch.as_deref().map(interactive::branch_name).unwrap_or("HEAD").to_owned();
        match ended {
            Ended::Rebased { session, head } => {
                self.activity.push(format!(
                    "Interactive rebase of '{on}' finished; it was at {} before (also in the reflog)",
                    session.head.short()
                ));
                self.rebase_done = Some(RebaseDone { previous: session.head, branch: session.branch, head });
            }
            Ended::Unchanged { .. } => self.activity.push(format!("Interactive rebase of '{on}' ended without changes")),
            Ended::Stale { .. } => {
                self.activity.push(format!("An interactive rebase of '{on}' Tine started had ended elsewhere; tidied up"))
            }
        }
        if let Some(stash) = session.autostash {
            self.pending_autostash.get_or_insert(stash);
        }
    }

    /// "Undo Rebase…" applies: HEAD is still where the rebase left it, on
    /// the same branch.
    pub fn can_undo_rebase(&self) -> bool {
        let Some(done) = &self.rebase_done else { return false };
        let on = match &self.refs.head {
            Some(tine_core::model::Head::Branch { name, .. }) => Some(format!("refs/heads/{name}")),
            _ => None,
        };
        self.status.state == RepoState::Clean && self.head_commit() == Some(done.head) && on == done.branch
    }

    /// Opens the reset dialog back to where the last interactive rebase
    /// started (hard reset, with the losses listed first). It runs only if
    /// the branch is still where the rebase left it; the undo stays offered
    /// until it ran (or is dismissed).
    pub fn undo_rebase(&mut self) {
        if !self.can_undo_rebase() {
            return;
        }
        let Some(done) = self.rebase_done.clone() else { return };
        let guard = super::dialog::ResetGuard { branch: done.branch, head: done.head };
        let label = format!("{} (before the rebase)", done.previous.short());
        self.request_reset_mode(done.previous, &label, tine_core::integrate::ResetMode::Hard, Some(guard));
    }
}

#[cfg(test)]
mod tests {
    use tine_core::interactive::RebaseCommit;

    use super::*;

    fn id(n: u8) -> Oid {
        Oid::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap()
    }

    fn dialog(n: u8) -> RebaseDialog {
        RebaseDialog::new(Range {
            base: Some(id(0)),
            head: id(n),
            branch: Some("refs/heads/main".into()),
            commits: (1..=n)
                .map(|i| RebaseCommit {
                    id: id(i),
                    subject: format!("c{i}"),
                    message: format!("c{i}"),
                    author: "a".into(),
                    author_time: 0,
                    published: false,
                })
                .collect(),
        })
    }

    #[test]
    fn written_group_messages_are_set_aside_not_lost() {
        let mut d = dialog(3);
        d.set_action(1, Action::Squash);
        assert_eq!(d.group_text(id(1)).as_deref(), Some("c1\n\nc2"), "git's combination to start with");
        assert!(d.plan.messages.is_empty(), "unchanged: git combines them");
        d.set_group_message(id(1), "mine".into());
        assert_eq!(d.plan.messages.get(&MessageKey::Group(id(2))).map(String::as_str), Some("mine"));
        // The group goes away: the message is kept aside, not used.
        d.set_action(1, Action::Pick);
        assert!(d.plan.messages.is_empty());
        assert_eq!(d.set_aside(), [(id(1), "mine".to_owned())]);
        // …and comes back with the group.
        d.set_action(1, Action::Squash);
        assert_eq!(d.plan.messages.get(&MessageKey::Group(id(2))).map(String::as_str), Some("mine"));
        assert!(d.set_aside().is_empty());
        // Discarded on request; back to git's combination on request.
        d.set_action(1, Action::Pick);
        d.discard_set_aside(id(1));
        assert!(d.set_aside().is_empty());
        d.set_action(1, Action::Squash);
        assert!(d.plan.messages.is_empty(), "discarded: not used again");
        d.set_group_message(id(1), "again".into());
        d.reset_group_message(id(1));
        assert!(d.plan.messages.is_empty());
        assert_eq!(d.group_text(id(1)).as_deref(), Some("c1\n\nc2"));
    }
}
