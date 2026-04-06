use crate::RuntimeError;
use std::path::Path;

/// Reference to a secret value stored outside the config file.
#[derive(Debug, Clone)]
pub enum SecretRef {
    /// Read from an environment variable.
    Env(Box<str>),
    /// Read from a file path.
    File(Box<str>),
}

/// Load a secret from the referenced source. Trims surrounding whitespace.
pub fn load_secret(secret_ref: &SecretRef) -> Result<Box<str>, RuntimeError> {
    match secret_ref {
        SecretRef::Env(var) => load_from_env(var),
        SecretRef::File(path) => load_from_file(Path::new(&**path)),
    }
}

fn load_from_env(var: &str) -> Result<Box<str>, RuntimeError> {
    match std::env::var(var) {
        Ok(value) => Ok(value.trim().into()),
        Err(std::env::VarError::NotPresent) => Err(RuntimeError::Secret(
            format!("environment variable {var} not set").into(),
        )),
        Err(std::env::VarError::NotUnicode(_)) => Err(RuntimeError::Secret(
            format!("environment variable {var} contains invalid Unicode").into(),
        )),
    }
}

fn load_from_file(path: &Path) -> Result<Box<str>, RuntimeError> {
    std::fs::read_to_string(path)
        .map(|v| v.trim().into())
        .map_err(|e| {
            RuntimeError::Secret(
                format!("failed to read secret from {}: {e}", path.display()).into(),
            )
        })
}
