//! Syntax highlighting and display options through RepoTab (M4c).

mod common;

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use common::{
    gated::{Gate, Gates, gated_tab},
    git, idle, poll_until,
};
use tine::{
    diff_model::RowKind,
    repo::{PatchTarget, RepoTab, Side, View, browse::Browse},
    settings::DocOptions,
    worker::Msg,
};
use tine_core::{CancelToken, highlight::Token, patch::PatchMode};

/// `code.rs` (tab-indented Rust) with two unstaged changes far apart, and
/// an untracked `notes.txt`.
fn rust_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "t"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    let mut body = String::from("fn main() {\n");
    for i in 0..30 {
        body.push_str(&format!("\tlet v{i} = \"{i}\"; // n{i}\n"));
    }
    body.push_str("}\n");
    std::fs::write(d.join("code.rs"), &body).unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "code"], &mut t);
    let changed = body
        .replace("\tlet v2 = \"2\"; // n2\n", "\tlet v2 = \"two\"; // n2\n\tlet extra = 2;\n")
        .replace("\tlet v25 = \"25\"; // n25\n", "\tlet v25 = \"twenty-five\";\n");
    std::fs::write(d.join("code.rs"), changed).unwrap();
    std::fs::write(d.join("notes.txt"), "notes\n").unwrap();
    dir
}

fn plain() -> DocOptions {
    DocOptions { highlight: false, ..Default::default() }
}

fn select(tab: &mut RepoTab, side: Side, path: &str) {
    let f = tab.list(side).into_iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not listed"));
    tab.click_change(side, &f, false, false);
}

fn diff_ready(tab: &RepoTab, path: &str) -> bool {
    tab.changes_diff.file.as_ref().is_some_and(|f| f.path == path)
        && matches!(tab.changes_diff.doc, Some(Ok(_)))
        && !tab.changes_diff.stale
}

fn colored(doc: &tine::diff_model::DiffDoc) -> bool {
    doc.rows.iter().any(|r| !r.spans.is_empty())
}

fn changes_doc(tab: &RepoTab) -> Arc<tine::diff_model::DiffDoc> {
    tab.changes_diff.doc.clone().unwrap().unwrap()
}

fn row_of(tab: &RepoTab, text: &str) -> usize {
    changes_doc(tab).rows.iter().position(|r| r.kind == RowKind::Added && r.text.trim() == text).unwrap()
}

fn open_changes(dir: &std::path::Path) -> RepoTab {
    let mut tab = RepoTab::open(dir, &egui::Context::default()).unwrap();
    tab.view = View::LocalChanges;
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "code.rs")));
    tab
}

fn staged_patch(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(["diff", "--cached", "-U0"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn line_staging_works_on_colored_diffs() {
    let repo = rust_repo();
    let mut tab = open_changes(repo.path());
    select(&mut tab, Side::Unstaged, "code.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs") && colored(&changes_doc(t))), "colors arrive");
    let doc = changes_doc(&tab);
    let extra = row_of(&tab, "let extra = 2;");
    assert!(doc.rows[extra].spans.iter().any(|s| s.token == Token::Keyword));

    // Stage just that line; the colors must not change what is staged.
    tab.select_diff_rows([extra].into(), Some(extra));
    tab.apply_lines(PatchMode::Stage, PatchTarget::Selection);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.iter().any(|f| f.path == "code.rs")));
    let staged = staged_patch(repo.path());
    assert!(staged.contains("+\tlet extra = 2;\n"), "{staged}");
    assert!(!staged.contains("two") && !staged.contains("twenty-five"), "{staged}");

    // Unstage it again from the (colored) staged diff.
    select(&mut tab, Side::Staged, "code.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs")
        && t.changes_selected.as_ref().is_some_and(|(s, _)| *s == Side::Staged)
        && colored(&changes_doc(t))));
    let extra = row_of(&tab, "let extra = 2;");
    tab.select_diff_rows([extra].into(), Some(extra));
    tab.apply_lines(PatchMode::Unstage, PatchTarget::Selection);
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.staged.is_empty()));
}

#[test]
fn colors_arriving_keep_the_line_selection() {
    let repo = rust_repo();
    let mut tab = open_changes(repo.path());
    tab.set_doc_options(plain());
    select(&mut tab, Side::Unstaged, "code.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs")));
    let doc = changes_doc(&tab);
    assert!(!colored(&doc), "plain when highlighting is off");
    let extra = row_of(&tab, "let extra = 2;");
    tab.select_diff_rows([extra].into(), Some(extra));

    let seq = tab.changes_diff.seq_for_test();
    let colors = Arc::new(doc.highlighted(b"code.rs", &CancelToken::default()).unwrap());
    // Colors of an older request are ignored…
    tab.worker_mut().send_for_test(Msg::DiffHighlighted(seq - 1, colors.clone()));
    tab.poll();
    assert!(!colored(&changes_doc(&tab)));
    // …those of the shown diff replace it, keeping the selection.
    tab.worker_mut().send_for_test(Msg::DiffHighlighted(seq, colors));
    tab.poll();
    assert!(colored(&changes_doc(&tab)));
    assert_eq!(tab.changes_diff.selected_rows, [extra].into());
    assert_eq!(tab.changes_diff.anchor, Some(extra));
    assert_eq!(tab.line_actions().unwrap().disabled, None);
}

#[test]
fn display_options_rebuild_shown_docs_without_blaming_again() {
    let repo = rust_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates::default());
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "code.rs")));
    tab.view = View::LocalChanges;
    select(&mut tab, Side::Unstaged, "code.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs") && colored(&changes_doc(t))));
    let head = tab.head_commit().unwrap();
    tab.open_blame(Some(head), "code.rs".into(), None);
    let blame = |t: &RepoTab| match t.browse.last() {
        Some(Browse::Blame(v)) => v.doc.clone().and_then(Result::ok),
        _ => None,
    };
    assert!(poll_until(&mut tab, |t| blame(t).is_some_and(|d| !d.spans.is_empty())), "the blame gets colors");
    assert_eq!(blame(&tab).unwrap().text[1], "    let v0 = \"0\"; // n0");
    let (blames, diffs) = (gates.blame.total(), gates.diff.total());

    tab.set_doc_options(DocOptions { tab_width: 8, highlight: false });
    // The current ones stay up until the new ones are ready.
    assert!(blame(&tab).is_some() && tab.changes_diff.doc.is_some());
    assert!(poll_until(&mut tab, |t| blame(t).is_some_and(|d| d.text[1].starts_with("        let") && d.spans.is_empty())));
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs")
        && changes_doc(t).rows.iter().any(|r| r.text == "        let extra = 2;")
        && !colored(&changes_doc(t))));
    assert_eq!(gates.blame.total(), blames, "the blame is reused, not run again");
    assert!(gates.diff.total() > diffs, "diffs are cheap to load again");

    // Colors come back, again without git blame.
    tab.set_doc_options(DocOptions { tab_width: 8, highlight: true });
    assert!(poll_until(&mut tab, |t| blame(t).is_some_and(|d| d.text[1].starts_with("        let") && !d.spans.is_empty())));
    assert_eq!(gates.blame.total(), blames);
}

#[test]
fn replaced_or_closed_diffs_stop_loading() {
    let repo = rust_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates { diff: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "code.rs")));
    select(&mut tab, Side::Unstaged, "code.rs");
    assert!(poll_until(&mut tab, |_| gates.diff.arrived() >= 1));
    let first = tab.changes_diff.cancel_token();
    select(&mut tab, Side::Unstaged, "notes.txt");
    assert!(first.is_cancelled(), "selecting another file stops the old diff");
    let second = tab.changes_diff.cancel_token();
    assert!(!second.is_cancelled());
    let running = tab.worker_mut().running_jobs();
    assert_eq!(running.count("diff"), 2);
    drop(tab);
    assert!(second.is_cancelled(), "closing the tab stops it");
    // The held jobs really end, without running git.
    gates.diff.open();
    assert!(wait_for(|| running.count("diff") == 0), "both jobs ended");
    assert_eq!(gates.stopped_diffs.load(Ordering::Relaxed), 2);
}

fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn highlighting_stops_with_its_pane() {
    let repo = rust_repo();
    // A large new file (just under the highlighting limit).
    let mut big = String::new();
    for i in 0..19_000 {
        big.push_str(&match i % 4 {
            0 => format!("/// Doc for f{i}.\n"),
            1 => format!("pub fn f{i}(x: &str) -> Option<usize> {{ x.find(\"{i}\") }}\n"),
            2 => format!("const C{i}: [u8; 4] = [0x{:02x}, 1, 2, 3]; // c\n", i % 256),
            _ => "\n".into(),
        });
    }
    std::fs::write(repo.path().join("big.rs"), big).unwrap();
    let mut tab = open_changes(repo.path());
    assert!(poll_until(&mut tab, |t| t.status.unstaged.iter().any(|f| f.path == "big.rs")));
    let running = tab.worker_mut().running_jobs();
    select(&mut tab, Side::Unstaged, "big.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "big.rs")), "the plain diff comes first");
    let plain = changes_doc(&tab);
    let still_highlighting = running.count("diff") == 1 && !colored(&plain);
    select(&mut tab, Side::Unstaged, "notes.txt");
    let stopped = Instant::now();
    assert!(wait_for(|| running.count("diff") == 0));
    let stop = stopped.elapsed();
    // What the whole highlighting takes here.
    let t = Instant::now();
    assert!(plain.highlighted(b"big.rs", &CancelToken::default()).is_some());
    let whole = t.elapsed();
    if still_highlighting {
        // Stopping takes a line or so per thread, not the rest of the file.
        assert!(stop < whole / 3, "stopping took {stop:?}; all of it takes {whole:?}");
    }
}

#[test]
fn display_options_changing_while_results_are_late() {
    let repo = rust_repo();
    let (mut tab, gates) = gated_tab(repo.path(), Gates { diff: Gate::closed(), blame: Gate::closed(), ..Default::default() });
    assert!(poll_until(&mut tab, |t| idle(t) && t.status.unstaged.iter().any(|f| f.path == "code.rs")));
    tab.view = View::LocalChanges;
    select(&mut tab, Side::Unstaged, "code.rs");
    let head = tab.head_commit().unwrap();
    tab.open_blame(Some(head), "code.rs".into(), None);
    assert!(poll_until(&mut tab, |_| gates.diff.arrived() == 1 && gates.blame.arrived() == 1));

    // New options while both are on their way: both start over.
    let wide = DocOptions { tab_width: 8, highlight: true };
    tab.set_doc_options(wide);
    assert!(poll_until(&mut tab, |_| gates.diff.arrived() == 2 && gates.blame.arrived() == 2));
    // The new ones arrive first, the old ones after.
    gates.diff.release(1);
    gates.blame.release(1);
    let blame = |t: &RepoTab| match t.browse.last() {
        Some(Browse::Blame(v)) => v.doc.clone().and_then(Result::ok),
        _ => None,
    };
    let wide_and_colored = |t: &RepoTab| {
        diff_ready(t, "code.rs")
            && colored(&changes_doc(t))
            && changes_doc(t).rows.iter().any(|r| r.text == "        let extra = 2;")
            && blame(t).is_some_and(|d| d.text[1].starts_with("        let") && !d.spans.is_empty())
    };
    assert!(poll_until(&mut tab, wide_and_colored));
    gates.diff.release(0);
    gates.blame.release(0);
    assert!(wait_for(|| gates.stopped_diffs.load(Ordering::Relaxed) == 1 && gates.stopped_blames.load(Ordering::Relaxed) == 1));
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        tab.poll();
        assert!(wide_and_colored(&tab), "an old result came back");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Changing again while the blame is only plain (being colored): the
    // older doc and colors never replace the newer ones.
    let narrow = DocOptions { tab_width: 2, highlight: true };
    tab.set_doc_options(narrow);
    tab.set_doc_options(wide);
    tab.set_doc_options(narrow);
    assert!(poll_until(&mut tab, |t| blame(t).is_some_and(|d| d.text[1].starts_with("  let") && !d.spans.is_empty())));
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        tab.poll();
        assert!(blame(&tab).is_some_and(|d| d.text[1].starts_with("  let") && !d.spans.is_empty()));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(gates.blame.total(), 2, "rebuilt without blaming again");
}

#[test]
fn discarding_lines_of_a_colored_diff() {
    let repo = rust_repo();
    let mut tab = open_changes(repo.path());
    select(&mut tab, Side::Unstaged, "code.rs");
    assert!(poll_until(&mut tab, |t| diff_ready(t, "code.rs") && colored(&changes_doc(t))));
    let extra = row_of(&tab, "let extra = 2;");
    tab.select_diff_rows([extra].into(), Some(extra));
    tab.apply_lines(PatchMode::Discard, PatchTarget::Selection);
    assert!(tab.pending_line_discard.is_some(), "asks first");
    tab.confirm_line_discard();
    assert!(poll_until(&mut tab, |t| idle(t) && diff_ready(t, "code.rs") && !changes_doc(t).rows.iter().any(|r| r.text.contains("extra"))));
    let text = std::fs::read_to_string(repo.path().join("code.rs")).unwrap();
    assert!(!text.contains("let extra = 2;"), "the line is gone");
    assert!(text.contains("\tlet v2 = \"two\"; // n2\n") && text.contains("twenty-five"), "the other changes stay");
}
