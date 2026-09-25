//! Delivery-input admission through the repository's shipped hooks.
//!
//! Each row runs a real hook process in an owned temporary repository. Only
//! external executables are controlled; the admission, identity, and phase
//! logic under test is the tracked script itself.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::delivery_fixture::{
    FixtureRepo, HookRun, Identity, PedantVerify, Tool, ToolBin, ambient_executable,
    assert_absent_from_system_path, isolated_command, logged_text, phase_hook, repository_file,
    repository_root, run_bounded,
};

const SUPPLY_CHAIN_HOOK: &str = ".github/scripts/check-supply-chain.sh";
const WORKFLOW_RUNNER: &str = ".github/scripts/reproduce-ci.sh";
const INPUT_RECORD: &str = ".github/supply-chain-inputs.sha256";
const LOCKFILE: &str = "Cargo.lock";
const ROOT_MANIFEST: &str = "Cargo.toml";
const MEMBER_MANIFEST: &str = "crates/demo/Cargo.toml";
const FIXTURE_MANIFEST: &str = "crates/demo/fixtures/probe/Cargo.toml";
const UNRECORDED_MANIFEST: &str = "crates/extra/Cargo.toml";
const RECORDED_INPUTS: [&str; 4] = [LOCKFILE, ROOT_MANIFEST, MEMBER_MANIFEST, FIXTURE_MANIFEST];
const REVIEWED_DEPENDENCY: &str = "\n[dependencies]\nhttpdate = \"1\"\n";
/// The status every hook reserves for unavailable infrastructure, never for a
/// policy answer about the tree.
const INFRASTRUCTURE_STATUS: i32 = 75;
const WEBPKI_EXCEPTION: &str = "error: rustls-webpki: target webpki has no complete source closure: [src/lib.rs declares mod test_utils] src/lib.rs declares mod test_utils: no source exists for the declared module (attempted src/test_utils.rs)";

const WORKFLOW_PHASES: [&str; 9] = [
    "hook-contract",
    "fmt",
    "clippy",
    "doc",
    "test",
    "deny",
    "pedant-source",
    "pedant-tests",
    "supply-chain",
];
/// Tool records the runner reads from its checkout.
const WORKFLOW_TOOL_RECORDS: [&str; 2] = ["rust-toolchain.toml", ".github/workflow-tools.toml"];

// --- Supply-chain input admission ---------------------------------------

/// A repository holding the shipped supply-chain hook and its inputs.
struct SupplyChainRepo {
    repo: FixtureRepo,
    base: Box<str>,
}

impl SupplyChainRepo {
    /// The reviewed baseline: current locked bytes, manifests, exact record.
    fn baseline() -> Self {
        let repo = FixtureRepo::new();
        copy_repository_script(&repo, SUPPLY_CHAIN_HOOK);
        repo.write(LOCKFILE, &repository_file(LOCKFILE));
        repo.write(
            ROOT_MANIFEST,
            b"[workspace]\nmembers = [\"crates/demo\"]\nresolver = \"3\"\n",
        );
        repo.write(MEMBER_MANIFEST, &package_manifest("demo"));
        repo.write(
            FIXTURE_MANIFEST,
            &[&package_manifest("probe")[..], b"\n[workspace]\n"].concat(),
        );
        write_exact_record(&repo);
        let base = repo.commit("baseline dependency inputs");
        Self { repo, base }
    }

    /// A reviewed manifest change committed with its exact record.
    fn reviewed() -> Self {
        let fixture = Self::baseline();
        fixture.append(MEMBER_MANIFEST, REVIEWED_DEPENDENCY);
        write_exact_record(&fixture.repo);
        fixture.repo.commit("reviewed dependency change");
        fixture
    }

    fn append(&self, relative: &str, text: &str) {
        let mut contents = self.repo.read(relative);
        contents.push_str(text);
        self.repo.write(relative, contents.as_bytes());
    }

    fn rewrite_record(&self, rewrite: impl FnOnce(&str) -> String) {
        let record = self.repo.read(INPUT_RECORD);
        self.repo.write(INPUT_RECORD, rewrite(&record).as_bytes());
    }

    /// Run the shipped hook once over `tools`, with the reviewed base still
    /// exported, and report how many verifications it made.
    ///
    /// Takes the tools so every row closes them the same way.
    fn run(&self, tools: ToolBin) -> (HookRun, usize) {
        let mut command = self.repo.command(&ambient_executable("bash"), &tools);
        command
            .arg(self.repo.path().join(SUPPLY_CHAIN_HOOK))
            .env("CAMBER_DEPENDENCY_BASE_SHA", &*self.base);
        let run = run_bounded(command);
        let calls = tools.verify_calls();
        tools.close();
        (run, calls)
    }

    fn close(self) {
        self.repo.close();
    }
}

/// Write the exact record: sorted paths and SHA-256 of their contents.
fn write_exact_record(repo: &FixtureRepo) {
    let mut sorted = RECORDED_INPUTS;
    sorted.sort_unstable();
    let mut command = repo.system_command(Path::new("shasum"));
    command.args(["-a", "256", "--"]).args(sorted);
    let run = run_bounded(command);
    assert_eq!(
        run.status, 0,
        "fixture record was not hashed:\n{}",
        run.output
    );
    repo.write(INPUT_RECORD, run.output.as_bytes());
}

/// Copy a tracked script into the fixture at the same path, executable.
fn copy_repository_script(repo: &FixtureRepo, relative: &str) {
    repo.write_executable(relative, repository_file(relative));
}

fn package_manifest(name: &str) -> Box<[u8]> {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
        .into_bytes()
        .into_boxed_slice()
}

fn replace_first_hash(record: &str, hash: &str) -> String {
    let (_, rest) = record
        .split_once("  ")
        .expect("record lines carry a hash and a path");
    format!("{hash}  {rest}")
}

/// One record rewrite: the reviewed record in, the rewritten record out.
type RecordRewrite = fn(&str) -> String;

/// Record rewrites that no longer state the exact input inventory.
fn malformed_records() -> [(&'static str, RecordRewrite); 6] {
    [
        ("non-hex hash", |record| {
            replace_first_hash(record, &"g".repeat(64))
        }),
        ("truncated hash", |record| {
            let (hash, _) = record.split_once("  ").expect("record lines carry a hash");
            replace_first_hash(record, &hash[1..])
        }),
        ("line without a path", |record| {
            let (first, rest) = record.split_once('\n').expect("record has several lines");
            let (hash, _) = first.split_once("  ").expect("record lines carry a hash");
            format!("{hash}\n{rest}")
        }),
        ("duplicate entry", |record| {
            let first = record.lines().next().expect("record has a first line");
            format!("{record}{first}\n")
        }),
        ("unsorted entries", |record| {
            let mut lines: Vec<&str> = record.lines().collect();
            lines.swap(0, 1);
            let mut swapped = lines.join("\n");
            swapped.push('\n');
            swapped
        }),
        ("empty record", |_| String::new()),
    ]
}

/// Commit what `case` changed, and require the hook to refuse it as a policy
/// answer before verification runs.
fn assert_committed_inputs_refused(case: &str, fixture: SupplyChainRepo) {
    fixture.repo.commit(case);
    let (run, calls) = fixture.run(ToolBin::pinned());
    assert!(
        run.status != 0 && run.status != INFRASTRUCTURE_STATUS,
        "unreviewed dependency inputs were admitted ({case}): exit {}\n{}",
        run.status,
        run.output
    );
    assert_eq!(
        calls, 0,
        "supply-chain verification ran before input admission refused ({case})"
    );
    fixture.close();
}

/// Require the hook to admit `case` and to have verified it exactly once.
fn assert_admitted_and_verified(run: &HookRun, calls: usize, case: &str) {
    assert_eq!(
        run.status, 0,
        "{case} was rejected: exit {}\n{}",
        run.status, run.output
    );
    assert_eq!(calls, 1, "{case} did not reach supply-chain verification");
}

fn verify_on_reviewed_inputs(verify: PedantVerify<'_>) -> (HookRun, usize) {
    let fixture = SupplyChainRepo::reviewed();
    let outcome = fixture.run(ToolBin::new(|_| Identity::Pinned, verify));
    fixture.close();
    outcome
}

/// A reviewed tree, and a tree nothing dependency-shaped changed in, are both
/// admitted and both reach verification.
fn assert_recorded_trees_are_admitted() {
    let (reviewed, calls) = verify_on_reviewed_inputs(PedantVerify::CLEAN);
    assert_admitted_and_verified(&reviewed, calls, "reviewed dependency inputs");

    let unchanged = SupplyChainRepo::baseline();
    unchanged.repo.write("README.md", b"unrelated change\n");
    unchanged.repo.commit("unrelated change");
    let (run, calls) = unchanged.run(ToolBin::pinned());
    assert_admitted_and_verified(&run, calls, "an unchanged exact record");
    unchanged.close();
}

/// Every tree whose inputs its record no longer states is refused before
/// verification runs.
fn assert_unrecorded_trees_are_refused() {
    let changed = SupplyChainRepo::reviewed();
    changed.append(MEMBER_MANIFEST, "serde = \"1\"\n");
    assert_committed_inputs_refused("manifest content changed after its record", changed);

    let added = SupplyChainRepo::reviewed();
    added
        .repo
        .write(UNRECORDED_MANIFEST, &package_manifest("extra"));
    assert_committed_inputs_refused("manifest added outside the record", added);

    let removed = SupplyChainRepo::reviewed();
    removed.repo.remove(FIXTURE_MANIFEST);
    assert_committed_inputs_refused("recorded manifest removed", removed);

    let relocked = SupplyChainRepo::reviewed();
    relocked.append(LOCKFILE, "\n# unreviewed lock change\n");
    assert_committed_inputs_refused("lockfile changed after its record", relocked);

    let omitted = SupplyChainRepo::reviewed();
    omitted.repo.remove(INPUT_RECORD);
    assert_committed_inputs_refused("input record omitted", omitted);

    for (case, rewrite) in malformed_records() {
        let malformed = SupplyChainRepo::reviewed();
        malformed.rewrite_record(rewrite);
        assert_committed_inputs_refused(case, malformed);
    }

    let stale = SupplyChainRepo::baseline();
    stale.append(MEMBER_MANIFEST, REVIEWED_DEPENDENCY);
    assert_committed_inputs_refused("stale record from the reviewed base", stale);
}

/// The one accepted diagnostic is admitted, and reported as the incomplete run
/// it is rather than as a comparison that passed.
fn assert_accepted_exception_is_reported_as_incomplete() {
    let (accepted, calls) = verify_on_reviewed_inputs(PedantVerify {
        output: WEBPKI_EXCEPTION,
        status: 2,
    });
    assert_admitted_and_verified(&accepted, calls, "the exact accepted WebPKI diagnostic");
    assert!(
        accepted.output.contains(WEBPKI_EXCEPTION),
        "accepted-exception output dropped the diagnostic:\n{}",
        accepted.output
    );
    let reported = accepted.output.to_ascii_lowercase();
    assert!(
        reported.contains("accepted") && reported.contains("incomplete"),
        "accepted-exception output does not identify a policy-accepted incomplete run:\n{}",
        accepted.output
    );
    assert!(
        !reported.contains("passed") && !reported.contains("succeeded"),
        "accepted-exception output claims a completed baseline comparison:\n{}",
        accepted.output
    );
}

/// Every verification result beside that exact diagnostic still fails.
fn assert_other_verification_results_fail() {
    let changed_exception = WEBPKI_EXCEPTION.replace("src/test_utils.rs", "src/test_utils/mod.rs");
    let appended_error =
        format!("{WEBPKI_EXCEPTION}\nerror: serde: baseline source closure drifted");
    let refused_verifications = [
        ("changed WebPKI text", changed_exception.as_str(), 2),
        ("appended error", appended_error.as_str(), 2),
        (
            "unrelated error",
            "error: ring: undeclared build script capability",
            2,
        ),
        (
            "unrelated finding",
            "finding: serde_json gained a network capability",
            1,
        ),
    ];
    for (case, output, status) in refused_verifications {
        let (run, calls) = verify_on_reviewed_inputs(PedantVerify { output, status });
        assert_ne!(
            run.status, 0,
            "supply-chain verification failure was admitted ({case}):\n{}",
            run.output
        );
        assert_eq!(
            calls, 1,
            "verification did not run on admitted inputs ({case})"
        );
    }
}

/// A verifier that is not there at all is unavailable infrastructure, not a
/// policy answer about the tree.
fn assert_missing_verifier_is_infrastructure() {
    let fixture = SupplyChainRepo::reviewed();
    let (run, _) = fixture.run(ToolBin::new(
        only(Tool::Pedant, Identity::Missing),
        PedantVerify::CLEAN,
    ));
    assert_eq!(
        run.status, INFRASTRUCTURE_STATUS,
        "a missing Pedant was not an infrastructure failure: exit {}\n{}",
        run.status, run.output
    );
    fixture.close();
}

#[test]
fn reviewed_dependency_inputs_accept_only_the_recorded_tree() {
    assert_absent_from_system_path("pedant");

    assert_recorded_trees_are_admitted();
    assert_unrecorded_trees_are_refused();
    assert_accepted_exception_is_reported_as_incomplete();
    assert_other_verification_results_fail();
    assert_missing_verifier_is_infrastructure();
}

// --- Workflow tool identity ---------------------------------------------

/// One `require_workflow_tools` run over a controlled tool directory.
struct ToolAdmission {
    run: HookRun,
    /// The directory the fixture's executables resolved from.
    bin: PathBuf,
    /// How many absent toolchains or components a rustup proxy installed.
    installs: usize,
}

fn run_tool_admission(identity: impl Fn(Tool) -> Identity) -> ToolAdmission {
    let tools = ToolBin::new(identity, PedantVerify::CLEAN);
    let root = repository_root();
    let command = isolated_command(
        &ambient_executable("bash"),
        &root,
        &tools.home(),
        &tools.search_path(),
    );
    let run = runner_library_call(
        command,
        r#"require_workflow_tools "$2""#,
        "workflow-tool-admission",
        &root.join(WORKFLOW_RUNNER),
        Some(root.as_path()),
    );
    let admission = ToolAdmission {
        run,
        bin: tools.bin(),
        installs: tools.install_calls(),
    };
    tools.close();
    admission
}

/// Source the shipped `runner` in library mode, then run `call` in that shell.
///
/// `label` is the shell's `$0`, `runner` its `$1`, and `argument`, when given,
/// its `$2`.
fn runner_library_call(
    mut command: Command,
    call: &str,
    label: &str,
    runner: &Path,
    argument: Option<&Path>,
) -> HookRun {
    command
        .arg("-c")
        .arg(format!("CAMBER_HOOK_LIBRARY_MODE=1 source \"$1\"\n{call}"))
        .arg(label)
        .arg(runner)
        .args(argument);
    run_bounded(command)
}

fn only(tool: Tool, identity: Identity) -> impl Fn(Tool) -> Identity {
    move |candidate| match candidate == tool {
        true => identity,
        false => Identity::Pinned,
    }
}

#[test]
fn workflow_tools_reject_unapproved_identity() {
    for tool in Tool::ALL {
        tool.executables()
            .iter()
            .for_each(|executable| assert_absent_from_system_path(executable));
    }

    let pinned = run_tool_admission(|_| Identity::Pinned);
    assert_eq!(
        pinned.run.status, 0,
        "pinned workflow tool identities were refused: exit {}\n{}",
        pinned.run.status, pinned.run.output
    );
    for tool in Tool::ALL {
        let evidence = format!(
            "Workflow tool {}: {} [{}]",
            tool.names()[0],
            tool.pinned().0,
            pinned.bin.join(tool.executables()[0]).display()
        );
        assert!(
            pinned.run.output.contains(&evidence),
            "admission evidence omits the resolved path and version of {tool:?}: \
             expected {evidence:?}\n{}",
            pinned.run.output
        );
    }

    for tool in Tool::ALL {
        for identity in [Identity::Mismatched, Identity::Unreadable] {
            let admission = run_tool_admission(only(tool, identity));
            assert_ne!(
                admission.run.status, 0,
                "unapproved workflow tool identity was admitted: {tool:?} {identity:?}\n{}",
                admission.run.output
            );
            assert!(
                tool.names()
                    .iter()
                    .any(|name| admission.run.output.contains(name)),
                "refused {tool:?} {identity:?} identity without naming the tool:\n{}",
                admission.run.output
            );
        }
    }

    for tool in Tool::ALL {
        let admission = run_tool_admission(only(tool, Identity::Missing));
        assert_eq!(
            admission.run.status, INFRASTRUCTURE_STATUS,
            "missing workflow tool {tool:?} was not an infrastructure failure: exit {}\n{}",
            admission.run.status, admission.run.output
        );
    }

    for tool in Tool::RUSTUP_MANAGED {
        let admission = run_tool_admission(only(tool, Identity::Uninstalled));
        assert_eq!(
            admission.installs, 0,
            "workflow tool admission installed an absent {tool:?}:\n{}",
            admission.run.output
        );
        assert_eq!(
            admission.run.status, INFRASTRUCTURE_STATUS,
            "uninstalled workflow tool {tool:?} was not an infrastructure failure: exit {}\n{}",
            admission.run.status, admission.run.output
        );
    }
}

// --- Workflow failure propagation ---------------------------------------

/// A committed checkout holding the shipped runner and phase-recording hooks.
struct WorkflowRepo {
    repo: FixtureRepo,
    tools: ToolBin,
}

/// Whether the scratch checkout's removal succeeds.
#[derive(Clone, Copy, Debug)]
enum Cleanup {
    Succeeds,
    Fails,
}

impl WorkflowRepo {
    fn new() -> Self {
        let repo = FixtureRepo::new();
        copy_repository_script(&repo, WORKFLOW_RUNNER);
        repo.write_executable(
            ".github/scripts/ci-selftest.sh",
            phase_hook("hook-contract"),
        );
        repo.write_executable(
            ".github/scripts/check-pedant.sh",
            phase_hook(r#""pedant-$1""#),
        );
        repo.write_executable(
            ".github/scripts/check-supply-chain.sh",
            phase_hook("supply-chain"),
        );
        WORKFLOW_TOOL_RECORDS
            .iter()
            .for_each(|record| repo.write(record, &repository_file(record)));
        repo.write("README.md", b"workflow fixture\n");
        repo.commit("workflow fixture");
        Self {
            repo,
            tools: ToolBin::pinned(),
        }
    }

    fn log(&self) -> PathBuf {
        self.repo.outside("phases.log")
    }

    fn command(&self, failure: Option<(&str, i32)>, cleanup: Cleanup) -> Command {
        match fs::remove_file(self.log()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("phase log was not reset: {error}"),
        }
        let mut command = self.repo.command(&ambient_executable("bash"), &self.tools);
        command
            .env("CAMBER_FIXTURE_LOG", self.log())
            .env("CARGO_TARGET_DIR", self.repo.outside("target"));
        if let Some((phase, status)) = failure {
            command
                .env("CAMBER_FIXTURE_FAIL_PHASE", phase)
                .env("CAMBER_FIXTURE_FAIL_STATUS", status.to_string());
        }
        if let Cleanup::Fails = cleanup {
            command.env("CAMBER_FIXTURE_FAIL_CLEANUP", "1");
        }
        command
    }

    /// `run_workflow_checks` under the conditional status capture main uses.
    fn run_checks(&self, failure: Option<(&str, i32)>) -> HookRun {
        let repo = self.repo.path();
        runner_library_call(
            self.command(failure, Cleanup::Succeeds),
            r#"status=0
run_workflow_checks "$2" || status=$?
exit "${status}""#,
            "workflow-checks",
            &repo.join(WORKFLOW_RUNNER),
            Some(repo.as_path()),
        )
    }

    /// The runner's real main, from a clean committed tree.
    fn run_main(&self, failure: Option<(&str, i32)>, cleanup: Cleanup) -> HookRun {
        let mut command = self.command(failure, cleanup);
        command.arg(self.repo.path().join(WORKFLOW_RUNNER));
        run_bounded(command)
    }

    /// The phases the last run recorded; no log means no phase ran.
    fn phases_run(&self) -> Box<[String]> {
        logged_text(&self.log())
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn inventory(&self) -> Box<[String]> {
        let run = runner_library_call(
            self.repo.command(&ambient_executable("bash"), &self.tools),
            "camber_workflow_checks",
            "workflow-inventory",
            &self.repo.path().join(WORKFLOW_RUNNER),
            None,
        );
        assert_eq!(
            run.status, 0,
            "workflow inventory was unreadable:\n{}",
            run.output
        );
        run.output.lines().map(str::to_owned).collect()
    }

    fn assert_scratch_removed(&self, case: &str) {
        let leftover: Box<[_]> = fs::read_dir(self.repo.scratch())
            .expect("scratch directory is readable")
            .map(|entry| entry.expect("scratch entry is readable").path())
            .collect();
        assert!(
            leftover.is_empty(),
            "workflow scratch roots survived ({case}): {leftover:?}"
        );
        let worktrees = self.repo.git(&["worktree", "list", "--porcelain"]).output;
        assert_eq!(
            worktrees
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1,
            "workflow scratch checkout survived ({case}):\n{worktrees}"
        );
    }

    /// Clear what a refused cleanup left behind, so the roots close.
    fn reclaim_scratch(&self) {
        for entry in fs::read_dir(self.repo.scratch()).expect("scratch directory is readable") {
            let path = entry.expect("scratch entry is readable").path();
            fs::remove_dir_all(&path).expect("leftover scratch root was not removed");
        }
        self.repo.git(&["worktree", "prune"]);
    }

    fn close(self) {
        self.tools.close();
        self.repo.close();
    }
}

/// Require one output line naming the failed phase and its status as tokens.
fn assert_names_failure(run: &HookRun, phase: &str, status: i32) {
    let status = status.to_string();
    let named = run.output.lines().any(|line| {
        let mut tokens =
            line.split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'));
        tokens.clone().any(|token| token == phase) && tokens.any(|token| token == status)
    });
    assert!(
        named,
        "workflow evidence does not name failed phase {phase} with status {status}:\n{}",
        run.output
    );
}

/// Require `run` to name the phase at `index` with `status`, and `runner` to
/// have run no phase behind it.
fn assert_failure_stopped(
    workflow: &WorkflowRepo,
    run: &HookRun,
    index: usize,
    status: i32,
    runner: &str,
) {
    let phase = WORKFLOW_PHASES[index];
    assert_names_failure(run, phase, status);
    assert_eq!(
        workflow.phases_run()[..],
        WORKFLOW_PHASES[..=index],
        "{runner} ran phases after {phase} failed with {status}"
    );
}

/// The fixture's table is the runner's own inventory, and a run nothing fails
/// in visits every phase in it once.
fn assert_every_phase_runs_once(workflow: &WorkflowRepo) {
    assert_eq!(
        workflow.inventory()[..],
        WORKFLOW_PHASES[..],
        "the fixture's phase table drifted from the runner's inventory"
    );

    let success = workflow.run_checks(None);
    assert_eq!(
        success.status, 0,
        "all-success workflow checks failed: exit {}\n{}",
        success.status, success.output
    );
    assert_eq!(
        workflow.phases_run()[..],
        WORKFLOW_PHASES[..],
        "all-success workflow did not run every phase once in order"
    );
}

/// Each phase's failure reaches the caller of `run_workflow_checks`, with no
/// later phase running behind it.
fn assert_each_failed_check_survives(workflow: &WorkflowRepo) {
    for (index, phase) in WORKFLOW_PHASES.into_iter().enumerate() {
        for status in [42, INFRASTRUCTURE_STATUS] {
            let run = workflow.run_checks(Some((phase, status)));
            assert_eq!(
                run.status, status,
                "workflow check failure was erased: {phase} exited {status}, runner returned {}\n{}",
                run.status, run.output
            );
            assert_failure_stopped(workflow, &run, index, status, "workflow checks");
        }
    }
}

/// The phases whose failure the runner's own main is driven through, each
/// behind a cleanup that succeeds.
const MAIN_FAILURES: [(&str, i32); 2] = [("test", 42), ("clippy", INFRASTRUCTURE_STATUS)];

/// One failed phase under the runner's real main, in its own checkout.
fn assert_main_preserves_failure(phase: &str, status: i32) {
    let workflow = WorkflowRepo::new();
    let run = workflow.run_main(Some((phase, status)), Cleanup::Succeeds);
    assert_eq!(
        run.status, status,
        "workflow check failure was erased by the runner's main: {phase} exited {status}, main returned {}\n{}",
        run.status, run.output
    );
    let index = WORKFLOW_PHASES
        .iter()
        .position(|candidate| *candidate == phase)
        .expect("phase is in the inventory");
    assert_failure_stopped(&workflow, &run, index, status, "the runner's main");
    workflow.assert_scratch_removed(phase);
    workflow.close();
}

/// A refused cleanup cannot replace the check failure that came before it.
fn assert_refused_cleanup_keeps_the_check_failure() {
    let workflow = WorkflowRepo::new();
    let run = workflow.run_main(Some(("fmt", 42)), Cleanup::Fails);
    assert_eq!(
        run.status, 42,
        "a cleanup failure replaced the fmt failure: main returned {}\n{}",
        run.status, run.output
    );
    workflow.reclaim_scratch();
    workflow.assert_scratch_removed("fmt failure with refused cleanup");
    workflow.close();
}

/// A refused cleanup behind successful checks is still a failed run.
fn assert_refused_cleanup_after_success_fails() {
    let workflow = WorkflowRepo::new();
    let run = workflow.run_main(None, Cleanup::Fails);
    assert_ne!(
        run.status, 0,
        "a cleanup failure after successful checks was reported as success:\n{}",
        run.output
    );
    assert_eq!(
        workflow.phases_run()[..],
        WORKFLOW_PHASES[..],
        "checks before the refused cleanup did not all run"
    );
    workflow.reclaim_scratch();
    workflow.assert_scratch_removed("refused cleanup after success");
    workflow.close();
}

/// The whole main, with nothing failing, runs every phase and leaves nothing.
fn assert_successful_main_runs_every_phase() {
    let workflow = WorkflowRepo::new();
    let run = workflow.run_main(None, Cleanup::Succeeds);
    assert_eq!(
        run.status, 0,
        "an all-success workflow main failed: exit {}\n{}",
        run.status, run.output
    );
    assert_eq!(
        workflow.phases_run()[..],
        WORKFLOW_PHASES[..],
        "the runner's main did not run every phase once in order"
    );
    workflow.assert_scratch_removed("all-success main");
    workflow.close();
}

#[test]
fn workflow_runner_preserves_every_failed_check() {
    let workflow = WorkflowRepo::new();
    assert_every_phase_runs_once(&workflow);
    assert_each_failed_check_survives(&workflow);
    workflow.close();

    for (phase, status) in MAIN_FAILURES {
        assert_main_preserves_failure(phase, status);
    }
    assert_refused_cleanup_keeps_the_check_failure();
    assert_refused_cleanup_after_success_fails();
    assert_successful_main_runs_every_phase();
}
