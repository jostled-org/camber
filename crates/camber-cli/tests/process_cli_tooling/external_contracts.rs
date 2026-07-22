use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::docker_support::{DockerImageGuard, ImageOperations, ImagePresence};
use crate::resources::{CleanupCompletion, ExternalRun, close_temp_dir_and_emit};
use crate::support::FixtureError;

#[test]
fn external_run_derives_command_specific_names() -> Result<(), FixtureError> {
    let run = ExternalRun::parse("Run_7-alpha")?;
    let project = run.compile_project_name();
    let image = run.docker_image_tag();

    assert_eq!(project.as_ref(), "camber-external-compile-Run_7-alpha");
    assert_eq!(image.as_ref(), "camber-external-cli:Run_7-alpha");
    Ok(())
}

#[test]
fn external_run_rejects_unsafe_or_empty_identity() {
    assert!(ExternalRun::parse("").is_err());
    assert!(ExternalRun::parse("run/with/path").is_err());
    assert!(ExternalRun::parse(&"x".repeat(65)).is_err());
}

#[test]
fn temp_dir_witness_observes_completed_removal() -> Result<(), FixtureError> {
    let run = ExternalRun::parse("temp-contract")?;
    let temp_dir = tempfile::tempdir()?;
    let removed_path = temp_dir.path().to_path_buf();
    let witness = RecordingWitness::new(Rc::new(RefCell::new(Vec::new())), removed_path);

    close_temp_dir_and_emit(temp_dir, witness, &run, run.compile_project_name().as_ref())?;
    Ok(())
}

#[test]
fn docker_guard_removes_then_inspects_then_emits() -> Result<(), FixtureError> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let run = ExternalRun::parse("guard-success")?;
    let operations = RecordingImageOperations::absent(Rc::clone(&events));
    let witness = RecordingWitness::new(Rc::clone(&events), PathBuf::new());
    let mut guard = DockerImageGuard::new(run, operations, witness);

    guard.cleanup()?;

    assert_eq!(&*events.borrow(), &["remove", "inspect", "witness"]);
    Ok(())
}

#[test]
fn docker_guard_cleans_during_unwind() -> Result<(), FixtureError> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let run = ExternalRun::parse("guard-panic")?;
    let operations = RecordingImageOperations::absent(Rc::clone(&events));
    let witness = RecordingWitness::new(Rc::clone(&events), PathBuf::new());

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = DockerImageGuard::new(run, operations, witness);
        std::hint::black_box(&guard);
        std::panic::resume_unwind(Box::new("contract panic"));
    }));

    assert!(unwind.is_err());
    assert_eq!(&*events.borrow(), &["remove", "inspect", "witness"]);
    Ok(())
}

#[test]
fn docker_guard_withholds_witness_while_image_exists() -> Result<(), FixtureError> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let run = ExternalRun::parse("guard-present")?;
    let operations = RecordingImageOperations::present(Rc::clone(&events));
    let witness = RecordingWitness::new(Rc::clone(&events), PathBuf::new());
    let mut guard = DockerImageGuard::new(run, operations, witness);

    assert!(guard.cleanup().is_err());
    assert_eq!(&*events.borrow(), &["remove", "inspect"]);
    Ok(())
}

struct RecordingImageOperations {
    events: Rc<RefCell<Vec<&'static str>>>,
    presence: ImagePresence,
}

impl RecordingImageOperations {
    fn absent(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            events,
            presence: ImagePresence::Absent,
        }
    }

    fn present(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            events,
            presence: ImagePresence::Present,
        }
    }
}

impl ImageOperations for RecordingImageOperations {
    fn remove(&mut self, image: &str) -> Result<(), FixtureError> {
        assert!(image.starts_with("camber-external-cli:"));
        self.events.borrow_mut().push("remove");
        Ok(())
    }

    fn inspect(&mut self, image: &str) -> Result<ImagePresence, FixtureError> {
        assert!(image.starts_with("camber-external-cli:"));
        self.events.borrow_mut().push("inspect");
        Ok(self.presence)
    }
}

struct RecordingWitness {
    events: Rc<RefCell<Vec<&'static str>>>,
    removed_path: PathBuf,
}

impl RecordingWitness {
    fn new(events: Rc<RefCell<Vec<&'static str>>>, removed_path: PathBuf) -> Self {
        Self {
            events,
            removed_path,
        }
    }
}

impl CleanupCompletion for RecordingWitness {
    fn emit(&mut self, run_id: &str, resource: &str) -> Result<(), FixtureError> {
        assert!(!run_id.is_empty());
        assert!(!resource.is_empty());
        assert!(!self.removed_path.try_exists()?);
        self.events.borrow_mut().push("witness");
        Ok(())
    }
}
