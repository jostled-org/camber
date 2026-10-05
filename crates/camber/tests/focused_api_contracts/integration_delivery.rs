//! The integration test-budget receipt, through the shipped budget runner.
//!
//! Each row runs `.github/scripts/check-integration-test-budget.sh` as a real
//! Bash process from an owned checkout of the tracked `.github` tree. Only
//! Cargo and rustc are controlled executables, placed ahead of a system-only
//! `PATH`. The selection, repetition, binding, and reporting logic under test
//! is the tracked runner itself. This proves the runner's admission and its
//! report, not product behavior: real executions supply that evidence.
//!
//! The runner contract these rows hold it to:
//!
//! - It takes no arguments. It builds the three named Cargo test roots once
//!   (`cargo test --no-run` naming each with `--test`), lists each case
//!   exactly and requires one listed test, then runs each case 100 times as
//!   one exact libtest filter under its own root.
//! - It runs the `cargo` and `rustc` that `PATH` selects, directly, under the
//!   inherited `CARGO_TARGET_DIR`.
//! - Each sample runs with a fresh, task-owned `TMPDIR`. Anything left in it
//!   is owned resource residue.
//! - The last line of its stdout is one JSON object carrying `tree_oid`,
//!   `rustc`, `host`, `profile`, `target_dir`, `executable_count`,
//!   `sample_count`, `per_case_duration_ms`, `target_bytes_before`,
//!   `target_bytes_after`, `owned_resource_residue`, and `result`.
//!   `per_case_duration_ms` maps each `root::filter` selector to its samples.
//!   `sample_count` counts the samples recorded across every case, so a
//!   refused or failed run reports only the samples that ran.
//! - Exit 0 and `"result": "passed"` only when every sample passed with no
//!   residue on the committed tree under the pinned toolchain. A failed
//!   build, an empty or ambiguous selection, a failed sample, or residue fails
//!   with `"result": "failed"` and keeps Cargo's status in the report. No
//!   Cargo is unavailable infrastructure: exit 75.
//! - Its own output stays bounded however much a sample prints.
//! - It keeps one log per case under `<target dir>/integration-budget`, names
//!   that directory on stderr, and records every sample there with a bounded
//!   tail of the sample's output.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::delivery_fixture::{
    FixtureRepo, INFRASTRUCTURE_STATUS, SYSTEM_PATH, Tool, ambient_executable,
    assert_absent_from_system_path, isolated_command, logged_text, quoted, repository_file,
    repository_root, run_reaped, write_executable, write_file,
};
use crate::temp_support::TempRoot;

const RUNNER: &str = ".github/scripts/check-integration-test-budget.sh";
const RED_DIAGNOSTIC: &str = "M9 final integration proof receipt is missing";
const SAMPLE_COUNT: u64 = 100;
/// The status a stub Cargo fails with.
pub(crate) const CARGO_FAILURE_STATUS: &str = "101";
/// The repeated deterministic cases: Cargo test root, then libtest filter.
const CASES: [(&str, &str); 3] = [
    (
        "component_runtime_resources",
        "integration_lifecycle::integration_admission_is_atomic_with_root_closure",
    ),
    (
        "component_integrations",
        "nats_operations::delivered_slow_consumer_event_closes_every_subscription",
    ),
    (
        "acceptance_owned_lifecycle",
        "dns_cleanup::dns_waiter_drop_keeps_provider_until_cleanup_is_named_or_deleted",
    ),
];
/// The case and sample the failing-sample row fails at.
const FAILING_CASE: usize = 1;
const FAILING_SAMPLE: u64 = 37;
/// How much one sample prints in the flooding row.
const FLOOD_BYTES: usize = 4 * 1024 * 1024;
/// The most the runner itself may print, across stdout and stderr.
const OUTPUT_BOUND: usize = 256 * 1024;
/// The most one case's log may hold: a bounded tail of each of its samples,
/// far below one flooding sample.
const CASE_LOG_BOUND: u64 = 1024 * 1024;
/// The log root the runner keeps under the inherited target directory.
const LOG_ROOT: &str = "integration-budget";
/// The stderr line that names one run's log directory.
const LOG_DIR_PREFIX: &str = "integration budget: logs: ";
/// Bytes already in the inherited target directory before the run.
const SEEDED_TARGET_BYTES: usize = 64 * 1024;
/// One budget run, from spawn to reap: three hundred stub samples plus the
/// runner's own bookkeeping.
const RUN_BOUND: Duration = Duration::from_secs(300);
const HOST_TRIPLE: &str = "fixture-budget-host";
/// A well-formed toolchain one release away from the pinned one.
const UNPINNED_RUSTC: &str = "rustc 1.97.0 (51ff8ff5d 2026-07-07)";
const UNPINNED_CARGO: &str = "cargo 1.97.0 (6f2c0a1b3 2026-06-24)";
const REQUIRED_FIELDS: [&str; 12] = [
    "tree_oid",
    "rustc",
    "host",
    "profile",
    "target_dir",
    "executable_count",
    "sample_count",
    "per_case_duration_ms",
    "target_bytes_before",
    "target_bytes_after",
    "owned_resource_residue",
    "result",
];

/// What the controlled toolchain does in one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Every sample passes; the first one floods its output.
    Passing,
    BuildFails,
    ZeroSelection,
    DoubleSelection,
    /// One sample of one case fails.
    SampleFails,
    /// The first sample leaves a file in its `TMPDIR`.
    Residue,
    /// rustc reports nothing.
    BlankRustc,
    /// rustc and Cargo report a release other than the pinned one.
    UnpinnedToolchain,
    /// No Cargo is on `PATH`.
    NoCargo,
    /// The workspace exceeds its executable budget.
    TooManyExecutables,
    /// Cargo returns no usable workspace inventory.
    InvalidMetadata,
}

impl Mode {
    fn word(self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::BuildFails => "build-fails",
            Self::ZeroSelection => "zero-selection",
            Self::DoubleSelection => "double-selection",
            Self::SampleFails => "sample-fails",
            Self::Residue => "residue",
            Self::BlankRustc => "blank-rustc",
            Self::UnpinnedToolchain => "unpinned",
            Self::NoCargo => "no-cargo",
            Self::TooManyExecutables => "too-many-executables",
            Self::InvalidMetadata => "invalid-metadata",
        }
    }
}

/// One call a stub Cargo recorded: the path it ran as, the target directory
/// it inherited, and its arguments.
#[derive(Debug)]
pub(crate) struct CargoCall {
    executable: Box<str>,
    target_dir: Box<str>,
    pub(crate) arguments: Box<str>,
}

/// One libtest call: the roots it named, its filter, whether it was exact,
/// and the `TMPDIR` it ran under.
#[derive(Debug)]
struct TestCall {
    roots: Box<str>,
    filter: Box<str>,
    exact: bool,
    tmpdir: Box<str>,
}

/// One recorded line in the stub log, in the order Cargo saw it.
#[derive(Debug)]
enum Step {
    Build(Box<str>),
    List(TestCall),
    Run(TestCall),
}

/// Everything one runner invocation left behind.
struct BudgetRun {
    status: i32,
    stdout: Box<str>,
    stderr: Box<str>,
    calls: Box<[CargoCall]>,
    steps: Box<[Step]>,
    cargo_executable: Box<str>,
    inherited_tmpdir: PathBuf,
}

impl RunnerReport for BudgetRun {
    const REFUSED: &'static str = "measurement";

    fn status(&self) -> i32 {
        self.status
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Owned(format!("{}{}", self.stdout, self.stderr))
    }

    fn cargo_calls(&self) -> &[CargoCall] {
        &self.calls
    }

    fn cargo_executable(&self) -> &str {
        &self.cargo_executable
    }
}

impl BudgetRun {
    fn output_len(&self) -> usize {
        self.stdout.len() + self.stderr.len()
    }

    fn summary(&self) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let line = self
            .stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .ok_or("stdout carries no summary line")?;
        match serde_json::from_str(line) {
            Ok(serde_json::Value::Object(map)) => Ok(map),
            Ok(other) => Err(format!("the summary line is not an object: {other}")),
            Err(error) => Err(format!("the summary line is not JSON ({error}): {line}")),
        }
    }

    fn builds(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::Build(roots) => Some(&**roots),
                _ => None,
            })
            .collect()
    }

    fn runs(&self) -> Vec<&TestCall> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::Run(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    fn lists(&self) -> Vec<&TestCall> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                Step::List(call) => Some(call),
                _ => None,
            })
            .collect()
    }
}

// --- Runner fixture, shared with `local_integration_delivery` ------------

/// What every stub Cargo does first: record the call, answer a version
/// query, and find the subcommand. The row's own body follows.
const CARGO_PRELUDE: &str = r#"#!/usr/bin/env bash
set -u
state=@STATE@
printf 'call\t%s\t%s\t%s\n' "$0" "${CARGO_TARGET_DIR-(unset)}" "$*" >>"${state}/cargo.log"
for argument in "$@"; do
    case "${argument}" in
        --version|-V) printf '%s\n' '@VERSION@'; exit 0 ;;
    esac
done
subcommand=''
for argument in "$@"; do
    case "${argument}" in
        +*|-*) ;;
        *) subcommand="${argument}"; break ;;
    esac
done
"#;

/// The controlled executables one row searches before the system path, and
/// the state its stubs record into.
pub(crate) struct StubTools {
    root: TempRoot,
}

impl StubTools {
    /// A tool directory holding real Bash, Git, ripgrep, and jq.
    pub(crate) fn new() -> Self {
        let root = TempRoot::new().expect("stub tool root was not created");
        let tools = Self { root };
        fs::create_dir_all(tools.bin()).expect("stub tool directory was not created");
        fs::create_dir_all(tools.state()).expect("stub tool state was not created");
        for name in ["bash", "git", "rg", "jq"] {
            std::os::unix::fs::symlink(ambient_executable(name), tools.bin().join(name))
                .expect("stub tool was not linked");
        }
        tools
    }

    pub(crate) fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    pub(crate) fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// Install a stub Cargo reporting `version` that runs `body` after the
    /// shared prelude. `@STATE@`, `@VERSION@`, and `@FAILURE@` are filled in.
    pub(crate) fn install_cargo(&self, version: &str, body: &str) {
        let script = format!("{CARGO_PRELUDE}{body}")
            .replace("@STATE@", &quoted(&self.state()))
            .replace("@VERSION@", version)
            .replace("@FAILURE@", CARGO_FAILURE_STATUS);
        write_executable(&self.cargo(), script);
    }

    /// The stub Cargo this row places first on `PATH`.
    pub(crate) fn cargo(&self) -> PathBuf {
        self.bin().join("cargo")
    }

    pub(crate) fn cargo_log(&self) -> Box<str> {
        logged_text(&self.state().join("cargo.log"))
    }

    pub(crate) fn close(self) {
        self.root.close().expect("stub tool root was not removed");
    }
}

/// Every call a stub Cargo recorded, in order.
pub(crate) fn cargo_calls(cargo_log: &str) -> Box<[CargoCall]> {
    cargo_log
        .lines()
        .filter_map(|line| line.strip_prefix("call\t"))
        .map(|line| {
            let [executable, target_dir, arguments] = tab_fields(line);
            CargoCall {
                executable: executable.into(),
                target_dir: target_dir.into(),
                arguments: arguments.into(),
            }
        })
        .collect()
}

/// The first `N` tab-separated fields of one stub log line. The last field
/// keeps any further tabs; a missing field is empty.
pub(crate) fn tab_fields<const N: usize>(line: &str) -> [&str; N] {
    let mut fields = line.splitn(N, '\t');
    std::array::from_fn(|_| fields.next().unwrap_or_default())
}

/// Why `calls` did not all run as the stub `cargo` under the declared
/// `target` with no target override of their own. Empty when they did.
fn undeclared_cargo(calls: &[CargoCall], cargo: &str, target: &Path) -> Vec<String> {
    let target = target.display().to_string();
    let strays: Vec<&CargoCall> = calls
        .iter()
        .filter(|call| {
            *call.executable != *cargo
                || *call.target_dir != *target
                || call.arguments.contains("--target-dir")
        })
        .collect();
    let mut problems = Vec::new();
    if calls.is_empty() {
        problems.push("the runner made no Cargo call".to_owned());
    }
    if !strays.is_empty() {
        problems.push(format!(
            "Cargo ran outside the declared environment ({cargo} under {target}): {strays:?}"
        ));
    }
    problems
}

/// Why `output` lost the status the failed stub Cargo call exited with.
fn lost_cargo_status(output: &str) -> Option<String> {
    let kept = format!("failed with status {CARGO_FAILURE_STATUS}");
    (!output.contains(&kept)).then(|| format!("the report lost Cargo's status: expected {kept:?}"))
}

/// A committed checkout holding the tracked `.github` tree and toolchain
/// record, and nothing that could stand in for a runner's logic.
pub(crate) fn tracked_checkout(message: &str) -> FixtureRepo {
    let repo = FixtureRepo::new();
    copy_tracked_inputs(&repo);
    repo.commit(message);
    repo
}

/// A Bash process running the tracked `runner` from `repo`, searching
/// `search_path`, under the checkout's scratch `TMPDIR` and the inherited
/// `target`.
pub(crate) fn runner_command(
    repo: &FixtureRepo,
    runner: &str,
    search_path: &str,
    target: &Path,
) -> Command {
    let mut command = isolated_command(
        &ambient_executable("bash"),
        &repo.path(),
        &repo.home(),
        search_path,
    );
    command
        .arg(repo.path().join(runner))
        .env("TMPDIR", repo.scratch())
        .env("CARGO_TARGET_DIR", target);
    command
}

/// The tracked `.github` tree and toolchain record, copied into `repo`.
pub(crate) fn copy_tracked_inputs(repo: &FixtureRepo) {
    copy_tree(&repository_root().join(".github"), repo, ".github");
    repo.write(
        "rust-toolchain.toml",
        &repository_file("rust-toolchain.toml"),
    );
}

fn copy_tree(source: &Path, repo: &FixtureRepo, relative: &str) {
    let entries = fs::read_dir(source)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source.display()));
    for entry in entries {
        let entry = entry.expect("tracked tree entry was not readable");
        let child = format!("{relative}/{}", entry.file_name().to_string_lossy());
        let metadata = entry
            .metadata()
            .expect("tracked tree metadata was not readable");
        if metadata.is_dir() {
            copy_tree(&entry.path(), repo, &child);
            continue;
        }
        let bytes = fs::read(entry.path()).expect("tracked file was not readable");
        match metadata.permissions().mode() & 0o111 != 0 {
            true => repo.write_executable(&child, bytes),
            false => repo.write(&child, &bytes),
        }
    }
}

// --- Controlled toolchain ------------------------------------------------

/// The budget stub Cargo's body: it answers exact listings and runs samples
/// as the row's mode says. It never builds anything.
const CARGO_BODY: &str = r#"mode=$(cat "${state}/mode")
if [ "${subcommand}" = metadata ]; then
    [ "${mode}" != invalid-metadata ] || exit 0
    cat "${state}/metadata.json"
    exit 0
fi
[ "${subcommand}" = test ] || exit 0
roots=''
pending_root=0
build_only=0
after=0
skip=0
listing=0
exact=0
filter=''
for argument in "$@"; do
    if [ "${after}" = 0 ]; then
        if [ "${pending_root}" = 1 ]; then
            roots="${roots}${argument},"
            pending_root=0
            continue
        fi
        case "${argument}" in
            --test) pending_root=1 ;;
            --test=*) roots="${roots}${argument#--test=}," ;;
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
        --exact) exact=1 ;;
        --test-threads|--format|--skip|--color|-Z|--logfile|--shuffle-seed) skip=1 ;;
        -*) ;;
        *) [ -n "${filter}" ] || filter="${argument}" ;;
    esac
done
if [ "${build_only}" = 1 ]; then
    printf 'build\t%s\n' "${roots}" >>"${state}/cargo.log"
    if [ "${mode}" = build-fails ]; then
        printf 'error: could not compile `camber` (test "%s")\n' "${roots}" >&2
        exit @FAILURE@
    fi
    exit 0
fi
if [ "${listing}" = 1 ]; then
    printf 'list\t%s\t%s\t%s\t%s\n' "${roots}" "${filter}" "${exact}" "${TMPDIR-(unset)}" >>"${state}/cargo.log"
    case "${mode}" in
        zero-selection) printf '\n0 tests, 0 benchmarks\n' ;;
        double-selection) printf '%s: test\n%s_sibling: test\n\n2 tests, 0 benchmarks\n' "${filter}" "${filter}" ;;
        *) printf '%s: test\n\n1 test, 0 benchmarks\n' "${filter}" ;;
    esac
    exit 0
fi
printf 'run\t%s\t%s\t%s\t%s\n' "${roots}" "${filter}" "${exact}" "${TMPDIR-(unset)}" >>"${state}/cargo.log"
key=$(printf '%s' "${filter}" | tr -c 'A-Za-z0-9_' '_')
count=1
if [ -f "${state}/count-${key}" ]; then
    count=$(( $(cat "${state}/count-${key}") + 1 ))
fi
printf '%s\n' "${count}" >"${state}/count-${key}"
case "${mode}" in
    zero-selection)
        printf '\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n'
        exit 0
        ;;
    double-selection)
        printf '\nrunning 2 tests\ntest %s ... ok\ntest %s_sibling ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' "${filter}" "${filter}"
        exit 0
        ;;
    passing)
        if [ ! -f "${state}/flooded" ]; then
            : >"${state}/flooded"
            head -c @FLOOD@ /dev/zero | tr '\0' 'x'
            printf '\n'
        fi
        ;;
    residue)
        if [ "${count}" = 1 ] && [ -n "${TMPDIR:-}" ]; then
            printf 'left behind\n' >"${TMPDIR}/fixture-residue"
        fi
        ;;
    sample-fails)
        if [ "${filter}" = '@FAILING_FILTER@' ] && [ "${count}" = @FAILING_SAMPLE@ ]; then
            printf '\nrunning 1 test\ntest %s ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n' "${filter}"
            exit @FAILURE@
        fi
        ;;
esac
printf '\nrunning 1 test\ntest %s ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' "${filter}"
"#;

/// A stub rustc: it reports the row's toolchain identity and host.
const RUSTC_SCRIPT: &str = r#"#!/usr/bin/env bash
mode=$(cat @STATE@/mode)
[ "${mode}" != blank-rustc ] || exit 0
release=$(printf '%s\n' '@VERSION@' | cut -d' ' -f2)
case " $* " in
    *" -vV "*|*" -Vv "*|*" --verbose "*)
        printf '%s\nbinary: rustc\ncommit-hash: fixture\ncommit-date: 2026-08-18\nhost: @HOST@\nrelease: %s\nLLVM version: 21.1.0\n' '@VERSION@' "${release}"
        ;;
    *) printf '%s\n' '@VERSION@' ;;
esac
"#;

/// The controlled toolchain for one row in `mode`.
fn budget_tools(mode: Mode) -> StubTools {
    let tools = StubTools::new();
    write_file(&tools.state().join("mode"), mode.word().as_bytes());
    write_budget_metadata(&tools, mode);
    let (rustc, cargo) = match mode {
        Mode::UnpinnedToolchain => (UNPINNED_RUSTC, UNPINNED_CARGO),
        _ => (Tool::Rustc.pinned().0, Tool::Cargo.pinned().0),
    };
    if mode != Mode::NoCargo {
        let body = CARGO_BODY
            .replace("@FLOOD@", &FLOOD_BYTES.to_string())
            .replace("@FAILING_FILTER@", CASES[FAILING_CASE].1)
            .replace("@FAILING_SAMPLE@", &FAILING_SAMPLE.to_string());
        tools.install_cargo(cargo, &body);
    }
    let script = RUSTC_SCRIPT
        .replace("@STATE@", &quoted(&tools.state()))
        .replace("@VERSION@", rustc)
        .replace("@HOST@", HOST_TRIPLE);
    write_executable(&tools.bin().join("rustc"), script);
    tools
}

fn write_budget_metadata(tools: &StubTools, mode: Mode) {
    let count = match mode {
        Mode::TooManyExecutables => 31,
        _ => 7,
    };
    let targets: Vec<_> = (0..count)
        .map(|index| serde_json::json!({"name": format!("test_{index}"), "kind": ["test"]}))
        .chain(std::iter::once(
            serde_json::json!({"name": "library", "kind": ["lib"]}),
        ))
        .collect();
    let metadata = serde_json::json!({
        "workspace_members": ["fixture"],
        "packages": [
            {"id": "fixture", "targets": targets},
            {"id": "dependency", "targets": [{"name": "excluded", "kind": ["test"]}]}
        ]
    });
    write_file(
        &tools.state().join("metadata.json"),
        metadata.to_string().as_bytes(),
    );
}

// --- Owned checkout ------------------------------------------------------

/// The target directory every row's runner inherits, seeded with bytes the
/// runner must count.
fn declared_target(repo: &FixtureRepo) -> PathBuf {
    let target = repo.outside("target");
    let seed = target.join("debug").join("fixture-seed");
    if !seed.exists() {
        write_file(&seed, &[b'x'; SEEDED_TARGET_BYTES]);
    }
    target
}

// --- One runner invocation -----------------------------------------------

fn run_budget(repo: &FixtureRepo, mode: Mode) -> BudgetRun {
    let tools = budget_tools(mode);
    let search_path = format!("{}:{SYSTEM_PATH}", tools.bin().display());
    let command = runner_command(repo, RUNNER, &search_path, &declared_target(repo));
    let (status, stdout, stderr) = run_reaped(command, RUN_BOUND, "budget runner");
    let cargo_log = tools.cargo_log();
    let run = BudgetRun {
        status,
        stdout,
        stderr,
        calls: cargo_calls(&cargo_log),
        steps: steps(&cargo_log),
        cargo_executable: tools.cargo().display().to_string().into(),
        inherited_tmpdir: repo.scratch(),
    };
    tools.close();
    run
}

fn test_call(fields: &str) -> TestCall {
    let [roots, filter, exact, tmpdir] = tab_fields(fields);
    TestCall {
        roots: roots.into(),
        filter: filter.into(),
        exact: exact == "1",
        tmpdir: tmpdir.into(),
    }
}

fn steps(cargo_log: &str) -> Box<[Step]> {
    cargo_log
        .lines()
        .filter_map(|line| {
            let (kind, fields) = line.split_once('\t')?;
            match kind {
                "build" => Some(Step::Build(fields.into())),
                "list" => Some(Step::List(test_call(fields))),
                "run" => Some(Step::Run(test_call(fields))),
                _ => None,
            }
        })
        .collect()
}

/// The root list a stub call recorded, as a set.
fn roots(recorded: &str) -> BTreeSet<&str> {
    recorded
        .split(',')
        .filter(|root| !root.is_empty())
        .collect()
}

fn selector((root, filter): (&str, &str)) -> String {
    format!("{root}::{filter}")
}

// --- Checks, shared with `local_integration_delivery` --------------------

/// What the shared checks read from one runner invocation.
pub(crate) trait RunnerReport {
    /// What a refusal refuses, as its diagnostic names it.
    const REFUSED: &'static str;
    fn status(&self) -> i32;
    /// Everything the runner wrote, stdout then stderr.
    fn output(&self) -> Cow<'_, str>;
    fn cargo_calls(&self) -> &[CargoCall];
    /// The stub Cargo the row placed first on `PATH`.
    fn cargo_executable(&self) -> &str;
}

/// One row's checks against one runner invocation, collecting failures.
pub(crate) struct Checks<'a, R> {
    pub(crate) row: &'a str,
    pub(crate) run: &'a R,
    pub(crate) failures: &'a mut Vec<String>,
}

impl<R: RunnerReport> Checks<'_, R> {
    /// The most runner output one failure quotes.
    const QUOTED_OUTPUT: usize = 8192;

    pub(crate) fn fail(&mut self, message: String) {
        let full = self.run.output();
        let mut output: String = full.chars().take(Self::QUOTED_OUTPUT).collect();
        if output.len() < full.len() {
            output.push_str("\n[... truncated by the test ...]");
        }
        self.failures.push(format!(
            "{}: {message}\n--- runner exit {} ---\n{output}",
            self.row,
            self.run.status()
        ));
    }

    pub(crate) fn exit_status(&mut self, expected: i32) {
        let status = self.run.status();
        if status != expected {
            self.fail(format!("exited {status}, expected {expected}"));
        }
    }

    /// A refusal: neither success nor unavailable infrastructure.
    pub(crate) fn refused(&mut self) {
        let status = self.run.status();
        if status == 0 || status == INFRASTRUCTURE_STATUS {
            self.fail(format!(
                "exited {status}; a failed {} is neither a pass nor unavailable infrastructure",
                R::REFUSED
            ));
        }
    }

    /// The report keeps the status the failed Cargo call exited with.
    pub(crate) fn kept_cargo_status(&mut self) {
        if let Some(problem) = lost_cargo_status(&self.run.output()) {
            self.fail(problem);
        }
    }

    /// Every Cargo call is the stub `PATH` selects, under the declared target
    /// directory, with no target override of its own.
    pub(crate) fn declared_cargo(&mut self, target: &Path) {
        let run = self.run;
        undeclared_cargo(run.cargo_calls(), run.cargo_executable(), target)
            .into_iter()
            .for_each(|problem| self.fail(problem));
    }
}

impl Checks<'_, BudgetRun> {
    /// Anything but a pass: a nonzero exit, and no summary claiming one.
    fn not_passed(&mut self) {
        if self.run.status == 0 {
            self.fail("exited 0 for a measurement it cannot bind".to_owned());
        }
        let claimed = self
            .run
            .summary()
            .ok()
            .and_then(|summary| summary.get("result").cloned());
        if claimed == Some(serde_json::Value::from("passed")) {
            self.fail("the summary claims a pass".to_owned());
        }
    }

    /// The summary exists, carries every required field, and reads `result`.
    fn summary_result(&mut self, result: &str) {
        let summary = match self.run.summary() {
            Ok(summary) => summary,
            Err(error) => return self.fail(error),
        };
        let missing: Vec<&str> = REQUIRED_FIELDS
            .iter()
            .copied()
            .filter(|field| !summary.contains_key(*field))
            .collect();
        if !missing.is_empty() {
            self.fail(format!("the summary lacks {missing:?}: {summary:?}"));
        }
        if summary.get("result").and_then(serde_json::Value::as_str) != Some(result) {
            self.fail(format!("the summary result is not {result:?}: {summary:?}"));
        }
    }

    fn no_sample_ran(&mut self) {
        let runs = self.run.runs();
        if !runs.is_empty() {
            self.fail(format!("samples ran after the refusal: {runs:?}"));
        }
    }

    /// `sample_count` is the samples the stub Cargo ran, and the samples
    /// `per_case_duration_ms` records: never the requested repetition.
    fn counted_samples(&mut self) {
        let summary = match self.run.summary() {
            Ok(summary) => summary,
            Err(error) => return self.fail(error),
        };
        let ran = self.run.runs().len() as u64;
        let Some(per_case) = summary
            .get("per_case_duration_ms")
            .and_then(serde_json::Value::as_object)
        else {
            return self.fail("per_case_duration_ms is not an object".to_owned());
        };
        let recorded: u64 = per_case
            .values()
            .filter_map(serde_json::Value::as_array)
            .map(|samples| samples.len() as u64)
            .sum();
        match u64_field(&summary, "sample_count") {
            Ok(count) if count == ran && count == recorded => {}
            Ok(count) => self.fail(format!(
                "sample_count {count} is not the {ran} samples that ran \
                 ({recorded} in per_case_duration_ms)"
            )),
            Err(error) => self.fail(error),
        }
    }

    fn bounded_output(&mut self) {
        let length = self.run.output_len();
        if length > OUTPUT_BOUND {
            self.failures.push(format!(
                "{}: the runner printed {length} bytes, above its {OUTPUT_BOUND}-byte bound, \
                 after one sample printed {FLOOD_BYTES}",
                self.row
            ));
        }
    }
}

/// One build names all three roots and precedes every listing and sample.
fn check_single_build(checks: &mut Checks<'_, BudgetRun>) {
    let run = checks.run;
    let builds = run.builds();
    let expected: BTreeSet<&str> = CASES.iter().map(|(root, _)| *root).collect();
    match builds.as_slice() {
        [only] if roots(only) == expected => {}
        _ => checks.fail(format!(
            "expected one build of {expected:?}, saw {builds:?}"
        )),
    }
    let first_test = run
        .steps
        .iter()
        .position(|step| !matches!(step, Step::Build(_)));
    let last_build = run
        .steps
        .iter()
        .rposition(|step| matches!(step, Step::Build(_)));
    match (first_test, last_build) {
        (Some(first_test), Some(last_build)) if last_build > first_test => {
            checks.fail("a build followed a listing or sample".to_owned());
        }
        _ => {}
    }
}

/// The run kept one bounded log per case under the target directory's log
/// root, recording each of its samples once, in order, with the tail of its
/// output. The flooding sample keeps its result line, not its flood.
fn check_case_logs(checks: &mut Checks<'_, BudgetRun>, target: &Path) {
    let run = checks.run;
    let Some(directory) = run
        .stderr
        .lines()
        .find_map(|line| line.strip_prefix(LOG_DIR_PREFIX))
    else {
        return checks.fail("the runner named no log directory".to_owned());
    };
    let directory = PathBuf::from(directory);
    let root = target.join(LOG_ROOT);
    if directory.parent() != Some(root.as_path()) {
        checks.fail(format!(
            "the log directory {} is not under {}",
            directory.display(),
            root.display()
        ));
    }
    let expected: Vec<u64> = (1..=SAMPLE_COUNT).collect();
    for (case, _) in CASES {
        let log = directory.join(format!("{case}.log"));
        let text = match fs::read(&log) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                checks.fail(format!("{} is unreadable: {error}", log.display()));
                continue;
            }
        };
        if text.len() as u64 > CASE_LOG_BOUND {
            checks.fail(format!(
                "{} holds {} bytes, above its {CASE_LOG_BOUND}-byte bound",
                log.display(),
                text.len()
            ));
        }
        let samples: Vec<u64> = text
            .lines()
            .filter_map(|line| {
                let (sample, _) = line.strip_prefix("== sample ")?.split_once(": status 0,")?;
                sample.parse().ok()
            })
            .collect();
        let results = text.matches("test result: ok. 1 passed;").count() as u64;
        if samples != expected || results != SAMPLE_COUNT {
            checks.fail(format!(
                "{} records samples {samples:?} with {results} passing results, \
                 expected samples 1 through {SAMPLE_COUNT} each passing once",
                log.display()
            ));
        }
    }
}

/// Each case was listed exactly under its root before its first sample.
fn check_exact_listings(checks: &mut Checks<'_, BudgetRun>) {
    let run = checks.run;
    for (root, filter) in CASES {
        let listed = run.steps.iter().position(|step| match step {
            Step::List(call) => {
                call.exact
                    && *call.filter == *filter
                    && roots(&call.roots) == BTreeSet::from([root])
            }
            _ => false,
        });
        let first_run = run.steps.iter().position(|step| match step {
            Step::Run(call) => *call.filter == *filter,
            _ => false,
        });
        match (listed, first_run) {
            (Some(listed), Some(first_run)) if listed < first_run => {}
            _ => checks.fail(format!(
                "{root}::{filter} was not listed exactly before it ran: {:?}",
                run.lists()
            )),
        }
    }
}

/// Each case ran exactly 100 times, alone, exactly, from its own root, and
/// nothing else ran.
fn check_exact_samples(checks: &mut Checks<'_, BudgetRun>) {
    let runs = checks.run.runs();
    for (root, filter) in CASES {
        let mine: Vec<&&TestCall> = runs.iter().filter(|call| *call.filter == *filter).collect();
        let exact = mine
            .iter()
            .all(|call| call.exact && roots(&call.roots) == BTreeSet::from([root]));
        if mine.len() as u64 != SAMPLE_COUNT || !exact {
            checks.fail(format!(
                "{root}::{filter} ran {} samples (exact under its root: {exact}), expected {SAMPLE_COUNT}",
                mine.len()
            ));
        }
    }
    let known: BTreeSet<&str> = CASES.iter().map(|(_, filter)| *filter).collect();
    let strays: Vec<&&TestCall> = runs
        .iter()
        .filter(|call| !known.contains(&*call.filter))
        .collect();
    if !strays.is_empty() {
        checks.fail(format!("samples ran outside the three cases: {strays:?}"));
    }
}

/// Every sample ran under its own fresh `TMPDIR`, which the runner removed.
fn check_owned_tmpdirs(checks: &mut Checks<'_, BudgetRun>) {
    let run = checks.run;
    let inherited = run.inherited_tmpdir.display().to_string();
    let mut problems = Vec::new();
    for call in run.runs() {
        let path = Path::new(&*call.tmpdir);
        let fresh = path.is_absolute() && *call.tmpdir != *inherited;
        if !fresh {
            problems.push(format!(
                "{} ran under {:?}, not a fresh task-owned directory",
                call.filter, call.tmpdir
            ));
        }
        if fresh && path.exists() {
            problems.push(format!(
                "{} left its directory {:?}",
                call.filter, call.tmpdir
            ));
        }
    }
    problems.dedup();
    problems
        .into_iter()
        .for_each(|problem| checks.fail(problem));
}

fn u64_field(
    summary: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<u64, String> {
    summary
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            format!(
                "{field} is not a non-negative integer: {:?}",
                summary.get(field)
            )
        })
}

fn str_field<'a>(
    summary: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<&'a str, String> {
    summary
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{field} is not a nonempty string: {:?}", summary.get(field)))
}

/// Each selector maps to exactly its 100 recorded samples.
fn check_per_case(summary: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(per_case) = summary
        .get("per_case_duration_ms")
        .and_then(serde_json::Value::as_object)
    else {
        return vec!["per_case_duration_ms is not an object".to_owned()];
    };
    let expected: BTreeSet<String> = CASES.into_iter().map(selector).collect();
    let keys: BTreeSet<String> = per_case.keys().cloned().collect();
    let mut problems = Vec::new();
    if keys != expected {
        problems.push(format!(
            "per_case_duration_ms keys are {keys:?}, expected {expected:?}"
        ));
    }
    for (key, samples) in per_case {
        let durations: Option<Vec<u64>> = samples.as_array().map(|samples| {
            samples
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .collect()
        });
        let complete = samples.as_array().map(Vec::len) == Some(SAMPLE_COUNT as usize)
            && durations.as_ref().map(Vec::len) == Some(SAMPLE_COUNT as usize);
        if !complete {
            problems.push(format!(
                "{key} does not record {SAMPLE_COUNT} integer millisecond samples: {samples}"
            ));
        }
    }
    problems
}

/// The passing summary binds the committed tree, the pinned toolchain, the
/// inherited target directory, and every sample.
fn check_passing_summary(checks: &mut Checks<'_, BudgetRun>, tree_oid: &str, target: &Path) {
    let summary = match checks.run.summary() {
        Ok(summary) => summary,
        Err(error) => return checks.fail(error),
    };
    let target = target.display().to_string();
    let pinned = Tool::Rustc.pinned().1;
    let mut problems = check_per_case(&summary);
    let expectations: [(&str, Result<bool, String>); 9] = [
        (
            "tree_oid",
            str_field(&summary, "tree_oid").map(|oid| oid == tree_oid),
        ),
        (
            "rustc",
            str_field(&summary, "rustc").map(|rustc| rustc.contains(pinned)),
        ),
        (
            "host",
            str_field(&summary, "host").map(|host| host == HOST_TRIPLE),
        ),
        ("profile", str_field(&summary, "profile").map(|_| true)),
        (
            "target_dir",
            str_field(&summary, "target_dir").map(|dir| dir == target),
        ),
        (
            "executable_count",
            u64_field(&summary, "executable_count").map(|n| n == 7),
        ),
        (
            "sample_count",
            u64_field(&summary, "sample_count").map(|n| n == SAMPLE_COUNT * CASES.len() as u64),
        ),
        (
            "target_bytes_before",
            u64_field(&summary, "target_bytes_before").map(|n| n >= SEEDED_TARGET_BYTES as u64),
        ),
        (
            "owned_resource_residue",
            u64_field(&summary, "owned_resource_residue").map(|n| n == 0),
        ),
    ];
    for (field, outcome) in expectations {
        match outcome {
            Ok(true) => {}
            Ok(false) => problems.push(format!("{field} is wrong: {:?}", summary.get(field))),
            Err(error) => problems.push(error),
        }
    }
    // An unreadable `target_bytes_before` is already reported above.
    match (
        u64_field(&summary, "target_bytes_before"),
        u64_field(&summary, "target_bytes_after"),
    ) {
        (_, Err(error)) => problems.push(error),
        (Ok(before), Ok(after)) if after < before => {
            problems.push(format!("target_bytes_after {after} is below {before}"));
        }
        _ => {}
    }
    problems
        .into_iter()
        .for_each(|problem| checks.fail(problem));
}

/// Each named case is a real test in its declared root.
fn check_cases_exist(failures: &mut Vec<String>) {
    let tests = repository_root().join("crates/camber/tests");
    for (root, filter) in CASES {
        let Some((module, name)) = filter.rsplit_once("::") else {
            failures.push(format!("{filter} names no module"));
            continue;
        };
        let mounted = fs::read_to_string(tests.join(format!("{root}.rs")))
            .is_ok_and(|text| text.contains(&format!("{root}/{module}.rs")));
        let defined = fs::read_to_string(tests.join(root).join(format!("{module}.rs")))
            .is_ok_and(|text| text.contains(&format!("fn {name}(")));
        if !mounted || !defined {
            failures.push(format!(
                "{root}::{filter} is not a test in its root (mounted: {mounted}, defined: {defined})"
            ));
        }
    }
}

// --- Rows ----------------------------------------------------------------

#[test]
fn integration_budget_receipt_rejects_missing_or_failed_samples() {
    assert!(
        repository_root().join(RUNNER).is_file(),
        "{RED_DIAGNOSTIC}: the integration budget runner {RUNNER} is absent"
    );
    for tool in ["cargo", "rustc"] {
        assert_absent_from_system_path(tool);
    }
    let repo = tracked_checkout("integration budget fixture");
    let mut failures = Vec::new();
    check_cases_exist(&mut failures);

    passing_budget(&repo, &mut failures);
    failed_build(&repo, &mut failures);
    ambiguous_selections(&repo, &mut failures);
    failed_sample(&repo, &mut failures);
    owned_residue(&repo, &mut failures);
    unbound_toolchain(&repo, &mut failures);
    unbound_tree(&repo, &mut failures);
    missing_cargo(&repo, &mut failures);
    refused_inventory(&repo, &mut failures);

    repo.close();
    assert!(
        failures.is_empty(),
        "{RED_DIAGNOSTIC}:\n{}",
        failures.join("\n\n")
    );
}

fn refused_inventory(repo: &FixtureRepo, failures: &mut Vec<String>) {
    for mode in [Mode::TooManyExecutables, Mode::InvalidMetadata] {
        let run = run_budget(repo, mode);
        let mut checks = Checks {
            row: mode.word(),
            run: &run,
            failures,
        };
        checks.refused();
        checks.summary_result("failed");
        checks.no_sample_ran();
        if mode == Mode::TooManyExecutables
            && !run
                .summary()
                .is_ok_and(|summary| summary["executable_count"] == 31)
        {
            checks.fail("the receipt must report all 31 workspace test executables".into());
        }
    }
}

fn passing_budget(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // One build, exact listings, 100 exact samples per case, bounded output,
    // and a summary bound to this tree, toolchain, and target directory.
    let tree_oid = repo
        .git(&["rev-parse", "HEAD^{tree}"])
        .output
        .trim()
        .to_owned();
    let run = run_budget(repo, Mode::Passing);
    let target = declared_target(repo);
    let mut checks = Checks {
        row: "passing budget",
        run: &run,
        failures,
    };
    checks.exit_status(0);
    checks.summary_result("passed");
    checks.declared_cargo(&target);
    checks.bounded_output();
    check_single_build(&mut checks);
    check_exact_listings(&mut checks);
    check_exact_samples(&mut checks);
    check_owned_tmpdirs(&mut checks);
    check_case_logs(&mut checks, &target);
    checks.counted_samples();
    check_passing_summary(&mut checks, &tree_oid, &target);
}

fn failed_build(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A build Cargo fails ends the measurement before any sample.
    let run = run_budget(repo, Mode::BuildFails);
    let mut checks = Checks {
        row: "failed build",
        run: &run,
        failures,
    };
    checks.refused();
    checks.summary_result("failed");
    checks.kept_cargo_status();
    checks.no_sample_ran();
    checks.counted_samples();
}

fn ambiguous_selections(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A listing of zero tests or of two tests is no exact selection, even
    // though the filtered test binary itself exits zero.
    for (row, mode) in [
        ("zero selected tests", Mode::ZeroSelection),
        ("two selected tests", Mode::DoubleSelection),
    ] {
        let run = run_budget(repo, mode);
        let mut checks = Checks {
            row,
            run: &run,
            failures,
        };
        checks.refused();
        checks.summary_result("failed");
        checks.no_sample_ran();
        checks.counted_samples();
    }
}

fn failed_sample(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // One failed sample of 300 fails the receipt and names its case.
    let run = run_budget(repo, Mode::SampleFails);
    let mut checks = Checks {
        row: "failed sample",
        run: &run,
        failures,
    };
    checks.refused();
    checks.summary_result("failed");
    checks.kept_cargo_status();
    checks.counted_samples();
    if !run.output().contains(CASES[FAILING_CASE].1) {
        checks.fail(format!(
            "the report does not name the failed case {}",
            CASES[FAILING_CASE].1
        ));
    }
}

fn owned_residue(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A file left in a sample's own directory is residue, and residue fails.
    let run = run_budget(repo, Mode::Residue);
    let mut checks = Checks {
        row: "owned residue",
        run: &run,
        failures,
    };
    checks.refused();
    checks.summary_result("failed");
    checks.counted_samples();
    match run
        .summary()
        .and_then(|summary| u64_field(&summary, "owned_resource_residue"))
    {
        Ok(count) if count > 0 => {}
        Ok(count) => checks.fail(format!(
            "owned_resource_residue does not count the residue: {count}"
        )),
        Err(error) => checks.fail(error),
    }
    let inherited = run.inherited_tmpdir.join("fixture-residue");
    if inherited.exists() {
        checks.fail("a sample wrote into the inherited TMPDIR".to_owned());
        fs::remove_file(&inherited).expect("stray residue was not removed");
    }
}

fn unbound_toolchain(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // A toolchain that reports nothing, or another release, cannot bind a
    // receipt.
    for (row, mode) in [
        ("blank rustc identity", Mode::BlankRustc),
        ("unpinned toolchain", Mode::UnpinnedToolchain),
    ] {
        let run = run_budget(repo, mode);
        let mut checks = Checks {
            row,
            run: &run,
            failures,
        };
        checks.not_passed();
    }
}

fn unbound_tree(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // Uncommitted tracked changes are not the tree a receipt names.
    let original = repo.read("rust-toolchain.toml");
    repo.write(
        "rust-toolchain.toml",
        format!("{original}# uncommitted\n").as_bytes(),
    );
    let run = run_budget(repo, Mode::Passing);
    repo.write("rust-toolchain.toml", original.as_bytes());
    let mut checks = Checks {
        row: "uncommitted tree",
        run: &run,
        failures,
    };
    checks.not_passed();
}

fn missing_cargo(repo: &FixtureRepo, failures: &mut Vec<String>) {
    // No Cargo is unavailable infrastructure, never a pass or a failure.
    let run = run_budget(repo, Mode::NoCargo);
    let mut checks = Checks {
        row: "missing Cargo",
        run: &run,
        failures,
    };
    checks.exit_status(INFRASTRUCTURE_STATUS);
    checks.no_sample_ran();
}
