use std::cell::Cell;
use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::support::FixtureError;
use crate::support::address_process::AddressChild;

const RUN_ID_ENVIRONMENT: &str = "CAMBER_EXTERNAL_RUN_ID";
const CLEANUP_WITNESS_ENVIRONMENT: &str = "CAMBER_EXTERNAL_CLEANUP_WITNESS";
const MAX_RUN_ID_BYTES: usize = 64;
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct ExternalInvocation {
    run: ExternalRun,
    witness: Option<CleanupWitness>,
    lease: UniqueTree,
    releases: Vec<ResourceRelease>,
    finalized: bool,
}

impl ExternalInvocation {
    pub fn start(command_name: &str) -> Result<Self, FixtureError> {
        let run = ExternalRun::from_environment()?;
        let witness = CleanupWitness::from_environment()?;
        let lease_name = run.unique_name(command_name);
        let lease = UniqueTree::create(&lease_name)?;
        Ok(Self {
            run,
            witness: Some(witness),
            lease,
            releases: Vec::new(),
            finalized: false,
        })
    }

    pub fn unique_name(&self, purpose: &str) -> Box<str> {
        self.run.unique_name(purpose)
    }

    pub fn track_listener(&mut self, purpose: &str, addr: SocketAddr) -> ReleaseMarker {
        let unique_name = self.unique_name(purpose);
        self.track_resource(format!("tcp-listener:{unique_name}@{addr}").into_boxed_str())
    }

    pub fn track_tree(&mut self, purpose: &str, path: &Path) -> ReleaseMarker {
        let unique_name = self.unique_name(purpose);
        self.track_resource(
            format!("directory-tree:{unique_name}@{}", path.display()).into_boxed_str(),
        )
    }

    pub fn finish(mut self) -> Result<(), FixtureError> {
        self.cleanup_and_emit()
    }

    fn track_resource(&mut self, identity: Box<str>) -> ReleaseMarker {
        let released = Rc::new(Cell::new(false));
        self.releases.push(ResourceRelease {
            identity,
            released: Rc::clone(&released),
        });
        ReleaseMarker { released }
    }

    fn cleanup_and_emit(&mut self) -> Result<(), FixtureError> {
        match self.finalized {
            true => return Ok(()),
            false => {}
        }
        match self.releases.iter().find(|release| !release.released.get()) {
            Some(release) => {
                return Err(FixtureError::new(format!(
                    "external resource was not observably released: {}",
                    release.identity
                )));
            }
            None => {}
        }

        self.lease.remove()?;
        let resources = std::iter::once(self.lease.identity())
            .chain(
                self.releases
                    .iter()
                    .map(|release| release.identity.as_ref()),
            )
            .collect::<Vec<_>>();
        let witness = self
            .witness
            .take()
            .ok_or_else(|| FixtureError::new("cleanup witness was already consumed"))?;
        self.finalized = true;
        witness.emit(&self.run, &resources)
    }
}

impl Drop for ExternalInvocation {
    fn drop(&mut self) {
        if self.cleanup_and_emit().is_err() {
            abort_cleanup();
        }
    }
}

pub struct ObservedAddressChild {
    child: Option<AddressChild>,
    addr: SocketAddr,
    release: ReleaseMarker,
}

impl ObservedAddressChild {
    pub fn new(child: AddressChild, addr: SocketAddr, release: ReleaseMarker) -> Self {
        Self {
            child: Some(child),
            addr,
            release,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(mut self) -> Result<(), FixtureError> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), FixtureError> {
        match self.release.is_released() {
            true => return Ok(()),
            false => {}
        }
        match self.child.as_mut() {
            Some(child) => child.terminate()?,
            None => {}
        }
        drop(self.child.take());
        wait_for_listener_release(self.addr, RELEASE_TIMEOUT)?;
        self.release.mark_released();
        Ok(())
    }
}

impl Drop for ObservedAddressChild {
    fn drop(&mut self) {
        if self.cleanup().is_err() {
            abort_cleanup();
        }
    }
}

pub struct UniqueTree {
    root: PathBuf,
    identity: Box<str>,
    removed: bool,
}

impl UniqueTree {
    pub fn create(name: &str) -> Result<Self, FixtureError> {
        let unique_name = crate::support::unique::build(name);
        let root = std::env::temp_dir().join(&*unique_name);
        std::fs::create_dir(&root)?;
        let identity = format!("temp-tree:{}", root.display()).into_boxed_str();
        Ok(Self {
            root,
            identity,
            removed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn remove(&mut self) -> Result<(), FixtureError> {
        match self.removed {
            true => return Ok(()),
            false => {}
        }
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match self.root.try_exists()? {
            true => Err(FixtureError::new(format!(
                "temporary tree still exists after removal: {}",
                self.root.display()
            ))),
            false => {
                self.removed = true;
                Ok(())
            }
        }
    }

    fn identity(&self) -> &str {
        &self.identity
    }
}

impl Drop for UniqueTree {
    fn drop(&mut self) {
        if self.remove().is_err() {
            abort_cleanup();
        }
    }
}

pub struct ReleaseMarker {
    released: Rc<Cell<bool>>,
}

impl ReleaseMarker {
    pub fn mark_released(&self) {
        self.released.set(true);
    }

    fn is_released(&self) -> bool {
        self.released.get()
    }
}

struct ResourceRelease {
    identity: Box<str>,
    released: Rc<Cell<bool>>,
}

struct ExternalRun {
    run_id: Box<str>,
    encoded_run_id: Box<str>,
}

impl ExternalRun {
    fn from_environment() -> Result<Self, FixtureError> {
        let run_id = required_environment(RUN_ID_ENVIRONMENT)?;
        Self::parse(&run_id)
    }

    fn parse(run_id: &str) -> Result<Self, FixtureError> {
        let valid_length = !run_id.is_empty() && run_id.len() <= MAX_RUN_ID_BYTES;
        let valid_alphabet = run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        match (valid_length, valid_alphabet) {
            (true, true) => Ok(Self {
                run_id: run_id.into(),
                encoded_run_id: hex_encode(run_id.as_bytes()),
            }),
            _ => Err(FixtureError::new(
                "CAMBER_EXTERNAL_RUN_ID must contain 1-64 URL-safe ASCII characters",
            )),
        }
    }

    fn unique_name(&self, purpose: &str) -> Box<str> {
        crate::support::unique::external_resource(&format!(
            "camber-{purpose}-{}",
            self.encoded_run_id
        ))
    }
}

struct CleanupWitness {
    path: PathBuf,
}

impl CleanupWitness {
    fn from_environment() -> Result<Self, FixtureError> {
        let path = PathBuf::from(required_environment(CLEANUP_WITNESS_ENVIRONMENT)?);
        validate_witness_path(&path)?;
        Ok(Self { path })
    }

    fn emit(self, run: &ExternalRun, resources: &[&str]) -> Result<(), FixtureError> {
        match resources.is_empty() {
            true => {
                return Err(FixtureError::new(
                    "cleanup witness has no resource identities",
                ));
            }
            false => {}
        }
        let document = WitnessDocument {
            run_id: &run.run_id,
            resources,
            cleanup_status: "completed",
        };
        let mut encoded = serde_json::to_vec(&document)?;
        encoded.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct WitnessDocument<'a> {
    run_id: &'a str,
    resources: &'a [&'a str],
    cleanup_status: &'static str,
}

fn required_environment(variable: &'static str) -> Result<String, FixtureError> {
    std::env::var(variable).map_err(|error| {
        FixtureError::new(format!(
            "{variable} must be set to valid Unicode for a selected external test: {error}"
        ))
    })
}

fn validate_witness_path(path: &Path) -> Result<(), FixtureError> {
    let valid = path.is_absolute()
        && path.parent().is_some_and(Path::is_dir)
        && path.file_name().is_some_and(|name| !name.is_empty())
        && !path.try_exists()?;
    match valid {
        true => Ok(()),
        false => Err(FixtureError::new(
            "CAMBER_EXTERNAL_CLEANUP_WITNESS must be an unused absolute path in an existing directory",
        )),
    }
}

fn wait_for_listener_release(addr: SocketAddr, timeout: Duration) -> Result<(), FixtureError> {
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Err(FixtureError::new(format!(
            "listener {addr} still accepted connections after child teardown"
        ))),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => Ok(()),
        Err(error) => Err(FixtureError::new(format!(
            "listener {addr} release could not be observed after child teardown: {error}"
        ))),
    }
}

fn hex_encode(input: &[u8]) -> Box<str> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(input.len() * 2);
    input.iter().for_each(|byte| {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    });
    encoded.into_boxed_str()
}

fn abort_cleanup() -> ! {
    std::process::abort()
}
