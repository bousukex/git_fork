//! Measures history loading and graph layout: `pengitto-bench <repo>...`.

use std::{path::Path, time::Instant};

use anyhow::Result;
use pengitto_core::{CancelToken, CliReader, RepoReader, graph::Graph, log::LogOptions};

/// Peak and current resident memory in MiB, from /proc.
/// FNV-1a, to pin inputs.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

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
fn refs_fingerprint(git: &pengitto_core::GitRunner) -> Result<String> {
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
    use pengitto_core::search::{SearchEnd, SearchKind, search};
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
    // With PENGITTO_BENCH_CANCEL_MS, cancel after that long and time the stop.
    if let Some(ms) = std::env::var("PENGITTO_BENCH_CANCEL_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
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
        anyhow::ensure!(end == SearchEnd::Stopped, "the search finished before the cancel; use a smaller PENGITTO_BENCH_CANCEL_MS");
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
    use pengitto_core::fuzzy::{Prepared, top_prepared};
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

/// `blame <repo> <path> [rev]`: git blame and the parsing, timed apart,
/// five runs.
fn bench_blame(repo: &Path, path: &str, rev: Option<&str>) -> Result<()> {
    let reader = CliReader::open(repo)?;
    let git = reader.runner();
    let rev = match rev {
        Some(r) => git.output(["rev-parse", "--verify", r])?,
        None => git.output(["rev-parse", "HEAD"])?,
    };
    let oid = pengitto_core::model::Oid::from_hex(rev.trim_ascii()).ok_or_else(|| anyhow::anyhow!("bad revision"))?;
    println!("{} — blame {path} at {}", repo.display(), oid.to_hex());
    let (mut git_times, mut parse_times, mut totals) = (Vec::new(), Vec::new(), Vec::new());
    let mut size = (0, 0);
    for _ in 0..5 {
        // What pengitto_core::blame::blame does, step by step.
        let start = Instant::now();
        let out = git.output(["blame", "--porcelain", &oid.to_hex(), "--", path])?;
        let git_done = Instant::now();
        let b = pengitto_core::blame::parse_porcelain(&out)?;
        let parsed = Instant::now();
        git_times.push(git_done - start);
        parse_times.push(parsed - git_done);
        let total = Instant::now();
        let b2 = pengitto_core::blame::blame(git, path.as_bytes(), Some(oid), false, &CancelToken::default())?;
        totals.push(total.elapsed());
        anyhow::ensure!(b2 == b, "the library and the steps disagree");
        size = (b.lines.len(), b.commits.len());
    }
    println!("  {} lines, {} commits", size.0, size.1);
    println!("  git blame   {}", percentiles(git_times));
    println!("  parsing     {}", percentiles(parse_times));
    println!("  blame()     {} (size checks + git + parsing)", percentiles(totals));
    let (peak, _) = memory_mib();
    println!("  memory: this process peak {peak:.0} MiB, git peak {:.0} MiB", children_peak_mib());
    Ok(())
}

/// `highlight <file>…`: syntax highlighting of each file in one thread, and
/// in parallel as one stream (as blame does), five runs each, with the
/// peak memory. `PENGITTO_BENCH_TWO_STREAMS=1` also parses it as two full
/// streams — an upper bound for a diff (the app parses a diff's old side
/// only up to its last deleted line; `tests/perf.rs` measures that path).
fn bench_highlight(files: &[String]) -> Result<()> {
    use pengitto_core::highlight::{Highlighter, syntax_for, syntaxes};
    println!("{} threads", std::thread::available_parallelism().map_or(1, |n| n.get()));
    let t = Instant::now();
    syntaxes();
    println!("syntax definitions loaded in {:.1?}", t.elapsed());
    let two_streams = std::env::var_os("PENGITTO_BENCH_TWO_STREAMS").is_some();
    for f in files {
        let text = String::from_utf8_lossy(&std::fs::read(f)?).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let syntax = syntax_for(f.as_bytes(), lines.first().copied()).ok_or_else(|| anyhow::anyhow!("no syntax for {f}"))?;
        let (mut single, mut one, mut two) = (Vec::new(), Vec::new(), Vec::new());
        let mut spans = 0;
        let cancel = CancelToken::default();
        for _ in 0..5 {
            let t = Instant::now();
            let mut h = Highlighter::new(syntax);
            spans = lines.iter().map(|l| h.line(l).len()).sum::<usize>();
            single.push(t.elapsed());
            let t = Instant::now();
            std::hint::black_box(pengitto_core::highlight::highlight_streams(syntax, std::slice::from_ref(&lines), &cancel, usize::MAX));
            one.push(t.elapsed());
            if two_streams {
                let t = Instant::now();
                std::hint::black_box(pengitto_core::highlight::highlight_streams(syntax, &[lines.clone(), lines.clone()], &cancel, usize::MAX));
                two.push(t.elapsed());
            }
        }
        println!("{f}: {} lines ({}), {spans} spans, fnv {:016x}", lines.len(), syntax.name, fnv(text.as_bytes()));
        println!("  one thread           {}", percentiles(single));
        println!("  parallel, 1 stream   {}", percentiles(one));
        if two_streams {
            println!("  parallel, 2 streams  {}", percentiles(two));
        }
    }
    let (peak, rss) = memory_mib();
    println!("memory: peak {peak:.0} MiB, now {rss:.0} MiB");
    Ok(())
}

/// `worktrees <repo>`: what M5c runs in the background — listing the
/// worktrees and telling whether HEAD uses Git LFS (each refs reload), the
/// LFS marks of the local changes (each status reload) and of HEAD's
/// commit, and what removing a worktree would lose (looked at in the dialog
/// and again before removing). Warm (one run first), 7 runs each.
fn bench_worktrees(repo: &Path) -> Result<()> {
    use pengitto_core::{
        GitRunner,
        diff::{DiffSource, commit_files},
        lfs::{self, LfsRow},
        worktree,
    };
    let git = GitRunner::new(repo);
    let reader = CliReader::open(repo)?;
    let time = |name: &str, f: &mut dyn FnMut() -> Result<String>| -> Result<()> {
        let what = f()?;
        let mut runs = Vec::new();
        for _ in 0..7 {
            let t = Instant::now();
            f()?;
            runs.push(t.elapsed());
        }
        println!("  {name:<28} {}  ({what})", percentiles(runs));
        Ok(())
    };
    let files = git.output(["ls-files", "-z"])?.split(|&b| b == 0).filter(|p| !p.is_empty()).count();
    println!("{}: {files} files", repo.display());
    time("worktree list", &mut || Ok(format!("{} worktrees", worktree::list(&git)?.len())))?;
    time("uses Git LFS (HEAD)", &mut || Ok(format!("{}", lfs::repo_uses_lfs(&git)?)))?;
    let status = reader.status()?;
    let head = reader.refs()?.head_target();
    let unstaged = LfsRow::rows(DiffSource::Unstaged, head, &status.unstaged);
    let staged = LfsRow::rows(DiffSource::Staged, head, &status.staged);
    time("LFS marks, local changes", &mut || {
        let (u, s) = (lfs::tracked_rows(&git, &unstaged)?, lfs::tracked_rows(&git, &staged)?);
        Ok(format!("{} + {} rows, {} marked", unstaged.len(), staged.len(), u.len() + s.len()))
    })?;
    if let Some(id) = head {
        let parent = git.output_with(["rev-parse", "-q", "--verify", &format!("{}^", id.to_hex())], &[1])?;
        let parent = pengitto_core::model::Oid::from_hex(parent.trim_ascii());
        let changed = commit_files(&git, id, parent)?;
        let source = DiffSource::Commit { id, parent };
        let rows = LfsRow::rows(source, None, &changed);
        time("LFS marks, HEAD's commit", &mut || Ok(format!("{} rows, {} marked", rows.len(), lfs::tracked_rows(&git, &rows)?.len())))?;
    }
    if let Some(w) = worktree::list(&git)?.into_iter().find(|w| !w.main && w.path.is_dir()) {
        time("what removing would lose", &mut || {
            let l = worktree::losses(&git, &w)?;
            Ok(format!("{}: {} changed, {} ignored", w.name(), l.changed.len(), l.ignored.len()))
        })?;
    }
    let (peak, _) = memory_mib();
    println!("  memory: peak {peak:.0} MiB");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("worktrees") {
        anyhow::ensure!(args.len() == 2, "usage: pengitto-bench worktrees <repo>");
        return bench_worktrees(Path::new(&args[1]));
    }
    if args.first().map(String::as_str) == Some("highlight") {
        anyhow::ensure!(args.len() >= 2, "usage: pengitto-bench highlight <file>…");
        return bench_highlight(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("blame") {
        anyhow::ensure!(args.len() >= 3, "usage: pengitto-bench blame <repo> <path> [rev]");
        return bench_blame(Path::new(&args[1]), &args[2], args.get(3).map(String::as_str));
    }
    if args.first().map(String::as_str) == Some("fuzzy") {
        anyhow::ensure!(args.len() >= 2, "usage: pengitto-bench fuzzy <repo> [query…]");
        return bench_fuzzy(Path::new(&args[1]), &args[2..]);
    }
    if args.first().map(String::as_str) == Some("search") {
        anyhow::ensure!(args.len() == 4, "usage: pengitto-bench search <repo> <message|author|changes|path> <query>");
        return bench_search(Path::new(&args[1]), &args[2], &args[3]);
    }
    anyhow::ensure!(!args.is_empty(), "usage: pengitto-bench <repo>... | pengitto-bench search <repo> <kind> <query>");
    for p in args {
        bench(Path::new(&p))?;
    }
    Ok(())
}
