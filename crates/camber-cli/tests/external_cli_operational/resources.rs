use std::io::Write;
use std::path::{Path, PathBuf};

use crate::support::FixtureError;

const RUN_ID_ENVIRONMENT: &str = "CAMBER_EXTERNAL_RUN_ID";
const CLEANUP_WITNESS_ENVIRONMENT: &str = "CAMBER_EXTERNAL_CLEANUP_WITNESS";
const MAX_RUN_ID_BYTES: usize = 64;

pub struct ExternalRun {
    run_id: Box<str>,
}

impl ExternalRun {
    pub fn from_environment() -> Result<Self, FixtureError> {
        let run_id = environment(RUN_ID_ENVIRONMENT)?;
        Self::parse(&run_id)
    }

    pub fn parse(run_id: &str) -> Result<Self, FixtureError> {
        let valid_length = !run_id.is_empty() && run_id.len() <= MAX_RUN_ID_BYTES;
        let valid_alphabet = run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));

        match (valid_length, valid_alphabet) {
            (true, true) => Ok(Self {
                run_id: run_id.into(),
            }),
            _ => Err(FixtureError::new(
                "CAMBER_EXTERNAL_RUN_ID must contain 1-64 URL-safe ASCII characters",
            )),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn compile_project_name(&self) -> Box<str> {
        format!("camber-external-compile-{}", self.run_id).into_boxed_str()
    }

    pub fn docker_image_tag(&self) -> Box<str> {
        format!("camber-external-cli:{}", self.run_id).into_boxed_str()
    }
}

pub trait CleanupCompletion {
    fn emit(&mut self, run_id: &str, resource: &str) -> Result<(), FixtureError>;
}

pub struct CleanupWitness {
    path: PathBuf,
}

impl CleanupWitness {
    pub fn from_environment() -> Result<Self, FixtureError> {
        let configured = environment(CLEANUP_WITNESS_ENVIRONMENT)?;
        let path = PathBuf::from(configured);
        validate_witness_path(&path)?;
        Ok(Self { path })
    }
}

impl CleanupCompletion for CleanupWitness {
    fn emit(&mut self, run_id: &str, resource: &str) -> Result<(), FixtureError> {
        validate_witness_value(run_id, "run ID")?;
        validate_witness_value(resource, "resource identity")?;

        let document = format!(
            "{{\"run_id\":\"{run_id}\",\"resources\":[\"{resource}\"],\"cleanup_status\":\"completed\"}}\n"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

pub fn close_temp_dir_and_emit<W>(
    temp_dir: tempfile::TempDir,
    mut witness: W,
    run: &ExternalRun,
    resource: &str,
) -> Result<(), FixtureError>
where
    W: CleanupCompletion,
{
    let removed_path = temp_dir.path().to_path_buf();
    temp_dir.close()?;

    match removed_path.try_exists()? {
        false => witness.emit(run.run_id(), resource),
        true => Err(FixtureError::new(format!(
            "temporary project still exists after cleanup: {}",
            removed_path.display()
        ))),
    }
}

fn environment(variable: &'static str) -> Result<String, FixtureError> {
    std::env::var(variable).map_err(|error| {
        FixtureError::new(format!(
            "{variable} must be set to valid Unicode for a selected external test: {error}"
        ))
    })
}

fn validate_witness_path(path: &Path) -> Result<(), FixtureError> {
    let parent_is_directory = path.parent().is_some_and(Path::is_dir);
    let has_file_name = path.file_name().is_some_and(|name| !name.is_empty());
    let destination_is_absent = !path.try_exists()?;

    match (
        path.is_absolute(),
        parent_is_directory,
        has_file_name,
        destination_is_absent,
    ) {
        (true, true, true, true) => Ok(()),
        _ => Err(FixtureError::new(
            "CAMBER_EXTERNAL_CLEANUP_WITNESS must be an unused absolute file path in an existing directory",
        )),
    }
}

fn validate_witness_value(value: &str, description: &str) -> Result<(), FixtureError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'));

    match valid {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "cleanup witness {description} contains unsafe characters"
        ))),
    }
}
