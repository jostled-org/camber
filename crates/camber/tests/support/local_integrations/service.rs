//! One run's local services: network, containers, root, and their teardown.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::FixtureError;
use super::engine::{Engine, excerpt};
use super::readiness::Readiness;
use crate::temp_support::TempRoot;

/// The label naming the run that owns a network or container.
pub const RUN_LABEL: &str = "camber.local.run";
/// The label naming the logical service a container provides.
pub const SERVICE_LABEL: &str = "camber.local.service";

const MAX_NAME_BYTES: usize = 48;
const LOG_TAIL_LINES: &str = "200";

/// One configuration file written into the fixture root and mounted
/// read-only into the service's container.
#[derive(Clone, Debug)]
pub struct ServiceFile {
    pub name: Box<str>,
    pub contents: Box<[u8]>,
    pub container_path: Box<str>,
}

#[derive(Clone, Debug)]
pub struct ServiceSpec {
    pub service: Box<str>,
    /// A pinned image reference.
    pub image: Box<str>,
    /// The container port published on an engine-chosen loopback port.
    pub port: u16,
    pub arguments: Box<[Box<str>]>,
    pub environment: Box<[(Box<str>, Box<str>)]>,
    pub files: Box<[ServiceFile]>,
    pub readiness: Readiness,
}

impl ServiceSpec {
    pub fn new(service: &str, image: &str, port: u16, readiness: Readiness) -> Self {
        Self {
            service: service.into(),
            image: image.into(),
            port,
            arguments: Box::default(),
            environment: Box::default(),
            files: Box::default(),
            readiness,
        }
    }
}

/// The cleanup witness: every resource teardown removed and proved absent.
#[derive(Debug)]
pub struct Teardown {
    pub network: Option<Box<str>>,
    pub containers: Box<[Box<str>]>,
    pub root: Option<PathBuf>,
}

impl Teardown {
    fn nothing() -> Self {
        Self {
            network: None,
            containers: Box::default(),
            root: None,
        }
    }
}

/// A start that did not reach readiness, with the teardown it ran.
#[derive(Debug)]
pub struct StartFailure {
    pub error: Box<FixtureError>,
    pub cleanup: Result<Teardown, FixtureError>,
}

struct OwnedContainer {
    service: Box<str>,
    name: Box<str>,
    endpoint: Option<SocketAddr>,
}

pub struct LocalServices {
    engine: Engine,
    run_id: Box<str>,
    network: Option<Box<str>>,
    containers: Vec<OwnedContainer>,
    root: Option<TempRoot>,
}

impl LocalServices {
    /// Start every service on one run-scoped network and wait, inside one
    /// shared `readiness_bound`, for each to acknowledge its protocol.
    ///
    /// Any failure tears down whatever was started before it returns.
    pub fn start(
        engine: Engine,
        run_id: &str,
        specs: &[ServiceSpec],
        readiness_bound: Duration,
    ) -> Result<Self, StartFailure> {
        let root = admit(run_id, specs).and_then(|()| TempRoot::new().map_err(FixtureError::from));
        let root = match root {
            Ok(root) => root,
            Err(error) => {
                return Err(StartFailure {
                    error: Box::new(error),
                    cleanup: Ok(Teardown::nothing()),
                });
            }
        };
        let mut services = Self {
            engine,
            run_id: run_id.into(),
            network: None,
            containers: Vec::with_capacity(specs.len()),
            root: Some(root),
        };
        match services.launch(specs, readiness_bound) {
            Ok(()) => Ok(services),
            Err(error) => {
                let cleanup = services.teardown();
                Err(StartFailure {
                    error: Box::new(error),
                    cleanup,
                })
            }
        }
    }

    pub fn endpoint(&self, service: &str) -> Option<SocketAddr> {
        self.containers
            .iter()
            .find(|container| &*container.service == service)
            .and_then(|container| container.endpoint)
    }

    pub fn network(&self) -> Option<&str> {
        self.network.as_deref()
    }

    pub fn container_names(&self) -> Box<[Box<str>]> {
        self.containers
            .iter()
            .map(|container| container.name.clone())
            .collect()
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_ref().map(TempRoot::path)
    }

    /// Tear everything down and return the cleanup witness.
    pub fn finish(mut self) -> Result<Teardown, FixtureError> {
        self.teardown()
    }

    fn launch(&mut self, specs: &[ServiceSpec], bound: Duration) -> Result<(), FixtureError> {
        let deadline = Instant::now() + bound;
        let run_label = format!("{RUN_LABEL}={}", self.run_id);
        let network = format!("camber-local-{}", self.run_id);
        // Recorded before creation, so an ambiguous create is still reaped.
        self.network = Some(network.clone().into_boxed_str());
        self.engine.run(&[
            "network",
            "create",
            "--label",
            run_label.as_str(),
            network.as_str(),
        ])?;
        for spec in specs {
            self.launch_container(spec, &network, &run_label)?;
        }
        for (index, spec) in specs.iter().enumerate() {
            let endpoint = self.published_endpoint(index, spec)?;
            self.await_ready(index, spec, endpoint, deadline, bound)?;
        }
        Ok(())
    }

    fn launch_container(
        &mut self,
        spec: &ServiceSpec,
        network: &str,
        run_label: &str,
    ) -> Result<(), FixtureError> {
        let name = format!("camber-local-{}-{}", self.run_id, spec.service);
        let mounts = self.write_files(spec)?;
        let service_label = format!("{SERVICE_LABEL}={}", spec.service);
        let publication = format!("127.0.0.1::{}", spec.port);
        let environment: Box<[String]> = spec
            .environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let mut args: Vec<&str> = vec![
            "run",
            "-d",
            "--name",
            name.as_str(),
            "--network",
            network,
            "--label",
            run_label,
            "--label",
            service_label.as_str(),
            "-p",
            publication.as_str(),
        ];
        for variable in &environment {
            args.extend(["-e", variable.as_str()]);
        }
        for mount in &mounts {
            args.extend(["-v", mount.as_str()]);
        }
        args.push(&spec.image);
        args.extend(spec.arguments.iter().map(|argument| &**argument));
        // Recorded before the run, so a container the engine created before
        // failing is still reaped.
        self.containers.push(OwnedContainer {
            service: spec.service.clone(),
            name: name.as_str().into(),
            endpoint: None,
        });
        self.engine.run(&args).map(drop)
    }

    fn write_files(&self, spec: &ServiceSpec) -> Result<Box<[String]>, FixtureError> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| FixtureError::Input("fixture root is already removed".into()))?;
        spec.files
            .iter()
            .map(|file| {
                let simple = !file.name.is_empty()
                    && !file.name.contains('/')
                    && !matches!(&*file.name, "." | "..");
                if !simple {
                    return Err(FixtureError::Input(
                        format!("service file name {:?} is not a plain name", file.name).into(),
                    ));
                }
                let host = root.path().join(&*file.name);
                std::fs::write(&host, &file.contents)?;
                let host = host
                    .to_str()
                    .ok_or_else(|| FixtureError::Input("fixture root is not valid UTF-8".into()))?;
                Ok(format!("{host}:{}:ro", file.container_path))
            })
            .collect()
    }

    fn published_endpoint(
        &mut self,
        index: usize,
        spec: &ServiceSpec,
    ) -> Result<SocketAddr, FixtureError> {
        let container = self
            .containers
            .get_mut(index)
            .ok_or_else(|| FixtureError::Input("service was never launched".into()))?;
        let port = format!("{}/tcp", spec.port);
        let output = self
            .engine
            .run(&["port", &*container.name, port.as_str()])?;
        let endpoint = output
            .lines()
            .filter_map(|line| line.trim().parse::<SocketAddr>().ok())
            .find(|address| address.is_ipv4() && address.ip().is_loopback())
            .ok_or_else(|| FixtureError::Endpoint {
                service: spec.service.clone(),
                output: excerpt(&output),
            })?;
        container.endpoint = Some(endpoint);
        Ok(endpoint)
    }

    fn await_ready(
        &self,
        index: usize,
        spec: &ServiceSpec,
        endpoint: SocketAddr,
        deadline: Instant,
        bound: Duration,
    ) -> Result<(), FixtureError> {
        spec.readiness
            .await_ready(endpoint, deadline)
            .map_err(|detail| FixtureError::Readiness {
                service: spec.service.clone(),
                endpoint,
                bound,
                detail,
                logs: self.logs(index),
            })
    }

    fn logs(&self, index: usize) -> Box<str> {
        let Some(container) = self.containers.get(index) else {
            return Box::default();
        };
        self.engine
            .transcript(&["logs", "--tail", LOG_TAIL_LINES, &*container.name])
            .unwrap_or_else(|error| error.to_string().into_boxed_str())
    }

    fn teardown(&mut self) -> Result<Teardown, FixtureError> {
        let mut residue: Vec<Box<str>> = Vec::new();
        let containers: Box<[Box<str>]> = self
            .containers
            .drain(..)
            .map(|container| container.name)
            .collect();
        for name in containers.iter().map(|name| &**name) {
            self.reap(
                &["rm", "-f", "-v", name],
                &["container", "inspect", name],
                name,
                &mut residue,
            );
        }
        let network = self.network.take();
        if let Some(network) = network.as_deref() {
            self.reap(
                &["network", "rm", network],
                &["network", "inspect", network],
                network,
                &mut residue,
            );
        }
        let root = self.root.take().map(|root| {
            let path = root.path().to_path_buf();
            if let Err(error) = root.close() {
                residue.push(format!("{}: {error}", path.display()).into_boxed_str());
            }
            path
        });
        match residue.is_empty() {
            true => Ok(Teardown {
                network,
                containers,
                root,
            }),
            false => Err(FixtureError::Residue(residue.into_boxed_slice())),
        }
    }

    /// Remove one resource, then prove the engine no longer knows it.
    fn reap(&self, remove: &[&str], inspect: &[&str], name: &str, residue: &mut Vec<Box<str>>) {
        let removal = match self.engine.run(remove) {
            Ok(_) => "the engine reported success".to_owned(),
            Err(error) => error.to_string(),
        };
        match self.engine.run(inspect) {
            Err(FixtureError::Engine { output, .. })
                if output.to_ascii_lowercase().contains("no such") => {}
            Err(error) => residue.push(
                format!("{name}: absence unproven: {error}; removal: {removal}").into_boxed_str(),
            ),
            Ok(_) => residue.push(format!("{name} survived removal: {removal}").into_boxed_str()),
        }
    }
}

impl Drop for LocalServices {
    fn drop(&mut self) {
        let owns = self.network.is_some() || !self.containers.is_empty() || self.root.is_some();
        if owns && let Err(error) = self.teardown() {
            eprintln!("local service fixture teardown left residue: {error}");
        }
    }
}

fn admit(run_id: &str, specs: &[ServiceSpec]) -> Result<(), FixtureError> {
    if specs.is_empty() {
        return Err(FixtureError::Input(
            "a fixture starts at least one service".into(),
        ));
    }
    validate_name("run ID", run_id)?;
    specs
        .iter()
        .try_for_each(|spec| validate_name("service", &spec.service))
}

fn validate_name(kind: &str, name: &str) -> Result<(), FixtureError> {
    let valid_length = !name.is_empty() && name.len() <= MAX_NAME_BYTES;
    let valid_alphabet = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    match valid_length && valid_alphabet {
        true => Ok(()),
        false => Err(FixtureError::Input(
            format!("{kind} {name:?} must be 1-{MAX_NAME_BYTES} lowercase ASCII letters, digits, or hyphens").into(),
        )),
    }
}
