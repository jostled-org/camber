use crate::error::BenchError;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BenchResult {
    pub req_per_sec: f64,
    pub latency_avg_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p90_ms: f64,
    pub latency_p99_ms: f64,
    pub error_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadGenerator {
    Wrk,
    Oha,
}

/// Detect which load generator is available. Prefers wrk (TechEmpower standard),
/// falls back to oha.
pub fn detect_load_generator() -> Option<LoadGenerator> {
    if tool_on_path("wrk") {
        return Some(LoadGenerator::Wrk);
    }
    if tool_on_path("oha") {
        return Some(LoadGenerator::Oha);
    }
    None
}

fn tool_on_path(name: &str) -> bool {
    // wrk exits 1 on --version, so just check the process spawns successfully.
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Check whether `oha` is on PATH. Kept for backward compatibility with existing tests.
pub fn oha_available() -> bool {
    tool_on_path("oha")
}

// --- wrk support ---

/// Run wrk against a URL and parse the output into a `BenchResult`.
pub fn run_wrk(url: &str, connections: u32, duration: Duration) -> Result<BenchResult, BenchError> {
    let duration_secs = format!("{}s", duration.as_secs().max(1));
    let connections_str = connections.to_string();

    let output = Command::new("wrk")
        .args([
            "-t2",
            "-d",
            &duration_secs,
            "-c",
            &connections_str,
            "--latency",
            url,
        ])
        .output()
        .map_err(|e| {
            BenchError::LoadGenerator(format!("wrk failed to start: {e}").into_boxed_str())
        })?;

    match output.status.success() {
        true => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_wrk_output(&stdout)
        }
        false => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(BenchError::LoadGenerator(
                format!("wrk failed: {stderr}").into_boxed_str(),
            ))
        }
    }
}

/// Parse wrk text output into a `BenchResult`.
///
/// Expected format (with `--latency` flag):
/// ```text
///   Thread Stats   Avg      Stdev     Max   +/- Stdev
///     Latency   523.45us  120.33us    5.23ms   89.12%
///   Latency Distribution
///      50%  516.00us
///      90%  640.00us
///      99%  820.00us
///   Requests/sec:  18823.40
/// ```
pub fn parse_wrk_output(text: &str) -> Result<BenchResult, BenchError> {
    let req_per_sec = parse_wrk_field(text, "Requests/sec:")?;
    let latency_avg_ms = parse_wrk_latency_avg(text)?;
    let latency_p50_ms = parse_wrk_percentile(text, "50%")?;
    let latency_p90_ms = parse_wrk_percentile(text, "90%")?;
    let latency_p99_ms = parse_wrk_percentile(text, "99%")?;

    // wrk reports errors on a "Socket errors:" line; count them if present
    let error_count = parse_wrk_errors(text);

    Ok(BenchResult {
        req_per_sec,
        latency_avg_ms,
        latency_p50_ms,
        latency_p90_ms,
        latency_p99_ms,
        error_count,
    })
}

fn parse_wrk_field(text: &str, label: &str) -> Result<f64, BenchError> {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix(label)
                .and_then(|rest| rest.trim().parse::<f64>().ok())
        })
        .ok_or_else(|| {
            BenchError::LoadGenerator(format!("wrk output missing '{label}'").into_boxed_str())
        })
}

/// Parse the average latency from the "Thread Stats" section.
/// Format: `Latency   523.45us  120.33us    5.23ms   89.12%`
fn parse_wrk_latency_avg(text: &str) -> Result<f64, BenchError> {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix("Latency")?;
            // First token after "Latency" is the average
            let avg_token = rest.split_whitespace().next()?;
            parse_duration_to_ms(avg_token)
        })
        .ok_or_else(|| BenchError::LoadGenerator("wrk output missing latency avg".into()))
}

/// Parse a percentile from the "Latency Distribution" section.
/// Format: `50%  516.00us`
fn parse_wrk_percentile(text: &str, pct: &str) -> Result<f64, BenchError> {
    // Only look at lines AFTER "Latency Distribution" to avoid matching
    // the "+/- Stdev" percentages in the Thread Stats section.
    let distribution_section = text
        .find("Latency Distribution")
        .map(|i| &text[i..])
        .unwrap_or("");

    distribution_section
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix(pct)?;
            let val_token = rest.split_whitespace().next()?;
            parse_duration_to_ms(val_token)
        })
        .ok_or_else(|| {
            BenchError::LoadGenerator(
                format!("wrk output missing percentile '{pct}'").into_boxed_str(),
            )
        })
}

/// Parse wrk duration tokens like `523.45us`, `1.23ms`, `2.00s` into milliseconds.
fn parse_duration_to_ms(token: &str) -> Option<f64> {
    if let Some(val) = token.strip_suffix("us") {
        return val.parse::<f64>().ok().map(|v| v / 1000.0);
    }
    if let Some(val) = token.strip_suffix("ms") {
        return val.parse::<f64>().ok();
    }
    if let Some(val) = token.strip_suffix('s') {
        return val.parse::<f64>().ok().map(|v| v * 1000.0);
    }
    None
}

/// Parse error count from wrk's "Socket errors:" line.
fn parse_wrk_errors(text: &str) -> u64 {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix("Socket errors:")?;
            // Format: "connect 0, read 0, write 0, timeout 0"
            Some(
                rest.split(',')
                    .filter_map(parse_wrk_error_part)
                    .sum::<u64>(),
            )
        })
        .unwrap_or(0)
}

fn parse_wrk_error_part(part: &str) -> Option<u64> {
    part.split_whitespace().last()?.parse::<u64>().ok()
}

// --- oha support ---

/// Run oha against a URL and parse JSON output into a `BenchResult`.
pub fn run_oha(url: &str, connections: u32, duration: Duration) -> Result<BenchResult, BenchError> {
    let duration_secs = duration.as_secs().max(1).to_string();
    let connections_str = connections.to_string();

    let output = Command::new("oha")
        .args([
            "--json",
            "-z",
            &duration_secs,
            "-c",
            &connections_str,
            "--no-tui",
            url,
        ])
        .output()
        .map_err(|e| {
            BenchError::LoadGenerator(format!("oha failed to start: {e}").into_boxed_str())
        })?;

    match output.status.success() {
        true => parse_oha_json(&output.stdout),
        false => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(BenchError::LoadGenerator(
                format!("oha failed: {stderr}").into_boxed_str(),
            ))
        }
    }
}

/// Parse oha JSON output into a `BenchResult`.
pub fn parse_oha_json(stdout: &[u8]) -> Result<BenchResult, BenchError> {
    let json: serde_json::Value = serde_json::from_slice(stdout).map_err(|e| {
        BenchError::LoadGenerator(format!("failed to parse oha json: {e}").into_boxed_str())
    })?;

    let summary = &json["summary"];
    let req_per_sec = summary["requestsPerSec"].as_f64().unwrap_or(0.0);
    let latency_avg_ms = summary["average"].as_f64().unwrap_or(0.0) * 1000.0;

    let percentiles = &json["latencyPercentiles"];
    let latency_p50_ms = oha_percentile(percentiles, 50.0);
    let latency_p90_ms = oha_percentile(percentiles, 90.0);
    let latency_p99_ms = oha_percentile(percentiles, 99.0);

    let error_count = json["statusCodeDistribution"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.starts_with("200"))
                .filter_map(|(_, v)| v.as_u64())
                .sum::<u64>()
        })
        .unwrap_or(0);

    Ok(BenchResult {
        req_per_sec,
        latency_avg_ms,
        latency_p50_ms,
        latency_p90_ms,
        latency_p99_ms,
        error_count,
    })
}

fn oha_percentile(percentiles: &serde_json::Value, pct: f64) -> f64 {
    percentiles
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|v| v["percentile"].as_f64() == Some(pct))
                .and_then(|v| v["latency"].as_f64())
        })
        .unwrap_or(0.0)
        * 1000.0
}

/// Dispatch a load test to the appropriate tool based on the generator variant.
fn dispatch_load(
    generator: LoadGenerator,
    url: &str,
    connections: u32,
    duration: Duration,
) -> Result<BenchResult, BenchError> {
    match generator {
        LoadGenerator::Wrk => run_wrk(url, connections, duration),
        LoadGenerator::Oha => run_oha(url, connections, duration),
    }
}

// --- Three-phase benchmark pattern (TechEmpower methodology) ---

/// Run a load generator against `url` using the TechEmpower three-phase pattern:
/// 1. Primer: 5s at 8 connections (discard results, verify server responds)
/// 2. Warmup: full duration at max concurrency (discard results)
/// 3. Measured: full duration at each concurrency level (collect results)
///
/// Sleeps 2 seconds between phases.
pub fn three_phase_bench(
    generator: LoadGenerator,
    url: &str,
    concurrency_levels: &[u32],
    duration: Duration,
) -> Result<Box<[(u32, BenchResult)]>, BenchError> {
    let max_concurrency = concurrency_levels.iter().copied().max().unwrap_or(8);

    // Phase 1: Primer — 5s at 8 connections, discard
    dispatch_load(generator, url, 8, Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_secs(2));

    // Phase 2: Warmup — full duration at max concurrency, discard
    dispatch_load(generator, url, max_concurrency, duration)?;
    std::thread::sleep(Duration::from_secs(2));

    // Phase 3: Measured — full duration at each concurrency level
    let mut results = Vec::with_capacity(concurrency_levels.len());
    for &conns in concurrency_levels {
        let result = dispatch_load(generator, url, conns, duration)?;
        results.push((conns, result));
    }

    Ok(results.into_boxed_slice())
}

/// Run a single load test using the best available external tool.
/// Returns an error if neither wrk nor oha is available.
pub fn run_load(
    url: &str,
    connections: u32,
    duration: Duration,
    warmup: bool,
) -> Result<BenchResult, BenchError> {
    let generator = detect_load_generator().ok_or_else(|| {
        BenchError::LoadGenerator("no load generator found (install wrk or oha)".into())
    })?;
    if warmup {
        dispatch_load(generator, url, connections, Duration::from_secs(2))?;
    }
    dispatch_load(generator, url, connections, duration)
}
