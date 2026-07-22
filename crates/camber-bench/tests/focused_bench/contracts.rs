use std::time::Duration;

use crate::support::FixtureError;

fn benchmark_result() -> camber_bench::load::BenchResult {
    camber_bench::load::BenchResult {
        req_per_sec: 100_000.0,
        latency_avg_ms: 0.5,
        latency_p50_ms: 0.3,
        latency_p90_ms: 0.9,
        latency_p99_ms: 1.2,
        error_count: 0,
    }
}

#[test]
fn external_resource_and_build_names_are_unique() -> Result<(), FixtureError> {
    let first_resource = crate::support::unique::external_resource("bench subject");
    let second_resource = crate::support::unique::external_resource("bench subject");
    let first_build = crate::support::unique::build("go fixture");
    let second_build = crate::support::unique::build("go fixture");
    assert_ne!(first_resource, second_resource);
    assert_ne!(first_build, second_build);
    assert!(first_resource.starts_with("bench-subject-"));
    assert!(first_build.starts_with("go-fixture-"));
    Ok(())
}

#[test]
fn report_formats_markdown_table() -> Result<(), FixtureError> {
    use camber_bench::report;
    let runs = [report::BenchmarkRun {
        name: "hello_text".into(),
        frameworks: Box::new([report::FrameworkRun {
            framework: "Camber".into(),
            results: Box::new([report::ConcurrencyResult {
                concurrency: 16,
                result: benchmark_result(),
            }]),
        }]),
    }];
    let markdown = report::format_markdown(&runs);
    assert!(
        markdown.contains("### hello_text"),
        "missing benchmark header"
    );
    assert!(markdown.contains("| Concurrency |"), "missing table header");
    assert!(markdown.contains("100000"), "missing req/s value");
    Ok(())
}

#[test]
fn loc_comparison_included() -> Result<(), FixtureError> {
    use camber_bench::report;
    let runs = [report::BenchmarkRun {
        name: "hello_text".into(),
        frameworks: Box::new([
            report::FrameworkRun {
                framework: "Camber".into(),
                results: Box::new([report::ConcurrencyResult {
                    concurrency: 16,
                    result: benchmark_result(),
                }]),
            },
            report::FrameworkRun {
                framework: "Axum".into(),
                results: Box::new([report::ConcurrencyResult {
                    concurrency: 16,
                    result: benchmark_result(),
                }]),
            },
        ]),
    }];
    let loc = report::LocComparison {
        camber_loc: 120,
        axum_loc: 200,
        go_loc: 180,
    };
    let markdown = report::format_markdown_with_loc(&runs, &loc);
    assert!(
        markdown.contains("Lines of Code"),
        "missing LOC table header"
    );
    assert!(markdown.contains("120"), "missing Camber LOC value");
    assert!(markdown.contains("200"), "missing Axum LOC value");
    Ok(())
}

#[test]
fn wrk_output_parsed_correctly() -> Result<(), FixtureError> {
    let output = "\
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
    let result = camber_bench::load::parse_wrk_output(output)?;
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
    Ok(())
}

#[test]
fn oha_json_parsed_correctly() -> Result<(), FixtureError> {
    let json = br#"{
        "summary": {"successRate": 1.0, "total": 50000.0, "slowest": 0.005, "fastest": 0.0001, "average": 0.00053, "requestsPerSec": 18500.0},
        "latencyPercentiles": [
            {"percentile": 50.0, "latency": 0.00052},
            {"percentile": 75.0, "latency": 0.00058},
            {"percentile": 90.0, "latency": 0.00065},
            {"percentile": 99.0, "latency": 0.00082}
        ],
        "statusCodeDistribution": {"200": 50000}
    }"#;
    let result = camber_bench::load::parse_oha_json(json)?;
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
    Ok(())
}

#[test]
fn loc_comparison_counts_lines() -> Result<(), FixtureError> {
    use camber_bench::loc;
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let servers = base.join("src/servers");
    let camber = loc::count_source_loc(&servers.join("camber_server.rs"))?;
    let axum = loc::count_source_loc(&servers.join("axum_server.rs"))?;
    let go = loc::count_source_loc(&base.join("go/main.go"))?;
    assert!(camber > 0, "camber LOC should be > 0");
    assert!(axum > 0, "axum LOC should be > 0");
    assert!(go > 0, "go LOC should be > 0");
    Ok(())
}

#[test]
fn orchestrate_rejects_tier_and_bench_together() -> Result<(), FixtureError> {
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
    let error = match orchestrate::run(&config, &binaries, LoadGenerator::Oha, &mut std::io::sink())
    {
        Ok(_) => return Err(FixtureError::new("tier+bench was accepted")),
        Err(error) => error,
    };
    match error {
        BenchError::InvalidConfig(message) => {
            assert_eq!(&*message, "--tier and --bench cannot be used together")
        }
        other => return Err(FixtureError::new(format!("unexpected error: {other}"))),
    }
    Ok(())
}
