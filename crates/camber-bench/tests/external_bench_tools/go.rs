use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::resources::{ExternalInvocation, ObservedAddressChild, ReleaseMarker, UniqueTree};
use crate::support::FixtureError;
use crate::support::address_process::{AddressChild, run_command};

const GO_BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const GO_READY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct GoBuild {
    tree: UniqueTree,
    binary: PathBuf,
    release: Option<ReleaseMarker>,
}

impl GoBuild {
    pub fn create(name: &str) -> Result<Self, FixtureError> {
        let tree = UniqueTree::create(name)?;
        let binary = tree.path().join("go-bench");
        Ok(Self {
            tree,
            binary,
            release: None,
        })
    }

    fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn root(&self) -> &Path {
        self.tree.path()
    }

    fn attach_release(&mut self, release: ReleaseMarker) {
        self.release = Some(release);
    }

    fn cleanup(&mut self) -> Result<(), FixtureError> {
        self.tree.remove()?;
        match self.release.as_ref() {
            Some(release) => release.mark_released(),
            None => {}
        }
        Ok(())
    }
}

impl Drop for GoBuild {
    fn drop(&mut self) {
        if self.cleanup().is_err() {
            std::process::abort();
        }
    }
}

fn build_go_server(invocation: &mut ExternalInvocation) -> Result<GoBuild, FixtureError> {
    let go = crate::support::tool::find_executable("go")
        .ok_or_else(|| FixtureError::new("external go lane requires the Go toolchain on PATH"))?;
    let build_name = invocation.unique_name("go-build");
    let mut build = GoBuild::create(&build_name)?;
    let release = invocation.track_tree("go-build", build.root());
    build.attach_release(release);
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("go");
    let mut command = Command::new(go);
    command
        .args(["build", "-o"])
        .arg(build.binary())
        .arg("main.go")
        .current_dir(source_dir);
    let (status, output) = run_command(&mut command, GO_BUILD_TIMEOUT)?;
    match status.success() {
        true => Ok(build),
        false => Err(FixtureError::new(format!(
            "go build failed: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

pub fn run_go_server_response_check() -> Result<(), FixtureError> {
    let mut invocation = ExternalInvocation::start("go-server-responds-if-go-available")?;
    let build = build_go_server(&mut invocation)?;
    let mut command = Command::new(build.binary());
    command.args(["--bench", "hello_text", "--port", "0"]);
    let mut child = AddressChild::spawn(&mut command)?;
    let addr = child.wait_for_address(GO_READY_TIMEOUT)?;
    let release = invocation.track_listener("go-server", addr);
    let server = ObservedAddressChild::new(child, addr, release);
    let response = crate::support::http::get(addr, "/", Duration::from_secs(5))?;
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"Hello, world!");
    server.shutdown()?;
    drop(build);
    invocation.finish()
}
