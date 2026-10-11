//! UI frame cost while scrolling a large history.
//!
//! `TINE_PERF_REPO=/path/to/big/repo cargo test --release -p tine --test perf -- --ignored --nocapture`

use std::time::{Duration, Instant};

use egui_kittest::Harness;

#[test]
#[ignore = "needs TINE_PERF_REPO"]
fn scroll_large_history() {
    let Ok(repo) = std::env::var("TINE_PERF_REPO") else { return };
    let mut h = Harness::builder()
        .with_size([1360.0, 860.0])
        .build_eframe(|cc| tine::app::TineApp::new(cc, vec![repo.clone().into()]));

    let start = Instant::now();
    // Wait for the full history (polls happen inside each frame).
    while start.elapsed() < Duration::from_secs(60) {
        h.step();
        std::thread::sleep(Duration::from_millis(20));
        if h.state().is_idle() {
            break;
        }
    }
    println!("history loaded in {:.2?} (including frame overhead)", start.elapsed());

    let mut times = Vec::new();
    for i in 0..300 {
        let key = if i < 200 { egui::Key::PageDown } else { egui::Key::ArrowDown };
        h.key_press(key);
        let t = Instant::now();
        h.step();
        times.push(t.elapsed());
    }
    times.sort();
    let avg = times.iter().sum::<Duration>() / times.len() as u32;
    println!(
        "frame CPU while scrolling: avg {:.2?}, p95 {:.2?}, max {:.2?}",
        avg,
        times[times.len() * 95 / 100],
        times[times.len() - 1]
    );
    let rss = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    println!("{}", rss.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")).collect::<Vec<_>>().join("  "));
}

/// One frame with a key press in it (kittest's `key_press` runs a frame per
/// event, which would mix several frames into one measurement).
fn frame_with_key(h: &mut Harness<'_, tine::app::TineApp>, key: egui::Key) -> Duration {
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
    }
    let t = Instant::now();
    h.step();
    t.elapsed()
}

fn stats(mut times: Vec<Duration>) -> String {
    times.sort();
    let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    format!("{} samples: p50 {:.2?}, p95 {:.2?}, max {:.2?}", times.len(), at(0.5), at(0.95), times[times.len() - 1])
}

/// Frame cost while a commit search runs, and when its matches arrive.
/// `TINE_PERF_QUERY` (default "borrowck") is searched in messages.
#[test]
#[ignore = "needs TINE_PERF_REPO"]
fn search_while_scrolling() {
    let Ok(repo) = std::env::var("TINE_PERF_REPO") else { return };
    let query = std::env::var("TINE_PERF_QUERY").unwrap_or_else(|_| "borrowck".into());
    let mut h = Harness::builder()
        .with_size([1360.0, 860.0])
        .build_eframe(|cc| tine::app::TineApp::new(cc, vec![repo.clone().into()]));
    let start = Instant::now();
    while !h.state().is_idle() {
        assert!(start.elapsed() < Duration::from_secs(60), "history did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    {
        let tab = h.state_mut().active_tab_mut().unwrap();
        tab.open_search();
        tab.search.query = query.clone();
        tab.search_changed();
    }
    h.step();
    // Scroll the history, not the search field.
    h.ctx.memory_mut(|m| {
        if let Some(f) = m.focused() {
            m.surrender_focus(f);
        }
    });
    let start = Instant::now();
    let (mut first_drawn, mut done) = (None, None);
    let mut times = Vec::new();
    for i in 0.. {
        times.push(frame_with_key(&mut h, if i % 2 == 0 { egui::Key::PageDown } else { egui::Key::ArrowDown }));
        let tab = h.state_mut().active_tab_mut().unwrap();
        assert!(tab.search.error.is_none(), "{:?}", tab.search.error);
        // Rows exist after a frame: that frame drew them.
        if first_drawn.is_none() && !tab.search.rows.is_empty() {
            first_drawn = Some(start.elapsed());
        }
        if tab.search.complete {
            done = Some((start.elapsed(), tab.search.matches.len()));
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "search did not complete");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (done, n) = done.expect("complete");
    let first = first_drawn.expect("matches were drawn");
    println!("query {query:?}: first match drawn {first:.2?} after typing (300ms debounce included), complete {done:.2?} ({n} matches)");
    println!("frames while searching and scrolling: {}", stats(times));
}


/// Blame of a long file: building the display model, the request to the
/// frame that first shows it (five times), then scrolling.
/// `TINE_PERF_BLAME` names the file (default "sequencer.c", as in git/git).
#[test]
#[ignore = "needs TINE_PERF_REPO"]
fn blame_and_scroll() {
    let Ok(repo) = std::env::var("TINE_PERF_REPO") else { return };
    let file = std::env::var("TINE_PERF_BLAME").unwrap_or_else(|_| "sequencer.c".into());
    let mut h = Harness::builder()
        .with_size([1360.0, 860.0])
        .build_eframe(|cc| tine::app::TineApp::new(cc, vec![repo.clone().into()]));
    let start = Instant::now();
    while !h.state().is_idle() {
        assert!(start.elapsed() < Duration::from_secs(60), "history did not load");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    let head = h.state_mut().active_tab_mut().unwrap().head_commit().unwrap();

    // The display model alone (built on the worker thread).
    let git = tine_core::CliReader::open(std::path::Path::new(&repo)).unwrap().runner().clone();
    let blame = tine_core::blame::blame(&git, file.as_bytes(), Some(head), false, &tine_core::CancelToken::default()).unwrap();
    let t = Instant::now();
    let doc = tine::blame_model::BlameDoc::new(blame, Default::default());
    println!("BlameDoc for {} lines: {:.2?}", doc.blame.lines.len(), t.elapsed());
    let t = Instant::now();
    let colored = doc.highlighted(file.as_bytes(), &Default::default()).expect("a known language");
    println!("highlighting it: {:.2?} ({} spans)", t.elapsed(), colored.spans.iter().map(Vec::len).sum::<usize>());

    let has_doc = |h: &mut Harness<'_, tine::app::TineApp>, colored: bool| {
        matches!(h.state_mut().active_tab_mut().unwrap().browse.last(),
            Some(tine::repo::browse::Browse::Blame(v)) if matches!(&v.doc, Some(Ok(d)) if !colored || !d.spans.is_empty()))
    };
    let (mut shown, mut first_frames, mut colored_at, mut colored_frames) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut same_frame = 0;
    for _ in 0..5 {
        let start = Instant::now();
        h.state_mut().active_tab_mut().unwrap().open_blame(Some(head), file.clone().into(), None);
        loop {
            // The frame in which the result is applied also draws it first.
            let t = Instant::now();
            h.step();
            let frame = t.elapsed();
            let tab = h.state_mut().active_tab_mut().unwrap();
            if let Some(tine::repo::browse::Browse::Blame(v)) = tab.browse.last()
                && let Some(doc) = &v.doc
            {
                let doc = doc.as_ref().expect("blamed");
                shown.push(start.elapsed());
                first_frames.push(frame);
                // Plain and colored may both arrive before one frame.
                if !doc.spans.is_empty() {
                    same_frame += 1;
                    colored_at.push(start.elapsed());
                    colored_frames.push(frame);
                }
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "blame did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        // Then the colors (the frame that swaps them in draws them first).
        while colored_at.len() < shown.len() {
            let t = Instant::now();
            h.step();
            let frame = t.elapsed();
            if has_doc(&mut h, true) {
                colored_at.push(start.elapsed());
                colored_frames.push(frame);
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "highlighting did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        h.state_mut().active_tab_mut().unwrap().clear_browse();
        h.step();
    }
    println!("blame {file} at {}: request → shown: {}", head.short(), stats(shown));
    println!("the frame that first shows it: {}", stats(first_frames));
    println!("request → colored: {} ({same_frame} of them in the first frame already)", stats(colored_at));
    println!("the frame that first shows the colors: {}", stats(colored_frames));
    h.state_mut().active_tab_mut().unwrap().open_blame(Some(head), file.clone().into(), None);
    while !has_doc(&mut h, true) {
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    let times: Vec<Duration> =
        (0..300).map(|i| frame_with_key(&mut h, if i % 3 == 2 { egui::Key::ArrowDown } else { egui::Key::PageDown })).collect();
    println!("frames while scrolling the colored blame: {}", stats(times));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    println!("{}", status.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")).collect::<Vec<_>>().join("  "));
}

/// FNV-1a, to pin the inputs of a measurement.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// The first `n` lines of `files` at `rev` of `repo`, concatenated (fixed
/// inputs: printed with their hash).
fn fixed_lines(repo: &std::path::Path, rev: &str, files: &[String], n: usize) -> String {
    let mut out = String::new();
    let mut lines = 0;
    for f in files {
        for l in git_out(repo, &["show", &format!("{rev}:{f}")]).lines() {
            if lines == n {
                return out;
            }
            out.push_str(l);
            out.push('\n');
            lines += 1;
        }
    }
    assert_eq!(lines, n, "not enough lines in {files:?}");
    out
}

/// 10,000 lines of C (git/git's sequencer.c then diff.c, at the HEAD of
/// `TINE_PERF_REPO`) and of Rust (Tine's own sources at 73cc960).
fn fixed_inputs(git_repo: &std::path::Path) -> Vec<(&'static str, String)> {
    let head = git_out(git_repo, &["rev-parse", "HEAD"]);
    let c = fixed_lines(git_repo, head.trim(), &["sequencer.c".into(), "diff.c".into()], 10_000);
    let tine = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rust_files: Vec<String> = git_out(&tine, &["ls-tree", "-r", "--name-only", "73cc960", "crates"])
        .lines()
        .filter(|f| f.ends_with(".rs"))
        .map(str::to_owned)
        .collect();
    let rust = fixed_lines(&tine, "73cc960", &rust_files, 10_000);
    println!("C: git/git {} sequencer.c + diff.c, 10000 lines, fnv {:016x}", &head[..8], fnv(c.as_bytes()));
    println!("Rust: tine 73cc960 crates/**/*.rs, 10000 lines, fnv {:016x}", fnv(rust.as_bytes()));
    vec![("big.c", c), ("big.rs", rust)]
}

/// A diff of `text`: every line added (`'+'`), or all of it context.
fn whole_file_diff(text: &str, marker: char) -> tine_core::model::FileDiff {
    let n = text.lines().count();
    let header = if marker == '+' { format!("@@ -0,0 +1,{n} @@") } else { format!("@@ -1,{n} +1,{n} @@") };
    let mut p = format!("--- a/x\n+++ b/x\n{header}\n");
    for l in text.lines() {
        p.push(marker);
        p.push_str(l);
        p.push('\n');
    }
    tine_core::diff::parse_patch(p.as_bytes())
}

fn peak_rss() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")).collect::<Vec<_>>().join("  ")
}

/// Syntax highlighting of large diffs as the worker does it after sending
/// the plain diff (`DiffDoc::highlighted`; the UI never waits for it).
/// `TINE_PERF_REPO` = git/git: sequencer.c across 3,000 commits, the
/// same with the whole file as context, and fixed 10,000-line C and Rust
/// inputs shown added whole and as all context.
#[test]
#[ignore = "needs TINE_PERF_REPO"]
fn highlight_large_diffs() {
    use tine::{diff_model::DiffDoc, settings::DocOptions};
    use tine_core::{
        CancelToken, RepoReader as _,
        diff::{DiffOptions, DiffSource},
        model::{ChangeKind, ChangedFile, FileDiff},
    };
    let Ok(repo) = std::env::var("TINE_PERF_REPO") else { return };
    let repo = std::path::PathBuf::from(repo);
    let reader = tine_core::CliReader::open(&repo).unwrap();
    let rev = |r: &str| tine_core::model::Oid::from_hex(git_out(&repo, &["rev-parse", r]).trim().as_bytes()).unwrap();
    let (head, old) = (rev("HEAD"), rev("HEAD~3000"));
    let file = ChangedFile { path: "sequencer.c".into(), orig_path: None, kind: ChangeKind::Modified, is_submodule: false };
    let source = DiffSource::Commit { id: head, parent: Some(old) };
    let cancel = CancelToken::default();
    println!("{} threads; git/git {} vs {}", std::thread::available_parallelism().map_or(1, |n| n.get()), head.short(), old.short());
    let mut cases: Vec<(String, &str, FileDiff)> = vec![
        ("sequencer.c, 3000 commits apart".into(), "sequencer.c", reader.file_diff(source, &file, DiffOptions::default(), &cancel).unwrap()),
        (
            "sequencer.c, whole file as context".into(),
            "sequencer.c",
            reader.file_diff(source, &file, DiffOptions { context_lines: 100_000, ..Default::default() }, &cancel).unwrap(),
        ),
    ];
    for (path, text) in fixed_inputs(&repo) {
        cases.push((format!("{path}: 10000 lines added"), path, whole_file_diff(&text, '+')));
        cases.push((format!("{path}: 10000 lines of context"), path, whole_file_diff(&text, ' ')));
    }
    println!("before: {}", peak_rss());
    for (name, path, diff) in cases {
        let t = Instant::now();
        let doc = DiffDoc::new(diff, DocOptions::default());
        let build = t.elapsed();
        let mut times = Vec::new();
        let mut spans = 0;
        for _ in 0..7 {
            let t = Instant::now();
            let colored = doc.highlighted(path.as_bytes(), &cancel).expect("highlighted");
            times.push(t.elapsed());
            spans = colored.rows.iter().map(|r| r.spans.len()).sum();
        }
        println!("{name}: {} rows, {spans} spans; plain doc {build:.2?}", doc.rows.len());
        println!("  highlighting: {}", stats(times));
    }
    println!("after: {}", peak_rss());
}

/// A large colored diff in the app: the request to the frame showing it
/// plain, then colored (that frame), and scrolling it. The fixed Rust input
/// (see [`fixed_inputs`]) is an untracked file of a new repository.
#[test]
#[ignore = "needs TINE_PERF_REPO"]
fn colored_diff_in_the_app() {
    let Ok(repo) = std::env::var("TINE_PERF_REPO") else { return };
    let rust = fixed_inputs(std::path::Path::new(&repo)).pop().unwrap().1;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git_out(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("big.rs"), &rust).unwrap();
    let mut h = Harness::builder().with_size([1360.0, 860.0]).build_eframe(|cc| {
        let mut app = tine::app::TineApp::new(cc, vec![d.into()]);
        app.show_local_changes();
        app
    });
    let start = Instant::now();
    while !h.state_mut().active_tab_mut().unwrap().status.unstaged.iter().any(|f| f.path == "big.rs") {
        assert!(start.elapsed() < Duration::from_secs(30));
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    let doc_state = |h: &mut Harness<'_, tine::app::TineApp>| match &h.state_mut().active_tab_mut().unwrap().changes_diff.doc {
        Some(Ok(d)) => Some(d.rows.iter().any(|r| !r.spans.is_empty())),
        _ => None,
    };
    let (mut shown, mut first, mut colored_at, mut swap, mut same_frame) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0);
    for _ in 0..5 {
        let tab = h.state_mut().active_tab_mut().unwrap();
        // Forget the shown diff, or it would stay up while reloading.
        tab.changes_selected = None;
        tab.changes_diff = tine::repo::DiffPane::default();
        let f = tab.list(tine::repo::Side::Unstaged).into_iter().find(|f| f.path == "big.rs").unwrap();
        let start = Instant::now();
        tab.click_change(tine::repo::Side::Unstaged, &f, false, false);
        loop {
            let t = Instant::now();
            h.step();
            let frame = t.elapsed();
            match doc_state(&mut h) {
                Some(colored) if shown.len() == colored_at.len() => {
                    shown.push(start.elapsed());
                    first.push(frame);
                    if colored {
                        same_frame += 1;
                        colored_at.push(start.elapsed());
                        swap.push(frame);
                        break;
                    }
                }
                Some(true) => {
                    colored_at.push(start.elapsed());
                    swap.push(frame);
                    break;
                }
                _ => {}
            }
            assert!(start.elapsed() < Duration::from_secs(60), "the diff did not load");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    println!("10000-line Rust file: request → shown plain: {}", stats(shown));
    println!("  the frame that first shows it: {}", stats(first));
    println!("  request → colored: {} ({same_frame} in the first frame already)", stats(colored_at));
    println!("  the frame that swaps the colors in: {}", stats(swap));
    // Scroll the colored diff with the wheel over it.
    h.input_mut().events.push(egui::Event::PointerMoved(egui::pos2(1050.0, 450.0)));
    h.step();
    let times: Vec<Duration> = (0..300)
        .map(|_| {
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -8.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            });
            let t = Instant::now();
            h.step();
            t.elapsed()
        })
        .collect();
    println!("  frames while scrolling it: {}", stats(times));
    println!("  {}", peak_rss());
}
