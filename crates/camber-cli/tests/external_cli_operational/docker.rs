use std::process::Command;

use crate::resources::{CleanupCompletion, CleanupWitness, ExternalRun};
use crate::support::{FixtureError, run_command};

#[derive(Clone, Copy)]
pub enum ImagePresence {
    Absent,
    Present,
}

pub trait ImageOperations {
    fn remove(&mut self, image: &str) -> Result<(), FixtureError>;
    fn inspect(&mut self, image: &str) -> Result<ImagePresence, FixtureError>;
}

enum CleanupState {
    Armed,
    Attempted,
    Complete,
}

pub struct DockerImageGuard<O, W>
where
    O: ImageOperations,
    W: CleanupCompletion,
{
    run: ExternalRun,
    image: Box<str>,
    operations: O,
    witness: W,
    state: CleanupState,
}

impl<O, W> DockerImageGuard<O, W>
where
    O: ImageOperations,
    W: CleanupCompletion,
{
    pub fn new(run: ExternalRun, operations: O, witness: W) -> Self {
        let image = run.docker_image_tag();
        Self {
            run,
            image,
            operations,
            witness,
            state: CleanupState::Armed,
        }
    }

    fn image(&self) -> &str {
        &self.image
    }

    pub fn cleanup(&mut self) -> Result<(), FixtureError> {
        self.state = CleanupState::Attempted;
        let removal = self.operations.remove(&self.image);
        let inspection = self.operations.inspect(&self.image);

        let result = match inspection {
            Ok(ImagePresence::Absent) => self.witness.emit(self.run.run_id(), &self.image),
            Ok(ImagePresence::Present) => Err(image_remains_error(&self.image, removal.err())),
            Err(inspect_error) => Err(inspection_error(inspect_error, removal.err())),
        };
        self.state = match &result {
            Ok(()) => CleanupState::Complete,
            Err(_) => CleanupState::Armed,
        };
        result
    }
}

impl<O, W> Drop for DockerImageGuard<O, W>
where
    O: ImageOperations,
    W: CleanupCompletion,
{
    fn drop(&mut self) {
        match self.state {
            CleanupState::Armed => drop(self.cleanup()),
            CleanupState::Attempted | CleanupState::Complete => {}
        }
    }
}

fn image_remains_error(image: &str, removal_error: Option<FixtureError>) -> FixtureError {
    match removal_error {
        Some(error) => FixtureError::new(format!(
            "Docker image {image} remains after removal failed: {error}"
        )),
        None => FixtureError::new(format!(
            "Docker image inspect reports {image} still exists after removal"
        )),
    }
}

fn inspection_error(
    inspect_error: FixtureError,
    removal_error: Option<FixtureError>,
) -> FixtureError {
    match removal_error {
        Some(remove_error) => FixtureError::new(format!(
            "Docker image removal failed: {remove_error}; absence inspection failed: {inspect_error}"
        )),
        None => FixtureError::new(format!(
            "Docker image absence inspection failed: {inspect_error}"
        )),
    }
}

struct DockerOperations;

impl ImageOperations for DockerOperations {
    fn remove(&mut self, image: &str) -> Result<(), FixtureError> {
        let mut command = Command::new("docker");
        command.args(["image", "rm", "--force", image]);
        let output = run_command(command)?;

        match output.status.success() {
            true => Ok(()),
            false => Err(FixtureError::new(format!(
                "docker image rm failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))),
        }
    }

    fn inspect(&mut self, image: &str) -> Result<ImagePresence, FixtureError> {
        let mut command = Command::new("docker");
        command.args(["image", "inspect", image]);
        let output = run_command(command)?;

        match output.status.success() {
            true => Ok(ImagePresence::Present),
            false if inspection_reports_absence(&output.stderr) => Ok(ImagePresence::Absent),
            false => Err(FixtureError::new(format!(
                "docker image inspect did not prove absence: {}",
                String::from_utf8_lossy(&output.stderr)
            ))),
        }
    }
}

fn inspection_reports_absence(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("no such image") || stderr.contains("no such object")
}

fn project_root() -> Result<&'static std::path::Path, FixtureError> {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no parent"))?
        .parent()
        .ok_or_else(|| FixtureError::new("workspace root is absent"))
}

fn build_image(project_root: &std::path::Path, image: &str) -> Result<(), FixtureError> {
    match project_root.join("Dockerfile").exists() {
        true => {}
        false => return Err(FixtureError::new("Dockerfile not found at project root")),
    }

    let mut command = Command::new("docker");
    command
        .args(["build", "--tag", image, "."])
        .current_dir(project_root);
    let output = run_command(command)?;

    match output.status.success() {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "docker build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

pub fn run_dockerfile_build() -> Result<(), FixtureError> {
    let run = ExternalRun::from_environment()?;
    let witness = CleanupWitness::from_environment()?;
    let mut image = DockerImageGuard::new(run, DockerOperations, witness);
    let execution = project_root().and_then(|root| build_image(root, image.image()));

    image.cleanup()?;
    execution
}
