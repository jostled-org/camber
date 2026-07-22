use std::io::Write;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use camber_bench::load::{BenchResult, LoadGenerator};

use crate::resources::{ExternalInvocation, ObservedAddressChild};
use crate::support::FixtureError;
use crate::support::address_process::{AddressChild, run_command};
use crate::support::process::{CAPTURE_LIMIT, CapturedOutput};

const FIXTURE_ENVIRONMENT: &str = "CAMBER_EXTERNAL_BENCH_FIXTURE";
const FIXTURE_VALUE: &str = "serve";
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const TOOL_CLEANUP_MARGIN: Duration = Duration::from_secs(10);

struct ExternalServer {
    child: ObservedAddressChild,
}

impl ExternalServer {
    fn spawn(invocation: &mut ExternalInvocation, test_name: &str) -> Result<Self, FixtureError> {
        let mut child =
            AddressChild::spawn_current_test(test_name, FIXTURE_ENVIRONMENT, FIXTURE_VALUE, true)?;
        let addr = child.wait_for_address(READY_TIMEOUT)?;
        let release = invocation.track_listener("load-fixture-server", addr);
        Ok(Self {
            child: ObservedAddressChild::new(child, addr, release),
        })
    }

    fn url(&self) -> String {
        format!("http://{}/", self.child.addr())
    }

    fn shutdown(self) -> Result<(), FixtureError> {
        self.child.shutdown()
    }
}

fn serve_fixture_child() -> Result<bool, FixtureError> {
    match std::env::var(FIXTURE_ENVIRONMENT).as_deref() {
        Ok(FIXTURE_VALUE) => {
            let server = crate::support::server::OwnedHttpServer::ok()?;
            println!("{}", server.addr());
            std::io::stdout().flush()?;
            std::thread::park_timeout(Duration::from_secs(30));
            drop(server);
            Ok(true)
        }
        Ok(_) | Err(_) => Ok(false),
    }
}

fn required_load_generator() -> Result<LoadGenerator, FixtureError> {
    match (
        crate::support::tool::find_executable("wrk"),
        crate::support::tool::find_executable("oha"),
    ) {
        (Some(_), _) => Ok(LoadGenerator::Wrk),
        (None, Some(_)) => Ok(LoadGenerator::Oha),
        (None, None) => Err(FixtureError::new(
            "external load_generators lane requires wrk or oha on PATH",
        )),
    }
}

fn run_load_generator(
    generator: LoadGenerator,
    url: &str,
    connections: u32,
    duration: Duration,
) -> Result<BenchResult, FixtureError> {
    let executable = match generator {
        LoadGenerator::Wrk => crate::support::tool::find_executable("wrk"),
        LoadGenerator::Oha => crate::support::tool::find_executable("oha"),
    }
    .ok_or_else(|| FixtureError::new("selected load generator is absent from PATH"))?;
    let mut command = Command::new(executable);
    configure_load_command(&mut command, generator, url, connections, duration);
    let timeout = duration
        .checked_add(TOOL_CLEANUP_MARGIN)
        .ok_or_else(|| FixtureError::new("load generator timeout overflow"))?;
    let (status, output) = run_command(&mut command, timeout)?;
    parse_load_output(generator, status, output)
}

fn configure_load_command(
    command: &mut Command,
    generator: LoadGenerator,
    url: &str,
    connections: u32,
    duration: Duration,
) {
    let seconds = duration.as_secs().max(1).to_string();
    let connections = connections.to_string();
    match generator {
        LoadGenerator::Wrk => {
            command.args([
                "-t2",
                "-d",
                &format!("{seconds}s"),
                "-c",
                &connections,
                "--latency",
                url,
            ]);
        }
        LoadGenerator::Oha => {
            command.args([
                "--json",
                "-z",
                &seconds,
                "-c",
                &connections,
                "--no-tui",
                url,
            ]);
        }
    }
}

fn parse_load_output(
    generator: LoadGenerator,
    status: ExitStatus,
    output: CapturedOutput,
) -> Result<BenchResult, FixtureError> {
    match (output.stdout_truncated, output.stderr_truncated) {
        (true, _) | (_, true) => {
            return Err(FixtureError::new(format!(
                "load generator output exceeded {CAPTURE_LIMIT} byte stream limit"
            )));
        }
        (false, false) => {}
    }
    match status.success() {
        true => parse_successful_load_output(generator, output),
        false => Err(FixtureError::new(format!(
            "load generator failed with {status}: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

fn parse_successful_load_output(
    generator: LoadGenerator,
    output: CapturedOutput,
) -> Result<BenchResult, FixtureError> {
    match generator {
        LoadGenerator::Wrk => {
            let stdout = String::from_utf8(output.stdout.into_vec())
                .map_err(|error| FixtureError::new(error.to_string()))?;
            camber_bench::load::parse_wrk_output(&stdout)
                .map_err(|error| FixtureError::new(error.to_string()))
        }
        LoadGenerator::Oha => camber_bench::load::parse_oha_json(&output.stdout)
            .map_err(|error| FixtureError::new(error.to_string())),
    }
}

fn run_three_phase(
    generator: LoadGenerator,
    url: &str,
    concurrency: &[u32],
    duration: Duration,
) -> Result<Vec<(u32, BenchResult)>, FixtureError> {
    concurrency
        .iter()
        .map(|connections| {
            run_load_generator(generator, url, *connections, Duration::from_secs(1))?;
            run_load_generator(generator, url, *connections, duration)?;
            let measured = run_load_generator(generator, url, *connections, duration)?;
            Ok((*connections, measured))
        })
        .collect()
}

fn external_framework_run(
    framework: &str,
    generator: LoadGenerator,
    url: &str,
) -> Result<camber_bench::report::FrameworkRun, FixtureError> {
    let result = run_three_phase(generator, url, &[8], Duration::from_secs(2))?
        .into_iter()
        .next()
        .ok_or_else(|| FixtureError::new("three-phase benchmark returned no measurement"))?;
    Ok(camber_bench::report::FrameworkRun {
        framework: framework.into(),
        results: Box::new([camber_bench::report::ConcurrencyResult {
            concurrency: result.0,
            result: result.1,
        }]),
    })
}

fn external_benchmark(
    name: &str,
    generator: LoadGenerator,
    url: &str,
) -> Result<camber_bench::report::BenchmarkRun, FixtureError> {
    let camber = external_framework_run("Camber", generator, url)?;
    let axum = external_framework_run("Axum", generator, url)?;
    Ok(camber_bench::report::BenchmarkRun {
        name: name.into(),
        frameworks: Box::new([camber, axum]),
    })
}

fn external_run(
    generator: LoadGenerator,
    url: &str,
    benchmarks: &[&str],
) -> Result<Box<[camber_bench::report::BenchmarkRun]>, FixtureError> {
    benchmarks
        .iter()
        .map(|name| external_benchmark(name, generator, url))
        .collect::<Result<Vec<_>, FixtureError>>()
        .map(Vec::into_boxed_slice)
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn load_generator_measures_requests_per_second() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("load-generator-measures-requests-per-second")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::load_generator_measures_requests_per_second",
    )?;
    let result = run_load_generator(
        required_load_generator()?,
        &server.url(),
        4,
        Duration::from_secs(2),
    )?;
    assert!(result.req_per_sec > 0.0, "expected req/s > 0");
    assert!(result.latency_avg_ms > 0.0, "expected avg latency > 0");
    assert!(result.latency_p99_ms > 0.0, "expected p99 latency > 0");
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn oha_produces_results_if_available() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("oha-produces-results-if-available")?;
    crate::support::tool::find_executable("oha")
        .ok_or_else(|| FixtureError::new("external load_generators lane requires oha on PATH"))?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::oha_produces_results_if_available",
    )?;
    let result = run_load_generator(
        LoadGenerator::Oha,
        &server.url(),
        10,
        Duration::from_secs(1),
    )?;
    assert!(result.req_per_sec > 0.0, "expected req/s > 0");
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn wrk_or_oha_detected() -> Result<(), FixtureError> {
    let invocation = ExternalInvocation::start("wrk-or-oha-detected")?;
    assert!(matches!(
        required_load_generator()?,
        LoadGenerator::Wrk | LoadGenerator::Oha
    ));
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn three_phase_runs_primer_warmup_measured() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("three-phase-runs-primer-warmup-measured")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::three_phase_runs_primer_warmup_measured",
    )?;
    let results = run_three_phase(
        required_load_generator()?,
        &server.url(),
        &[8],
        Duration::from_secs(2),
    )?;
    assert_eq!(results.len(), 1, "expected one measured result");
    assert_eq!(results[0].0, 8, "concurrency should be 8");
    assert!(results[0].1.req_per_sec > 0.0);
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn multiple_concurrency_levels_produce_multiple_results() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation =
        ExternalInvocation::start("multiple-concurrency-levels-produce-multiple-results")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::multiple_concurrency_levels_produce_multiple_results",
    )?;
    let results = run_three_phase(
        required_load_generator()?,
        &server.url(),
        &[8, 16],
        Duration::from_secs(2),
    )?;
    assert_eq!(results.len(), 2, "expected two measured results");
    assert_eq!(results[0].0, 8);
    assert_eq!(results[1].0, 16);
    assert!(results.iter().all(|(_, result)| result.req_per_sec > 0.0));
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn full_run_tier1_produces_report() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("full-run-tier1-produces-report")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::full_run_tier1_produces_report",
    )?;
    let benchmarks = ["hello_text", "hello_json", "path_param", "static_file"];
    let runs = external_run(required_load_generator()?, &server.url(), &benchmarks)?;
    let markdown = camber_bench::report::format_markdown(&runs);
    for name in benchmarks {
        assert!(markdown.contains(name), "missing benchmark: {name}");
    }
    for run in &runs {
        assert!(run.framework_run("Camber").is_some());
        assert!(run.framework_run("Axum").is_some());
    }
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn full_run_json_output_is_valid() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("full-run-json-output-is-valid")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::full_run_json_output_is_valid",
    )?;
    let runs = external_run(required_load_generator()?, &server.url(), &["hello_text"])?;
    let json = camber_bench::report::format_json(&runs)
        .map_err(|error| FixtureError::new(error.to_string()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&json).map_err(|error| FixtureError::new(error.to_string()))?;
    let array = parsed
        .as_array()
        .ok_or_else(|| FixtureError::new("JSON report was not an array"))?;
    assert_eq!(array.len(), 1);
    assert_eq!(array[0]["name"], "hello_text");
    assert!(array[0]["frameworks"].is_array());
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn bench_progress_output_present() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("bench-progress-output-present")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::bench_progress_output_present",
    )?;
    let runs = external_run(
        required_load_generator()?,
        &server.url(),
        &["hello_text", "hello_json", "path_param", "static_file"],
    )?;
    let mut progress = Vec::new();
    writeln!(progress, "[bench] completed {} benchmark(s)", runs.len())?;
    let progress =
        String::from_utf8(progress).map_err(|error| FixtureError::new(error.to_string()))?;
    assert!(progress.contains("[bench]"));
    server.shutdown()?;
    invocation.finish()
}
