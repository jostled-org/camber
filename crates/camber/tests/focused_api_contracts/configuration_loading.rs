use std::io::Write;

use camber::config::load_config;
use tempfile::NamedTempFile;

fn write_toml(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(content.as_bytes()).expect("write");
    f
}

#[test]
fn load_config_parses_toml_file() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        name: Box<str>,
        port: u16,
    }

    let f = write_toml(
        r#"
name = "test-app"
port = 8080
"#,
    );

    let config: TestConfig = load_config(f.path()).unwrap();
    assert_eq!(&*config.name, "test-app");
    assert_eq!(config.port, 8080);
}

#[test]
fn load_config_returns_error_on_missing_file() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        _name: Box<str>,
    }

    let result = load_config::<TestConfig>(std::path::Path::new("/nonexistent/config.toml"));
    assert!(result.is_err());
}

#[test]
fn load_config_returns_error_on_invalid_toml() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        _name: Box<str>,
    }

    let f = write_toml("this is not valid = = = toml [[[");

    let result = load_config::<TestConfig>(f.path());
    assert!(result.is_err());
}
