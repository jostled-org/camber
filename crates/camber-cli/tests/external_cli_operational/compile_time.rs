use std::path::Path;
use std::time::Instant;

use crate::resources::{CleanupWitness, ExternalRun, close_temp_dir_and_emit};
use crate::support::FixtureError;
use crate::support::generated_project::{camber_new, cargo_succeeds, patch_local_crates};

fn measure_incremental_build(root: &Path, project_name: &str) -> Result<(), FixtureError> {
    let project_dir = root.join(project_name);

    camber_new(root, project_name, "http")?;
    patch_local_crates(&project_dir)?;
    cargo_succeeds(&project_dir, &["build"], "the initial build")?;

    let main_rs = project_dir.join("src/main.rs");
    let source = std::fs::read_to_string(&main_rs)?;
    let modified = source.replace("\"Hello, world!\"", "\"Hello, incremental!\"");
    std::fs::write(&main_rs, modified)?;

    let start = Instant::now();
    cargo_succeeds(&project_dir, &["build"], "the incremental build")?;
    let elapsed = start.elapsed();

    match elapsed.as_secs() < 5 {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "incremental compile took {elapsed:?}, exceeds 5-second ceiling"
        ))),
    }
}

#[test]
#[ignore = "controlled-host measurement; owner: Camber CLI maintainers; run: cargo test -p camber-cli --test external_cli_operational external_compile_time::incremental_compile_under_5_seconds -- --exact --ignored"]
fn incremental_compile_under_5_seconds() -> Result<(), FixtureError> {
    let run = ExternalRun::from_environment()?;
    let witness = CleanupWitness::from_environment()?;
    let project_name = run.compile_project_name();
    let temp_dir = tempfile::tempdir()?;
    let execution = measure_incremental_build(temp_dir.path(), &project_name);

    close_temp_dir_and_emit(temp_dir, witness, &run, &project_name)?;
    execution
}
