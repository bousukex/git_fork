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
