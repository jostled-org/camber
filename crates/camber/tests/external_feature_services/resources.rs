use std::io::Write;
use std::path::{Path, PathBuf};

const RUN_ID_ENVIRONMENT: &str = "CAMBER_EXTERNAL_RUN_ID";
const CLEANUP_WITNESS_ENVIRONMENT: &str = "CAMBER_EXTERNAL_CLEANUP_WITNESS";
const MAX_RUN_ID_BYTES: usize = 64;
const DNS_HEX_LABEL_BYTES: usize = 48;

#[derive(Debug, thiserror::Error)]
pub enum ExternalResourceError {
    #[error("{variable} must be set to valid Unicode for a selected external test")]
    Environment {
        variable: &'static str,
        #[source]
        source: std::env::VarError,
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
        let run_id = environment(RUN_ID_ENVIRONMENT)?;
        Self::parse(&run_id)
    }

    pub fn parse(run_id: &str) -> Result<Self, ExternalResourceError> {
        let valid_length = !run_id.is_empty() && run_id.len() <= MAX_RUN_ID_BYTES;
        let valid_alphabet = run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));

        match (valid_length, valid_alphabet) {
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
        let configured = environment(CLEANUP_WITNESS_ENVIRONMENT)?;
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

#[derive(serde::Serialize)]
struct WitnessDocument<'a> {
    run_id: &'a str,
    resources: &'a [&'a str],
    cleanup_status: &'static str,
}

fn environment(variable: &'static str) -> Result<String, ExternalResourceError> {
    std::env::var(variable)
        .map_err(|source| ExternalResourceError::Environment { variable, source })
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
