//! Measures history loading and graph layout: `tine-bench <repo>...`.

use std::{path::Path, time::Instant};

use anyhow::Result;
use tine_core::{CancelToken, CliReader, RepoReader, graph::Graph, log::LogOptions};

/// Peak and current resident memory in MiB, from /proc.
fn memory_mib() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1024.0
    };
    (field("VmHWM:"), field("VmRSS:"))
}

fn bench(path: &Path) -> Result<()> {
    let start = Instant::now();
    let reader = CliReader::open(path)?;
    let refs = reader.refs()?;
    let t_refs = start.elapsed();

    let mut graph = Graph::new();
    let mut commits = Vec::new();
    let mut first_batch = None;
    let mut graph_time = std::time::Duration::ZERO;
    reader.log(&LogOptions::default(), &CancelToken::default(), &mut |batch| {
        let t = Instant::now();
        for c in &batch {
            graph.push(c.id, &c.parents);
        }
        graph_time += t.elapsed();
        commits.extend(batch);
        first_batch.get_or_insert_with(|| start.elapsed());
        true
    })?;
    graph.finish();
    let total = start.elapsed();
    let max_width = graph.rows().iter().map(|r| r.width).max().unwrap_or(0);
    let edges: usize = graph.rows().iter().map(|r| r.edges.len()).sum();
    let (peak, rss) = memory_mib();

    println!("{}", path.display());
    println!("  refs            {:>8} ({} refs)", format!("{:.0?}", t_refs), refs.refs.len());
    println!("  first batch     {:>8}", format!("{:.0?}", first_batch.unwrap_or_default()));
    println!("  all commits     {:>8} ({} commits)", format!("{:.2?}", total), commits.len());
    println!(
        "  graph layout    {:>8} (max width {max_width}, {edges} edges, {:.0} MiB)",
        format!("{:.0?}", graph_time),
        graph.heap_bytes() as f64 / (1 << 20) as f64
    );
    println!("  memory          peak {peak:.0} MiB, now {rss:.0} MiB");
    Ok(())
}

/// Peak resident memory of finished child processes (git), in MiB.
fn children_peak_mib() -> f64 {
    // SAFETY: getrusage fills the struct we pass.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    usage.ru_maxrss as f64 / 1024.0
}

/// What a search runs over: the revision set is every ref, so record it.
fn refs_fingerprint(git: &tine_core::GitRunner) -> Result<String> {
    let out = git.output(["for-each-ref", "--format=%(objectname) %(refname)"])?;
    let head = git.output(["rev-parse", "HEAD"])?;
    // FNV-1a: stable across runs and machines.
    let hash = out.iter().chain(head.iter()).fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    Ok(format!("HEAD {} · {} refs · fingerprint {hash:016x}", String::from_utf8_lossy(&head).trim(), out.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count()))
}

fn percentiles(mut v: Vec<std::time::Duration>) -> String {
    v.sort();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("p50 {:.1?}  p95 {:.1?}  max {:.1?}", at(0.5), at(0.95), v[v.len() - 1])
}

/// `search <repo> <message|author|changes|path> <query>`: time to the first
/// match and to the end, run three times (the first run is cold).
fn bench_search(repo: &Path, kind: &str, query: &str) -> Result<()> {
    use tine_core::search::{SearchEnd, SearchKind, search};
    let kind = match kind {
        "message" => SearchKind::Message,
        "author" => SearchKind::Author,
        "changes" => SearchKind::Changes,
        "path" => SearchKind::Path,
        other => anyhow::bail!("unknown search kind {other}"),
    };
    let reader = CliReader::open(repo)?;
    println!("{} — {kind:?} {query:?}", repo.display());
    println!("  {}", refs_fingerprint(reader.runner())?);
    // With TINE_BENCH_CANCEL_MS, cancel after that long and time the stop.
    if let Some(ms) = std::env::var("TINE_BENCH_CANCEL_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        let cancel = CancelToken::default();
        let c2 = cancel.clone();
        let fired = std::sync::Arc::new(std::sync::Mutex::new(None));
        let f2 = fired.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            *f2.lock().unwrap() = Some(Instant::now());
            c2.cancel();
        });
        let end = search(reader.runner(), kind, query, &cancel, |_| true)?;
        anyhow::ensure!(end == SearchEnd::Stopped, "the search finished before the cancel; use a smaller TINE_BENCH_CANCEL_MS");
        let at = fired.lock().unwrap().expect("cancel fired");
        println!("  cancelled after {ms}ms: stopped {:.0?} after the cancel", at.elapsed());
        return Ok(());
    }
    for run in 1..=3 {
        let start = Instant::now();
        let mut first = None;
        let mut count = 0;
        let mut batches = 0;
        let end = search(reader.runner(), kind, query, &CancelToken::default(), |b| {
            first.get_or_insert_with(|| start.elapsed());
            count += b.len();
            batches += 1;
            true
        })?;
        anyhow::ensure!(end == SearchEnd::Complete, "search did not complete: {end:?}");
        println!(
            "  run {run}: first match {:>8}, all {:>8} ({count} matches in {batches} batches)",
            first.map(|d| format!("{d:.0?}")).unwrap_or_else(|| "-".into()),
            format!("{:.2?}", start.elapsed())
        );
    }
    println!("  git peak memory {:.0} MiB", children_peak_mib());
    Ok(())
}

/// `fuzzy <repo> [query…]`: Quick Launch filtering over the repository's
/// tracked files (the index, or HEAD's tree without one).
fn bench_fuzzy(repo: &Path, queries: &[String]) -> Result<()> {
    use tine_core::fuzzy::{Prepared, top_prepared};
    let reader = CliReader::open(repo)?;
    let git = reader.runner();
    let mut out = git.output(["ls-files", "--cached", "--deduplicate", "-z"])?;
    let source = if out.is_empty() {
        out = git.output(["ls-tree", "-r", "-z", "--name-only", "HEAD"])?;
        "HEAD tree"
    } else {
        "index"
    };
    let files: Vec<String> =
        out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    let start = Instant::now();
    let prepared: Vec<Prepared> = files.iter().map(|f| Prepared::new(f)).collect();
    println!("{} — {} files from the {source}, prepared in {:.1?}", repo.display(), files.len(), start.elapsed());
    println!("  {}", refs_fingerprint(git)?);
    let defaults = ["m", "main", "srcmain", "xyzzy"].map(String::from);
    for q in if queries.is_empty() { &defaults[..] } else { queries } {
        let times: Vec<_> = (0..50)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(top_prepared(q, &prepared, 50));
                t.elapsed()
            })
            .collect();
        println!("  {q:>40}: {}", percentiles(times));
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("fuzzy") {
        anyhow::ensure!(args.len() >= 2, "usage: tine-bench fuzzy <repo> [query…]");
        return bench_fuzzy(Path::new(&args[1]), &args[2..]);
    }
    if args.first().map(String::as_str) == Some("search") {
        anyhow::ensure!(args.len() == 4, "usage: tine-bench search <repo> <message|author|changes|path> <query>");
        return bench_search(Path::new(&args[1]), &args[2], &args[3]);
    }
    anyhow::ensure!(!args.is_empty(), "usage: tine-bench <repo>... | tine-bench search <repo> <kind> <query>");
    for p in args {
        bench(Path::new(&p))?;
    }
    Ok(())
}
