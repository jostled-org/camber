use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const RUN_ID_ENVIRONMENT: &str = "CAMBER_EXTERNAL_RUN_ID";
const CLEANUP_WITNESS_ENVIRONMENT: &str = "CAMBER_EXTERNAL_CLEANUP_WITNESS";
const MAX_RUN_ID_BYTES: usize = 64;
const DNS_HEX_LABEL_BYTES: usize = 48;
const MAX_SQS_QUEUE_BYTES: usize = 80;
const MAX_NATS_STREAM_BYTES: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum ExternalResourceError {
    #[error("{variable} must be set to valid Unicode for a selected external test")]
    Environment {
        variable: &'static str,
        #[source]
        source: std::env::VarError,
    },
    #[error("{variable} is not a socket address")]
    Address {
        variable: &'static str,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("invalid external run ID: {0}")]
    RunId(Box<str>),
    #[error("invalid external resource name: {0}")]
    ResourceName(Box<str>),
    #[error("invalid cleanup witness path: {0}")]
    WitnessPath(Box<str>),
    #[error("cleanup witness serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cleanup witness I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub struct ExternalRun {
    run_id: Box<str>,
    encoded_run_id: Box<str>,
}

impl ExternalRun {
    pub fn from_environment() -> Result<Self, ExternalResourceError> {
        let run_id = lane_variable(RUN_ID_ENVIRONMENT)?;
        Self::parse(&run_id)
    }

    pub fn parse(run_id: &str) -> Result<Self, ExternalResourceError> {
        let valid_length = !run_id.is_empty() && run_id.len() <= MAX_RUN_ID_BYTES;

        match (valid_length, url_safe(run_id)) {
            (true, true) => Ok(Self {
                run_id: run_id.into(),
                encoded_run_id: hex_encode(run_id.as_bytes()),
            }),
            _ => Err(ExternalResourceError::RunId(
                "expected 1-64 URL-safe ASCII characters".into(),
            )),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn nats_subject(&self, purpose: &str) -> Box<str> {
        format!("camber.test.{purpose}.{}", self.encoded_run_id).into_boxed_str()
    }

    pub fn nats_queue_group(&self, purpose: &str) -> Box<str> {
        format!("camber-workers-{purpose}-{}", self.encoded_run_id).into_boxed_str()
    }

    /// A Standard queue name unique to this run: letters, digits, `-`, and
    /// `_`, at most 80 bytes.
    pub fn sqs_queue(&self, purpose: &str) -> Result<Box<str>, ExternalResourceError> {
        self.scoped_name(
            purpose,
            MAX_SQS_QUEUE_BYTES,
            "derived SQS queue name is not a valid Standard queue name",
        )
    }

    /// A JetStream stream name unique to this run: letters, digits, `-`, and
    /// `_`, at most 255 bytes, so no subject token, wildcard, or path
    /// separator can reach the server.
    pub fn nats_stream(&self, purpose: &str) -> Result<Box<str>, ExternalResourceError> {
        self.scoped_name(
            purpose,
            MAX_NATS_STREAM_BYTES,
            "derived JetStream stream name is not a valid stream name",
        )
    }

    /// `camber-<purpose>-<run>`, when `purpose` is non-empty and the name is
    /// URL-safe and at most `max_bytes` long.
    ///
    /// # Errors
    ///
    /// [`ExternalResourceError::ResourceName`] with `refusal` otherwise.
    fn scoped_name(
        &self,
        purpose: &str,
        max_bytes: usize,
        refusal: &'static str,
    ) -> Result<Box<str>, ExternalResourceError> {
        let name = format!("camber-{purpose}-{}", self.run_id);
        match (
            !purpose.is_empty(),
            name.len() <= max_bytes,
            url_safe(&name),
        ) {
            (true, true, true) => Ok(name.into_boxed_str()),
            _ => Err(ExternalResourceError::ResourceName(refusal.into())),
        }
    }

    pub fn dns_subdomain(&self, domain: &str) -> Result<Box<str>, ExternalResourceError> {
        let domain = normalized_domain(domain)?;
        let mut resource = String::with_capacity(self.encoded_run_id.len() + domain.len() + 9);
        resource.push_str("camber-");

        for (index, chunk) in self
            .encoded_run_id
            .as_bytes()
            .chunks(DNS_HEX_LABEL_BYTES)
            .enumerate()
        {
            match index {
                0 => {}
                _ => resource.push('.'),
            }
            resource.extend(chunk.iter().map(|byte| char::from(*byte)));
        }
        resource.push('.');
        resource.push_str(&domain);

        match resource.len() <= 253 {
            true => Ok(resource.into_boxed_str()),
            false => Err(ExternalResourceError::ResourceName(
                "derived DNS name exceeds 253 bytes".into(),
            )),
        }
    }
}

pub struct CleanupWitness {
    path: PathBuf,
}

impl CleanupWitness {
    pub fn from_environment() -> Result<Self, ExternalResourceError> {
        let configured = lane_variable(CLEANUP_WITNESS_ENVIRONMENT)?;
        let path = PathBuf::from(configured);
        validate_witness_path(&path)?;
        Ok(Self { path })
    }

    pub fn emit(self, run: &ExternalRun, resources: &[&str]) -> Result<(), ExternalResourceError> {
        match resources.is_empty() {
            true => {
                return Err(ExternalResourceError::ResourceName(
                    "cleanup witness requires at least one resource".into(),
                ));
            }
            false => {}
        }

        let witness = WitnessDocument {
            run_id: run.run_id(),
            resources,
            cleanup_status: "completed",
        };
        let mut encoded = serde_json::to_vec(&witness)?;
        encoded.push(b'\n');

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    }
}

/// The run ID and cleanup witness path every selected external test reads.
pub fn selected_run() -> Result<(ExternalRun, CleanupWitness), ExternalResourceError> {
    Ok((
        ExternalRun::from_environment()?,
        CleanupWitness::from_environment()?,
    ))
}

#[derive(serde::Serialize)]
struct WitnessDocument<'a> {
    run_id: &'a str,
    resources: &'a [&'a str],
    cleanup_status: &'static str,
}

/// The value the lane runner published in `variable`. There is no default: a
/// missing value fails the test rather than reach a service the run does not
/// own.
pub fn lane_variable(variable: &'static str) -> Result<String, ExternalResourceError> {
    std::env::var(variable)
        .map_err(|source| ExternalResourceError::Environment { variable, source })
}

/// The socket address the lane runner published in `variable`.
pub fn lane_address(variable: &'static str) -> Result<SocketAddr, ExternalResourceError> {
    lane_variable(variable)?
        .parse()
        .map_err(|source| ExternalResourceError::Address { variable, source })
}

/// Only ASCII letters, digits, `-`, and `_`.
fn url_safe(name: &str) -> bool {
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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

fn normalized_domain(domain: &str) -> Result<Box<str>, ExternalResourceError> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let labels_are_valid = domain.split('.').all(valid_dns_label);
    let domain_is_valid = !domain.is_empty() && domain.len() <= 253 && labels_are_valid;

    match domain_is_valid {
        true => Ok(domain.into_boxed_str()),
        false => Err(ExternalResourceError::ResourceName(
            "ACME_TEST_DOMAIN is not a safe ASCII DNS name".into(),
        )),
    }
}

fn valid_dns_label(label: &str) -> bool {
    let valid_length = !label.is_empty() && label.len() <= 63;
    let valid_edges = label
        .bytes()
        .next()
        .zip(label.bytes().next_back())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    let valid_alphabet = label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    valid_length && valid_edges && valid_alphabet
}

fn validate_witness_path(path: &Path) -> Result<(), ExternalResourceError> {
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
        _ => Err(ExternalResourceError::WitnessPath(
            "expected an unused absolute file path in an existing directory".into(),
        )),
    }
}
