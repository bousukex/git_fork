//! Modal dialogs for branch, history and stash operations.

use egui::{Key, RichText};
use tine_core::{
    integrate::{MergeMode, ResetMode},
    model::{RefKind, RepoState},
    sequencer::Resolution,
};

use crate::{
    repo::{
        Needs, RepoTab,
        dialog::{Dialog, Integration},
    },
    theme::Palette,
};

/// Fast check of the common git ref-name rules for live feedback; git's own
/// check runs when the operation does.
pub fn ref_name_problem(name: &str) -> Option<&'static str> {
    let n = name.trim();
    if n.is_empty() {
        return Some("Enter a name");
    }
    if n.chars().any(char::is_whitespace) {
        return Some("Names can't contain spaces");
    }
    if n.contains("..") || n.contains("@{") || n == "@" || n.chars().any(|c| "~^:?*[\\".contains(c) || c.is_control()) {
        return Some("Contains characters git doesn't allow (.. ~ ^ : ? * [ \\)");
    }
    if n.starts_with('-')
        || n.starts_with('/')
        || n.ends_with('/')
        || n.ends_with('.')
        || n.ends_with(".lock")
        || n.contains("//")
        || n.split('/').any(|c| c.starts_with('.'))
    {
        return Some("Not a valid git name");
    }
    None
}

fn state_name(state: RepoState) -> &'static str {
    match state {
        RepoState::Merging => "merge",
        RepoState::Rebasing => "rebase",
        RepoState::CherryPicking => "cherry-pick",
        RepoState::Reverting => "revert",
        RepoState::Bisecting => "bisect",
        RepoState::Clean => "operation",
    }
}

/// Right-aligned confirm/cancel buttons; returns (confirm, cancel).
fn buttons(ui: &mut egui::Ui, confirm: &str, danger: bool, enabled: bool) -> (bool, bool) {
    buttons_with_enter(ui, confirm, danger, enabled, true)
}

/// Like [`buttons`]; `enter_ok` is false while a multi-line field has focus
/// (Enter there inserts a line break). Enter that ends an IME composition
/// never confirms either.
fn buttons_with_enter(ui: &mut egui::Ui, confirm: &str, danger: bool, enabled: bool, enter_ok: bool) -> (bool, bool) {
    let p = Palette::of(ui.ctx());
    let mut out = (false, false);
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let text = if danger { RichText::new(confirm).color(egui::Color32::WHITE) } else { RichText::new(confirm).strong() };
        let mut b = egui::Button::new(text);
        if danger {
            b = b.fill(p.status_deleted);
        }
        let enter = enabled && enter_ok && crate::input::pressed(ui.ctx(), egui::Modifiers::NONE, Key::Enter);
        out.0 = ui.add_enabled(enabled, b).clicked() || enter;
        out.1 = ui.button("Cancel").clicked();
    });
    out
}

fn hint(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.label(RichText::new(text).small().color(p.text_weak));
}

fn problem(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.label(RichText::new(text).small().color(p.status_deleted));
}

fn name_field(ui: &mut egui::Ui, value: &mut String, hint_text: &str) {
    let resp = ui.add(egui::TextEdit::singleline(value).hint_text(hint_text).desired_width(f32::INFINITY));
    if !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
        resp.request_focus();
    }
}

fn branch_exists(tab: &RepoTab, name: &str) -> bool {
    tab.refs.refs.iter().any(|r| r.kind == RefKind::LocalBranch && r.short_name == name.trim())
}

fn tag_exists(tab: &RepoTab, name: &str) -> bool {
    tab.refs.refs.iter().any(|r| r.kind == RefKind::Tag && r.short_name == name.trim())
}

fn current(tab: &RepoTab) -> String {
    match &tab.refs.head {
        Some(tine_core::model::Head::Branch { name, .. } | tine_core::model::Head::Unborn { name }) => name.clone(),
        _ => "HEAD".into(),
    }
}

type Act = Box<dyn FnOnce(&mut RepoTab)>;

/// Shows the tab's pending dialog, if any.
pub fn show(ctx: &egui::Context, tab: &mut RepoTab) {
    let Some(mut dialog) = tab.dialog.take() else { return };
    if let Dialog::InteractiveRebase(d) = &mut dialog {
        let (dirty, busy) = (tab.status.has_tracked_changes(), tab.busy());
        match super::rebase::show(ctx, d, dirty, busy) {
            super::rebase::Outcome::Keep => tab.dialog = Some(dialog),
            super::rebase::Outcome::Close => {}
            super::rebase::Outcome::Start => {
                if let Dialog::InteractiveRebase(d) = dialog {
                    tab.start_interactive_rebase(*d);
                }
            }
        }
        return;
    }
    let p = Palette::of(ctx);
    let mut act: Option<Act> = None;
    let mut cancel = false;
    let dirty = tab.status.has_tracked_changes();
    let clean_block = tab.block_reason(Needs::CleanState);
    let current = current(tab);

    let modal = egui::Modal::new(egui::Id::new("repo-dialog")).show(ctx, |ui| {
        ui.set_width(460.0);
        match &mut dialog {
            Dialog::InteractiveRebase(_) => unreachable!("shown above"),
            Dialog::CreateBranch { start, start_label, name, checkout } => {
                ui.heading("Create Branch");
                hint(ui, &format!("Starting at {start_label}"));
                name_field(ui, name, "Branch name");
                let issue = ref_name_problem(name)
                    .or_else(|| branch_exists(tab, name).then_some("A branch with this name already exists"));
                if let Some(why) = issue.filter(|_| !name.is_empty()) {
                    problem(ui, why);
                }
                ui.add_enabled_ui(clean_block.is_none(), |ui| {
                    ui.checkbox(checkout, "Check out the new branch");
                });
                if let Some(why) = clean_block {
                    *checkout = false;
                    hint(ui, why);
                }
                let (ok, no) = buttons(ui, "Create", false, issue.is_none());
                if ok {
                    let (n, s, c) = (name.clone(), start.clone(), *checkout);
                    act = Some(Box::new(move |t| t.create_branch(&n, &s, c)));
                }
                cancel = no;
            }
            Dialog::RenameBranch { old, new } => {
                ui.heading(format!("Rename '{old}'"));
                name_field(ui, new, "New name");
                let issue = ref_name_problem(new)
                    .or_else(|| (new.trim() != old && branch_exists(tab, new)).then_some("A branch with this name already exists"))
                    .or_else(|| (new.trim() == old).then_some("Enter a different name"));
                if let Some(why) = issue.filter(|_| !new.is_empty()) {
                    problem(ui, why);
                }
                let (ok, no) = buttons(ui, "Rename", false, issue.is_none());
                if ok {
                    let (o, n) = (old.clone(), new.clone());
                    act = Some(Box::new(move |t| t.rename_branch(&o, &n)));
                }
                cancel = no;
            }
            Dialog::DeleteBranch { name, force } => {
                ui.heading(format!("Delete branch '{name}'?"));
                if *force {
                    ui.label(
                        RichText::new(format!(
                            "'{name}' has commits that are not merged into the current branch or its upstream. \
                             They are lost unless another branch or tag points to them."
                        ))
                        .color(p.status_deleted),
                    );
                } else {
                    ui.label("The branch is removed locally; its commits stay reachable from other branches.");
                }
                let (ok, no) = buttons(ui, if *force { "Delete Anyway" } else { "Delete" }, *force, true);
                if ok {
                    let (n, f) = (name.clone(), *force);
                    act = Some(Box::new(move |t| t.delete_branch(&n, f)));
                }
                cancel = no;
            }
            Dialog::CreateTag { target, target_label, name, message } => {
                ui.heading("Create Tag");
                hint(ui, &format!("On {target_label}"));
                name_field(ui, name, "Tag name (e.g. v1.2.0)");
                let message_focused = ui
                    .add(
                        egui::TextEdit::multiline(message)
                            .hint_text("Message (leave empty for a lightweight tag)")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    )
                    .has_focus();
                let issue = ref_name_problem(name).or_else(|| tag_exists(tab, name).then_some("A tag with this name already exists"));
                if let Some(why) = issue.filter(|_| !name.is_empty()) {
                    problem(ui, why);
                }
                let (ok, no) = buttons_with_enter(ui, "Create", false, issue.is_none(), !message_focused);
                if ok {
                    let (n, tg, m) = (name.clone(), target.clone(), message.clone());
                    act = Some(Box::new(move |t| t.create_tag(&n, &tg, &m)));
                }
                cancel = no;
            }
            Dialog::DeleteTag { name } => {
                ui.heading(format!("Delete tag '{name}'?"));
                ui.label("The tag is removed from this repository only (not from remotes).");
                let (ok, no) = buttons(ui, "Delete", true, true);
                if ok {
                    let n = name.clone();
                    act = Some(Box::new(move |t| t.delete_tag(&n)));
                }
                cancel = no;
            }
            Dialog::RemoteNameTaken { remote, local, upstream, new_name } => {
                ui.heading(format!("Check out {remote}"));
                let tracking = match upstream {
                    Some(u) => format!("tracks {u}"),
                    None => "doesn't track a remote branch".into(),
                };
                ui.label(format!("A local branch '{local}' already exists and {tracking}."));
                ui.add_space(4.0);
                ui.label("Create a new local branch tracking it:");
                name_field(ui, new_name, "Local branch name");
                let issue = ref_name_problem(new_name)
                    .or_else(|| branch_exists(tab, new_name).then_some("A branch with this name already exists"));
                if let Some(why) = issue.filter(|_| !new_name.is_empty()) {
                    problem(ui, why);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(format!("Switch to existing '{local}'")).clicked() {
                        let l = local.clone();
                        act = Some(Box::new(move |t| t.checkout_branch(&l)));
                    }
                });
                let (ok, no) = buttons(ui, "Create & Track", false, issue.is_none());
                if ok {
                    let (n, r) = (new_name.clone(), remote.clone());
                    act = Some(Box::new(move |t| t.create_tracking(&n, &r)));
                }
                cancel = no;
            }
            Dialog::Detach { target, label } => {
                ui.heading(format!("Check out {label}?"));
                ui.label(
                    "This checks out the commit without a branch (detached HEAD). New commits won't belong \
                     to any branch until you create one.",
                );
                let (ok, no) = buttons(ui, "Check Out", false, true);
                if ok {
                    let (tg, l) = (target.clone(), label.clone());
                    act = Some(Box::new(move |t| t.checkout_detached(&tg, &l)));
                }
                cancel = no;
            }
            Dialog::StashAndSwitch { target } => {
                ui.heading(format!("Switch to '{}'?", target.label()));
                ui.label(
                    "Your local changes would be overwritten by the switch. Tine can stash them, switch, and \
                     re-apply them on the new branch.",
                );
                hint(ui, "If re-applying conflicts, the changes stay in the stash and the conflicts are shown.");
                let (ok, no) = buttons(ui, "Stash & Switch", false, true);
                if ok {
                    let tg = target.clone();
                    act = Some(Box::new(move |t| t.stash_and_switch(tg)));
                }
                cancel = no;
            }
            Dialog::Merge { target, label, mode, stash_first } => {
                ui.heading(format!("Merge {label} into {current}"));
                ui.radio_value(mode, MergeMode::Default, "Fast-forward if possible, otherwise create a merge commit");
                ui.radio_value(mode, MergeMode::NoFastForward, "Always create a merge commit (--no-ff)");
                ui.radio_value(mode, MergeMode::FastForwardOnly, "Fast-forward only (--ff-only)");
                let ok_enabled = stash_choice(ui, dirty, stash_first);
                let (ok, no) = buttons(ui, "Merge", false, ok_enabled);
                if ok {
                    let action = Integration::Merge { target: target.clone(), mode: *mode };
                    let s = *stash_first && dirty;
                    act = Some(Box::new(move |t| t.integrate(action, s)));
                }
                cancel = no;
            }
            Dialog::Rebase { onto, label, stash_first } => {
                ui.heading(format!("Rebase {current} onto {label}"));
                ui.label(format!(
                    "The commits of {current} that aren't in {label} are re-created on top of it. Don't rebase \
                     commits that others already have."
                ));
                let ok_enabled = stash_choice(ui, dirty, stash_first);
                let (ok, no) = buttons(ui, "Rebase", false, ok_enabled);
                if ok {
                    let action = Integration::Rebase { onto: onto.clone() };
                    let s = *stash_first && dirty;
                    act = Some(Box::new(move |t| t.integrate(action, s)));
                }
                cancel = no;
            }
            Dialog::DirtyTree { action } => {
                ui.heading("Local changes");
                ui.label(format!(
                    "You have uncommitted changes. To {} safely, Tine stashes them first and re-applies them \
                     afterwards.",
                    action.describe()
                ));
                let (ok, no) = buttons(ui, "Stash & Continue", false, true);
                if ok {
                    let a = action.clone();
                    act = Some(Box::new(move |t| t.integrate(a, true)));
                }
                cancel = no;
            }
            Dialog::Reset { target, label, mode, lost, changed, guard } => {
                ui.heading(format!("Reset {current} to {label}"));
                if *changed {
                    ui.label(
                        RichText::new("What a hard reset would discard changed since you opened this dialog; review it again.")
                            .color(p.status_modified),
                    );
                }
                ui.radio_value(mode, ResetMode::Soft, "Soft: keep all changes, staged");
                ui.radio_value(mode, ResetMode::Mixed, "Mixed: keep all changes, unstaged");
                ui.radio_value(mode, ResetMode::Hard, RichText::new("Hard: discard all uncommitted changes").color(p.status_deleted));
                if *mode == ResetMode::Hard && !lost.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new("These changes will be lost:").color(p.status_deleted));
                    egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        for f in lost.iter() {
                            ui.label(RichText::new(f).monospace().small());
                        }
                    });
                }
                hint(ui, "Commits after the target stay reachable from other refs or the reflog.");
                let (ok, no) = buttons(ui, "Reset", *mode == ResetMode::Hard, true);
                if ok {
                    let (tg, m, l, g) = (*target, *mode, lost.clone(), guard.clone());
                    act = Some(Box::new(move |t| t.reset(tg, m, l, g)));
                }
                cancel = no;
            }
            Dialog::Stash { message, include_untracked, keep_index } => {
                ui.heading("Stash Changes");
                name_field(ui, message, "Message (optional)");
                ui.checkbox(include_untracked, "Include untracked files");
                ui.checkbox(keep_index, "Keep staged changes in the index");
                let (ok, no) = buttons(ui, "Stash", false, true);
                if ok {
                    let (m, u, k) = (message.clone(), *include_untracked, *keep_index);
                    act = Some(Box::new(move |t| t.stash_push(&m, u, k)));
                }
                cancel = no;
            }
            Dialog::DropStash { oid, label } => {
                ui.heading("Drop stash?");
                ui.label(RichText::new(label.as_str()).monospace());
                ui.label("A dropped stash can only be recovered with low-level git commands.");
                let (ok, no) = buttons(ui, "Drop", true, true);
                if ok {
                    let o = *oid;
                    act = Some(Box::new(move |t| t.drop_stash(o)));
                }
                cancel = no;
            }
            Dialog::Abort { state } => {
                let what = state_name(*state);
                ui.heading(format!("Abort the {what}?"));
                ui.label(format!(
                    "Everything returns to how it was before the {what} started; conflict resolutions made since \
                     then are discarded."
                ));
                let (ok, no) = buttons(ui, &format!("Abort {what}"), true, true);
                if ok {
                    let s = *state;
                    act = Some(Box::new(move |t| t.abort(s)));
                }
                cancel = no;
            }
            Dialog::EmptyStep { state } => {
                ui.heading("Nothing to commit");
                ui.label(format!(
                    "The changes of this {} step are already present, so its commit would be empty.",
                    state_name(*state)
                ));
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Skip It").strong()).clicked() {
                        act = Some(Box::new(|t| t.skip()));
                    }
                    if ui.button("Keep as Empty Commit").clicked() {
                        act = Some(Box::new(|t| t.commit_empty_step()));
                    }
                    cancel = ui.button("Decide Later").clicked();
                });
            }
            Dialog::MarkResolved { paths } => {
                ui.heading("Conflict markers remain");
                ui.label("These files still contain conflict markers (<<<<<<<):");
                for path in paths.iter() {
                    ui.label(RichText::new(path.to_string()).monospace().small());
                }
                let (ok, no) = buttons(ui, "Mark Resolved Anyway", false, true);
                if ok {
                    let ps = paths.clone();
                    act = Some(Box::new(move |t| t.resolve_confirmed(ps, Resolution::MarkResolved)));
                }
                cancel = no;
            }
            Dialog::Fetch { remote, prune } => {
                ui.heading("Fetch");
                let label = |r: &Option<String>| r.clone().unwrap_or_else(|| "All remotes".into());
                egui::ComboBox::from_label("Remote").selected_text(label(remote)).show_ui(ui, |ui| {
                    ui.selectable_value(remote, None, "All remotes");
                    for r in &tab.refs.remotes {
                        ui.selectable_value(remote, Some(r.clone()), r);
                    }
                });
                ui.checkbox(prune, "Remove remote branches that no longer exist (prune)");
                let (ok, no) = buttons(ui, "Fetch", false, true);
                if ok {
                    let (r, pr) = (remote.clone(), *prune);
                    act = Some(Box::new(move |t| t.fetch(r, pr)));
                }
                cancel = no;
            }
            Dialog::Pull { remote_branch, rebase, stash_first } => {
                ui.heading(format!("Pull into {current}"));
                let shown = if remote_branch.is_empty() { "Choose a remote branch".to_owned() } else { remote_branch.clone() };
                egui::ComboBox::from_label("From").selected_text(shown).width(300.0).show_ui(ui, |ui| {
                    for r in tab.refs.refs.iter().filter(|r| r.kind == RefKind::RemoteBranch) {
                        ui.selectable_value(remote_branch, r.short_name.clone(), &r.short_name);
                    }
                });
                ui.checkbox(rebase, "Rebase my commits instead of merging (--rebase)");
                let ok_enabled = stash_choice(ui, dirty, stash_first) && !remote_branch.is_empty();
                let (ok, no) = buttons(ui, "Pull", false, ok_enabled);
                if ok {
                    let (rb, re, s) = (remote_branch.clone(), *rebase, *stash_first && dirty);
                    act = Some(Box::new(move |t| t.pull(&rb, re, s)));
                }
                cancel = no;
            }
            Dialog::Push { branch, remote, remote_branch, set_upstream, tags, force, lease } => {
                ui.heading(format!("Push {branch}"));
                egui::Grid::new("push-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("To remote");
                    egui::ComboBox::from_id_salt("push-remote").selected_text(remote.as_str()).show_ui(ui, |ui| {
                        for r in &tab.refs.remotes {
                            ui.selectable_value(remote, r.clone(), r);
                        }
                    });
                    ui.end_row();
                    ui.label("As branch");
                    ui.add(egui::TextEdit::singleline(remote_branch).desired_width(260.0));
                    ui.end_row();
                });
                let target = remote_branch.trim().to_owned();
                // What a force push may overwrite is fixed when the target is
                // first shown, not re-read when a fetch moves it meanwhile.
                if lease.0 != *remote || lease.1 != target {
                    *lease = (remote.clone(), target.clone(), tab.tracking_target(remote, &target));
                }
                let issue = ref_name_problem(remote_branch);
                if let Some(why) = issue {
                    problem(ui, why);
                }
                ui.checkbox(set_upstream, format!("Track {remote}/{target} as the upstream of {branch}"));
                ui.checkbox(tags, "Also push all tags");
                ui.checkbox(force, RichText::new("Force push (overwrite the remote branch)").color(p.status_deleted));
                if *force {
                    let text = match lease.2 {
                        Some(o) => format!(
                            "Replaces {remote}/{target} only if it is still at {} (as last fetched); commits others pushed since are not overwritten.",
                            o.short()
                        ),
                        None => format!("Only if {remote} has no branch '{target}' yet (none was fetched)."),
                    };
                    hint(ui, &text);
                }
                let (ok, no) = buttons(ui, if *force { "Force Push" } else { "Push" }, *force, issue.is_none());
                if ok {
                    let (b, r, rb, u, tg) = (branch.clone(), remote.clone(), remote_branch.clone(), *set_upstream, *tags);
                    let lease = force.then_some(lease.2);
                    act = Some(Box::new(move |t| t.push(&b, &r, &rb, u, tg, lease)));
                }
                cancel = no;
            }
            Dialog::DeleteRemoteRef { remote, full_ref } => {
                let (what, name) = match full_ref.strip_prefix("refs/tags/") {
                    Some(t) => ("tag", t),
                    None => ("branch", full_ref.strip_prefix("refs/heads/").unwrap_or(full_ref)),
                };
                ui.heading(format!("Delete {what} '{name}' on {remote}?"));
                ui.label(format!(
                    "The {what} is removed from {remote} for everyone. Local branches and tags are not changed."
                ));
                let (ok, no) = buttons(ui, "Delete on Remote", true, true);
                if ok {
                    let (r, f) = (remote.clone(), full_ref.clone());
                    act = Some(Box::new(move |t| t.delete_remote_ref(&r, &f)));
                }
                cancel = no;
            }
            Dialog::UndoStashApply { oid } => {
                ui.heading("Undo applying the stash?");
                ui.label(format!(
                    "Tracked files go back to HEAD and files the stash added are removed. Stash {} itself is kept.",
                    oid.short()
                ));
                let (ok, no) = buttons(ui, "Undo Apply", true, true);
                if ok {
                    act = Some(Box::new(|t| t.undo_stash_apply()));
                }
                cancel = no;
            }
        }
    });

    if let Some(a) = act {
        a(tab);
    } else if !(cancel || modal.should_close()) {
        tab.dialog = Some(dialog);
    }
}

/// With local changes, offers to stash them first; returns whether the
/// operation may proceed.
fn stash_choice(ui: &mut egui::Ui, dirty: bool, stash_first: &mut bool) -> bool {
    if !dirty {
        return true;
    }
    ui.add_space(6.0);
    ui.checkbox(stash_first, "Stash my local changes first and re-apply them afterwards");
    if !*stash_first {
        problem(ui, "With uncommitted changes, an abort could not restore them; commit or stash them first.");
    }
    *stash_first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names() {
        for ok in ["main", "feature/日本語", "v1.2.0", "fix-123"] {
            assert_eq!(ref_name_problem(ok), None, "{ok}");
        }
        for bad in ["", " ", "a b", "a..b", "-x", "x/", "x.lock", "a//b", ".hidden", "a/.b", "x~1", "a:b", "@", "x@{1}"] {
            assert!(ref_name_problem(bad).is_some(), "{bad:?}");
        }
    }
}
