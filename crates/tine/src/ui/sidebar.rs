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
    OpenSubmodule(String),
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
    let p = Palette::of(ui.ctx());
    let (rect, resp) = super::list_row(ui, ITEM_H, selected, true);
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

fn show_tree(ui: &mut egui::Ui, tree: &Tree<'_>, depth: usize, salt: &str, selected: Option<Oid>, actions: &mut Vec<Action>) {
    for (name, sub) in &tree.folders {
        let id = ui.id().with((salt, name, depth));
        egui::CollapsingHeader::new(RichText::new(name).size(13.0))
            .id_salt(id)
            .default_open(true)
            // The collapsing body already indents its children.
            .show(ui, |ui| show_tree(ui, sub, 0, salt, selected, actions));
    }
    for (name, r) in &tree.leaves {
        let resp = item(ui, depth, name, &tracking_text(r.track), selected == Some(r.target), r.is_head);
        if resp.clicked() {
            actions.push(Action::Reveal(r.target));
        }
        resp.on_hover_text(&r.full_name);
    }
}

fn section(ui: &mut egui::Ui, title: &str, count: usize, add: impl FnOnce(&mut egui::Ui)) {
    let p = Palette::of(ui.ctx());
    egui::CollapsingHeader::new(RichText::new(format!("{title}  {count}")).small().strong().color(p.text_weak))
        .id_salt(title)
        .default_open(title != "Tags")
        .show(ui, add);
}

pub fn show(ui: &mut egui::Ui, tab: &mut RepoTab) {
    let mut actions = Vec::new();
    ui.add_space(6.0);
    ui.add(
        egui::TextEdit::singleline(&mut tab.sidebar_filter)
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
        section(ui, "Branches", n, |ui| show_tree(ui, &local, 0, "local", selected, &mut actions));

        let mut remotes: BTreeMap<&str, Tree<'_>> = BTreeMap::new();
        let mut n = 0;
        for r in of_kind(RefKind::RemoteBranch) {
            let (remote, name) = r.short_name.split_once('/').unwrap_or((&r.short_name, ""));
            remotes.entry(remote).or_default().insert(name, r);
            n += 1;
        }
        section(ui, "Remotes", n, |ui| {
            for (remote, tree) in &remotes {
                egui::CollapsingHeader::new(RichText::new(*remote).size(13.0))
                    .id_salt(("remote", *remote))
                    .default_open(true)
                    .show(ui, |ui| show_tree(ui, tree, 0, remote, selected, &mut actions));
            }
        });

        let mut tags: Vec<&GitRef> = of_kind(RefKind::Tag).collect();
        // Newest-looking versions first.
        tags.sort_by(|a, b| natord(&b.short_name, &a.short_name));
        section(ui, "Tags", tags.len(), |ui| {
            for r in tags {
                let resp = item(ui, 0, &r.short_name, "", selected == Some(r.target), false);
                if resp.clicked() {
                    actions.push(Action::Reveal(r.target));
                }
            }
        });

        let submodules: Vec<String> =
            tab.refs.submodules.iter().map(|s| s.to_string()).filter(|s| matches(s)).collect();
        if !submodules.is_empty() {
            section(ui, "Submodules", submodules.len(), |ui| {
                for s in submodules {
                    let changed = tab.status.unstaged.iter().any(|f| f.is_submodule && f.path == s.as_str());
                    let resp = item(ui, 0, &s, if changed { "modified" } else { "" }, false, false)
                        .on_hover_text("Double-click to open in a new tab");
                    if resp.double_clicked() {
                        actions.push(Action::OpenSubmodule(s));
                    }
                }
            });
        }

        let stashes: Vec<_> = tab.refs.stashes.iter().filter(|s| matches(&s.message)).collect();
        section(ui, "Stashes", stashes.len(), |ui| {
            for s in stashes {
                let resp = item(ui, 0, &s.message, &format!("{{{}}}", s.index), selected == Some(s.id), false);
                if resp.clicked() {
                    actions.push(Action::ShowStash(s.id));
                }
            }
        });
    });

    for a in actions {
        match a {
            Action::View(v) => tab.view = v,
            Action::Reveal(id) => tab.reveal_commit(id),
            Action::ShowStash(id) => tab.select_commit(id),
            Action::OpenSubmodule(path) => tab.open_request = Some(tab.paths.workdir.join(path)),
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
