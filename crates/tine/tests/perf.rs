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
    let doc = tine::blame_model::BlameDoc::new(blame);
    println!("BlameDoc for {} lines: {:.2?}", doc.blame.lines.len(), t.elapsed());

    let (mut shown, mut first_frames) = (Vec::new(), Vec::new());
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
                doc.as_ref().expect("blamed");
                shown.push(start.elapsed());
                first_frames.push(frame);
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "blame did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        h.state_mut().active_tab_mut().unwrap().clear_browse();
        h.step();
    }
    println!("blame {file} at {}: request → shown: {}", head.short(), stats(shown));
    println!("the frame that first shows it: {}", stats(first_frames));
    h.state_mut().active_tab_mut().unwrap().open_blame(Some(head), file.clone().into(), None);
    while !matches!(h.state_mut().active_tab_mut().unwrap().browse.last(), Some(tine::repo::browse::Browse::Blame(v)) if v.doc.is_some()) {
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    let times: Vec<Duration> =
        (0..300).map(|i| frame_with_key(&mut h, if i % 3 == 2 { egui::Key::ArrowDown } else { egui::Key::PageDown })).collect();
    println!("frames while scrolling the blame: {}", stats(times));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    println!("{}", status.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")).collect::<Vec<_>>().join("  "));
}
