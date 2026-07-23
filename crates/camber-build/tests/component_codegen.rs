use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CARGO_DEADLINE: Duration = Duration::from_secs(180);
const CHILD_FIXTURE_DEADLINE: Duration = Duration::from_secs(10);
const FORCED_TIMEOUT: Duration = Duration::from_millis(100);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROCESS_REAP_DEADLINE: Duration = Duration::from_secs(5);
const DESCENDANT_MARKER: &str = "CAMBER_DESCENDANT_PID=";
const GENERATED_FILE: &str = "contract.rs";
const TARGET_MARKER: &str = "CAMBER_TARGET_PATH=";
const TIMEOUT_CHILD_ENV: &str = "CAMBER_CODEGEN_TIMEOUT_CHILD";
const UNWIND_CHILD_ENV: &str = "CAMBER_CODEGEN_UNWIND_CHILD";
const WORKSPACE_MARKER: &str = "CAMBER_WORKSPACE_PATH=";
static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const GENERATOR_SOURCE: &str = r#"
use std::env;
use std::io;
use std::path::PathBuf;

fn required_path(name: &'static str) -> io::Result<PathBuf> {
    env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("missing {name}"))
    })
}

fn main() -> io::Result<()> {
    let proto = required_path("CAMBER_PROTO")?;
    let include = required_path("CAMBER_PROTO_INCLUDE")?;
    let descriptor = required_path("CAMBER_DESCRIPTOR_PATH")?;

    camber_build::configure()
        .file_descriptor_set_path(descriptor)
        .compile_protos(&[proto], &[include])
}
"#;

const UNARY_PROTO: &str = r#"
syntax = "proto3";
package contract;

service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
}

message HelloRequest {
  string name = 1;
}

message HelloReply {
  string message = 1;
}
"#;

const UNARY_VERIFIER: &str = r#"
include!(concat!(env!("CAMBER_GENERATED_DIR"), "/contract.rs"));

struct GreetingService;

#[tonic::async_trait]
impl greeter_service::Greeter for GreetingService {
    async fn say_hello(
        &self,
        request: tonic::Request<HelloRequest>,
    ) -> Result<tonic::Response<HelloReply>, tonic::Status> {
        let name = request.into_inner().name;
        Ok(tonic::Response::new(HelloReply {
            message: format!("hello {name}"),
        }))
    }
}

fn requires_tonic_service<T: greeter_server::Greeter>() {}

fn main() {
    requires_tonic_service::<greeter_service::Bridge<GreetingService>>();
    let _: greeter_server::GreeterServer<greeter_service::Bridge<GreetingService>> =
        greeter_service::serve(GreetingService);
}
"#;

const REBUILT_PROTO: &str = r#"
syntax = "proto3";
package contract;

service Current {
  rpc Fetch (CurrentRequest) returns (CurrentReply);
}

message CurrentRequest {
  uint64 id = 1;
}

message CurrentReply {
  uint64 id = 1;
}
"#;

const REBUILT_VERIFIER: &str = r#"
include!(concat!(env!("CAMBER_GENERATED_DIR"), "/contract.rs"));

// This module collides if regeneration leaves the old Greeter bridge behind.
pub mod greeter_service {}

struct CurrentService;

#[tonic::async_trait]
impl current_service::Current for CurrentService {
    async fn fetch(
        &self,
        request: tonic::Request<CurrentRequest>,
    ) -> Result<tonic::Response<CurrentReply>, tonic::Status> {
        Ok(tonic::Response::new(CurrentReply {
            id: request.into_inner().id,
        }))
    }
}

fn requires_tonic_service<T: current_server::Current>() {}

fn main() {
    requires_tonic_service::<current_service::Bridge<CurrentService>>();
    let _: current_server::CurrentServer<current_service::Bridge<CurrentService>> =
        current_service::serve(CurrentService);
}
"#;

const STREAMING_PROTO: &str = r#"
syntax = "proto3";
package contract;

service Streamer {
  rpc Chat (stream StreamRequest) returns (stream StreamReply);
}

message StreamRequest {
  bytes payload = 1;
}

message StreamReply {
  bytes payload = 1;
}
"#;

const STREAMING_VERIFIER: &str = r#"
include!(concat!(env!("CAMBER_GENERATED_DIR"), "/contract.rs"));

fn main() {}
"#;

const INVALID_PROTO: &str = r#"
syntax = "proto3";
package contract;
this is not a protobuf declaration;
"#;

#[test]
fn phase6_codegen_contracts() -> io::Result<()> {
    let started = Instant::now();
    let fixture = Fixture::new()?;

    streaming_rpc_is_rejected_with_context(&fixture)?;
    invalid_proto_returns_an_explicit_error(&fixture)?;
    successful_unary_generation_and_paths(&fixture)?;
    repeated_generation_replaces_bridge_output(&fixture)?;

    let workspace_path = fixture.workspace.path().to_path_buf();
    assert!(
        workspace_path.join("Cargo.lock").is_file(),
        "fixture Cargo.lock was not isolated under {}",
        workspace_path.display()
    );
    assert!(
        fixture.target.is_dir(),
        "fixture target was not isolated under {}",
        workspace_path.display()
    );
    fixture.cleanup()?;
    assert!(
        !workspace_path.exists(),
        "fixture workspace leaked at {}",
        workspace_path.display()
    );

    println!(
        "phase6_codegen_contracts completed in {:?}",
        started.elapsed()
    );
    Ok(())
}

#[test]
fn codegen_child_timeout_reaps_process_tree() -> io::Result<()> {
    let mut child = spawn_test_fixture("codegen_timeout_child_fixture", TIMEOUT_CHILD_ENV)?;
    let output_reader = ChildOutputReader::start(child.take_stdout()?)?;
    let descendant_id = output_reader.read_process_id(DESCENDANT_MARKER)?;

    let outcome = child.wait(FORCED_TIMEOUT)?;
    let output = output_reader.finish()?;
    assert!(
        matches!(outcome, WaitOutcome::TimedOut(_)),
        "timeout child exited before its forced deadline\n{output}"
    );
    assert!(
        !outcome.status().success(),
        "terminated timeout child reported success\n{output}"
    );
    wait_for_process_exit(descendant_id)?;
    Ok(())
}

#[test]
fn codegen_workspace_drop_removes_tree_after_unwind() -> io::Result<()> {
    let mut child = spawn_test_fixture("codegen_unwind_child_fixture", UNWIND_CHILD_ENV)?;
    let output_reader = ChildOutputReader::start(child.take_stdout()?)?;
    let outcome = child.wait(CHILD_FIXTURE_DEADLINE)?;
    let output = output_reader.finish()?;
    assert!(
        matches!(outcome, WaitOutcome::Exited(_)),
        "unwind fixture exceeded its deadline\n{output}"
    );
    assert!(
        !outcome.status().success(),
        "forced assertion unwind unexpectedly succeeded\n{output}"
    );

    let workspace = marker_path(&output, WORKSPACE_MARKER)?;
    let target = marker_path(&output, TARGET_MARKER)?;
    assert!(
        !target.exists(),
        "target tree survived assertion unwind at {}\n{output}",
        target.display()
    );
    assert!(
        !workspace.exists(),
        "workspace tree survived assertion unwind at {}\n{output}",
        workspace.display()
    );
    Ok(())
}

#[test]
fn codegen_timeout_child_fixture() -> io::Result<()> {
    match std::env::var_os(TIMEOUT_CHILD_ENV) {
        Some(_) => run_codegen_timeout_child_fixture(),
        None => codegen_child_timeout_reaps_process_tree(),
    }
}

fn run_codegen_timeout_child_fixture() -> io::Result<()> {
    let mut descendant = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "codegen_parked_descendant_fixture",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove(TIMEOUT_CHILD_ENV)
        .env(TIMEOUT_CHILD_ENV, "descendant")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match descendant.try_wait()? {
        Some(status) => Err(io::Error::other(format!(
            "descendant exited before readiness with {status}"
        ))),
        None => {
            println!("{DESCENDANT_MARKER}{}", descendant.id());
            io::stdout().flush()?;
            let status = descendant.wait()?;
            Err(io::Error::other(format!(
                "parked descendant exited unexpectedly with {status}"
            )))
        }
    }
}

#[test]
fn codegen_parked_descendant_fixture() -> io::Result<()> {
    match std::env::var(TIMEOUT_CHILD_ENV).as_deref() {
        Ok("descendant") => loop {
            std::thread::park();
        },
        Ok(_) | Err(_) => Ok(()),
    }
}

#[test]
fn codegen_unwind_child_fixture() -> io::Result<()> {
    match std::env::var_os(UNWIND_CHILD_ENV) {
        Some(_) => run_codegen_unwind_child_fixture(),
        None => codegen_workspace_drop_removes_tree_after_unwind(),
    }
}

fn run_codegen_unwind_child_fixture() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let proof_file = fixture.target.join("unwind-proof/nested/artifact");
    let proof_parent = proof_file
        .parent()
        .ok_or_else(|| io::Error::other("target proof file has no parent"))?;
    fs::create_dir_all(proof_parent)?;
    fs::write(&proof_file, b"target tree exists before unwind")?;
    println!("{WORKSPACE_MARKER}{}", fixture.workspace.path().display());
    println!("{TARGET_MARKER}{}", fixture.target.display());
    io::stdout().flush()?;
    assert!(
        !proof_file.exists(),
        "forced fixture assertion unwind while target tree exists"
    );
    Ok(())
}

fn spawn_test_fixture(test_name: &str, environment_name: &str) -> io::Result<ChildGuard> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(environment_name, "fixture")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_process_group(&mut command);
    ChildGuard::spawn(command)
}

fn marker_path(output: &str, marker: &str) -> io::Result<PathBuf> {
    output
        .lines()
        .find_map(|line| line.split_once(marker).map(|(_, value)| value.trim()))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other(format!("fixture output omitted {marker}\n{output}")))
}

fn successful_unary_generation_and_paths(fixture: &Fixture) -> io::Result<()> {
    let started = Instant::now();
    fixture.write_proto(UNARY_PROTO)?;
    fixture.write_verifier("verify_unary", UNARY_VERIFIER)?;

    let descriptor = fixture.artifacts.join("unary_descriptor.bin");
    let generation = fixture.generate(&descriptor)?;
    assert!(generation.status.success(), "{generation}");

    let generated = fixture.output.join(GENERATED_FILE);
    assert!(
        generated.is_file(),
        "requested output was not created at {}",
        generated.display()
    );
    assert_nonempty_file(&descriptor, "descriptor set")?;

    let compilation = fixture.check("verify_unary")?;
    assert!(compilation.status.success(), "{compilation}");
    println!(
        "unary generation, paths, signatures, and Tonic compatibility: {:?}",
        started.elapsed()
    );
    Ok(())
}

fn repeated_generation_replaces_bridge_output(fixture: &Fixture) -> io::Result<()> {
    let started = Instant::now();
    fixture.write_proto(REBUILT_PROTO)?;
    fixture.write_verifier("verify_rebuilt", REBUILT_VERIFIER)?;

    let descriptor = fixture.artifacts.join("rebuilt_descriptor.bin");
    let generation = fixture.generate(&descriptor)?;
    assert!(generation.status.success(), "{generation}");
    assert_nonempty_file(&descriptor, "rebuilt descriptor set")?;

    let compilation = fixture.check("verify_rebuilt")?;
    assert!(compilation.status.success(), "{compilation}");
    println!("repeated generation and rebuild: {:?}", started.elapsed());
    Ok(())
}

fn streaming_rpc_is_rejected_with_context(fixture: &Fixture) -> io::Result<()> {
    let started = Instant::now();
    fixture.write_proto(STREAMING_PROTO)?;
    fixture.write_verifier("verify_streaming", STREAMING_VERIFIER)?;

    let descriptor = fixture.artifacts.join("streaming_descriptor.bin");
    let generation = fixture.generate(&descriptor)?;
    assert!(generation.status.success(), "{generation}");

    let compilation = fixture.check("verify_streaming")?;
    println!(
        "Red proof — streaming verifier status: {}",
        compilation.status
    );
    assert!(
        !compilation.status.success(),
        "streaming bridge unexpectedly compiled\n{compilation}"
    );
    assert!(
        compilation.stderr.contains(
            "camber-build: streaming RPCs are not yet supported (method `chat` in service `Streamer`)"
        ),
        "streaming rejection omitted the method/service diagnostic\n{compilation}"
    );
    println!("streaming rejection diagnostic: {:?}", started.elapsed());
    Ok(())
}

fn invalid_proto_returns_an_explicit_error(fixture: &Fixture) -> io::Result<()> {
    let started = Instant::now();
    fixture.write_proto(INVALID_PROTO)?;

    let descriptor = fixture.artifacts.join("invalid_descriptor.bin");
    let generation = fixture.generate(&descriptor)?;
    println!(
        "Red proof — invalid proto generator status: {}",
        generation.status
    );
    assert!(
        !generation.status.success(),
        "invalid proto unexpectedly generated code\n{generation}"
    );
    assert!(
        generation
            .stderr
            .contains(&fixture.proto.display().to_string()),
        "invalid-proto error omitted the input path\n{generation}"
    );
    assert!(
        generation.stderr.contains("Expected top-level statement"),
        "invalid-proto error omitted protoc's parse diagnostic\n{generation}"
    );
    println!("invalid proto error: {:?}", started.elapsed());
    Ok(())
}

fn assert_nonempty_file(path: &Path, description: &str) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    assert!(
        metadata.len() > 0,
        "{description} at {} was empty",
        path.display()
    );
    Ok(())
}

struct Fixture {
    workspace: TempWorkspace,
    proto: PathBuf,
    output: PathBuf,
    artifacts: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new() -> io::Result<Self> {
        let workspace = TempWorkspace::new()?;
        let proto_dir = workspace.path().join("proto");
        let output = workspace.path().join("requested-output");
        let artifacts = workspace.path().join("requested-descriptors");
        let bin_dir = workspace.path().join("src/bin");
        fs::create_dir_all(&proto_dir)?;
        fs::create_dir_all(&output)?;
        fs::create_dir_all(&artifacts)?;
        fs::create_dir_all(&bin_dir)?;

        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = format!(
            r#"[package]
name = "camber-build-codegen-contract"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
camber-build = {{ path = {:?} }}
prost = "=0.14.4"
tonic = {{ version = "=0.14.6", default-features = false, features = ["codegen"] }}
tonic-prost = "=0.14.6"
"#,
            crate_root
        );
        fs::write(workspace.path().join("Cargo.toml"), manifest)?;
        fs::write(bin_dir.join("generate.rs"), GENERATOR_SOURCE)?;

        Ok(Self {
            proto: proto_dir.join("service.proto"),
            output,
            artifacts,
            target: workspace.path().join("isolated-target"),
            workspace,
        })
    }

    fn write_proto(&self, source: &str) -> io::Result<()> {
        fs::write(&self.proto, source)
    }

    fn write_verifier(&self, name: &str, source: &str) -> io::Result<()> {
        fs::write(
            self.workspace.path().join(format!("src/bin/{name}.rs")),
            source,
        )
    }

    fn generate(&self, descriptor: &Path) -> io::Result<CommandOutput> {
        // The isolated fixture crate resolves its own dependencies, which land
        // on newer patch versions than the workspace lockfile pins. `--offline`
        // then fails on any cache that lacks those exact versions — a cold CI
        // runner only has what the main build fetched. Allow network resolution.
        self.cargo(
            &["run", "--quiet", "--bin", "generate"],
            &[
                ("OUT_DIR", self.output.as_path()),
                ("CAMBER_PROTO", self.proto.as_path()),
                (
                    "CAMBER_PROTO_INCLUDE",
                    self.proto.parent().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "proto has no parent")
                    })?,
                ),
                ("CAMBER_DESCRIPTOR_PATH", descriptor),
            ],
        )
    }

    fn check(&self, binary: &str) -> io::Result<CommandOutput> {
        self.cargo(
            &["check", "--quiet", "--bin", binary],
            &[("CAMBER_GENERATED_DIR", self.output.as_path())],
        )
    }

    fn cargo(
        &self,
        arguments: &[&str],
        environment: &[(&str, &Path)],
    ) -> io::Result<CommandOutput> {
        let mut command = Command::new("cargo");
        command
            .args(arguments)
            .current_dir(self.workspace.path())
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null());
        environment.iter().for_each(|(name, value)| {
            command.env(name, value);
        });
        let capture_sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stdout = self
            .workspace
            .path()
            .join(format!("cargo-{capture_sequence}.stdout"));
        let stderr = self
            .workspace
            .path()
            .join(format!("cargo-{capture_sequence}.stderr"));
        run_bounded(command, arguments.join(" "), &stdout, &stderr)
    }

    fn cleanup(mut self) -> io::Result<()> {
        self.workspace.cleanup()
    }
}

struct TempWorkspace {
    path: PathBuf,
    cleaned: bool,
}

impl TempWorkspace {
    fn new() -> io::Result<Self> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "camber-build-phase6-{}-{}-{sequence}",
            std::process::id(),
            elapsed.as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path)?;
        Ok(Self {
            path,
            cleaned: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(&mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if !self.cleaned && self.cleanup().is_err() {
            std::process::abort();
        }
    }
}

struct CommandOutput {
    command: Box<str>,
    status: ExitStatus,
    stdout: Box<str>,
    stderr: Box<str>,
}

impl fmt::Display for CommandOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "command: cargo {}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            self.command, self.status, self.stdout, self.stderr
        )
    }
}

fn run_bounded(
    mut command: Command,
    description: String,
    stdout_path: &Path,
    stderr_path: &Path,
) -> io::Result<CommandOutput> {
    let stdout = fs::File::create(stdout_path)?;
    let stderr = fs::File::create(stderr_path)?;
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    configure_process_group(&mut command);
    let mut child = ChildGuard::spawn(command)?;
    let outcome = child.wait(CARGO_DEADLINE)?;
    let result = CommandOutput {
        command: description.into_boxed_str(),
        status: outcome.status(),
        stdout: String::from_utf8_lossy(&fs::read(stdout_path)?)
            .into_owned()
            .into_boxed_str(),
        stderr: String::from_utf8_lossy(&fs::read(stderr_path)?)
            .into_owned()
            .into_boxed_str(),
    };
    match outcome {
        WaitOutcome::Exited(_) => Ok(result),
        WaitOutcome::TimedOut(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Cargo exceeded {CARGO_DEADLINE:?}\n{result}"),
        )),
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(mut command: Command) -> io::Result<Self> {
        command.stdin(Stdio::null());
        let child = command.spawn()?;
        Ok(Self { child: Some(child) })
    }

    fn take_stdout(&mut self) -> io::Result<ChildStdout> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::other("child is not owned"))?
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout was not piped"))
    }

    fn wait(&mut self, timeout: Duration) -> io::Result<WaitOutcome> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::other("Cargo deadline overflow"))?;
        loop {
            let status = self
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("Cargo child is not owned"))?
                .try_wait()?;
            match status {
                Some(status) => {
                    self.child.take();
                    return Ok(WaitOutcome::Exited(status));
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(PROCESS_POLL_INTERVAL);
                }
                None => return self.terminate().map(WaitOutcome::TimedOut),
            }
        }
    }

    fn terminate(&mut self) -> io::Result<ExitStatus> {
        let process_id = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("Cargo child is not owned"))?
            .id();
        match terminate_process_tree(process_id) {
            Ok(()) => self.reap(),
            Err(tree_error) => self.terminate_child_after_tree_failure(tree_error),
        }
    }

    fn terminate_child_after_tree_failure(
        &mut self,
        tree_error: io::Error,
    ) -> io::Result<ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("Cargo child is not owned"))?;
        let process_id = child.id();
        match child.try_wait()? {
            Some(status) => {
                self.child.take();
                Ok(status)
            }
            None => {
                child.kill().map_err(|kill_error| {
                    io::Error::other(format!(
                        "process-tree termination failed: {tree_error}; child termination failed: {kill_error}"
                    ))
                })?;
                let status = self.reap()?;
                Err(io::Error::other(format!(
                    "process-tree termination failed before child {process_id} was reaped with {status}: {tree_error}"
                )))
            }
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now()
            .checked_add(PROCESS_REAP_DEADLINE)
            .ok_or_else(|| io::Error::other("Cargo reap deadline overflow"))?;
        loop {
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("Cargo child is not owned"))?;
            match child.try_wait()? {
                Some(status) => {
                    self.child.take();
                    return Ok(status);
                }
                None if Instant::now() < deadline => {
                    std::thread::sleep(PROCESS_POLL_INTERVAL);
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("Cargo child {} was not reaped", child.id()),
                    ));
                }
            }
        }
    }
}

struct ChildOutputReader {
    process_ids: Receiver<u32>,
    reader: JoinHandle<io::Result<Box<str>>>,
}

impl ChildOutputReader {
    fn start(stdout: ChildStdout) -> io::Result<Self> {
        let (process_id_sender, process_ids) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("codegen-child-output".to_owned())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let mut output = String::new();
                loop {
                    let mut line = String::new();
                    match stdout.read_line(&mut line)? {
                        0 => return Ok(output.into_boxed_str()),
                        _ => {
                            forward_process_id(&process_id_sender, process_id_from_line(&line)?)?;
                            output.push_str(&line);
                        }
                    }
                }
            })?;
        Ok(Self {
            process_ids,
            reader,
        })
    }

    fn read_process_id(&self, marker: &str) -> io::Result<u32> {
        self.process_ids
            .recv_timeout(CHILD_FIXTURE_DEADLINE)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("child fixture did not emit {marker}"),
                ),
                RecvTimeoutError::Disconnected => io::Error::other(format!(
                    "child fixture closed stdout before emitting {marker}"
                )),
            })
    }

    fn finish(self) -> io::Result<Box<str>> {
        self.reader
            .join()
            .map_err(|_| io::Error::other("child stdout reader panicked"))?
    }
}

fn forward_process_id(sender: &mpsc::Sender<u32>, process_id: Option<u32>) -> io::Result<()> {
    match process_id {
        Some(process_id) => sender
            .send(process_id)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process-id receiver closed")),
        None => Ok(()),
    }
}

fn process_id_from_line(line: &str) -> io::Result<Option<u32>> {
    line.split_once(DESCENDANT_MARKER)
        .map(|(_, value)| {
            value.trim().parse().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid descendant process id: {error}"),
                )
            })
        })
        .transpose()
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() && self.terminate().is_err() {
            std::process::abort();
        }
    }
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut(ExitStatus),
}

impl WaitOutcome {
    fn status(&self) -> ExitStatus {
        match self {
            Self::Exited(status) | Self::TimedOut(status) => *status,
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_: &mut Command) {}

fn wait_for_process_exit(process_id: u32) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(PROCESS_REAP_DEADLINE)
        .ok_or_else(|| io::Error::other("descendant reap deadline overflow"))?;
    loop {
        match process_is_running(process_id)? {
            false => return Ok(()),
            true if Instant::now() < deadline => std::thread::sleep(PROCESS_POLL_INTERVAL),
            true => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("descendant process {process_id} survived tree termination"),
                ));
            }
        }
    }
}

#[cfg(unix)]
fn process_is_running(process_id: u32) -> io::Result<bool> {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(process_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
}

#[cfg(windows)]
fn process_is_running(process_id: u32) -> io::Result<bool> {
    let filter = format!("PID eq {process_id}");
    let output = Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()?;
    match output.status.success() {
        true => {
            let expected = process_id.to_string();
            Ok(String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split(',').nth(1))
                .any(|value| value.trim_matches('"') == expected))
        }
        false => Err(io::Error::other(format!(
            "tasklist failed while checking child {process_id}: {}",
            output.status
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_running(process_id: u32) -> io::Result<bool> {
    Err(io::Error::other(format!(
        "process checks are unsupported for child {process_id}"
    )))
}

#[cfg(unix)]
fn terminate_process_tree(process_id: u32) -> io::Result<()> {
    // `--` terminates option parsing so the leading-dash process-group operand
    // is not read as a flag. Without it, util-linux `kill` still delivers the
    // signal but exits non-zero, which BSD `kill` does not; the spurious error
    // then propagates into ChildGuard::drop and aborts the whole test binary.
    let status = Command::new("/bin/kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{process_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(io::Error::other(format!(
            "failed to terminate process group {process_id}: {status}"
        ))),
    }
}

#[cfg(windows)]
fn terminate_process_tree(process_id: u32) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &process_id.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(io::Error::other(format!(
            "failed to terminate process tree {process_id}: {status}"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(process_id: u32) -> io::Result<()> {
    Err(io::Error::other(format!(
        "process-tree termination is unsupported for child {process_id}"
    )))
}
