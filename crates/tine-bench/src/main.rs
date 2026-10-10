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

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!paths.is_empty(), "usage: tine-bench <repo>...");
    for p in paths {
        bench(Path::new(&p))?;
    }
    Ok(())
}
