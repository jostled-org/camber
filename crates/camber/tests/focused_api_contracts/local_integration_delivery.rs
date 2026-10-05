//! Selected local-service evidence through the shipped lane runner.
//!
//! Each row runs `.github/scripts/check-local-integrations.sh` as a real Bash
//! process from an owned checkout of the tracked `.github` tree. Only the
//! container engine and Cargo are controlled executables, placed ahead of a
//! system-only `PATH`; the selection, identity, and evidence logic under test
//! is the tracked runner itself. No broker behavior is claimed here.
//!
//! The runner contract these rows hold it to:
//!
//! - It takes one lane, `nats`, `sqs`, or `dns`, plus optional exact tests.
//!   A missing or unknown lane fails before any engine or test effect.
//! - The engine is `docker` on `PATH`. An absent engine exits 75.
//! - Every started image is pinned by digest in `.github/workflow-tools.toml`,
//!   and the engine must report that identity before any test runs.
//! - Each selected test is listed exactly (`--exact --ignored --list`) and
//!   must list one test, then runs alone with its own
//!   `CAMBER_EXTERNAL_CLEANUP_WITNESS` path. A passing test counts only with a
//!   fresh witness naming this run's `CAMBER_EXTERNAL_RUN_ID`.
//! - It writes `<lane>.json` for every lane into
//!   `CAMBER_EXTERNAL_EVIDENCE_DIR`, carrying `lane`, `run_id`, and a
//!   `status` of `passed`, `failed`, `infrastructure_unavailable`, or
//!   `not_selected`.
//! - The NATS lane starts its server with JetStream and container-local
//!   storage, and admits both the Core and the stored-message matrix.
//! - It removes every network and container it started, and nothing else.
//! - It runs the `cargo` that `PATH` selects, directly, under the inherited
//!   `CARGO_TARGET_DIR`. Ignored repository tooling never wraps it, and a
//!   failed Cargo call keeps its status in the runner's report.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::delivery_fixture::{
    FixtureRepo, HookRun, INFRASTRUCTURE_STATUS, SYSTEM_PATH, Tool, assert_absent_from_system_path,
    logged_text, quoted, repository_root, repository_text, run_bounded, write_file,
};
use crate::integration_delivery::{
    CargoCall, Checks, RunnerReport, StubTools, cargo_calls, copy_tracked_inputs, runner_command,
    tab_fields, tracked_checkout,
};
use crate::local_integrations::engine_stub::{
    EngineStub, ImageIdentity, SUBSTITUTED_DIGEST, StartedContainer,
};

const RUNNER: &str = ".github/scripts/check-local-integrations.sh";
const TOOL_INVENTORY: &str = ".github/workflow-tools.toml";
const RED_DIAGNOSTIC: &str = "M9 selected local evidence must fail closed";
const LANES: [&str; 3] = ["nats", "sqs", "dns"];
const RUN_ID: &str = "local-fixture-run";
const STALE_RUN_ID: &str = "earlier-fixture-run";
const TEST_ROOT: &str = "external_feature_services";
const FOREIGN_CONTAINER: &str = "foreign-service";
const FOREIGN_NETWORK: &str = "foreign-network";
const CARGO_DIAGNOSTIC: &str = "local lane Cargo must run in the declared execution environment";
const JETSTREAM_DIAGNOSTIC: &str = "NATS_JS_LOCAL_LANE_CONTRACT";
/// The Core matrix the NATS lane already owns.
const NATS_CORE_SELECTOR: &str = "external_nats::nats_local_contract_matrix";
/// The stored-message matrix the NATS lane must also own.
const NATS_JETSTREAM_SELECTOR: &str = "external_nats::nats_local_jetstream_publish_matrix";
/// Where ignored repository tooling once offered the runner a build lease.
const PRIVATE_LEASE: &str = "docs/scripts/with_build_lease.sh";
/// What the private lease records when the runner calls it, outside the tree.
const LEASE_LOG: &str = "private-lease.log";
/// The alternate target tree the private lease would create.
const LEASE_TARGET: &str = "private-lease-target";

/// How many tests an exact listing reports.
#[derive(Clone, Copy, Debug)]
enum Listing {
    One,
    Zero,
    /// The listing call itself fails.
    Fails,
}

/// What a selected test run does.
#[derive(Clone, Copy, Debug)]
enum Outcome {
    Passes,
    Fails,
    OmitsWitness,
    StaleWitness,
}

#[derive(Clone, Copy, Debug)]
enum EngineState {
    Present(ImageIdentity),
    Absent,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    engine: EngineState,
    listing: Listing,
    outcome: Outcome,
    failed_disk_sample: Option<usize>,
}

impl Scenario {
    const PASSING: Self = Self {
        engine: EngineState::Present(ImageIdentity::Pinned),
        listing: Listing::One,
        outcome: Outcome::Passes,
        failed_disk_sample: None,
    };
}

/// One exact test execution the stub Cargo recorded.
#[derive(Debug)]
struct TestRun {
    selected: Box<str>,
    witness: Box<str>,
    arguments: Box<str>,
}

/// Everything one runner invocation left behind.
struct LaneRun {
    hook: HookRun,
    evidence: BTreeMap<&'static str, Result<Evidence, Box<str>>>,
    listed: Box<[Box<str>]>,
    test_runs: Box<[TestRun]>,
    cargo_calls: Box<[CargoCall]>,
    /// The stub Cargo the row placed first on `PATH`.
    cargo_executable: Box<str>,
    engine_calls: Box<str>,
    started: Box<[StartedContainer]>,
    residue: Box<[Box<str>]>,
    foreign_survived: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Evidence {
    lane: Box<str>,
    run_id: Box<str>,
    status: Box<str>,
    disk: Option<serde_json::Value>,
}

// --- Controlled Cargo ----------------------------------------------------

/// The lane stub Cargo's body: it answers exact listings and runs selected
/// tests as the scenario says. It never builds anything.
const CARGO_BODY: &str = r#"if [ "${subcommand}" != test ]; then
    printf 'other\t%s\n' "$*" >>"${state}/cargo.log"
    exit 0
fi
listing=0
selected=''
after=0
skip=0
build_only=0
for argument in "$@"; do
    if [ "${after}" = 0 ]; then
        case "${argument}" in
            --no-run) build_only=1 ;;
            --) after=1 ;;
        esac
        continue
    fi
    if [ "${skip}" = 1 ]; then
        skip=0
        continue
    fi
    case "${argument}" in
        --list) listing=1 ;;
        --test-threads|--format|--skip|--color|-Z|--logfile|--shuffle-seed) skip=1 ;;
        -*) ;;
        *) [ -n "${selected}" ] || selected="${argument}" ;;
    esac
done
if [ "${build_only}" = 1 ]; then
    printf 'build\t%s\n' "$*" >>"${state}/cargo.log"
    exit 0
fi
if [ "${listing}" = 1 ]; then
    printf 'list\t%s\n' "${selected}" >>"${state}/cargo.log"
    case "$(cat "${state}/listing")" in
        one) printf '%s: test\n\n1 test, 0 benchmarks\n' "${selected}" ;;
        fails) printf 'error: the test root could not be listed\n' >&2; exit @FAILURE@ ;;
        *) printf '\n0 tests, 0 benchmarks\n' ;;
    esac
    exit 0
fi
witness="${CAMBER_EXTERNAL_CLEANUP_WITNESS:-}"
printf 'run\t%s\t%s\t%s\n' "${selected}" "${witness}" "$*" >>"${state}/cargo.log"
case "$(cat "${state}/listing")" in
    one) ;;
    *)
        printf '\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n'
        exit 0
        ;;
esac
outcome=$(cat "${state}/outcome")
if [ "${outcome}" = fails ]; then
    printf '\nrunning 1 test\ntest %s ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n' "${selected}"
    exit @FAILURE@
fi
printf '\nrunning 1 test\ntest %s ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' "${selected}"
case "${outcome}" in
    passes) run_id="${CAMBER_EXTERNAL_RUN_ID:-}" ;;
    stale-witness) run_id='@STALE@' ;;
    *) exit 0 ;;
esac
[ -n "${witness}" ] || exit 0
printf '{"run_id":"%s","resources":["%s"],"cleanup_status":"completed"}\n' "${run_id}" "${selected}" >"${witness}"
"#;

/// The controlled toolchain for one row's `scenario`.
fn lane_tools(scenario: Scenario) -> StubTools {
    let tools = StubTools::new();
    tools.install_cargo(
        Tool::Cargo.pinned().0,
        &CARGO_BODY.replace("@STALE@", STALE_RUN_ID),
    );
    let listing: &[u8] = match scenario.listing {
        Listing::One => b"one\n",
        Listing::Zero => b"zero\n",
        Listing::Fails => b"fails\n",
    };
    let outcome: &[u8] = match scenario.outcome {
        Outcome::Passes => b"passes\n",
        Outcome::Fails => b"fails\n",
        Outcome::OmitsWitness => b"omits-witness\n",
        Outcome::StaleWitness => b"stale-witness\n",
    };
    write_file(&tools.state().join("listing"), listing);
    write_file(&tools.state().join("outcome"), outcome);
    tools
}

// --- Owned checkout ------------------------------------------------------

/// A private build lease: it records each call, then runs Cargo under an
/// alternate target tree of its own.
const LEASE_SCRIPT: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" >>@LOG@
while [ "$#" -gt 0 ] && [ "$1" != cargo ]; do shift; done
[ "$#" -eq 0 ] || shift
mkdir -p @TARGET@
CARGO_TARGET_DIR=@TARGET@ exec cargo "$@"
"#;

/// A lane checkout that also holds ignored private tooling: an executable
/// build lease where repository tooling would keep one. The runner must not
/// branch on it.
fn private_tooling_checkout() -> FixtureRepo {
    let repo = FixtureRepo::new();
    copy_tracked_inputs(&repo);
    repo.write(".gitignore", b"/docs/\n");
    repo.commit("local lane fixture with ignored tooling");
    let lease = LEASE_SCRIPT
        .replace("@LOG@", &quoted(&repo.outside(LEASE_LOG)))
        .replace("@TARGET@", &quoted(&repo.outside(LEASE_TARGET)));
    repo.write_executable(PRIVATE_LEASE, lease);
    repo
}

// --- One runner invocation -----------------------------------------------

fn run_lane(repo: &FixtureRepo, row: &str, arguments: &[&str], scenario: Scenario) -> LaneRun {
    let tools = lane_tools(scenario);
    let engine = EngineStub::new();
    engine.seed_image_file("/test/certs/pebble.minica.pem", b"controlled trust file\n");
    if let Some(sample) = scenario.failed_disk_sample {
        engine.fail_disk_sample(sample);
    }
    engine.seed_foreign(FOREIGN_CONTAINER, FOREIGN_NETWORK);
    let search_path = match scenario.engine {
        EngineState::Present(identity) => {
            engine.set_identity(identity);
            format!(
                "{}:{}:{SYSTEM_PATH}",
                tools.bin().display(),
                engine.bin().display()
            )
        }
        EngineState::Absent => format!("{}:{SYSTEM_PATH}", tools.bin().display()),
    };
    let evidence_dir = repo.outside(&format!("evidence-{row}"));
    fs::create_dir_all(&evidence_dir).expect("evidence directory was not created");

    let mut command = runner_command(repo, RUNNER, &search_path, &declared_target(repo));
    command
        .args(arguments)
        .env("CAMBER_EXTERNAL_RUN_ID", RUN_ID)
        .env("CAMBER_EXTERNAL_EVIDENCE_DIR", &evidence_dir);
    let run = run_bounded(command);
    let lane_run = collect_lane_run(run, &evidence_dir, &tools, &engine);
    engine.close();
    tools.close();
    lane_run
}

/// The target directory every row's runner inherits.
fn declared_target(repo: &FixtureRepo) -> PathBuf {
    repo.outside("target")
}

fn collect_lane_run(
    hook: HookRun,
    evidence_dir: &Path,
    tools: &StubTools,
    engine: &EngineStub,
) -> LaneRun {
    let evidence = LANES
        .iter()
        .map(|lane| (*lane, read_evidence(evidence_dir, lane)))
        .collect();
    let cargo_log = tools.cargo_log();
    let listed = cargo_log
        .lines()
        .filter_map(|line| line.strip_prefix("list\t"))
        .map(Box::from)
        .collect();
    let test_runs = cargo_log
        .lines()
        .filter_map(|line| line.strip_prefix("run\t"))
        .map(|line| {
            let [selected, witness, arguments] = tab_fields(line);
            TestRun {
                selected: selected.into(),
                witness: witness.into(),
                arguments: arguments.into(),
            }
        })
        .collect();
    let containers = engine.containers();
    let networks = engine.networks();
    let residue = containers
        .iter()
        .filter(|name| ***name != *FOREIGN_CONTAINER)
        .chain(networks.iter().filter(|name| ***name != *FOREIGN_NETWORK))
        .cloned()
        .collect();
    let foreign_survived = containers.iter().any(|name| **name == *FOREIGN_CONTAINER)
        && networks.iter().any(|name| **name == *FOREIGN_NETWORK);
    let started = engine
        .started()
        .into_iter()
        .filter(|started| *started.name != *FOREIGN_CONTAINER)
        .collect();
    LaneRun {
        hook,
        evidence,
        listed,
        test_runs,
        cargo_calls: cargo_calls(&cargo_log),
        cargo_executable: tools.cargo().display().to_string().into(),
        engine_calls: engine.calls(),
        started,
        residue,
        foreign_survived,
    }
}

fn read_evidence(directory: &Path, lane: &str) -> Result<Evidence, Box<str>> {
    let path = directory.join(format!("{lane}.json"));
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("{} is unreadable: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not JSON: {error}\n{text}", path.display()))?;
    let field = |name: &str| -> Result<Box<str>, Box<str>> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(Box::from)
            .ok_or_else(|| format!("{} has no string {name}: {text}", path.display()).into())
    };
    Ok(Evidence {
        lane: field("lane")?,
        run_id: field("run_id")?,
        status: field("status")?,
        disk: value.get("disk").cloned(),
    })
}

/// The tracked tool inventory that pins every local service image.
fn tool_inventory() -> Box<str> {
    repository_text(TOOL_INVENTORY)
}

// --- Checks --------------------------------------------------------------

impl RunnerReport for LaneRun {
    const REFUSED: &'static str = "selection";

    fn status(&self) -> i32 {
        self.hook.status
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.hook.output)
    }

    fn cargo_calls(&self) -> &[CargoCall] {
        &self.cargo_calls
    }

    fn cargo_executable(&self) -> &str {
        &self.cargo_executable
    }
}

impl Checks<'_, LaneRun> {
    /// `selected` carries `status`; every other lane is `not_selected`.
    fn evidence(&mut self, selected: &str, status: &str) {
        for lane in LANES {
            let expected = match lane == selected {
                true => status,
                false => "not_selected",
            };
            let lane_run = self.run;
            let observed = match &lane_run.evidence[lane] {
                Ok(evidence) => evidence,
                Err(error) => {
                    self.fail(format!("{lane} evidence is missing: {error}"));
                    continue;
                }
            };
            let wanted = Evidence {
                lane: lane.into(),
                run_id: RUN_ID.into(),
                status: expected.into(),
                disk: observed.disk.clone(),
            };
            if *observed != wanted {
                self.fail(format!(
                    "{lane} evidence is {observed:?}, expected {wanted:?}"
                ));
            }
        }
    }

    fn no_test_ran(&mut self) {
        if !self.run.test_runs.is_empty() {
            self.fail(format!(
                "tests ran after the selection was refused: {:?}",
                self.run.test_runs
            ));
        }
    }

    fn nothing_started(&mut self) {
        let created_network = self
            .run
            .engine_calls
            .lines()
            .any(|call| call.starts_with("network create") && !call.contains(FOREIGN_NETWORK));
        if !self.run.started.is_empty() || created_network {
            self.fail(format!(
                "engine effects preceded the refusal:\n{}",
                self.run.engine_calls
            ));
        }
    }

    /// The selected test was both listed and executed through Cargo.
    fn listed_and_executed(&mut self) {
        let lane_run = self.run;
        let calls = |flag: &str| {
            lane_run
                .cargo_calls
                .iter()
                .filter(|call| {
                    call.arguments
                        .split_whitespace()
                        .any(|argument| argument == flag)
                })
                .count()
        };
        if calls("--list") == 0 || calls("--test-threads=1") == 0 {
            self.fail(format!(
                "Cargo did not both list and execute the selection: {:?}",
                lane_run.cargo_calls
            ));
        }
    }

    fn reaped_only_owned(&mut self) {
        if !self.run.residue.is_empty() {
            self.fail(format!(
                "owned engine resources survived: {:?}",
                self.run.residue
            ));
        }
        if !self.run.foreign_survived {
            self.fail("the runner removed a container or network it does not own".to_owned());
        }
    }
}

/// Every started image is digest-pinned and recorded in the tool inventory,
/// on a run-scoped network, published only on an ephemeral loopback port.
fn check_started_services(checks: &mut Checks<'_, LaneRun>, inventory: &str) {
    if checks.run.started.is_empty() {
        checks.fail("the selected lane started no local service".to_owned());
    }
    let lane_run = checks.run;
    for started in &lane_run.started {
        let digest = started
            .image
            .split_once("@sha256:")
            .map(|(_, digest)| digest)
            .filter(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        match digest {
            Some(digest) if inventory.contains(digest) => {}
            Some(_) => checks.fail(format!(
                "{} runs {}, whose digest {TOOL_INVENTORY} does not pin",
                started.name, started.image
            )),
            None => checks.fail(format!(
                "{} runs {}, which is not pinned by digest",
                started.name, started.image
            )),
        }
        if started.network.is_empty() || *started.network == *FOREIGN_NETWORK {
            checks.fail(format!(
                "{} is not on a run-scoped network: {:?}",
                started.name, started.network
            ));
        }
        let loopback = !started.publications.is_empty()
            && started
                .publications
                .iter()
                .all(|publication| publication.starts_with("127.0.0.1::"));
        if !loopback {
            checks.fail(format!(
                "{} publishes {:?}, not only ephemeral loopback ports",
                started.name, started.publications
            ));
        }
    }
}

/// Each run is one exactly listed test, alone, from the declared root, with a
/// witness path of its own.
fn check_exact_runs(checks: &mut Checks<'_, LaneRun>) {
    let lane_run = checks.run;
    let runs = &lane_run.test_runs;
    if runs.is_empty() {
        checks.fail("the selected lane ran no test".to_owned());
    }
    let root_flag = format!(" {TEST_ROOT} ");
    let mut witnesses: Vec<&str> = Vec::with_capacity(runs.len());
    let mut problems = Vec::new();
    for run in runs.iter() {
        let arguments = format!(" {} ", run.arguments);
        let exact = [" --exact ", " --ignored ", root_flag.as_str()]
            .iter()
            .all(|flag| arguments.contains(flag));
        if !exact {
            problems.push(format!(
                "{} did not run as one exact ignored test from {TEST_ROOT}: {}",
                run.selected, run.arguments
            ));
        }
        if run.selected.is_empty() || !lane_run.listed.contains(&run.selected) {
            problems.push(format!(
                "{:?} ran without its exact listing: listed {:?}",
                run.selected, checks.run.listed
            ));
        }
        if !Path::new(&*run.witness).is_absolute() || witnesses.contains(&&*run.witness) {
            problems.push(format!(
                "{} was not given its own absolute witness path: {:?}",
                run.selected, run.witness
            ));
        }
        witnesses.push(&run.witness);
    }
    problems
        .into_iter()
        .for_each(|problem| checks.fail(problem));
}

// --- Rows ----------------------------------------------------------------

#[test]
fn local_integration_selection_requires_exact_tests_and_cleanup() {
    assert!(
        repository_root().join(RUNNER).is_file(),
        "{RED_DIAGNOSTIC}: the shared local lane runner {RUNNER} is absent"
    );
    for engine in ["docker", "podman"] {
        assert_absent_from_system_path(engine);
    }
    let inventory = tool_inventory();
    assert!(
        !inventory.contains(SUBSTITUTED_DIGEST.trim_start_matches("sha256:")),
        "the tool inventory pins the stub's substituted identity"
    );
    let repo = tracked_checkout("local lane fixture");
    let mut failures = Vec::new();

    passing_selection(&repo, &inventory, &mut failures);
    failed_disk_evidence(&repo, &mut failures);
    unavailable_engine(&repo, &mut failures);
    wrong_image(&repo, &mut failures);
    empty_test_selections(&repo, &mut failures);
    failed_test_evidence(&repo, &mut failures);
    invalid_lane_selections(&repo, &mut failures);

    repo.close();
    assert!(
        failures.is_empty(),
        "{RED_DIAGNOSTIC}:\n{}",
        failures.join("\n\n")
    );
}

fn passing_selection(repo: &FixtureRepo, inventory: &str, failures: &mut Vec<String>) {
    // The pinned, listed, witnessed selection passes and says so.
    let lane_run = run_lane(repo, "baseline", &["nats"], Scenario::PASSING);
    let mut checks = Checks {
        row: "passing nats selection",
        run: &lane_run,
        failures,
    };
    checks.exit_status(0);
    checks.evidence("nats", "passed");
    check_disk_evidence(&mut checks);
    check_started_services(&mut checks, inventory);
    check_exact_runs(&mut checks);
    checks.reaped_only_owned();
}

#[test]
fn local_dns_lane_requires_authorization_reuse() {
    let repo = tracked_checkout("DNS authorization reuse fixture");
    let run = run_lane(&repo, "dns-reuse", &["dns"], Scenario::PASSING);
    assert_eq!(run.hook.status, 0, "{}", run.hook.output);
    assert!(
        run.engine_calls.contains("PEBBLE_AUTHZREUSE=100"),
        "the renewal lane must exercise authorization reuse deterministically: {}",
        run.engine_calls,
    );
    assert!(run.residue.is_empty(), "owned services survived");
    assert!(run.foreign_survived, "foreign services were removed");
    repo.close();
}

#[test]
fn local_nats_lane_enables_jetstream_and_selects_storage_proof() {
    let inventory = tool_inventory();
    let repo = tracked_checkout("NATS JetStream lane fixture");
    let mut failures = Vec::new();

    jetstream_default_selection(&repo, &inventory, &mut failures);
    jetstream_exact_selection(&repo, &mut failures);

    repo.close();
    assert!(
        failures.is_empty(),
        "{JETSTREAM_DIAGNOSTIC}:\n{}",
        failures.join("\n\n")
    );
}

fn jetstream_default_selection(repo: &FixtureRepo, inventory: &str, failures: &mut Vec<String>) {
    // The whole NATS lane starts JetStream with container-local storage and
    // runs both the Core and the stored-message matrix, each witnessed.
    let lane_run = run_lane(repo, "jetstream-default", &["nats"], Scenario::PASSING);
    let mut checks = Checks {
        row: "default NATS selection with JetStream",
        run: &lane_run,
        failures,
    };
    checks.exit_status(0);
    checks.evidence("nats", "passed");
    check_jetstream_server(&mut checks);
    check_started_services(&mut checks, inventory);
    check_exact_runs(&mut checks);
    check_selected_once(&mut checks, &[NATS_CORE_SELECTOR, NATS_JETSTREAM_SELECTOR]);
    checks.reaped_only_owned();
}

fn jetstream_exact_selection(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // The stored-message matrix is an admitted exact selection of its own.
    let lane_run = run_lane(
        repo,
        "jetstream-exact",
        &["nats", NATS_JETSTREAM_SELECTOR],
        Scenario::PASSING,
    );
    let mut checks = Checks {
        row: "exact JetStream selection",
        run: &lane_run,
        failures,
    };
    checks.exit_status(0);
    checks.evidence("nats", "passed");
    check_jetstream_server(&mut checks);
    check_exact_runs(&mut checks);
    check_selected_once(&mut checks, &[NATS_JETSTREAM_SELECTOR]);
    checks.reaped_only_owned();
}

/// The name the runner gives this run's NATS container:
/// `camber-local-<run>-<lane>-<service>`.
fn nats_container() -> String {
    format!("camber-local-{RUN_ID}-nats-nats")
}

/// The engine call that created this run's NATS container.
fn nats_start_call(engine_calls: &str) -> Option<&str> {
    let name = format!(" --name {} ", nats_container());
    engine_calls.lines().find(|call| {
        let call = format!(" {call} ");
        (call.starts_with(" run ") || call.starts_with(" create ")) && call.contains(&name)
    })
}

/// The NATS server enables JetStream and keeps its storage in the container.
fn check_jetstream_server(checks: &mut Checks<'_, LaneRun>) {
    let lane_run = checks.run;
    let Some(call) = nats_start_call(&lane_run.engine_calls) else {
        return checks.fail(format!(
            "no engine call started {}:\n{}",
            nats_container(),
            lane_run.engine_calls
        ));
    };
    let jetstream = call
        .split_whitespace()
        .any(|argument| matches!(argument, "-js" | "--jetstream"));
    if !jetstream {
        checks.fail(format!("the NATS server does not enable JetStream: {call}"));
    }
    let mount = call.split_whitespace().find(|argument| {
        matches!(*argument, "-v" | "--volume" | "--mount")
            || argument.starts_with("--volume=")
            || argument.starts_with("--mount=")
    });
    if let Some(mount) = mount {
        checks.fail(format!(
            "the NATS server stores outside its container through {mount}: {call}"
        ));
    }
}

/// Each of `expected` is listed and executed exactly once, and nothing else is.
fn check_selected_once(checks: &mut Checks<'_, LaneRun>, expected: &[&str]) {
    let lane_run = checks.run;
    let mut problems = Vec::new();
    for selector in expected {
        let listed = lane_run
            .listed
            .iter()
            .filter(|listed| ***listed == **selector)
            .count();
        let executed = lane_run
            .test_runs
            .iter()
            .filter(|run| *run.selected == **selector)
            .count();
        if listed != 1 || executed != 1 {
            problems.push(format!(
                "{selector} was listed {listed} and executed {executed} times, not once each"
            ));
        }
    }
    let unexpected: Vec<&str> = lane_run
        .test_runs
        .iter()
        .map(|run| &*run.selected)
        .filter(|selected| !expected.contains(selected))
        .collect();
    if !unexpected.is_empty() {
        problems.push(format!(
            "the lane executed tests outside its selection: {unexpected:?}"
        ));
    }
    problems
        .into_iter()
        .for_each(|problem| checks.fail(problem));
}

fn check_disk_evidence(checks: &mut Checks<'_, LaneRun>) {
    let lane_run = checks.run;
    let disk = match &lane_run.evidence["nats"] {
        Ok(Evidence {
            disk: Some(disk), ..
        }) => disk,
        Ok(_) => {
            return checks.fail("the selected lane recorded no disk measurements".to_owned());
        }
        Err(error) => return checks.fail(format!("nats evidence is missing: {error}")),
    };
    for (phase, size) in [("before", 1024), ("after", 2048)] {
        let snapshot = &disk[phase];
        if snapshot["images"][0]["size_bytes"] != size
            || snapshot["engine_report"].as_str().is_none_or(str::is_empty)
            || snapshot["cargo_target_kib"].as_u64().is_none()
        {
            checks.fail(format!("invalid {phase} disk snapshot: {snapshot}"));
        }
    }
}

fn failed_disk_evidence(repo: &FixtureRepo, failures: &mut Vec<String>) {
    for sample in [1, 2] {
        let lane_run = run_lane(
            repo,
            &format!("disk-failure-{sample}"),
            &["nats"],
            Scenario {
                failed_disk_sample: Some(sample),
                ..Scenario::PASSING
            },
        );
        let mut checks = Checks {
            row: "failed disk measurement",
            run: &lane_run,
            failures,
        };
        checks.exit_status(1);
        checks.evidence("nats", "failed");
        checks.reaped_only_owned();
        if sample == 1 {
            checks.no_test_ran();
        }
    }
}

fn unavailable_engine(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // No engine is infrastructure, never a pass or a failed assertion.
    let lane_run = run_lane(
        repo,
        "missing-engine",
        &["nats"],
        Scenario {
            engine: EngineState::Absent,
            ..Scenario::PASSING
        },
    );
    let mut checks = Checks {
        row: "missing engine",
        run: &lane_run,
        failures,
    };
    checks.exit_status(INFRASTRUCTURE_STATUS);
    checks.evidence("nats", "infrastructure_unavailable");
    checks.no_test_ran();
}

fn wrong_image(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // An engine answering with another image identity refuses before tests.
    let lane_run = run_lane(
        repo,
        "wrong-image",
        &["nats"],
        Scenario {
            engine: EngineState::Present(ImageIdentity::Substituted),
            ..Scenario::PASSING
        },
    );
    let mut checks = Checks {
        row: "wrong image identity",
        run: &lane_run,
        failures,
    };
    checks.refused();
    checks.evidence("nats", "failed");
    checks.no_test_ran();
    checks.reaped_only_owned();
}

fn empty_test_selections(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A selection that lists no test fails in every lane, even though the
    // filtered-out test binary itself exits zero.
    for lane in LANES {
        let lane_run = run_lane(
            repo,
            &format!("zero-{lane}"),
            &[lane],
            Scenario {
                listing: Listing::Zero,
                ..Scenario::PASSING
            },
        );
        let row = format!("zero selected tests in {lane}");
        let mut checks = Checks {
            row: &row,
            run: &lane_run,
            failures,
        };
        checks.refused();
        checks.evidence(lane, "failed");
        checks.no_test_ran();
        checks.reaped_only_owned();
    }
}

fn failed_test_evidence(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A failed test, a missing witness, and a stale witness cannot pass.
    for (row, outcome) in [
        ("failed test", Outcome::Fails),
        ("missing witness", Outcome::OmitsWitness),
        ("stale witness", Outcome::StaleWitness),
    ] {
        let lane_run = run_lane(
            repo,
            &row.replace(' ', "-"),
            &["nats"],
            Scenario {
                outcome,
                ..Scenario::PASSING
            },
        );
        let mut checks = Checks {
            row,
            run: &lane_run,
            failures,
        };
        checks.refused();
        checks.evidence("nats", "failed");
        checks.reaped_only_owned();
    }
}

fn invalid_lane_selections(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A missing or unknown lane fails before any engine or test effect.
    for (row, arguments) in [
        ("empty selection", &[][..]),
        ("unknown lane", &["kafka"][..]),
    ] {
        let lane_run = run_lane(repo, &row.replace(' ', "-"), arguments, Scenario::PASSING);
        let mut checks = Checks {
            row,
            run: &lane_run,
            failures,
        };
        checks.refused();
        checks.no_test_ran();
        checks.nothing_started();
    }
}

#[test]
fn local_integration_cargo_uses_the_declared_execution_environment() {
    let inventory = tool_inventory();
    let repo = private_tooling_checkout();
    let mut failures = Vec::new();

    declared_passing_selection(&repo, &inventory, &mut failures);
    declared_failed_listing(&repo, &mut failures);
    declared_failed_test(&repo, &mut failures);
    no_private_tooling_effects(&repo, &mut failures);

    repo.close();
    assert!(
        failures.is_empty(),
        "{CARGO_DIAGNOSTIC}:\n{}",
        failures.join("\n\n")
    );
}

fn declared_passing_selection(repo: &FixtureRepo, inventory: &str, failures: &mut Vec<String>) {
    // The selection lists and runs through the PATH Cargo, under the
    // inherited target directory, and keeps its exact-selection and cleanup
    // evidence.
    let lane_run = run_lane(repo, "declared-baseline", &["nats"], Scenario::PASSING);
    let mut checks = Checks {
        row: "declared Cargo for a passing selection",
        run: &lane_run,
        failures,
    };
    checks.exit_status(0);
    checks.evidence("nats", "passed");
    checks.declared_cargo(&declared_target(repo));
    checks.listed_and_executed();
    check_started_services(&mut checks, inventory);
    check_exact_runs(&mut checks);
    checks.reaped_only_owned();
}

fn declared_failed_listing(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A listing Cargo fails is a failed selection that keeps Cargo's status.
    let lane_run = run_lane(
        repo,
        "declared-failed-listing",
        &["nats"],
        Scenario {
            listing: Listing::Fails,
            ..Scenario::PASSING
        },
    );
    let mut checks = Checks {
        row: "declared Cargo for a failed listing",
        run: &lane_run,
        failures,
    };
    checks.refused();
    checks.evidence("nats", "failed");
    checks.declared_cargo(&declared_target(repo));
    checks.kept_cargo_status();
    checks.no_test_ran();
}

fn declared_failed_test(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A test Cargo fails is a failed selection that keeps Cargo's status.
    let lane_run = run_lane(
        repo,
        "declared-failed-test",
        &["nats"],
        Scenario {
            outcome: Outcome::Fails,
            ..Scenario::PASSING
        },
    );
    let mut checks = Checks {
        row: "declared Cargo for a failed test",
        run: &lane_run,
        failures,
    };
    checks.refused();
    checks.evidence("nats", "failed");
    checks.declared_cargo(&declared_target(repo));
    checks.kept_cargo_status();
    checks.reaped_only_owned();
}

fn no_private_tooling_effects(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // No row reached the ignored lease, and no row made a target tree of its
    // own, in the checkout or beside it.
    let lease_calls = logged_text(&repo.outside(LEASE_LOG));
    if !lease_calls.is_empty() {
        failures.push(format!(
            "the runner branched on ignored tooling {PRIVATE_LEASE}:\n{lease_calls}"
        ));
    }
    for (what, path) in [
        ("the private lease target", repo.outside(LEASE_TARGET)),
        ("a checkout target tree", repo.path().join("target")),
    ] {
        if path.exists() {
            failures.push(format!("the runner created {what} at {}", path.display()));
        }
    }
}
