//! Left sidebar: views, branches, remotes, tags and stashes.

use std::collections::BTreeMap;

use egui::{Align2, FontFamily, FontId, Pos2, Rect, RichText};
use tine_core::model::{GitRef, Oid, RefKind, Tracking};

use super::paint_text;
use crate::{
    repo::{RepoTab, View},
    theme::Palette,
};

const ITEM_H: f32 = 22.0;
const INDENT: f32 = 14.0;

enum Action {
    View(View),
    Reveal(Oid),
    ShowStash(Oid),
    Ref(super::menus::RefAction),
    /// Path relative to the work tree, as raw bytes (may not be UTF-8).
    OpenSubmodule(bstr::BString),
}

/// Branch names grouped into folders by `/`.
#[derive(Default)]
struct Tree<'a> {
    folders: BTreeMap<String, Tree<'a>>,
    leaves: Vec<(&'a str, &'a GitRef)>,
}

impl<'a> Tree<'a> {
    fn insert(&mut self, name: &'a str, r: &'a GitRef) {
        match name.split_once('/') {
            Some((dir, rest)) => self.folders.entry(dir.to_owned()).or_default().insert(rest, r),
            None => self.leaves.push((name, r)),
        }
    }
}

fn item(ui: &mut egui::Ui, depth: usize, text: &str, right: &str, selected: bool, bold: bool) -> egui::Response {
    labeled_item(ui, depth, text, right, selected, bold, text)
}

/// A sidebar row; `a11y` names it for screen readers and UI tests
/// (e.g. "branch main", "tag v1.0").
#[allow(clippy::too_many_arguments)]
fn labeled_item(ui: &mut egui::Ui, depth: usize, text: &str, right: &str, selected: bool, bold: bool, a11y: &str) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = super::list_row(ui, ITEM_H, selected, true);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, a11y));
    let left = rect.left() + 10.0 + depth as f32 * INDENT;
    let family = if bold { FontFamily::Name(crate::fonts::BOLD.into()) } else { FontFamily::Proportional };
    let right_w = if right.is_empty() { 0.0 } else { 70.0 };
    let text_rect = Rect::from_min_max(Pos2::new(left, rect.top()), Pos2::new(rect.right() - 8.0 - right_w, rect.bottom()));
    paint_text(ui.painter(), text_rect, text, FontId::new(13.0, family), p.text, Align2::LEFT_CENTER);
    if !right.is_empty() {
        let r = Rect::from_min_max(Pos2::new(rect.right() - 8.0 - right_w, rect.top()), Pos2::new(rect.right() - 8.0, rect.bottom()));
        paint_text(ui.painter(), r, right, FontId::proportional(11.5), p.text_weak, Align2::RIGHT_CENTER);
    }
    resp
}

fn tracking_text(t: Tracking) -> String {
    match t {
        Tracking::Diverged { ahead, behind } => {
            let mut s = String::new();
            if ahead > 0 {
                s += &format!("↑{ahead}");
            }
            if behind > 0 {
                if !s.is_empty() {
                    s.push(' ');
                }
                s += &format!("↓{behind}");
            }
            s
        }
        Tracking::Gone => "gone".into(),
        Tracking::None => String::new(),
    }
}

fn show_tree(
    ui: &mut egui::Ui,
    tab: &RepoTab,
    tree: &Tree<'_>,
    depth: usize,
    salt: &str,
    selected: Option<Oid>,
    actions: &mut Vec<Action>,
) {
    for (name, sub) in &tree.folders {
        let id = ui.id().with((salt, name, depth));
        egui::CollapsingHeader::new(RichText::new(name).size(13.0))
            .id_salt(id)
            .default_open(true)
            // The collapsing body already indents its children.
            .show(ui, |ui| show_tree(ui, tab, sub, 0, salt, selected, actions));
    }
    for (name, r) in &tree.leaves {
        let kind = if r.kind == RefKind::RemoteBranch { "remote branch" } else { "branch" };
        let a11y = format!("{kind} {}", r.short_name);
        let resp = labeled_item(ui, depth, name, &tracking_text(r.track), selected == Some(r.target), r.is_head, &a11y);
        if resp.double_clicked() {
            // Like Fork: double-click checks out.
            actions.push(Action::Ref(match r.kind {
                RefKind::RemoteBranch => super::menus::RefAction::CheckoutRemote((*r).clone()),
                _ => super::menus::RefAction::Checkout(r.short_name.clone()),
            }));
        } else if resp.clicked() {
            actions.push(Action::Reveal(r.target));
        }
        let mut chosen = None;
        resp.on_hover_text(&r.full_name).context_menu(|ui| super::menus::branch_menu(ui, tab, r, &mut chosen));
        actions.extend(chosen.map(Action::Ref));
    }
}

fn section(ui: &mut egui::Ui, title: &str, count: usize, add: impl FnOnce(&mut egui::Ui)) {
    let p = Palette::of(ui.ctx());
    egui::CollapsingHeader::new(RichText::new(format!("{title}  {count}")).small().strong().color(p.text_weak))
        .id_salt(title)
        .default_open(title != "Tags")
        .show(ui, add);
}

/// The branch filter field at the top of the sidebar.
pub fn filter_id(tab: &RepoTab) -> egui::Id {
    egui::Id::new(("sidebar-filter", &tab.paths.workdir))
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let mut actions = Vec::new();
    ui.add_space(6.0);
    let id = filter_id(tab);
    ui.add(
        egui::TextEdit::singleline(&mut tab.sidebar_filter)
            .id(id)
            .hint_text("Filter")
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);
    let filter = tab.sidebar_filter.to_lowercase();
    let matches = |s: &str| filter.is_empty() || s.to_lowercase().contains(&filter);
    let selected = tab.selected_commit;

    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let changes = tab.status.change_count();
        let right = if changes > 0 { changes.to_string() } else { String::new() };
        if item(ui, 0, "Local Changes", &right, tab.view == View::LocalChanges, false).clicked() {
            actions.push(Action::View(View::LocalChanges));
        }
        let commits = tab.history.commits.len();
        let right = if tab.history.loading { "…".to_owned() } else { commits.to_string() };
        if item(ui, 0, "All Commits", &right, tab.view == View::History, false).clicked() {
            actions.push(Action::View(View::History));
        }
        ui.add_space(6.0);

        let refs = &tab.refs.refs;
        let of_kind = |k: RefKind| refs.iter().filter(move |r| r.kind == k && matches(&r.short_name));

        let mut local = Tree::default();
        let mut n = 0;
        for r in of_kind(RefKind::LocalBranch) {
            local.insert(&r.short_name, r);
            n += 1;
        }
        section(ui, "Branches", n, |ui| show_tree(ui, tab, &local, 0, "local", selected, &mut actions));

        // Every configured remote, also those without fetched branches yet.
        let mut remotes: BTreeMap<&str, Tree<'_>> =
            tab.refs.remotes.iter().map(|r| (r.as_str(), Tree::default())).collect();
        let mut n = 0;
        for r in of_kind(RefKind::RemoteBranch) {
            let (remote, name) = crate::repo::remote::split_remote_branch(&tab.refs.remotes, &r.short_name)
                .map(|(remote, _)| {
                    let remote = tab.refs.remotes.iter().find(|x| **x == remote).expect("from the list").as_str();
                    (remote, &r.short_name[remote.len() + 1..])
                })
                .unwrap_or_else(|| r.short_name.split_once('/').unwrap_or((&r.short_name, "")));
            remotes.entry(remote).or_default().insert(name, r);
            n += 1;
        }
        section(ui, "Remotes", n, |ui| {
            for (remote, tree) in &remotes {
                let resp = egui::CollapsingHeader::new(RichText::new(*remote).size(13.0))
                    .id_salt(("remote", *remote))
                    .default_open(true)
                    .show(ui, |ui| show_tree(ui, tab, tree, 0, remote, selected, &mut actions));
                let mut chosen = None;
                resp.header_response.context_menu(|ui| super::menus::remote_menu(ui, tab, remote, &mut chosen));
                actions.extend(chosen.map(Action::Ref));
            }
        });

        let mut tags: Vec<&GitRef> = of_kind(RefKind::Tag).collect();
        // Newest-looking versions first.
        tags.sort_by(|a, b| natord(&b.short_name, &a.short_name));
        section(ui, "Tags", tags.len(), |ui| {
            for r in tags {
                let resp =
                    labeled_item(ui, 0, &r.short_name, "", selected == Some(r.target), false, &format!("tag {}", r.short_name));
                if resp.clicked() {
                    actions.push(Action::Reveal(r.target));
                }
                let mut chosen = None;
                resp.context_menu(|ui| super::menus::tag_menu(ui, tab, r, &mut chosen));
                actions.extend(chosen.map(Action::Ref));
            }
        });

        // Keep the raw path for opening; the display string may be lossy.
        let submodules: Vec<(&bstr::BString, String)> =
            tab.refs.submodules.iter().map(|s| (s, s.to_string())).filter(|(_, d)| matches(d)).collect();
        if !submodules.is_empty() {
            section(ui, "Submodules", submodules.len(), |ui| {
                for (raw, display) in submodules {
                    let changed = tab.status.unstaged.iter().any(|f| f.is_submodule && &f.path == raw);
                    let resp = item(ui, 0, &display, if changed { "modified" } else { "" }, false, false)
                        .on_hover_text("Double-click to open in a new tab");
                    if resp.double_clicked() {
                        actions.push(Action::OpenSubmodule(raw.clone()));
                    }
                }
            });
        }

        let stashes: Vec<_> = tab.refs.stashes.iter().filter(|s| matches(&s.message)).collect();
        section(ui, "Stashes", stashes.len(), |ui| {
            for s in stashes {
                let resp = labeled_item(
                    ui,
                    0,
                    &s.message,
                    &format!("{{{}}}", s.index),
                    selected == Some(s.id),
                    false,
                    &format!("stash {}", s.index),
                );
                if resp.clicked() {
                    actions.push(Action::ShowStash(s.id));
                }
                let mut chosen = None;
                resp.context_menu(|ui| super::menus::stash_menu(ui, tab, s, &mut chosen));
                actions.extend(chosen.map(Action::Ref));
            }
        });
    });

    for a in actions {
        match a {
            Action::View(v) => tab.view = v,
            Action::Reveal(id) => tab.reveal_commit(id),
            Action::ShowStash(id) => tab.select_commit(id),
            Action::Ref(a) => super::menus::apply(tab, a),
            Action::OpenSubmodule(path) => {
                use std::os::unix::ffi::OsStrExt as _;
                tab.open_request = Some(tab.paths.workdir.join(std::ffi::OsStr::from_bytes(&path)));
            }
        }
    }
}

/// Natural ordering so `v1.10` sorts after `v1.9`.
fn natord(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<Result<u64, &str>> {
        let mut out = Vec::new();
        let mut rest = s;
        while !rest.is_empty() {
            let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if digits > 0 {
                out.push(rest[..digits].parse().map_err(|_| &rest[..digits]));
                rest = &rest[digits..];
            } else {
                let end = rest.find(|c: char| c.is_ascii_digit()).unwrap_or(rest.len());
                out.push(Err(&rest[..end]));
                rest = &rest[end..];
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(&cb) {
        let o = match (x, y) {
            (Ok(x), Ok(y)) => x.cmp(y),
            (Err(x), Err(y)) => x.cmp(y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        };
        if o.is_ne() {
            return o;
        }
    }
    ca.len().cmp(&cb.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["v1.10", "v1.9", "v1.9.1", "v2.0-rc1", "v2.0"];
        v.sort_by(|a, b| natord(a, b));
        assert_eq!(v, ["v1.9", "v1.9.1", "v1.10", "v2.0", "v2.0-rc1"]);
    }
}
