use camber::secret::{SecretRef, load_secret};
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn loads_token_from_env_var() {
    let expected = std::env::var("HOME").expect("HOME must be set");

    let result = load_secret(&SecretRef::Env("HOME".into()));

    assert_eq!(&*result.unwrap(), expected.trim());
}

#[test]
fn loads_token_from_file() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(b"secret456\n").expect("write");

    let path: Box<str> = file.path().to_str().expect("utf8 path").into();
    let result = load_secret(&SecretRef::File(path));

    assert_eq!(&*result.unwrap(), "secret456");
}

#[test]
fn missing_env_var_returns_error() {
    let result = load_secret(&SecretRef::Env("NONEXISTENT_VAR_12345".into()));

    let error = result.unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("NONEXISTENT_VAR_12345"),
        "error should name the variable: {message}"
    );
}

#[test]
fn missing_file_returns_error() {
    let result = load_secret(&SecretRef::File("/tmp/nonexistent_token_file".into()));

    let error = result.unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("nonexistent_token_file"),
        "error should name the file: {message}"
    );
}
