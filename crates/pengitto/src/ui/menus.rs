//! Context menus for commits, branches, tags and stashes, shared by the
//! history list and the sidebar. Menus only record the chosen action; it
//! is applied after drawing (when the tab can be borrowed mutably).

use pengitto_core::model::{CommitSummary, GitRef, Head, Oid, RefKind, Stash};

use crate::repo::{Needs, RepoTab};

#[derive(Debug, Clone)]
pub enum RefAction {
    Checkout(String),
    CheckoutRemote(GitRef),
    /// Detached checkout of (target, label).
    Detach(String, String),
    CreateBranch(String, String),
    CreateTag(String, String),
    Merge(String, String),
    Rebase(String, String),
    CherryPick(String),
    Revert(String),
    Reset(Oid, String),
    Rename(String),
    DeleteBranch(String),
    DeleteTag(String),
    StashApply(Oid),
    StashPop(Oid),
    StashDrop(Oid, String),
    /// Fetch one remote (`None`: all).
    Fetch(Option<String>),
    /// Pull into the current branch, from a remote branch if given.
    Pull(Option<String>),
    Push(String),
    PushTag(String, String),
    /// Delete (remote, full ref) on the remote.
    DeleteRemote(String, String),
    /// Interactive rebase of the commits after this one (`None`: from the
    /// root, including it).
    InteractiveRebase(Option<Oid>),
    /// A new worktree with this local branch checked out.
    NewWorktree(String),
}

pub fn apply(tab: &mut RepoTab, a: RefAction) {
    match a {
        RefAction::Checkout(b) => tab.checkout_branch(&b),
        RefAction::CheckoutRemote(r) => tab.checkout_remote(&r),
        RefAction::Detach(t, l) => tab.request_detach(&t, &l),
        RefAction::CreateBranch(s, l) => tab.open_create_branch(&s, &l),
        RefAction::CreateTag(t, l) => tab.open_create_tag(&t, &l),
        RefAction::Merge(t, l) => tab.request_merge(&t, &l),
        RefAction::Rebase(t, l) => tab.request_rebase(&t, &l),
        RefAction::CherryPick(c) => tab.request_cherry_pick(&c),
        RefAction::Revert(c) => tab.request_revert(&c),
        RefAction::InteractiveRebase(base) => tab.open_interactive_rebase(base),
        RefAction::NewWorktree(b) => tab.request_add_worktree(Some(b)),
        RefAction::Reset(o, l) => tab.request_reset(o, &l),
        RefAction::Rename(b) => tab.request_rename_branch(&b),
        RefAction::DeleteBranch(b) => tab.request_delete_branch(&b),
        RefAction::DeleteTag(t) => tab.request_delete_tag(&t),
        RefAction::StashApply(o) => tab.stash_apply(o, false),
        RefAction::StashPop(o) => tab.stash_apply(o, true),
        RefAction::StashDrop(o, l) => tab.request_drop_stash(o, &l),
        RefAction::Fetch(r) => tab.open_fetch_dialog(r),
        RefAction::Pull(from) => tab.open_pull_dialog(from),
        RefAction::Push(b) => tab.open_push_dialog(Some(b)),
        RefAction::PushTag(t, r) => tab.push_tag(&t, &r),
        RefAction::DeleteRemote(r, f) => tab.request_delete_remote_ref(&r, &f),
    }
}

/// A menu entry that is disabled (with the reason as tooltip) when blocked.
fn item(ui: &mut egui::Ui, label: impl Into<String>, block: Option<&str>) -> bool {
    let resp = ui.add_enabled(block.is_none(), egui::Button::new(label.into()));
    let clicked = resp.on_disabled_hover_text(block.unwrap_or_default()).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn current_branch(tab: &RepoTab) -> Option<&str> {
    match &tab.refs.head {
        Some(Head::Branch { name, .. }) => Some(name),
        _ => None,
    }
}

fn short(s: &str) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(40).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Merge/rebase entries for a ref (not offered for the current branch).
fn integrate_items(ui: &mut egui::Ui, tab: &RepoTab, target: &str, label: &str, out: &mut Option<RefAction>) {
    let clean = tab.block_reason(Needs::CleanState);
    let Some(cur) = current_branch(tab) else { return };
    if item(ui, format!("Merge '{label}' into '{cur}'…"), clean) {
        *out = Some(RefAction::Merge(target.into(), label.into()));
    }
    if item(ui, format!("Rebase '{cur}' onto '{label}'…"), clean) {
        *out = Some(RefAction::Rebase(target.into(), label.into()));
    }
}

pub fn commit_menu(ui: &mut egui::Ui, tab: &RepoTab, c: &CommitSummary, out: &mut Option<RefAction>) {
    let clean = tab.block_reason(Needs::CleanState);
    let idle = tab.block_reason(Needs::Idle);
    let hex = c.id.to_hex();
    let label = format!("{} {}", c.id.short(), short(&c.subject));
    let here: Vec<&GitRef> = tab.refs_by_commit.get(&c.id).into_iter().flatten().map(|&i| &tab.refs.refs[i]).collect();

    for r in here.iter().filter(|r| r.kind == RefKind::LocalBranch && !r.is_head) {
        if item(ui, format!("Check Out '{}'", r.short_name), clean) {
            *out = Some(RefAction::Checkout(r.short_name.clone()));
        }
    }
    for r in here.iter().filter(|r| r.kind == RefKind::RemoteBranch) {
        if item(ui, format!("Check Out '{}'", r.short_name), clean) {
            *out = Some(RefAction::CheckoutRemote((*r).clone()));
        }
    }
    if item(ui, "Check Out This Commit…", clean) {
        *out = Some(RefAction::Detach(hex.clone(), label.clone()));
    }
    ui.separator();
    if item(ui, "Create Branch Here…", idle) {
        *out = Some(RefAction::CreateBranch(hex.clone(), label.clone()));
    }
    if item(ui, "Create Tag Here…", idle) {
        *out = Some(RefAction::CreateTag(hex.clone(), label.clone()));
    }
    if tab.head_commit() != Some(c.id) {
        ui.separator();
        match here.iter().find(|r| matches!(r.kind, RefKind::LocalBranch | RefKind::RemoteBranch) && !r.is_head) {
            // Full ref names, so a tag with the same short name can't win.
            Some(r) => integrate_items(ui, tab, &r.full_name, &r.short_name, out),
            None => integrate_items(ui, tab, &hex, &c.id.short(), out),
        }
        if item(ui, "Cherry-pick", clean) {
            *out = Some(RefAction::CherryPick(hex.clone()));
        }
    }
    if item(ui, "Revert", clean) {
        *out = Some(RefAction::Revert(hex.clone()));
    }
    if let Some(cur) = current_branch(tab)
        && tab.head_commit() != Some(c.id)
        && item(ui, format!("Reset '{cur}' to Here…"), clean)
    {
        *out = Some(RefAction::Reset(c.id, label.clone()));
    }
    // The commits after this one (it has to be in HEAD's history; that is
    // checked when the dialog opens).
    if tab.head_commit() != Some(c.id) && item(ui, "Interactive Rebase from Here…", clean) {
        *out = Some(RefAction::InteractiveRebase(Some(c.id)));
    }
    if c.parents.is_empty() && item(ui, "Interactive Rebase Including the Root…", clean) {
        *out = Some(RefAction::InteractiveRebase(None));
    }
    ui.separator();
    if ui.button("Copy Commit Hash").clicked() {
        ui.ctx().copy_text(hex.clone());
        ui.close();
    }
    if ui.button("Copy Commit Message").clicked() {
        ui.ctx().copy_text(c.subject.clone());
        ui.close();
    }
}

pub fn branch_menu(ui: &mut egui::Ui, tab: &RepoTab, r: &GitRef, out: &mut Option<RefAction>) {
    let clean = tab.block_reason(Needs::CleanState);
    let idle = tab.block_reason(Needs::Idle);
    let name = &r.short_name;
    let checkout = match r.kind {
        RefKind::LocalBranch if !r.is_head => Some(RefAction::Checkout(name.clone())),
        RefKind::RemoteBranch => Some(RefAction::CheckoutRemote(r.clone())),
        _ => None,
    };
    if let Some(a) = checkout
        && item(ui, format!("Check Out '{name}'"), clean)
    {
        *out = Some(a);
    }
    if !r.is_head {
        integrate_items(ui, tab, &r.full_name, name, out);
    }
    ui.separator();
    if item(ui, format!("Create Branch from '{name}'…"), idle) {
        *out = Some(RefAction::CreateBranch(r.full_name.clone(), name.clone()));
    }
    if item(ui, format!("Create Tag on '{name}'…"), idle) {
        *out = Some(RefAction::CreateTag(r.full_name.clone(), name.clone()));
    }
    if r.kind == RefKind::LocalBranch {
        let elsewhere = tab.worktrees.iter().any(|w| !tab.is_this_worktree(w) && w.branch_name() == Some(name.as_str()));
        let block = if r.is_head {
            Some("It is checked out here")
        } else if elsewhere {
            Some("It is checked out in another worktree")
        } else {
            idle
        };
        if item(ui, "Check Out in New Worktree…", block) {
            *out = Some(RefAction::NewWorktree(name.clone()));
        }
    }
    if r.kind == RefKind::LocalBranch {
        ui.separator();
        if r.is_head && item(ui, "Pull…", tab.remote_block(Needs::CleanState, true)) {
            *out = Some(RefAction::Pull(None));
        }
        if item(ui, format!("Push '{name}'…"), tab.remote_block(Needs::Idle, false)) {
            *out = Some(RefAction::Push(name.clone()));
        }
    }
    if r.kind == RefKind::RemoteBranch
        && let Some((remote, branch)) = crate::repo::remote::split_remote_branch(&tab.refs.remotes, name)
    {
        ui.separator();
        if let Some(cur) = current_branch(tab)
            && item(ui, format!("Pull '{name}' into '{cur}'…"), tab.remote_block(Needs::CleanState, true))
        {
            *out = Some(RefAction::Pull(Some(name.clone())));
        }
        if item(ui, format!("Fetch '{remote}'"), tab.remote_block(Needs::Idle, false)) {
            *out = Some(RefAction::Fetch(Some(remote.clone())));
        }
        if item(ui, format!("Delete '{branch}' on '{remote}'…"), tab.remote_block(Needs::Idle, false)) {
            *out = Some(RefAction::DeleteRemote(remote, format!("refs/heads/{branch}")));
        }
    }
    if r.kind == RefKind::LocalBranch {
        ui.separator();
        if item(ui, "Rename…", clean) {
            *out = Some(RefAction::Rename(name.clone()));
        }
        let delete_block = if r.is_head { Some("The current branch can't be deleted") } else { clean };
        if item(ui, "Delete…", delete_block) {
            *out = Some(RefAction::DeleteBranch(name.clone()));
        }
    }
    ui.separator();
    if ui.button("Copy Branch Name").clicked() {
        ui.ctx().copy_text(name.clone());
        ui.close();
    }
}

pub fn tag_menu(ui: &mut egui::Ui, tab: &RepoTab, r: &GitRef, out: &mut Option<RefAction>) {
    let clean = tab.block_reason(Needs::CleanState);
    let idle = tab.block_reason(Needs::Idle);
    let name = &r.short_name;
    if item(ui, format!("Check Out '{name}'…"), clean) {
        *out = Some(RefAction::Detach(r.full_name.clone(), format!("tag {name}")));
    }
    integrate_items(ui, tab, &r.full_name, name, out);
    if item(ui, format!("Create Branch from '{name}'…"), idle) {
        *out = Some(RefAction::CreateBranch(r.full_name.clone(), name.clone()));
    }
    if !tab.refs.remotes.is_empty() {
        ui.separator();
        let block = tab.remote_block(Needs::Idle, false);
        for remote in &tab.refs.remotes {
            if item(ui, format!("Push to '{remote}'"), block) {
                *out = Some(RefAction::PushTag(name.clone(), remote.clone()));
            }
        }
        for remote in &tab.refs.remotes {
            if item(ui, format!("Delete on '{remote}'…"), block) {
                *out = Some(RefAction::DeleteRemote(remote.clone(), r.full_name.clone()));
            }
        }
    }
    ui.separator();
    if item(ui, "Delete…", idle) {
        *out = Some(RefAction::DeleteTag(name.clone()));
    }
}

/// Menu of a remote's header in the sidebar.
pub fn remote_menu(ui: &mut egui::Ui, tab: &RepoTab, remote: &str, out: &mut Option<RefAction>) {
    if item(ui, format!("Fetch '{remote}'…"), tab.remote_block(Needs::Idle, false)) {
        *out = Some(RefAction::Fetch(Some(remote.to_owned())));
    }
    if ui.button("Copy Remote Name").clicked() {
        ui.ctx().copy_text(remote.to_owned());
        ui.close();
    }
}

pub fn stash_menu(ui: &mut egui::Ui, tab: &RepoTab, s: &Stash, out: &mut Option<RefAction>) {
    let clean = tab.block_reason(Needs::CleanState);
    if item(ui, "Apply", clean) {
        *out = Some(RefAction::StashApply(s.id));
    }
    if item(ui, "Pop (Apply and Drop)", clean) {
        *out = Some(RefAction::StashPop(s.id));
    }
    ui.separator();
    if item(ui, "Drop…", tab.stash_drop_block(s.id)) {
        *out = Some(RefAction::StashDrop(s.id, s.message.clone()));
    }
}
