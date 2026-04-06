use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

// --- Subprocess server helpers ---

fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

struct ServerProcess {
    child: Child,
    port: u16,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_server(binary: &str, args: &[&str]) -> ServerProcess {
    let port = find_free_port();
    let port_str = port.to_string();

    let mut full_args: Vec<&str> = args.to_vec();
    full_args.push("--port");
    full_args.push(&port_str);

    let mut child = Command::new(binary)
        .args(&full_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {binary}: {e}"));

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.trim() == "ready",
        "expected 'ready' from {binary}, got: {line:?}"
    );

    ServerProcess { child, port }
}

const BENCH_CAMBER: &str = env!("CARGO_BIN_EXE_bench-camber");
const BENCH_AXUM: &str = env!("CARGO_BIN_EXE_bench-axum");
const BENCH_UPSTREAM: &str = env!("CARGO_BIN_EXE_bench-upstream");

fn test_binaries() -> camber_bench::orchestrate::Binaries {
    camber_bench::orchestrate::Binaries {
        camber: BENCH_CAMBER.into(),
        axum: BENCH_AXUM.into(),
        upstream: BENCH_UPSTREAM.into(),
    }
}

// --- HTTP helper ---

fn http_get(url: &str) -> (u16, String) {
    use std::io::{Read, Write};

    let stripped = url.strip_prefix("http://").unwrap_or(url);
    let (addr_str, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };

    let mut stream = None;
    for _ in 0..40 {
        match std::net::TcpStream::connect(addr_str) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut stream = stream.expect("failed to connect after retries");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr_str}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let status_line = response.lines().next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();

    let body_start = response.find("\r\n\r\n").unwrap() + 4;
    let body = response[body_start..].to_string();

    (status, body)
}

// === Step 1 v1 tests ===

/// Start a minimal HTTP server using tokio/hyper directly (not camber)
/// for testing the load generator in isolation.
fn start_trivial_server() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                    let (reader, mut writer) = stream.into_split();
                    let mut buf_reader = BufReader::new(reader);
                    let mut line = String::new();
                    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    loop {
                        // Read until empty line (end of HTTP headers)
                        loop {
                            line.clear();
                            match buf_reader.read_line(&mut line).await {
                                Ok(0) => return,
                                Ok(_) if line.trim().is_empty() => break,
                                Ok(_) => continue,
                                Err(_) => return,
                            }
                        }
                        if writer.write_all(response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
    });

    std::thread::sleep(Duration::from_millis(50));
    addr
}

#[test]
fn load_generator_measures_requests_per_second() {
    if camber_bench::load::detect_load_generator().is_none() {
        eprintln!("skipping: no load generator (wrk/oha) available");
        return;
    }
    let addr = start_trivial_server();
    let url = format!("http://{addr}/");

    let result = camber_bench::load::run_load(&url, 4, Duration::from_secs(2), false).unwrap();

    assert!(result.req_per_sec > 0.0, "expected req/s > 0");
    assert!(result.latency_avg_ms > 0.0, "expected avg latency > 0");
    assert!(result.latency_p99_ms > 0.0, "expected p99 latency > 0");
}

#[test]
fn camber_hello_text_server_responds_200() {
    let (addr, _handle) = camber_bench::servers::camber_server::start_hello_text().unwrap();
    let (status, body) = http_get(&format!("http://{addr}/"));

    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
}

#[test]
fn report_formats_markdown_table() {
    use camber_bench::report;

    let result = camber_bench::load::BenchResult {
        req_per_sec: 100_000.0,
        latency_avg_ms: 0.5,
        latency_p50_ms: 0.3,
        latency_p90_ms: 0.9,
        latency_p99_ms: 1.2,
        error_count: 0,
    };
    let runs = [report::BenchmarkRun {
        name: "hello_text".into(),
        frameworks: Box::new([report::FrameworkRun {
            framework: "Camber".into(),
            results: Box::new([report::ConcurrencyResult {
                concurrency: 16,
                result,
            }]),
        }]),
    }];

    let md = report::format_markdown(&runs);

    assert!(md.contains("### hello_text"), "missing benchmark header");
    assert!(md.contains("| Concurrency |"), "missing table header");
    assert!(md.contains("100000"), "missing req/s value");
}

// === Step 2 v1 tests ===

#[test]
fn axum_hello_text_server_responds_200() {
    let (addr, _handle) = camber_bench::servers::axum_server::start_hello_text().unwrap();
    let (status, body) = http_get(&format!("http://{addr}/"));

    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
}

#[test]
fn camber_path_param_returns_extracted_id() {
    let (addr, _handle) = camber_bench::servers::camber_server::start_path_param().unwrap();
    let (status, body) = http_get(&format!("http://{addr}/users/42"));

    assert_eq!(status, 200);
    assert!(body.contains("42"), "response should contain extracted id");
}

// === Step 3 v1 tests ===

#[test]
fn camber_db_query_returns_json_after_simulated_latency() {
    let upstream = camber_bench::servers::upstream::start().unwrap();
    let (addr, _handle) = camber_bench::servers::camber_server::start_db_query(upstream).unwrap();

    let start = std::time::Instant::now();
    let (status, body) = http_get(&format!("http://{addr}/query"));
    let elapsed = start.elapsed();

    assert_eq!(status, 200);
    assert!(
        body.contains("\"id\""),
        "response should be JSON with id field"
    );
    assert!(
        elapsed >= Duration::from_millis(1),
        "expected >= 1ms latency from simulated db, got {elapsed:?}"
    );
}

#[test]
fn camber_middleware_stack_applies_all_middleware() {
    let upstream = camber_bench::servers::upstream::start().unwrap();
    let (addr, _handle) =
        camber_bench::servers::camber_server::start_middleware_stack(upstream).unwrap();

    // Send request with Origin header to trigger CORS
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("{addr}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://example.com\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let response_lower = response.to_lowercase();
    assert!(
        response_lower.contains("access-control-allow-origin"),
        "response should have CORS headers, got: {response}"
    );
    assert!(response.contains("200 OK"), "response should be 200");
}

// === Step 4 v1 tests ===

#[test]
fn camber_db_query_returns_json_via_fan_out_module() {
    let upstream = camber_bench::servers::upstream::start().unwrap();
    let (addr, _handle) = camber_bench::servers::camber_server::start_db_query(upstream).unwrap();
    let (status, body) = http_get(&format!("http://{addr}/query"));

    assert_eq!(status, 200);
    assert!(
        body.contains("\"id\""),
        "response should be JSON with id field"
    );
}

// === Step 5 v1 tests ===

#[test]
fn go_server_responds_if_go_available() {
    use camber_bench::servers::go_server;

    if !go_server::go_available() {
        eprintln!("skipping: go not on PATH");
        return;
    }

    let binary = go_server::prepare_binary().unwrap();
    let (addr, _handle) = go_server::start(&binary, "hello_text", &[]).unwrap();
    let (status, body) = http_get(&format!("http://{addr}/"));

    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
}

#[test]
fn oha_produces_results_if_available() {
    use camber_bench::load;

    if !load::oha_available() {
        eprintln!("skipping: oha not on PATH");
        return;
    }

    let addr = start_trivial_server();
    let url = format!("http://{addr}/");
    let result = load::run_oha(&url, 10, Duration::from_secs(1)).unwrap();

    assert!(result.req_per_sec > 0.0, "expected req/s > 0");
}

// === Step 6 v1 tests ===

#[test]
fn loc_comparison_included() {
    use camber_bench::report;

    let runs = [report::BenchmarkRun {
        name: "hello_text".into(),
        frameworks: Box::new([
            report::FrameworkRun {
                framework: "Camber".into(),
                results: Box::new([report::ConcurrencyResult {
                    concurrency: 16,
                    result: camber_bench::load::BenchResult {
                        req_per_sec: 100_000.0,
                        latency_avg_ms: 0.5,
                        latency_p50_ms: 0.3,
                        latency_p90_ms: 0.9,
                        latency_p99_ms: 1.2,
                        error_count: 0,
                    },
                }]),
            },
            report::FrameworkRun {
                framework: "Axum".into(),
                results: Box::new([report::ConcurrencyResult {
                    concurrency: 16,
                    result: camber_bench::load::BenchResult {
                        req_per_sec: 110_000.0,
                        latency_avg_ms: 0.4,
                        latency_p50_ms: 0.25,
                        latency_p90_ms: 0.8,
                        latency_p99_ms: 1.0,
                        error_count: 0,
                    },
                }]),
            },
        ]),
    }];

    let loc = report::LocComparison {
        camber_loc: 120,
        axum_loc: 200,
        go_loc: 180,
    };

    let md = report::format_markdown_with_loc(&runs, &loc);

    assert!(md.contains("Lines of Code"), "missing LOC table header");
    assert!(md.contains("120"), "missing Camber LOC value");
    assert!(md.contains("200"), "missing Axum LOC value");
}

// === Step 1 v2 tests (subprocess binaries) ===

#[test]
fn bench_camber_hello_text_responds_200() {
    let server = spawn_server(BENCH_CAMBER, &["--bench", "hello_text"]);
    let (status, body) = http_get(&format!("http://127.0.0.1:{}/", server.port));

    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
}

#[test]
fn bench_axum_hello_text_responds_200() {
    let server = spawn_server(BENCH_AXUM, &["--bench", "hello_text"]);
    let (status, body) = http_get(&format!("http://127.0.0.1:{}/", server.port));

    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
}

#[test]
fn bench_upstream_responds_with_delay() {
    use std::io::{Read, Write as IoWrite};

    let server = spawn_server(BENCH_UPSTREAM, &[]);

    let start = std::time::Instant::now();

    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", server.port);
    stream.write_all(request.as_bytes()).unwrap();

    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap();
    let response = std::str::from_utf8(&buf[..n]).unwrap();
    let elapsed = start.elapsed();

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected 200, got: {response}"
    );
    assert!(
        elapsed >= Duration::from_millis(1),
        "expected >= 1ms latency, got {elapsed:?}"
    );
}

#[test]
fn bench_camber_db_query_hits_upstream() {
    let upstream = spawn_server(BENCH_UPSTREAM, &[]);
    let upstream_addr = format!("127.0.0.1:{}", upstream.port);
    let server = spawn_server(
        BENCH_CAMBER,
        &["--bench", "db_query", "--upstream", &upstream_addr],
    );

    let (status, body) = http_get(&format!("http://127.0.0.1:{}/query", server.port));

    assert_eq!(status, 200);
    assert!(
        body.contains("\"id\""),
        "response should be JSON with id field, got: {body}"
    );
}

// === Step 2 v2 tests (load generator parsing) ===

#[test]
fn wrk_or_oha_detected() {
    use camber_bench::load;

    match load::detect_load_generator() {
        Some(lg) => {
            assert!(
                matches!(lg, load::LoadGenerator::Wrk | load::LoadGenerator::Oha),
                "expected Wrk or Oha variant"
            );
        }
        None => {
            // Neither wrk nor oha on PATH — skip
            return;
        }
    }
}

#[test]
fn wrk_output_parsed_correctly() {
    use camber_bench::load;

    let wrk_output = "\
Running 10s test @ http://127.0.0.1:8080/
  2 threads and 10 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency   523.45us  120.33us    5.23ms   89.12%
    Req/Sec     9.43k   412.34     10.23k    72.00%
  Latency Distribution
     50%  516.00us
     75%  580.00us
     90%  640.00us
     99%  820.00us
  188234 requests in 10.00s, 22.12MB read
Requests/sec:  18823.40
Transfer/sec:      2.21MB";

    let result = load::parse_wrk_output(wrk_output).unwrap();

    assert!(
        (result.req_per_sec - 18823.4).abs() < 0.1,
        "req/s: {}",
        result.req_per_sec
    );
    assert!(
        (result.latency_avg_ms - 0.52345).abs() < 0.001,
        "avg: {}",
        result.latency_avg_ms
    );
    assert!(
        (result.latency_p50_ms - 0.516).abs() < 0.001,
        "p50: {}",
        result.latency_p50_ms
    );
    assert!(
        (result.latency_p90_ms - 0.640).abs() < 0.001,
        "p90: {}",
        result.latency_p90_ms
    );
    assert!(
        (result.latency_p99_ms - 0.820).abs() < 0.001,
        "p99: {}",
        result.latency_p99_ms
    );
}

#[test]
fn oha_json_parsed_correctly() {
    use camber_bench::load;

    let oha_json = r#"{
        "summary": {
            "successRate": 1.0,
            "total": 50000.0,
            "slowest": 0.005,
            "fastest": 0.0001,
            "average": 0.00053,
            "requestsPerSec": 18500.0
        },
        "latencyPercentiles": [
            {"percentile": 50.0, "latency": 0.00052},
            {"percentile": 75.0, "latency": 0.00058},
            {"percentile": 90.0, "latency": 0.00065},
            {"percentile": 99.0, "latency": 0.00082}
        ],
        "statusCodeDistribution": {
            "200": 50000
        }
    }"#;

    let result = load::parse_oha_json(oha_json.as_bytes()).unwrap();

    assert!(
        (result.req_per_sec - 18500.0).abs() < 0.1,
        "req/s: {}",
        result.req_per_sec
    );
    assert!(
        (result.latency_avg_ms - 0.53).abs() < 0.001,
        "avg: {}",
        result.latency_avg_ms
    );
    assert!(
        (result.latency_p50_ms - 0.52).abs() < 0.001,
        "p50: {}",
        result.latency_p50_ms
    );
    assert!(
        (result.latency_p90_ms - 0.65).abs() < 0.001,
        "p90: {}",
        result.latency_p90_ms
    );
    assert!(
        (result.latency_p99_ms - 0.82).abs() < 0.001,
        "p99: {}",
        result.latency_p99_ms
    );
    assert_eq!(result.error_count, 0);
}

// === Step 3 v2 tests (three-phase bench) ===

#[test]
fn three_phase_runs_primer_warmup_measured() {
    use camber_bench::load;

    let Some(lg) = load::detect_load_generator() else {
        return;
    };

    let server = spawn_server(BENCH_CAMBER, &["--bench", "hello_text"]);
    let url = format!("http://127.0.0.1:{}/", server.port);

    let results = load::three_phase_bench(lg, &url, &[8], Duration::from_secs(2)).unwrap();

    assert_eq!(results.len(), 1, "expected one measured result");
    assert_eq!(results[0].0, 8, "concurrency should be 8");
    assert!(
        results[0].1.req_per_sec > 0.0,
        "expected req/s > 0, got {}",
        results[0].1.req_per_sec
    );
}

#[test]
fn multiple_concurrency_levels_produce_multiple_results() {
    use camber_bench::load;

    let Some(lg) = load::detect_load_generator() else {
        return;
    };

    let server = spawn_server(BENCH_CAMBER, &["--bench", "hello_text"]);
    let url = format!("http://127.0.0.1:{}/", server.port);

    let results = load::three_phase_bench(lg, &url, &[8, 16], Duration::from_secs(2)).unwrap();

    assert_eq!(results.len(), 2, "expected two measured results");
    assert_eq!(results[0].0, 8);
    assert_eq!(results[1].0, 16);
    assert!(results[0].1.req_per_sec > 0.0);
    assert!(results[1].1.req_per_sec > 0.0);
}

// === Step 4 v2 tests (full orchestrator) ===

#[test]
fn full_run_tier1_produces_report() {
    use camber_bench::{load, orchestrate, report};

    let Some(lg) = load::detect_load_generator() else {
        return;
    };

    let binaries = test_binaries();
    let config = orchestrate::RunConfig {
        tier: Some(1),
        bench: None,
        concurrency: vec![8].into_boxed_slice(),
        duration: Duration::from_secs(2),
    };

    let runs = orchestrate::run(&config, &binaries, lg, &mut std::io::sink()).unwrap();
    let md = report::format_markdown(&runs);

    for name in ["hello_text", "hello_json", "path_param", "static_file"] {
        assert!(md.contains(name), "missing benchmark: {name}");
    }

    for run in runs.iter() {
        let camber = run.framework_run("Camber");
        let axum = run.framework_run("Axum");
        assert!(camber.is_some(), "{} missing Camber result", run.name);
        assert!(axum.is_some(), "{} missing Axum result", run.name);
    }
}

#[test]
fn full_run_json_output_is_valid() {
    use camber_bench::{load, orchestrate, report};

    let Some(lg) = load::detect_load_generator() else {
        return;
    };

    let binaries = test_binaries();
    let config = orchestrate::RunConfig {
        tier: None,
        bench: Some("hello_text".into()),
        concurrency: vec![8].into_boxed_slice(),
        duration: Duration::from_secs(2),
    };

    let runs = orchestrate::run(&config, &binaries, lg, &mut std::io::sink()).unwrap();
    let json_str = report::format_json(&runs).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let arr = parsed.as_array().expect("JSON should be an array");
    assert_eq!(arr.len(), 1, "should have 1 benchmark entry");
    assert_eq!(arr[0]["name"], "hello_text");
    assert!(arr[0]["frameworks"].is_array(), "missing frameworks");
}

// === Step 5 v2 tests (Go integration + LOC) ===

// === spawn_blocking_dispatch step 3 tests ===

#[test]
fn bench_progress_output_present() {
    use camber_bench::{load, orchestrate};

    let Some(lg) = load::detect_load_generator() else {
        return;
    };

    let binaries = test_binaries();
    let config = orchestrate::RunConfig {
        tier: Some(1),
        bench: None,
        concurrency: vec![8].into_boxed_slice(),
        duration: Duration::from_secs(2),
    };

    let mut progress_buf = Vec::new();
    let _runs = orchestrate::run(&config, &binaries, lg, &mut progress_buf).unwrap();
    let progress = String::from_utf8(progress_buf).unwrap();

    assert!(
        progress.contains("[bench]"),
        "progress output should contain [bench] lines, got: {progress}"
    );
}

#[test]
fn loc_comparison_counts_lines() {
    use camber_bench::loc;
    use std::path::Path;

    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let servers = base.join("src/servers");

    let camber = loc::count_source_loc(&servers.join("camber_server.rs")).unwrap();
    let axum = loc::count_source_loc(&servers.join("axum_server.rs")).unwrap();
    let go = loc::count_source_loc(&base.join("go/main.go")).unwrap();

    assert!(camber > 0, "camber LOC should be > 0");
    assert!(axum > 0, "axum LOC should be > 0");
    assert!(go > 0, "go LOC should be > 0");
}

#[test]
fn orchestrate_rejects_tier_and_bench_together() {
    use camber_bench::{error::BenchError, load::LoadGenerator, orchestrate};

    let binaries = orchestrate::Binaries {
        camber: "unused-camber".into(),
        axum: "unused-axum".into(),
        upstream: "unused-upstream".into(),
    };
    let config = orchestrate::RunConfig {
        tier: Some(1),
        bench: Some("hello_text".into()),
        concurrency: vec![8].into_boxed_slice(),
        duration: Duration::from_secs(1),
    };

    let error = orchestrate::run(&config, &binaries, LoadGenerator::Oha, &mut std::io::sink())
        .expect_err("tier+bench should be rejected");

    match error {
        BenchError::InvalidConfig(message) => {
            assert_eq!(&*message, "--tier and --bench cannot be used together");
        }
        other => panic!("unexpected error: {other}"),
    }
}
