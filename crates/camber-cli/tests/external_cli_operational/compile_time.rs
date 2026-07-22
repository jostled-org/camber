use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::resources::{CleanupWitness, ExternalRun, close_temp_dir_and_emit};
use crate::support::{FixtureError, run_command};

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

fn camber_crate_path() -> Result<PathBuf, FixtureError> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no parent"))?
        .join("camber"))
}

fn camber_build_crate_path() -> Result<PathBuf, FixtureError> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no parent"))?
        .join("camber-build"))
}

fn patch_local_crates(project_dir: &Path) -> Result<(), FixtureError> {
    let config_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&config_dir)?;
    let patch = format!(
        "[patch.crates-io]\ncamber = {{ path = \"{}\" }}\ncamber-build = {{ path = \"{}\" }}\n",
        camber_crate_path()?.display(),
        camber_build_crate_path()?.display(),
    );
    std::fs::write(config_dir.join("config.toml"), patch)?;
    Ok(())
}

fn require_success(
    description: &str,
    output: crate::support::process::CommandOutput,
) -> Result<(), FixtureError> {
    match output.status.success() {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "{description}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

fn measure_incremental_build(root: &Path, project_name: &str) -> Result<(), FixtureError> {
    let project_dir = root.join(project_name);

    let mut command = Command::new(camber_bin());
    command
        .args(["new", project_name, "--template", "http"])
        .current_dir(root);
    require_success("camber new failed", run_command(command)?)?;
    patch_local_crates(&project_dir)?;

    let mut command = Command::new("cargo");
    command.args(["build"]).current_dir(&project_dir);
    require_success("initial build failed", run_command(command)?)?;

    let main_rs = project_dir.join("src/main.rs");
    let source = std::fs::read_to_string(&main_rs)?;
    let modified = source.replace("\"Hello, world!\"", "\"Hello, incremental!\"");
    std::fs::write(&main_rs, modified)?;

    let start = Instant::now();
    let mut command = Command::new("cargo");
    command.args(["build"]).current_dir(&project_dir);
    let incremental = run_command(command)?;
    let elapsed = start.elapsed();
    require_success("incremental build failed", incremental)?;

    match elapsed.as_secs() < 5 {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "incremental compile took {elapsed:?}, exceeds 5-second ceiling"
        ))),
    }
}

#[test]
#[ignore = "external lane compile_time; owner: Camber CLI maintainers; run: gh workflow run external-evidence.yml -f lane=compile_time"]
fn incremental_compile_under_5_seconds() -> Result<(), FixtureError> {
    let run = ExternalRun::from_environment()?;
    let witness = CleanupWitness::from_environment()?;
    let project_name = run.compile_project_name();
    let temp_dir = tempfile::tempdir()?;
    let execution = measure_incremental_build(temp_dir.path(), &project_name);

    close_temp_dir_and_emit(temp_dir, witness, &run, &project_name)?;
    execution
}
