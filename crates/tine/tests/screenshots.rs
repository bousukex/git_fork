//! Renders key screens to `target/screenshots/*.png` for visual review.
//!
//! Needs a wgpu adapter (Mesa's lavapipe works headless), so these are
//! opt-in: `cargo test -p tine --test screenshots -- --ignored`.

mod common;

use std::path::PathBuf;

use common::{git, sample_repo, settle};
use egui::Theme;
use egui_kittest::Harness;

static PROMPTS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Credential prompts are process-wide and would show in every app: the
/// test that opens one runs alone ([`exclusive`]), the others side by side.
fn serial() -> std::sync::RwLockReadGuard<'static, ()> {
    PROMPTS.read().unwrap_or_else(|e| e.into_inner())
}

fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
    PROMPTS.write().unwrap_or_else(|e| e.into_inner())
}

fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn shoot<S: eframe::App + 'static>(
    name: &str,
    size: [f32; 2],
    build: impl Fn(&mut eframe::CreationContext<'_>) -> S,
    drive: impl Fn(&mut Harness<'_, S>),
) {
    for theme in [Theme::Light, Theme::Dark] {
        let mut harness = Harness::builder()
            .with_size(size)
            .with_pixels_per_point(1.0)
            .with_theme(theme)
            .wgpu()
            .build_eframe(|cc| {
                tine::setup_context(&cc.egui_ctx);
                build(cc)
            });
        harness.run_steps(4);
        drive(&mut harness);
        let img = harness.render().expect("render");
        let suffix = if theme == Theme::Dark { "dark" } else { "light" };
        img.save(out_dir().join(format!("{name}-{suffix}.png"))).unwrap();
    }
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn ime_probe() {
    let _serial = serial();
    shoot("ime_probe", [900.0, 520.0], |_| tine::probe::ImeProbe::default(), |_| {});
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn history_view() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    shoot(
        "history",
        [1360.0, 820.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            // Select the newest commit (merge) with the keyboard, then the one below.
            h.key_press(egui::Key::ArrowDown);
            h.key_press(egui::Key::ArrowDown);
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn local_changes_view() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    shoot(
        "changes",
        [1360.0, 820.0],
        |cc| {
            let mut app = tine::app::TineApp::new(cc, vec![path.clone()]);
            app.show_local_changes();
            app
        },
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            // Runs once per theme on the same repository: stage only once.
            if let Some(readme) = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "README.md") {
                tab.stage(vec![readme]);
            }
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            tab.commit_subject = "READMEに使い方を追記".into();
            tab.commit_body = "cargo での実行方法を説明する。".into();
            settle(h);
        },
    );
}

/// Many branches forked from one commit, to check that lanes shrink to fit.
fn many_branches_repo(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    std::fs::write(d.join("base.txt"), "base\n").unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "base"], &mut t);
    for i in 0..n {
        git(d, &["checkout", "-q", "-b", &format!("topic-{i:02}"), "main"], &mut t);
        std::fs::write(d.join(format!("f{i}.txt")), "x\n").unwrap();
        git(d, &["add", "-A"], &mut t);
        git(d, &["commit", "-q", "-m", &format!("topic {i}")], &mut t);
    }
    git(d, &["checkout", "-q", "main"], &mut t);
    dir
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn many_lanes() {
    let _serial = serial();
    let repo = many_branches_repo(45);
    let path = repo.path().to_path_buf();
    shoot("many_lanes", [1100.0, 700.0], |cc| tine::app::TineApp::new(cc, vec![path.clone()]), |h| {
        settle(h);
        // Show the rows where all lanes are open (just above the base commit).
        h.key_press(egui::Key::End);
        settle(h);
    });
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn line_staging_view() {
    let _serial = serial();
    let repo = sample_repo();
    let d = repo.path();
    let mut t = 1_950_000_000;
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("long.txt"), &body).unwrap();
    git(d, &["add", "long.txt"], &mut t);
    git(d, &["commit", "-qm", "long file"], &mut t);
    std::fs::write(d.join("long.txt"), body.replace("line 2\n", "line two\n").replace("line 28\n", "line 28\nextra after 28\nmore\n")).unwrap();
    let path = d.to_path_buf();
    shoot(
        "line_staging",
        [1360.0, 820.0],
        |cc| {
            let mut app = tine::app::TineApp::new(cc, vec![path.clone()]);
            app.show_local_changes();
            app
        },
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            let f = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "long.txt").unwrap();
            tab.click_change(tine::repo::Side::Unstaged, &f, false, false);
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            if let Some(Ok(doc)) = &tab.changes_diff.doc {
                let row = doc.rows.iter().position(|r| r.text == "extra after 28").unwrap();
                tab.select_diff_rows([row].into(), Some(row));
            }
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn merge_conflict_view() {
    let _serial = serial();
    let repo = common::diverged_repo();
    let path = repo.path().to_path_buf();
    shoot(
        "conflict",
        [1360.0, 820.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            if tab.status.state == tine_core::model::RepoState::Clean {
                tab.integrate(
                    tine::repo::dialog::Integration::Merge { target: "topic".into(), mode: Default::default() },
                    false,
                );
            }
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            tab.output_open = true;
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn reset_dialog() {
    let _serial = serial();
    let repo = common::diverged_repo();
    std::fs::write(repo.path().join("g.txt"), "unsaved\n").unwrap();
    let path = repo.path().to_path_buf();
    shoot(
        "reset_dialog",
        [1100.0, 700.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            let base = tab.history.commits.iter().find(|c| c.subject == "base").unwrap().id;
            tab.request_reset(base, "base");
            if let Some(tine::repo::dialog::Dialog::Reset { mode, .. }) = &mut tab.dialog {
                *mode = tine_core::integrate::ResetMode::Hard;
            }
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn stash_conflict_view() {
    let _serial = serial();
    let repo = common::diverged_repo();
    let d = repo.path().to_path_buf();
    let mut t = 1_900_000_000;
    std::fs::write(d.join("f.txt"), "stashed edit\n").unwrap();
    git(&d, &["stash", "-q"], &mut t);
    std::fs::write(d.join("f.txt"), "conflicting\n").unwrap();
    git(&d, &["commit", "-qam", "Conflicting edit"], &mut t);
    let path = d.clone();
    shoot(
        "stash_conflict",
        [1360.0, 820.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            // Each theme starts from the clean tree (the stash is kept).
            let mut t = 1_900_000_000;
            git(&d, &["reset", "-q", "--hard"], &mut t);
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            let oid = tab.refs.stashes[0].id;
            tab.stash_apply(oid, false);
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            tab.view = tine::repo::View::LocalChanges;
            settle(h);
        },
    );
}

/// A clone of a bare remote with one unpushed commit (and the dirs to keep).
fn remote_setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let (bare, mine) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut t = 1_880_000_000;
    git(bare.path(), &["init", "-q", "--bare", "-b", "main"], &mut t);
    git(mine.path(), &["clone", "-q", &format!("file://{}", bare.path().display()), "."], &mut t);
    git(mine.path(), &["config", "user.name", "山田 太郎"], &mut t);
    git(mine.path(), &["config", "user.email", "taro@example.com"], &mut t);
    std::fs::write(mine.path().join("f.txt"), "f\n").unwrap();
    git(mine.path(), &["add", "-A"], &mut t);
    git(mine.path(), &["commit", "-qm", "First commit"], &mut t);
    git(mine.path(), &["push", "-q", "origin", "main"], &mut t);
    git(mine.path(), &["switch", "-q", "-c", "feature/ログイン"], &mut t);
    std::fs::write(mine.path().join("g.txt"), "g\n").unwrap();
    git(mine.path(), &["add", "-A"], &mut t);
    git(mine.path(), &["commit", "-qm", "ログイン画面を追加"], &mut t);
    (bare, mine)
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn push_dialog() {
    let _serial = serial();
    let (_bare, mine) = remote_setup();
    let path = mine.path().to_path_buf();
    shoot(
        "push_dialog",
        [1100.0, 700.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            tab.open_push_dialog(None);
            if let Some(tine::repo::dialog::Dialog::Push { force, tags, .. }) = &mut tab.dialog {
                *force = true;
                *tags = true;
            }
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn askpass_dialog() {
    let _exclusive = exclusive();
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let paths = tine_core::RepoPaths { workdir: dir.path().into(), git_dir: dir.path().into(), common_dir: dir.path().into() };
    let (tx, _rx) = crossbeam_channel::unbounded();
    let (env_tx, env_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    tine::ops_registry::submit(
        std::path::Path::new("/test/screenshot/askpass"),
        &paths,
        tine::worker::OpKind::Push("main".into()),
        tx,
        egui::Context::default(),
        Box::new(move |ctx| {
            let env = tine::askpass::env_for(ctx.id, "Push", std::path::Path::new("/home/me/プロジェクト"), None).unwrap();
            env_tx.send(env).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(60));
            Ok(tine::worker::OpOutcome::default())
        }),
    )
    .unwrap();
    let env = env_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
    let (socket, token) =
        (PathBuf::from(get(tine::askpass::SOCKET_ENV)), get(tine::askpass::TOKEN_ENV).into_string().unwrap());
    // Two prompts in turn: one for each theme.
    let asker = std::thread::spawn(move || {
        for _ in 0..2 {
            let _ = tine::askpass::request(&socket, &token, "", "Password for 'https://alice@github.com': ");
        }
    });
    let repo = sample_repo();
    let repo_path = repo.path().to_path_buf();
    shoot(
        "askpass_dialog",
        [1100.0, 700.0],
        |cc| tine::app::TineApp::new(cc, vec![repo_path.clone()]),
        |h| {
            settle(h);
            while tine::askpass::pending().is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            settle(h);
        },
    );
    // Answer both prompts so the asker finishes.
    for _ in 0..2 {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Some(p) = tine::askpass::pending() {
                tine::askpass::answer(p.id, tine::askpass::Answer::Value(String::new()));
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = asker.join();
    release_tx.send(()).unwrap();
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn quick_launch() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    shoot(
        "quick_launch",
        [1100.0, 700.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            h.state_mut().run_command(tine::shortcuts::Command::QuickLaunch);
            h.state_mut().quick_launch().unwrap().query = "fe".into();
            settle(h);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn commit_search() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    for (name, only) in [("search", false), ("search_only_matches", true)] {
        shoot(
            name,
            [1100.0, 600.0],
            |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
            |h| {
                settle(h);
                let tab = h.state_mut().active_tab_mut().unwrap();
                tab.open_search();
                tab.search.query = "readme".into();
                tab.search.only_matches = only;
                tab.search_changed();
                settle(h);
                h.state_mut().active_tab_mut().unwrap().step_match(true);
                settle(h);
            },
        );
    }
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn file_history_and_blame() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    for (name, blame) in [("file_history", false), ("blame", true)] {
        shoot(
            name,
            [1200.0, 640.0],
            |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
            |h| {
                settle(h);
                let tab = h.state_mut().active_tab_mut().unwrap();
                let head = tab.head_commit().unwrap();
                if blame {
                    tab.open_blame(Some(head), "main.rs".into(), None);
                    settle(h);
                    if let Some(tine::repo::browse::Browse::Blame(v)) = h.state_mut().active_tab_mut().unwrap().browse.last_mut() {
                        v.selected = Some(1);
                    }
                } else {
                    tab.open_file_history(head, "main.rs".into(), None);
                }
                settle(h);
            },
        );
    }
}

/// A Rust file with a few kinds of tokens, changed in the work tree.
fn rust_change_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut t = 1_760_000_000;
    git(d, &["init", "-q", "-b", "main"], &mut t);
    git(d, &["config", "user.name", "山田 太郎"], &mut t);
    git(d, &["config", "user.email", "taro@example.com"], &mut t);
    let before = r#"use std::collections::HashMap;

/// Counts words in `text`.
pub struct Counter {
    counts: HashMap<String, usize>,
    limit: usize,
}

impl Counter {
    pub fn new(limit: usize) -> Self {
        Counter { counts: HashMap::new(), limit }
    }

    pub fn add(&mut self, text: &str) {
        for word in text.split_whitespace() {
            *self.counts.entry(word.to_owned()).or_insert(0) += 1;
        }
    }

    pub fn top(&self) -> Vec<(&str, usize)> {
        let mut all: Vec<_> = self.counts.iter().map(|(w, n)| (w.as_str(), *n)).collect();
        all.sort_by(|a, b| b.1.cmp(&a.1));
        all.truncate(self.limit);
        all
    }
}
"#;
    std::fs::write(d.join("counter.rs"), before).unwrap();
    git(d, &["add", "-A"], &mut t);
    git(d, &["commit", "-q", "-m", "単語を数える"], &mut t);
    let after = before
        .replace("/// Counts words in `text`.", "/// Counts words in `text` (case-insensitive).")
        .replace(
            "            *self.counts.entry(word.to_owned()).or_insert(0) += 1;",
            "            // 大文字と小文字を区別しない\n            let key = word.to_lowercase();\n            *self.counts.entry(key).or_insert(0) += 1;",
        )
        .replace("        all.sort_by(|a, b| b.1.cmp(&a.1));", "        all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));");
    std::fs::write(d.join("counter.rs"), after).unwrap();
    dir
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn highlighted_diff() {
    let _serial = serial();
    let repo = rust_change_repo();
    let path = repo.path().to_path_buf();
    // The default look, and a larger code font with wider tabs.
    for (name, settings) in [
        ("highlighted_diff", tine::settings::Settings::default()),
        ("highlighted_diff_large", tine::settings::Settings { diff_font_size: 17.0, tab_width: 8, syntax_highlighting: true }),
    ] {
        shoot_highlighted_diff(name, &path, settings);
    }
}

fn shoot_highlighted_diff(name: &str, path: &std::path::Path, settings: tine::settings::Settings) {
    shoot(
        name,
        [1360.0, 760.0],
        |cc| {
            let mut app = tine::app::TineApp::new(cc, vec![path.to_path_buf()]);
            app.set_settings(settings);
            app.show_local_changes();
            app
        },
        |h| {
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            let f = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "counter.rs").unwrap();
            tab.click_change(tine::repo::Side::Unstaged, &f, false, false);
            settle(h);
            let tab = h.state_mut().active_tab_mut().unwrap();
            let Some(Ok(doc)) = &tab.changes_diff.doc else { panic!("no diff") };
            assert!(doc.rows.iter().any(|r| !r.spans.is_empty()), "colored");
            // One selected line, to show selection over colors.
            let row = doc.rows.iter().position(|r| r.text.contains("to_lowercase")).unwrap();
            tab.select_diff_rows([row].into(), Some(row));
            h.run_steps(2);
        },
    );
}

#[test]
#[ignore = "needs a GPU adapter; run with --ignored"]
fn settings_dialog() {
    let _serial = serial();
    let repo = sample_repo();
    let path = repo.path().to_path_buf();
    shoot(
        "settings",
        [1200.0, 820.0],
        |cc| tine::app::TineApp::new(cc, vec![path.clone()]),
        |h| {
            settle(h);
            h.state_mut().run_command(tine::shortcuts::Command::Settings);
            h.run_steps(6);
        },
    );
}
