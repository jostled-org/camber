use std::io::Write;
use std::process::ExitStatus;
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

fn run_load_generator(
    generator: LoadGenerator,
    url: &str,
    connections: u32,
    duration: Duration,
) -> Result<BenchResult, FixtureError> {
    let mut command = camber_bench::load::load_command(generator, url, connections, duration);
    let timeout = duration
        .checked_add(TOOL_CLEANUP_MARGIN)
        .ok_or_else(|| FixtureError::new("load generator timeout overflow"))?;
    let (status, output) = run_command(&mut command, timeout)?;
    parse_load_output(generator, status, output)
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

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn wrk_adapter_accepts_live_output() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("wrk-adapter-live-output")?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::wrk_adapter_accepts_live_output",
    )?;
    let result = run_load_generator(LoadGenerator::Wrk, &server.url(), 4, Duration::from_secs(2))?;
    assert!(result.req_per_sec > 0.0, "expected req/s > 0");
    assert!(result.latency_avg_ms > 0.0, "expected avg latency > 0");
    assert!(result.latency_p99_ms > 0.0, "expected p99 latency > 0");
    server.shutdown()?;
    invocation.finish()
}

#[test]
#[ignore = "external lane load_generators; owner: Camber benchmark maintainers; run: gh workflow run external-evidence.yml -f lane=load_generators"]
fn oha_adapter_accepts_live_output() -> Result<(), FixtureError> {
    match serve_fixture_child()? {
        true => return Ok(()),
        false => {}
    }
    let mut invocation = ExternalInvocation::start("oha-adapter-live-output")?;
    crate::support::tool::find_executable("oha")
        .ok_or_else(|| FixtureError::new("external load_generators lane requires oha on PATH"))?;
    let server = ExternalServer::spawn(
        &mut invocation,
        "external_load_generators::oha_adapter_accepts_live_output",
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
