use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::{FixtureError, run_command};

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

fn camber_crate_path() -> Result<PathBuf, FixtureError> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no parent"))?
        .join("camber"))
}

fn camber_build_crate_path() -> Result<PathBuf, FixtureError> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| FixtureError::new("camber-cli manifest has no parent"))?
        .join("camber-build"))
}

fn patch_local_crates(project_dir: &Path) -> Result<(), FixtureError> {
    let config_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&config_dir)?;
    let patch = format!(
        "[patch.crates-io]\ncamber = {{ path = \"{}\" }}\ncamber-build = {{ path = \"{}\" }}\n",
        camber_crate_path()?.display(),
        camber_build_crate_path()?.display(),
    );
    std::fs::write(config_dir.join("config.toml"), patch)?;
    Ok(())
}

fn run_camber_new(dir: &Path, name: &str, template: &str) -> Result<(), FixtureError> {
    let mut command = Command::new(camber_bin());
    command
        .args(["new", name, "--template", template])
        .current_dir(dir);
    let output = run_command(command)?;
    assert!(
        output.status.success(),
        "camber new --template {template} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn cargo_check(project_dir: &Path) -> Result<(), FixtureError> {
    let mut command = Command::new("cargo");
    command.args(["check"]).current_dir(project_dir);
    let check = run_command(command)?;
    assert!(
        check.status.success(),
        "cargo check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    Ok(())
}

fn cargo_check_features(project_dir: &Path, features: &str) -> Result<(), FixtureError> {
    let mut command = Command::new("cargo");
    command
        .args(["check", "--features", features])
        .current_dir(project_dir);
    let check = run_command(command)?;
    assert!(
        check.status.success(),
        "cargo check --features {features} failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    Ok(())
}

fn read_file(project_dir: &Path, relative: &str) -> Result<String, FixtureError> {
    Ok(std::fs::read_to_string(project_dir.join(relative))?)
}

fn assert_current_camber_requirement(project_dir: &Path) -> Result<(), FixtureError> {
    let manifest = read_file(project_dir, "Cargo.toml")?;
    assert!(
        manifest.contains("camber = \"0\""),
        "generated manifest should select the current pre-1.0 Camber release: {manifest}"
    );
    assert!(
        !manifest.contains("camber = \"0.1\""),
        "generated manifest retained the obsolete 0.1 requirement"
    );
    Ok(())
}

#[test]
fn http_template_compiles_and_runs() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let project_dir = dir.path().join("test-http");
    run_camber_new(dir.path(), "test-http", "http")?;
    patch_local_crates(&project_dir)?;

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");
    assert_current_camber_requirement(&project_dir)?;
    let main_rs = read_file(&project_dir, "src/main.rs")?;
    assert!(
        main_rs.contains("use_middleware"),
        "http template should demonstrate middleware"
    );
    assert!(
        main_rs.contains("param("),
        "http template should demonstrate path parameters"
    );
    assert!(
        main_rs.contains("http::get(") || main_rs.contains("http::post("),
        "http template should demonstrate outbound HTTP"
    );
    assert!(
        main_rs.contains("async {"),
        "http template should demonstrate async handlers"
    );
    cargo_check(&project_dir)?;
    Ok(())
}

#[test]
fn fanout_template_compiles_and_runs() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let project_dir = dir.path().join("test-fanout");
    run_camber_new(dir.path(), "test-fanout", "fanout")?;
    patch_local_crates(&project_dir)?;

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");
    assert_current_camber_requirement(&project_dir)?;
    let main_rs = read_file(&project_dir, "src/main.rs")?;
    assert!(
        main_rs.contains("spawn"),
        "fanout template should demonstrate spawn"
    );
    assert!(
        main_rs.contains("spawn_async"),
        "fanout template should demonstrate async fan-out"
    );
    assert!(
        main_rs.contains("http::get(") || main_rs.contains("http::post("),
        "fanout template should demonstrate outbound HTTP"
    );
    cargo_check(&project_dir)?;
    Ok(())
}

#[test]
fn advanced_template_compiles() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let project_dir = dir.path().join("test-advanced");
    run_camber_new(dir.path(), "test-advanced", "advanced")?;
    patch_local_crates(&project_dir)?;

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");
    assert_current_camber_requirement(&project_dir)?;
    let main_rs = read_file(&project_dir, "src/main.rs")?;
    assert!(
        main_rs.contains("grpc"),
        "advanced template should demonstrate gRPC"
    );
    assert!(
        main_rs.contains(".ws("),
        "advanced template should demonstrate WebSocket"
    );
    assert!(
        main_rs.contains(".proxy("),
        "advanced template should demonstrate proxy"
    );
    assert!(
        main_rs.contains("async {"),
        "advanced template should demonstrate async handlers"
    );
    assert!(
        main_rs.contains("use_middleware("),
        "advanced template should demonstrate async middleware"
    );
    assert!(
        project_dir.join("build.rs").exists(),
        "advanced template needs build.rs for protobuf"
    );
    let manifest = read_file(&project_dir, "Cargo.toml")?;
    assert!(
        manifest.contains("camber-build = \"0\""),
        "advanced template should select the current pre-1.0 build helper: {manifest}"
    );
    let build_rs = read_file(&project_dir, "build.rs")?;
    assert!(build_rs.contains("std::io::Result<()>"));
    assert!(!build_rs.contains("Box<dyn"));
    let proto_files: Vec<_> = std::fs::read_dir(project_dir.join("proto"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "proto"))
        .collect();
    assert!(
        !proto_files.is_empty(),
        "advanced template should include a .proto file"
    );
    cargo_check_features(&project_dir, "ws,grpc")?;
    Ok(())
}

#[test]
fn unknown_template_returns_error() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let mut command = Command::new(camber_bin());
    command
        .args(["new", "test-bad", "--template", "nonexistent"])
        .current_dir(dir.path());
    let output = run_command(command)?;
    assert!(
        !output.status.success(),
        "camber new with unknown template should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("http") && stderr.contains("fanout") && stderr.contains("advanced"),
        "error should list available templates, got: {stderr}"
    );
    Ok(())
}

#[test]
fn rejects_project_name_with_path_separator() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let mut command = Command::new(camber_bin());
    command
        .args(["new", "nested/project", "--template", "http"])
        .current_dir(dir.path());
    let output = run_command(command)?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must not contain path separators"));
    assert!(!dir.path().join("nested").exists());
    Ok(())
}

#[test]
fn rejects_invalid_cargo_package_name() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let mut command = Command::new(camber_bin());
    command
        .args(["new", "bad name", "--template", "http"])
        .current_dir(dir.path());
    let output = run_command(command)?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not a valid Cargo package name"));
    assert!(!dir.path().join("bad name").exists());
    Ok(())
}
