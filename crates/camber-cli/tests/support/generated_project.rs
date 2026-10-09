//! Shared setup for the Cargo projects these tests generate.
//!
//! Every generated project builds against the local workspace crates and the
//! inherited `CARGO_TARGET_DIR`, so a compiler failure names this tree's code
//! and a warm cache stays warm.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::process::{CommandOutput, camber_bin};
use crate::support::{FixtureError, run_command};

/// The workspace root, two levels above this crate's manifest.
pub fn workspace_root() -> Result<&'static Path, FixtureError> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no workspace root"))
}

/// The local checkout of the workspace crate `name`.
pub fn workspace_crate_path(name: &str) -> Result<PathBuf, FixtureError> {
    Ok(workspace_root()?.join("crates").join(name))
}

/// Pin a generated project to the dependency versions this tree builds with.
pub fn pin_workspace_lock(project_dir: &Path) -> Result<(), FixtureError> {
    std::fs::copy(
        workspace_root()?.join("Cargo.lock"),
        project_dir.join("Cargo.lock"),
    )?;
    Ok(())
}

/// Point a generated project's crates.io requirements at the local crates,
/// and pin it to the workspace lock.
pub fn patch_local_crates(project_dir: &Path) -> Result<(), FixtureError> {
    pin_workspace_lock(project_dir)?;
    let config_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&config_dir)?;
    let patch = format!(
        "[patch.crates-io]\ncamber = {{ path = \"{}\" }}\ncamber-build = {{ path = \"{}\" }}\n",
        workspace_crate_path("camber")?.display(),
        workspace_crate_path("camber-build")?.display(),
    );
    std::fs::write(config_dir.join("config.toml"), patch)?;
    Ok(())
}

/// Run `row` in a fresh temporary directory, then close the directory. A
/// failed close fails the row.
pub fn in_temp_dir<R>(row: R) -> Result<(), FixtureError>
where
    R: FnOnce(&Path) -> Result<(), FixtureError>,
{
    let dir = tempfile::tempdir()?;
    let observed = row(dir.path());
    FixtureError::with_cleanup(observed, dir.close().map_err(FixtureError::from))
}

/// Run `command` and return its output, or fail with its stderr unless it
/// succeeds. `context` names the command in the failure.
pub fn command_succeeds(command: Command, context: &str) -> Result<CommandOutput, FixtureError> {
    let output = run_command(command)?;
    match output.status.success() {
        true => Ok(output),
        false => Err(FixtureError::new(format!(
            "{context} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

/// `camber <args>`, ready to run in `dir`.
pub fn camber_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(camber_bin());
    command.args(args).current_dir(dir);
    command
}

/// Run `camber new <name> --template <template>` in `dir`.
pub fn camber_new(dir: &Path, name: &str, template: &str) -> Result<(), FixtureError> {
    let command = camber_command(dir, &["new", name, "--template", template]);
    command_succeeds(command, &format!("camber new --template {template}")).map(|_| ())
}

/// Run `camber context` in `dir`, which writes `llms.txt` there.
pub fn camber_context(dir: &Path) -> Result<(), FixtureError> {
    command_succeeds(camber_command(dir, &["context"]), "camber context").map(|_| ())
}

/// Run `cargo <args>` in `project_dir` and fail with its stderr unless it
/// succeeds. `context` names the inputs, so a failure says what it compiled.
pub fn cargo_succeeds(
    project_dir: &Path,
    cargo_args: &[&str],
    context: &str,
) -> Result<(), FixtureError> {
    let mut command = Command::new("cargo");
    command.args(cargo_args).current_dir(project_dir);
    let description = format!("cargo {} for {context}", cargo_args.join(" "));
    command_succeeds(command, &description).map(|_| ())
}

/// The target directory Cargo resolves for `project_dir`, which is the
/// inherited `CARGO_TARGET_DIR` when one is set.
pub fn cargo_target_dir(project_dir: &Path) -> Result<PathBuf, FixtureError> {
    let mut command = Command::new("cargo");
    command
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(project_dir);
    let output = command_succeeds(command, "cargo metadata")?;
    if output.stdout_truncated {
        return Err(FixtureError::new(
            "cargo metadata output exceeded the capture limit",
        ));
    }
    let metadata = std::str::from_utf8(&output.stdout)
        .map_err(|error| FixtureError::new(format!("cargo metadata is not UTF-8: {error}")))?;
    target_directory(metadata)
}

/// The `target_directory` string in `cargo metadata` JSON. An escaped path
/// fails rather than being read raw.
pub fn target_directory(metadata: &str) -> Result<PathBuf, FixtureError> {
    let missing = || {
        FixtureError::new(format!(
            "cargo metadata named no target directory: {metadata}"
        ))
    };
    let (_, value) = metadata
        .split_once("\"target_directory\":\"")
        .ok_or_else(missing)?;
    let end = value.find(['"', '\\']).ok_or_else(missing)?;
    match value[end..].starts_with('"') {
        true => Ok(PathBuf::from(&value[..end])),
        false => Err(FixtureError::new(format!(
            "cargo metadata target directory holds a JSON escape after `{}`",
            &value[..end]
        ))),
    }
}
