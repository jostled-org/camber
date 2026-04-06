use crate::load::BenchResult;
use std::fmt::Write;

/// Results for one framework across all concurrency levels.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameworkRun {
    pub framework: Box<str>,
    pub results: Box<[ConcurrencyResult]>,
}

/// A single measured result at one concurrency level.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConcurrencyResult {
    pub concurrency: u32,
    pub result: BenchResult,
}

/// All results for one benchmark (all frameworks, all concurrency levels).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BenchmarkRun {
    pub name: Box<str>,
    pub frameworks: Box<[FrameworkRun]>,
}

impl BenchmarkRun {
    pub fn framework_run(&self, framework: &str) -> Option<&FrameworkRun> {
        self.frameworks.iter().find(|f| &*f.framework == framework)
    }
}

// --- Markdown formatting ---

pub fn format_markdown(runs: &[BenchmarkRun]) -> Box<str> {
    let mut out = String::new();
    write_all_benchmarks(&mut out, runs);
    out.into_boxed_str()
}

/// Format a single benchmark result as a markdown table.
/// Used for incremental output as each benchmark completes.
pub fn format_one_benchmark(run: &BenchmarkRun) -> Box<str> {
    let mut out = String::new();
    write_benchmark_table(&mut out, run);
    out.into_boxed_str()
}

fn write_all_benchmarks(out: &mut String, runs: &[BenchmarkRun]) {
    out.push_str("## Camber Benchmark Results\n");
    for run in runs {
        out.push('\n');
        write_benchmark_table(out, run);
    }
}

fn write_benchmark_table(out: &mut String, run: &BenchmarkRun) {
    let _ = writeln!(out, "### {}\n", run.name);

    let concurrency_levels = collect_concurrency_levels(run);
    let frameworks: Box<[&str]> = run
        .frameworks
        .iter()
        .map(|f| &*f.framework)
        .collect::<Vec<_>>()
        .into_boxed_slice();

    // Header
    out.push_str("| Concurrency |");
    for fw in &frameworks {
        let _ = write!(out, " {fw} req/s | p50 | p90 | p99 |");
    }
    if frameworks.len() >= 2 {
        let _ = write!(out, " {}/{} |", frameworks[0], frameworks[1]);
    }
    out.push('\n');

    // Separator
    out.push_str("|-------------|");
    for _ in &frameworks {
        out.push_str("------:|-----:|-----:|-----:|");
    }
    if frameworks.len() >= 2 {
        out.push_str("------:|");
    }
    out.push('\n');

    // Rows — one per concurrency level
    for &conc in &concurrency_levels {
        let _ = write!(out, "| {conc} |");
        let rps_pair = write_concurrency_row(out, &run.frameworks, conc);
        write_ratio_cell(out, rps_pair, frameworks.len());
        out.push('\n');
    }
}

fn write_concurrency_row(
    out: &mut String,
    frameworks: &[FrameworkRun],
    conc: u32,
) -> (Option<f64>, Option<f64>) {
    let mut first_rps = None;
    let mut second_rps = None;
    for (i, fr) in frameworks.iter().enumerate() {
        let rps = write_framework_cell(out, fr, conc);
        match i {
            0 => first_rps = rps,
            1 => second_rps = rps,
            _ => {}
        }
    }
    (first_rps, second_rps)
}

/// Write a single framework cell (req/s + p50/p90/p99), returning its req/s if present.
fn write_framework_cell(out: &mut String, fr: &FrameworkRun, conc: u32) -> Option<f64> {
    match result_at(fr, conc) {
        Some(r) => {
            write_rps_cell(out, r);
            let _ = write!(
                out,
                " {:.2}ms | {:.2}ms | {:.2}ms |",
                r.latency_p50_ms, r.latency_p90_ms, r.latency_p99_ms
            );
            Some(r.req_per_sec)
        }
        None => {
            let _ = write!(out, " — | — | — | — |");
            None
        }
    }
}

fn collect_concurrency_levels(run: &BenchmarkRun) -> Box<[u32]> {
    let mut levels = std::collections::BTreeSet::new();
    for fr in run.frameworks.iter() {
        for cr in fr.results.iter() {
            levels.insert(cr.concurrency);
        }
    }
    levels.into_iter().collect::<Vec<_>>().into_boxed_slice()
}

fn result_at(fr: &FrameworkRun, concurrency: u32) -> Option<&BenchResult> {
    fr.results
        .iter()
        .find(|cr| cr.concurrency == concurrency)
        .map(|cr| &cr.result)
}

fn write_rps_cell(out: &mut String, r: &BenchResult) {
    match r.error_count {
        0 => {
            let _ = write!(out, " {:.0} req/s |", r.req_per_sec);
        }
        n => {
            let _ = write!(out, " {:.0} req/s ({n} err) |", r.req_per_sec);
        }
    }
}

fn write_ratio_cell(out: &mut String, rps_pair: (Option<f64>, Option<f64>), fw_count: usize) {
    if fw_count < 2 {
        return;
    }
    match rps_pair {
        (Some(a), Some(b)) if b > 0.0 => {
            let _ = write!(out, " {:.0}% |", (a / b) * 100.0);
        }
        _ => {
            let _ = write!(out, " — |");
        }
    }
}

// --- LOC comparison ---

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocComparison {
    pub camber_loc: usize,
    pub axum_loc: usize,
    pub go_loc: usize,
}

pub fn format_markdown_with_loc(runs: &[BenchmarkRun], loc: &LocComparison) -> Box<str> {
    let mut out = String::new();
    write_all_benchmarks(&mut out, runs);
    out.push('\n');
    format_loc_table(&mut out, loc);
    out.into_boxed_str()
}

fn format_loc_table(out: &mut String, loc: &LocComparison) {
    let camber_f = loc.camber_loc as f64;

    out.push_str("## Lines of Code\n\n");
    out.push_str("| Framework | Lines | Ratio |\n");
    out.push_str("|-----------|------:|------:|\n");
    let _ = writeln!(out, "| Camber | {} | — |", loc.camber_loc);
    write_loc_row(out, "Axum", loc.axum_loc, camber_f);
    write_loc_row(out, "Go", loc.go_loc, camber_f);
}

fn write_loc_row(out: &mut String, name: &str, lines: usize, camber_f: f64) {
    let ratio = match lines {
        0 => 0.0,
        n => (camber_f / n as f64) * 100.0,
    };
    let _ = writeln!(out, "| {name} | {lines} | {ratio:.0}% |");
}

// --- JSON output ---

pub fn format_json(runs: &[BenchmarkRun]) -> Result<Box<str>, serde_json::Error> {
    let json = serde_json::to_string_pretty(runs)?;
    Ok(json.into_boxed_str())
}
