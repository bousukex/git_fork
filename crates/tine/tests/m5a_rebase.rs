//! Interactive rebase against real git, with the built `tine` as git's
//! editor (M5a). Results are checked down to each commit's message and
//! tree, the index, the work tree and the session left behind.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use common::git;
use tine_core::{
    CancelToken, GitRunner,
    integrate::{Step, StopReason},
    interactive::{self, Action, Editor, Ended, MessageKey, Plan, Range},
    model::{Oid, RepoState},
    sequencer, status,
};

fn editor() -> Editor {
    Editor { program: PathBuf::from(env!("CARGO_BIN_EXE_tine")) }
}

fn runner(dir: &Path) -> GitRunner {
    GitRunner::new(dir)
        .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .with_env("GIT_COMMITTER_DATE", "@1900000000 +0900")
}

fn out(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").current_dir(dir).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

/// main with c1..c5, each adding its own file `fN` (so any order works);
/// returns the repo and the commit before c1.
fn repo(n: usize) -> (tempfile::TempDir, Oid) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("base"), "base\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    let base = oid(d, "HEAD");
    for i in 1..=n {
        std::fs::write(d.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", &format!("c{i}\n\nbody {i}")], &mut t);
    }
    (dir, base)
}

fn oid(d: &Path, rev: &str) -> Oid {
    Oid::from_hex(out(d, &["rev-parse", rev]).trim().as_bytes()).unwrap()
}

/// Messages from the base up to HEAD, newest first.
fn messages(d: &Path, base: Oid) -> Vec<String> {
    out(d, &["log", "--format=%B%x00", &format!("{}..HEAD", base.to_hex())])
        .split('\0')
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect()
}

fn files(d: &Path) -> Vec<String> {
    let mut f: Vec<String> = out(d, &["ls-tree", "--name-only", "HEAD"]).lines().map(Into::into).collect();
    f.sort();
    f
}

fn clean(d: &Path) -> bool {
    out(d, &["status", "--porcelain"]).is_empty()
}

/// The commit of range item `n` (1-based: c1 is 1).
fn c(r: &Range, n: usize) -> Oid {
    r.commits[n - 1].id
}

fn plan(r: &Range, items: &[(usize, Action)]) -> Plan {
    Plan { items: items.iter().map(|&(n, a)| (c(r, n), a)).collect(), messages: Default::default() }
}

fn git_dir(d: &Path) -> PathBuf {
    d.join(".git")
}

fn session(d: &Path) -> bool {
    interactive::session_dir(&git_dir(d)).exists()
}

fn start(d: &Path, r: &Range, p: &Plan) -> anyhow::Result<Step> {
    interactive::start(&runner(d), &git_dir(d), r, p, &editor(), None)
}

/// Continue as the app does it.
fn cont(d: &Path) -> anyhow::Result<Step> {
    let s = interactive::rebase_runner(&runner(d), &git_dir(d), &editor())?;
    interactive::finish_reword(&s, &git_dir(d))?;
    sequencer::continue_op(&s, &git_dir(d), RepoState::Rebasing)
}

fn finish(d: &Path) -> Option<Ended> {
    interactive::finish_session(&runner(d), &git_dir(d)).unwrap()
}

#[test]
fn reorders_drops_squashes_fixes_up_and_rewords() {
    use Action::*;
    let (dir, base) = repo(5);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    assert_eq!(r.commits.len(), 5);
    // Oldest first: c2 moves first; c3 squashes into it; c1 fixes up into
    // that too; c4 dropped; c5 reworded.
    let mut p = plan(&r, &[(2, Pick), (3, Squash), (1, Fixup), (4, Drop), (5, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 5)), "c5 reworded\n\nwith a body".into());
    p.messages.insert(MessageKey::Group(c(&r, 1)), "c2+c3+c1 together".into());
    assert!(p.problems(&r).is_empty(), "{:?}", p.problems(&r));
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c5 reworded\n\nwith a body", "c2+c3+c1 together"]);
    assert_eq!(files(d), ["base", "f1", "f2", "f3", "f5"], "c4 dropped");
    assert!(clean(d));
    // The app asks right after the operation: over, so the session goes.
    assert!(matches!(finish(d), Some(Ended::Rebased { .. })));
    assert!(!session(d));
    assert!(finish(d).is_none(), "once");
}

#[test]
fn the_combined_message_is_asked_where_the_group_ends() {
    use Action::*;
    let (dir, base) = repo(4);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // A group ending in a fixup, with a drop in between: git asks for the
    // message at c4.
    let mut p = plan(&r, &[(1, Pick), (2, Squash), (3, Drop), (4, Fixup)]);
    let g = &p.groups()[0];
    assert_eq!(p.items[g.terminal().unwrap()].0, c(&r, 4));
    assert_eq!(p.default_group_message(g, &r), "c1\n\nbody 1\n\nc2\n\nbody 2");
    p.messages.insert(MessageKey::Group(c(&r, 4)), "all in one".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["all in one"]);
    assert_eq!(files(d), ["base", "f1", "f2", "f4"]);

    // Unchanged in the dialog: git combines the messages itself.
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Pick), (2, Squash)]);
    assert!(p.needed_messages().is_empty());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c1\n\nbody 1\n\nc2\n\nbody 2"]);
}

#[test]
fn stops_at_edit_to_amend_or_add_commits() {
    use Action::*;
    let (dir, base) = repo(3);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Pick), (2, Edit), (3, Pick)]);
    let step = start(d, &r, &p).unwrap();
    let at = oid(d, "HEAD");
    assert_eq!(step, Step::Stopped(StopReason::Edit(at)), "stopped, not done");
    assert_eq!(status::repo_state(&git_dir(d)), RepoState::Rebasing);
    assert!(session(d), "kept while stopped");
    assert!(finish(d).is_none(), "not over yet");
    // Amend it, and add a commit after it.
    std::fs::write(d.join("f2"), "2 amended\n").unwrap();
    out(d, &["commit", "-qa", "--amend", "-m", "c2 amended"]);
    std::fs::write(d.join("extra"), "x\n").unwrap();
    out(d, &["add", "extra"]);
    out(d, &["commit", "-qm", "added while editing"]);
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c3\n\nbody 3", "added while editing", "c2 amended", "c1\n\nbody 1"]);
    assert_eq!(out(d, &["show", "HEAD~2:f2"]), "2 amended\n");
    assert!(matches!(finish(d), Some(Ended::Rebased { .. })));
    assert!(!session(d));
}

/// main where c2 and c3 both rewrite `shared` (moving c3 before c2
/// conflicts).
fn conflicting_repo() -> (tempfile::TempDir, Oid) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    std::fs::write(d.join("shared"), "0\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "base"], &mut t);
    let base = oid(d, "HEAD");
    for (i, body) in [(1, "1\n"), (2, "2\n"), (3, "3\n")] {
        std::fs::write(d.join(if i == 1 { "own" } else { "shared" }), body).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", &format!("c{i}")], &mut t);
    }
    (dir, base)
}

fn resolve_with_theirs(d: &Path) {
    out(d, &["checkout", "--theirs", "shared"]);
    out(d, &["add", "shared"]);
}

#[test]
fn a_reworded_commit_that_conflicts_keeps_its_new_message() {
    use Action::*;
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // c3 first (conflicts: it expects c2's "2"), reworded.
    let mut p = plan(&r, &[(1, Pick), (3, Reword), (2, Pick)]);
    p.messages.insert(MessageKey::Reword(c(&r, 3)), "c3 reworded".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts);
    resolve_with_theirs(d);
    // c2 then conflicts too.
    assert_eq!(cont(d).unwrap(), Step::Conflicts);
    resolve_with_theirs(d);
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c2", "c3 reworded", "c1"]);
    assert!(clean(d));
}

#[test]
fn a_group_that_conflicts_at_its_end_gets_its_message() {
    use Action::*;
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // c1, then c3 squashed into it (conflicts), then c2.
    let mut p = plan(&r, &[(1, Pick), (3, Squash), (2, Pick)]);
    p.messages.insert(MessageKey::Group(c(&r, 3)), "c1 and c3".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts);
    resolve_with_theirs(d);
    let step = cont(d).unwrap();
    if step == Step::Conflicts {
        resolve_with_theirs(d);
        assert_eq!(cont(d).unwrap(), Step::Done);
    }
    assert_eq!(messages(d, base), ["c2", "c1 and c3"]);
}

#[test]
fn a_group_that_conflicts_in_the_middle_gets_its_message_at_the_end() {
    use Action::*;
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // c3 (in the middle of the group) and then c2 conflict.
    let mut p = plan(&r, &[(1, Pick), (3, Squash), (2, Squash)]);
    p.messages.insert(MessageKey::Group(c(&r, 2)), "all three".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts);
    resolve_with_theirs(d);
    assert_eq!(cont(d).unwrap(), Step::Conflicts, "c2 now");
    assert!(session(d));
    resolve_with_theirs(d);
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["all three"]);
    assert_eq!(out(d, &["show", "HEAD:shared"]), "2\n");
    assert!(clean(d));
    assert!(matches!(finish(d), Some(Ended::Rebased { .. })));
}

#[test]
fn a_reword_after_a_skipped_commit_still_gets_its_message() {
    use Action::*;
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(3, Pick), (1, Reword), (2, Drop)]);
    p.messages.insert(MessageKey::Reword(c(&r, 1)), "c1 reworded".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts, "c3 without c2 conflicts");
    let s = interactive::rebase_runner(&runner(d), &git_dir(d), &editor()).unwrap();
    assert_eq!(sequencer::skip(&s, &git_dir(d), RepoState::Rebasing).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c1 reworded"]);
}

#[test]
fn the_users_rebase_settings_change_nothing() {
    use Action::*;
    let (dir, base) = repo(3);
    let d = dir.path();
    for (k, v) in [
        ("rebase.autoSquash", "true"),
        ("rebase.updateRefs", "true"),
        ("rebase.rebaseMerges", "true"),
        ("rebase.autoStash", "true"),
        ("rebase.missingCommitsCheck", "error"),
        ("rebase.abbreviateCommands", "true"),
        ("rebase.instructionFormat", "%s by %an"),
    ] {
        out(d, &["config", k, v]);
    }
    // A subject autosquash would act on, and a branch updateRefs would move.
    let mut t = 1_800_100_000;
    std::fs::write(d.join("f9"), "9\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "fixup! c1"], &mut t);
    out(d, &["branch", "side", "HEAD~1"]);
    let side = oid(d, "side");
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Pick), (2, Reword), (3, Pick), (4, Pick)]);
    p.messages.insert(MessageKey::Reword(c(&r, 2)), "c2 new".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["fixup! c1", "c3\n\nbody 3", "c2 new", "c1\n\nbody 1"], "no autosquash");
    assert_eq!(oid(d, "side"), side, "no update-refs");
}

#[test]
fn failures_stop_and_keep_the_session() {
    use Action::*;
    // A commit-msg hook that refuses the reworded message.
    let (dir, base) = repo(2);
    let d = dir.path();
    let hook = d.join(".git/hooks/commit-msg");
    std::fs::write(&hook, "#!/bin/sh\ngrep -q forbidden \"$1\" && { echo 'no forbidden words' >&2; exit 1; }\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Pick), (2, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 2)), "forbidden".into());
    let e = format!("{:#}", start(d, &r, &p).unwrap_err());
    assert!(e.contains("no forbidden words"), "{e}");
    assert_eq!(status::repo_state(&git_dir(d)), RepoState::Rebasing);
    assert!(session(d), "kept: the rebase is still going");
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
    assert!(matches!(finish(d), Some(Ended::Unchanged { .. })));

    // The message file disappeared: an error, not git's default message.
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Edit), (2, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 2)), "c2 new".into());
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    std::fs::remove_file(interactive::session_dir(&git_dir(d)).join(format!("msg/reword-{}", c(&r, 2).to_hex()))).unwrap();
    let e = format!("{:#}", cont(d).unwrap_err());
    assert!(e.contains("missing") || e.contains("editor"), "{e}");
    assert!(session(d));
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
    assert_eq!(messages(d, base), ["c2\n\nbody 2", "c1\n\nbody 1"], "back as it was");
    assert!(matches!(finish(d), Some(Ended::Unchanged { .. })));
    assert!(!session(d));

    // A pre-commit hook refuses (git runs it for a reworded commit).
    let (dir, base) = repo(2);
    let d = dir.path();
    let hook = d.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'not now' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Pick), (2, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 2)), "c2 new".into());
    let e = format!("{:#}", start(d, &r, &p).unwrap_err());
    assert!(e.contains("not now"), "{e}");
    assert_eq!(status::repo_state(&git_dir(d)), RepoState::Rebasing);
    assert!(session(d));
    // git made the commit with the old message and left the rewording to
    // whoever continues: Continue rewords it (the hook refuses again here).
    let made = oid(d, "HEAD");
    let e = format!("{:#}", cont(d).unwrap_err());
    assert!(e.contains("not now"), "{e}");
    assert_eq!(oid(d, "HEAD"), made);
    assert_eq!(status::repo_state(&git_dir(d)), RepoState::Rebasing);
    // The hook gone, Continue commits it with the planned message.
    std::fs::remove_file(&hook).unwrap();
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c2 new", "c1\n\nbody 1"]);
    assert!(clean(d));
}

#[test]
fn a_plan_made_before_head_moved_is_not_run() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(2, Pick), (1, Pick)]);
    let mut t = 1_800_200_000;
    std::fs::write(d.join("late"), "x\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "late"], &mut t);
    let e = format!("{:#}", start(d, &r, &p).unwrap_err());
    assert!(e.contains("HEAD moved"), "{e}");
    assert!(!session(d) && status::repo_state(&git_dir(d)) == RepoState::Clean);
}

#[test]
fn abort_and_outside_endings_clear_the_session() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let before = oid(d, "HEAD");
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Edit), (2, Pick)]);
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
    assert_eq!(oid(d, "HEAD"), before);
    let Some(Ended::Unchanged { session: s }) = finish(d) else { panic!("aborted") };
    assert_eq!(s.head, before);
    assert!(!session(d));

    // Finished from the command line: noticed afterwards.
    assert!(matches!(start(d, &r, &plan(&r, &[(2, Edit), (1, Pick)])).unwrap(), Step::Stopped(_)));
    let o = Command::new("git").current_dir(d).args(["rebase", "--continue"]).env("GIT_EDITOR", "true").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(matches!(finish(d), Some(Ended::Rebased { .. })));
    assert!(!session(d));
}

#[test]
fn works_in_a_linked_worktree_and_from_a_path_with_quotes() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let wt = d.parent().unwrap().join(format!("{}-wt", d.file_name().unwrap().to_string_lossy()));
    out(d, &["worktree", "add", "-q", "-b", "other", wt.to_str().unwrap()]);
    let wt_git = PathBuf::from(out(&wt, &["rev-parse", "--absolute-git-dir"]).trim());
    assert_ne!(wt_git, git_dir(d));
    // tine somewhere with a blank and a quote in its path.
    let tools = tempfile::tempdir().unwrap();
    let odd = tools.path().join("my tools/it's");
    std::fs::create_dir_all(&odd).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_tine"), odd.join("tine")).unwrap();
    let editor = Editor { program: odd.join("tine") };
    let r = interactive::load_range(&runner(&wt), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(2, Pick), (1, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 1)), "c1 in the worktree".into());
    assert_eq!(interactive::start(&runner(&wt), &wt_git, &r, &p, &editor, None).unwrap(), Step::Done);
    assert_eq!(messages(&wt, base), ["c1 in the worktree", "c2\n\nbody 2"]);
    assert_eq!(messages(d, base), ["c2\n\nbody 2", "c1\n\nbody 1"], "main untouched");
    let _ = std::fs::remove_dir_all(&wt);
}

#[test]
fn the_root_and_what_cant_be_rebased() {
    let (dir, _) = repo(2);
    let d = dir.path();
    // From the root: the root commit itself is in the plan.
    let r = interactive::load_range(&runner(d), None, &CancelToken::default()).unwrap();
    assert_eq!(r.commits.len(), 3);
    let mut p = Plan::new(&r);
    p.items[0].1 = Action::Reword;
    p.messages.insert(MessageKey::Reword(r.commits[0].id), "the root".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    assert_eq!(out(d, &["log", "--format=%s", "-1", "HEAD~2"]).trim(), "the root");

    // A merge anywhere in the range (here on the second-parent side).
    let mut t = 1_800_300_000;
    let base = oid(d, "HEAD");
    out(d, &["checkout", "-q", "-b", "side"]);
    std::fs::write(d.join("s"), "s\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "side"], &mut t);
    out(d, &["checkout", "-q", "main"]);
    std::fs::write(d.join("m"), "m\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "main"], &mut t);
    git(d, &["merge", "-q", "--no-ff", "-m", "merge", "side"], &mut t);
    let e = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap_err();
    assert!(e.to_string().contains("merge commits"), "{e}");

    // Too many.
    let (dir, base) = repo(0);
    let d = dir.path();
    let mut script = String::new();
    for i in 0..1001 {
        script.push_str(&format!("commit refs/heads/main\ncommitter T <t@x> {} +0000\ndata 2\nc\n", 1_800_000_000 + i));
        if i == 0 {
            script.push_str(&format!("from {}\n", base.to_hex()));
        }
        script.push('\n');
    }
    let mut child = Command::new("git").current_dir(d).args(["fast-import", "--quiet"]).stdin(std::process::Stdio::piped()).spawn().unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), script.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    out(d, &["reset", "-q", "--hard", "main"]);
    let e = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap_err();
    assert!(e.to_string().contains("more than 1000"), "{e}");
}

/// Each commit from the base up to HEAD (newest first): its subject and
/// every file with its content — the whole tree, not just the names.
fn snapshot(d: &Path, base: Oid) -> Vec<(String, Vec<(String, String)>)> {
    let ids: Vec<String> = out(d, &["rev-list", &format!("{}..HEAD", base.to_hex())]).lines().map(Into::into).collect();
    ids.iter()
        .map(|id| {
            let subject = out(d, &["log", "-1", "--format=%s", id]).trim().to_owned();
            let mut files: Vec<(String, String)> = out(d, &["ls-tree", "-r", "--name-only", id])
                .lines()
                .map(|f| (f.to_owned(), out(d, &["show", &format!("{id}:{f}")])))
                .collect();
            files.sort();
            (subject, files)
        })
        .collect()
}

#[test]
fn every_rewritten_commit_has_the_right_tree() {
    use Action::*;
    let (dir, base) = repo(4);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // c3 first, c1 fixed up into it, c2 dropped, c4 kept.
    let p = plan(&r, &[(3, Pick), (1, Fixup), (2, Drop), (4, Pick)]);
    assert_eq!(start(d, &r, &p).unwrap(), Step::Done);
    let f = |n: &str| (n.to_owned(), format!("{}\n", &n[1..]));
    let base_file = ("base".to_owned(), "base\n".to_owned());
    assert_eq!(
        snapshot(d, base),
        [
            ("c4".to_owned(), vec![base_file.clone(), f("f1"), f("f3"), f("f4")]),
            ("c3".to_owned(), vec![base_file, f("f1"), f("f3")]),
        ]
    );
    assert!(clean(d), "index and work tree match HEAD");
    assert!(out(d, &["stash", "list"]).is_empty());
}

#[test]
fn an_amend_file_left_by_a_squash_is_no_edit_stop() {
    use Action::*;
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    // c3 squashed into c1 conflicts (it expects c2's content).
    let p = plan(&r, &[(1, Pick), (3, Squash), (2, Pick)]);
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts);
    resolve_with_theirs(d);
    assert!(git_dir(d).join("rebase-merge/amend").exists(), "git left an amend file");
    assert_eq!(tine_core::integrate::stop_reason(&git_dir(d)), StopReason::Other, "yet it isn't an edit");
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
}

#[test]
fn git_combines_messages_as_they_are_when_it_gets_there() {
    use Action::*;
    // Amended at an edit stop before the squash: the combination uses the
    // amended message.
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Edit), (2, Squash)]);
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(StopReason::Edit(_))));
    out(d, &["commit", "-q", "--amend", "-m", "c1 amended"]);
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c1 amended\n\nc2\n\nbody 2"]);

    // The last member skipped: its message isn't in the result.
    let (dir, base) = conflicting_repo();
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Pick), (2, Drop), (3, Squash)]);
    assert_eq!(start(d, &r, &p).unwrap(), Step::Conflicts);
    let s = interactive::rebase_runner(&runner(d), &git_dir(d), &editor()).unwrap();
    assert_eq!(sequencer::skip(&s, &git_dir(d), RepoState::Rebasing).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c1"]);
}

#[test]
fn a_todo_changed_outside_tine_gets_gits_messages() {
    use Action::*;
    let (dir, base) = repo(3);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Pick), (2, Edit), (3, Squash)]);
    p.messages.insert(MessageKey::Group(c(&r, 3)), "OURS".into());
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    // Someone turns the squash into `fixup -c` (use and edit c3's message).
    let todo = git_dir(d).join("rebase-merge/git-rebase-todo");
    let text = std::fs::read_to_string(&todo).unwrap().replace("squash ", "fixup -c ");
    std::fs::write(&todo, text).unwrap();
    assert_eq!(cont(d).unwrap(), Step::Done, "no error: git's message stays");
    assert_eq!(messages(d, base), ["c3\n\nbody 3", "c1\n\nbody 1"]);
}

#[test]
fn a_session_left_behind_isnt_used_for_someone_elses_rebase() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Edit), (2, Pick)]);
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    // Aborted on the command line, then another rebase started there.
    out(d, &["rebase", "--abort"]);
    let o = Command::new("git")
        .current_dir(d)
        .args(["rebase", "-i", "HEAD~1"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_SEQUENCE_EDITOR", "sed -i 1s/^pick/edit/")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(status::repo_state(&git_dir(d)), RepoState::Rebasing);
    assert!(session(d));
    let s = interactive::read_session(&git_dir(d)).unwrap().unwrap();
    assert!(!interactive::owns_current_rebase(&s));
    // Continuing it doesn't involve Tine's editor.
    let plain = interactive::rebase_runner(&runner(d), &git_dir(d), &editor()).unwrap();
    assert!(plain.command(["version"]).get_envs().all(|(k, _)| k != "TINE_REBASE_DIR"));
    assert!(matches!(finish(d), Some(Ended::Stale { .. })));
    assert!(!session(d));
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
}

#[test]
fn the_end_is_judged_by_the_rebased_branch() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    assert_eq!(r.branch.as_deref(), Some("refs/heads/main"));
    let p = plan(&r, &[(2, Edit), (1, Pick)]);
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    // Aborted outside, then another branch checked out.
    out(d, &["rebase", "--abort"]);
    out(d, &["checkout", "-q", "-b", "elsewhere", "HEAD~1"]);
    assert!(matches!(finish(d), Some(Ended::Unchanged { .. })), "main is where it was");
}

#[test]
fn the_editor_writes_only_the_files_of_its_rebase() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Edit), (2, Pick)]);
    assert!(matches!(start(d, &r, &p).unwrap(), Step::Stopped(_)));
    let sdir = interactive::session_dir(&git_dir(d));
    let id = interactive::read_session(&git_dir(d)).unwrap().unwrap().id;
    let head_before = std::fs::read(git_dir(d).join("HEAD")).unwrap();
    // Not git's todo / message file: refused.
    assert!(interactive::sequence_editor(&git_dir(d).join("HEAD"), &sdir, &id).is_err());
    std::fs::write(git_dir(d).join("COMMIT_EDITMSG"), "x").unwrap();
    assert!(interactive::sequence_editor(&git_dir(d).join("COMMIT_EDITMSG"), &sdir, &id).is_err());
    assert!(interactive::message_editor(&git_dir(d).join("rebase-merge/git-rebase-todo"), &sdir, &id).is_err());
    // A link named like the message file, pointing elsewhere.
    let elsewhere = dir.path().join("elsewhere");
    std::fs::write(&elsewhere, "keep").unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("COMMIT_EDITMSG");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    assert!(interactive::message_editor(&link, &sdir, &id).is_err());
    // A directory that isn't a session directory, a wrong session id.
    assert!(interactive::message_editor(&git_dir(d).join("COMMIT_EDITMSG"), &git_dir(d), &id).is_err());
    assert!(interactive::message_editor(&git_dir(d).join("COMMIT_EDITMSG"), &sdir, "other").is_err());
    assert_eq!(std::fs::read(git_dir(d).join("HEAD")).unwrap(), head_before);
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep");
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();
}

#[test]
fn a_reworded_commit_kept_empty_keeps_its_message() {
    use Action::*;
    // c3 undoes c2: with c2 dropped, c3 becomes empty.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_800_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "T"], &mut t);
    git(d, &["config", "user.email", "t@example.com"], &mut t);
    for (i, content) in [(0, "a\n"), (1, "b\n"), (2, "c\n"), (3, "b\n")] {
        std::fs::write(d.join("f"), content).unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-qm", &format!("c{i}")], &mut t);
    }
    let base = oid(d, "HEAD~3");
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let mut p = plan(&r, &[(1, Pick), (2, Drop), (3, Reword)]);
    p.messages.insert(MessageKey::Reword(c(&r, 3)), "c3 kept, reworded".into());
    assert_eq!(start(d, &r, &p).unwrap(), Step::Empty, "git asks what to do with it");
    let msg = interactive::empty_reword_message(&git_dir(d)).unwrap().expect("the planned message");
    sequencer::commit_empty(&runner(d), Some(&msg)).unwrap();
    assert_eq!(cont(d).unwrap(), Step::Done);
    assert_eq!(messages(d, base), ["c3 kept, reworded", "c1"]);
}

#[test]
fn the_stash_is_recorded_before_git_runs_and_reading_can_be_stopped() {
    use Action::*;
    let (dir, base) = repo(2);
    let d = dir.path();
    let r = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap();
    let p = plan(&r, &[(1, Edit), (2, Pick)]);
    let stash = oid(d, "HEAD~1");
    assert!(matches!(interactive::start(&runner(d), &git_dir(d), &r, &p, &editor(), Some(stash)).unwrap(), Step::Stopped(_)));
    assert_eq!(interactive::read_session(&git_dir(d)).unwrap().unwrap().autostash, Some(stash));
    sequencer::abort(&runner(d), RepoState::Rebasing).unwrap();

    let stopped = CancelToken::default();
    stopped.cancel();
    let e = interactive::load_range(&runner(d), Some(base), &stopped).unwrap_err();
    assert!(e.is::<tine_core::Cancelled>(), "{e:#}");
}

#[test]
fn merges_anywhere_in_the_range_are_found() {
    let (dir, base) = repo(1);
    let d = dir.path();
    let mut t = 1_800_400_000;
    // side: s1, merged with side2 (a merge on side's own line), then main
    // merges side.
    out(d, &["checkout", "-q", "-b", "side2"]);
    std::fs::write(d.join("t"), "t\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "t1"], &mut t);
    out(d, &["checkout", "-q", "-b", "side", "main"]);
    std::fs::write(d.join("s"), "s\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "s1"], &mut t);
    git(d, &["merge", "-q", "--no-ff", "-m", "side merges side2", "side2"], &mut t);
    let inner = oid(d, "HEAD");
    out(d, &["checkout", "-q", "main"]);
    std::fs::write(d.join("m"), "m\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-qm", "m1"], &mut t);
    git(d, &["merge", "-q", "--no-ff", "-m", "main merges side", "side"], &mut t);
    let outer = oid(d, "HEAD");
    let e = interactive::load_range(&runner(d), Some(base), &CancelToken::default()).unwrap_err();
    let Some(interactive::RangeProblem::Merges(ids)) = e.downcast_ref::<interactive::RangeProblem>() else { panic!("{e:#}") };
    assert!(ids.contains(&inner), "the merge off the first-parent line");
    assert!(ids.contains(&outer));
}
